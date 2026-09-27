# ТЗ: Real-Time аудио-ядро (v2.0)

> **Префикс спеки: `A2.0`** (зарегистрирован в реестре `ROADMAP.md`).
> Референсы (эталон надёжного RT-аудио): MPD — «player thread → MusicPipe →
> output»; mpv — «decode → ao buffer → sound thread».
> - [MPD `src/player/Thread.cxx`](https://github.com/MusicPlayerDaemon/MPD/blob/master/src/player/Thread.cxx)
>   (player thread, MusicChunk/MusicPipe — lock-free SPSC)
> - [mpv `audio/out/ao.c`](https://github.com/mpv-player/mpv/blob/master/audio/out/ao.c)
>   (ao-buffer между декодером и звуковым потоком)

## 1. Общие положения

### 1.1. Назначение
Привести аудио-подсистему к детерминизму MPD/mpv:
- (а) строгая гарантия «0 аллокаций/сжатий в RT-колбэках»;
- (б) честная UI-индикация, когда bit-perfect не может быть гарантирован;
- (в) вынос декодирования Symphonia из RT-колбэка в отдельный поток
  (Producer → lock-free ring → Consumer) и устранение `std::sync::Mutex`
  из пути аудиоданных.

### 1.2. Область
- Модули: `src/audio/player.rs` (колбэки, `PlaybackCore`), `src/audio/output.rs`
  (`Resampler` — без изменений, только сверка границ), `src/audio/decoder.rs`
  (только чтение), `src/audio/dop.rs`, `src/audio/dsd.rs` — не менять;
  `src/app/ui_manager.rs`, `src/app/playback_manager.rs`, `ui/status.slint`
  (бейдж), `src/main.rs` (окно/init) — только при задаче §4.
- Каналы: PCM (f32/i16/u8/i32) и DoP (i32, Direct Output). Native DSD вне
  области (cpal не имеет бэкенда).
- Вне области: визуализатор (уже lock-free через `rtrb`), устройство-выбор.

### 1.3. Термины
| Термин | Значение |
|---|---|
| Producer | Поток-работник: декодирование + ресемплинг |
| Consumer | RT-колбэк `cpal`: чтение из ring → volume/dither → формат |
| Ring | Lock-free SPSC-очередь `rtrb::RingBuffer<f32>` |
| xrun/underrun | Пропуск кадров из-за превышения бюджета времени |
| Bit-perfect | Вывод без ресемплинга и софтверной громкости |
| Direct Output | DoP-путь: без volume/dither/resampler, лог. оригинальные DSD |

### 1.4. Принципы (обязательные)
- В `audio_callback_*` запрещены: аллокация, `std::sync::Mutex`, `unwrap()`,
  логирование, блокировка. Всё состояние из колбэка читается атомарно
  (`Ordering::Relaxed`) или из ring.
- Управление (seek/open/volume) — только вне RT; колбэк никогда не ждёт.

## 2. Целевая архитектура

### 2.1. Схема
```
                    ┌──────────────────────────────────────────────┐
 UI-поток            │  PlaybackWorker (Producer, новый поток)       │
 ──────────────────► │  Box<dyn AudioSource> → Resampler             │
 set_volume/muted  │  (Symphonia decode, CIC/DoP)                  │
 seek → cmd        │              │ rtrb::Producer                  │
                   │              ▼                                 │
                   │   RingBuffer<f32> (SPSC, фикс. cap)             │
                   └──────────────────┬─────────────────────────────┘
                                      │ rtrb::Consumer
                                      ▼
                  audio_callback_* (cpal RT, Consumer-Only)
                  pull → volume/mute/dither → формат → устройство
                  (атомики: volume, muted, bit_perfect, dither,
                   seek-generation, natural_end, pos)
```

### 2.2. Ключевые требования
- Колбэк держит **только** `Arc<RtConsumer>` (ring-consumer + атомики). Никакого
  доступа к decoder/resampler — их владеет исключительно Worker.
- `Mutex<PlaybackCore>` сокращается до контроля open/seek на стороне UI (не в
  RT-пути) либо исчезает.
- Интерфейс `Player` (snapshot/seek/play/toggle/…) сохраняется без изменений для
  UI-слоя (совместимость менеджеров).

## 3. Гарантия нулевых аллокаций в RT-колбэках (A2.0-3)

Текущий пул `scratch_f32/scratch_release` уже амортизирует аллокации, но
`shrink_to(64K)` при `capacity > 128K` (`src/audio/player.rs:184-186`) — допустимый
деаллок в RT. Цель — детерминизм без оговорок.

### 3.1. Преаллокация scratch до фиксированного потолка (A2.0-3.1)
- В `open_pcm`/`open_dop` создавать `scratch = vec![0.0f32; MAX_OUT_SAMPLES]`,
  `MAX_OUT_SAMPLES = 1 << 16` (65536 сэмплов ≈ 8K фреймов стерео @44.1 кГц;
  покрывает любой реальный `cpal`-буфер; постоянные ~256 КБ RAM).
- `scratch`-пул работает по схеме «только расти до потолка, никогда не сжиматься».
- Проверка: `cargo test audio::player` зелёный; после первого колбэка
  `scratch.capacity() == MAX_OUT_SAMPLES`.

### 3.2. Guard «не растить» в колбэках (A2.0-3.2)
- В `audio_callback_i16/u8/i32_pcm/i32_dop/f32`: `if data.len() > c.scratch.len()`
  → отдать тишину и выйти (по образцу zero-alloc-гирды `Resampler::push`,
  `src/audio/output.rs:142-147`). Никакого `resize` в колбэке.
- Проверка: юнит-тест с `data.len() > capacity` → тишина, без паники, без роста
  буфера.

### 3.3. Запрет сжатия буфера (A2.0-3.3)
- Удалить `shrink_to(64K)` из `scratch_release`; оставить `clear()`.
- Проверка: `cargo clippy` чист; capacity константна после первого вызова.

### 3.4. Тест-детектор аллокаций в колбэке (A2.0-3.4)
- Юнит-тест с counting allocator (обёртка над `System` через `#[global_allocator]`
  в `#[cfg(test)]`-модуле): прогнать все 5 колбэков на длинных буферах,
  зафиксировать 0 новых аллокаций после стартовой преаллокации.
- Проверка: `cargo test` — 0 аллокаций в колбэках.

## 4. Честная индикация bit-perfect (A2.0-4)

Сейчас при `bit_perfect && resampler.is_enabled()` печатается WARN в UI-потоке
(`src/audio/player.rs:335-342`), но в UI нет никакого сигнала.

### 4.1. Состояние «bit-perfect без гарантии» (A2.0-4.1)
- В `open_pcm` результат проверки сохранять в `core.bit_perfect_resampled:
  Arc<AtomicBool>` (relaxed-чтение в UI-тике), `eprintln` остаётся как
  диагностика.
- Проверка: `Player::bit_perfect_resampled()` возвращает `true` при
  bit-perfect + ресемплинге, `false` при native-rate и при выключенном bit-perfect.

### 4.2. UI-бейдж (A2.0-4.2)
- Расширить статус-бар (`ui/status.slint` + `playback_manager.rs`): условный бейдж
  «Resample (device limit)» рядом с «Bit-perfect», тултип «устройство не
  поддерживает нативную частоту — активен ресемплинг».
- Ползунки громкости/баланса остаются заблокированными при bit-perfect
  (требование AGENTS/SOLID уже выполнено в `ui_manager.rs`).
- Проверка: ручной тест на устройстве с 48 кГц и треке 44.1 кГц — бейдж виден;
  на native-rate матче — скрыт.

## 5. Producer/Consumer: декодирование вне RT (A2.0-5)

### 5.1. PlaybackWorker — поток декодера и ресемплера (A2.0-5.1)
- Новая структура `PlaybackWorker` (в `src/audio/worker.rs`): владеет
  `Box<dyn AudioSource>` + `Resampler`, пишет interleaved f32 финального rate в
  `rtrb::Producer<f32>`. Цикл: `next_frames() → res.push → res.pull → producer`.
  Доп. выгода: `decoder.scratch.resize` (`src/audio/decoder.rs:337`) и внутренние
  аллокации Symphonia перестают быть RT-проблемой.
- Жизненный цикл: `spawn` в `open`, `join` в `stop`/смене устройства
  (пересоздание).
- Проверка: headless-тест worker-цикла с `MockSource` → ring наполняется,
  порядок/число кадров точны; API `Player` без регрессий.

### 5.2. Ring buffer: размер и бюджет (A2.0-5.2)
- Новое настраиваемое поле `Settings` (секция `[audio]`): `ring_buffer_ms: u32` —
  буфер-толерантность «задержка vs устойчивость», дефолт **1500**, диапазон
  **[100..10000]**. Хранится в `Settings` (TOML-roundtrip).
- `RING_SAMPLES = ms/1000 × out_rate × out_ch`, пересчёт при `open`/смене
  устройства; нижняя граница — не менее 2× периодов cpal-буфера.
- Worker поддерживает заполненность ≥ 1/4 (мягкая цель) → стартовый underrun
  исключён при любом разумном ms.
- Проверка: unit-тесты ms→samples и валидация диапазона; TOML-roundtrip
  (расширить `src/settings.rs`); изменение применяется со следующего `open`.

### 5.3. Колбэки как Consumer (A2.0-5.3)
- `audio_callback_*` получают `Arc<RtConsumer>`; логика: прочитать из ring по
  запросу; при `!playing` — тишина и **не консьюмить** (позиция не уходит);
  f32 — читать прямо в `data`; i16/u8/i32 — читать в scratch (§3) и
  квантовать/дизерить как сегодня.
- Проверка: все существующие тесты `audio::player::tests` проходят без изменений
  семантики (адаптация API минимальна).

### 5.4. Управляющее состояние без Mutex (атомики) (A2.0-5.4)
- `RtConsumer` несёт атомики: `volume_bits: AtomicU32`, `muted`, `playing`,
  `bit_perfect`, `dither`, `pos_samples: AtomicU64`, `natural_end`,
  `seek_generation: AtomicU64`, `rt_err: AtomicU8`. `snapshot()/format()` читают
  их без замков.
- Осознанно: **единственный** способ действительно убрать lock из RT — этот;
  «атомики при живом `Mutex`» не работают, т.к. колбэку всё равно нужно
  исключение к decoder/resampler.
- Проверка: в `audio_callback_*` нет `lock()/try_lock()` (grep); тесты снапшота.

### 5.5. Seek-хендшейк (A2.0-5.5)
- UI: `seek(cmd)` повышает `seek_generation` и пишет целевой sample.
- Worker видит смену генерации → `decoder.seek()`, `resampler.reset()`, позиция —
  с новой точки.
- Consumer при смене генерации сбрасывает локальный кэш поколения и **дренит**
  ring до пустого (старые сэмплы не проигрываются).
- Проверка: во время игры seek не даёт «стадию»/старые кадры; unit-тест
  генерации.

### 5.6. Пауза/стоп/EOF (A2.0-5.6)
- Пауза: `playing=false` → worker останавливает запись; колбэк отдаёт тишину.
  Resume — без seek.
- EOF: worker ставит `eof`; когда `eof` и ring опустел — колбэк ставит
  `natural_end`, `playing=false`. `play()` на finished — ревинд через seek-путь.
- Проверка: регрессия тестов `callback_marks_natural_end_at_eof`,
  `toggle_pauses_and_resumes`, `stop_rewinds_and_confirms_manual_end`.

### 5.7. DoP passthrough (A2.0-5.7)
- Worker в DoP-режиме пакует слова (0x00_FF_FF…) как f32 (текущая семантика
  DoP-контейнера), колбэк `i32_dop` сдвигает `<<8`; volume/dither не применяются.
- Проверка: тесты DoP-фрейминга (`dop.rs`) зелёные; ручная проверка DSD на
  устройстве (если доступно) — без изменений битов.

### 5.8. Смена устройства/трека (A2.0-5.8)
- `open`: остановить/присоединить старый worker, собрать новый ring, спавнить
  новый worker, перестроить stream на новом устройстве; прежняя точка позиции
  (seek near pos) — через seek-хендшейк §5.5.
- Проверка: ручной переключатель устройства не роняет поток; `cargo test` чист.

## 6. Критерии приёмки
- `cargo test` — все существующие (157 lib + 13 bin на момент спеки) + новые
  зелёные; `cargo clippy` — 0 новых предупреждений.
- В `audio_callback_*` отсутствуют: `Mutex`, `Vec::resize/shrink`, `unwrap`,
  логирование. Гарантия проверена тестом §3.4.
- UI-бейдж из §4 показывает честное состояние софтверного тракта.
- Ручная проверка воспроизведения (PCM/DSD→PCM/DoP), seek, пауза, смена
  устройства идентична поведению до рефакторинга (нет регрессий/щелчков).

## 7. Порядок выполнения (план сессий)
1. A2.0-3 (3.1→3.2→3.3→3.4) — быстрый харденинг, 1 сессия.
2. A2.0-4 (4.1→4.2) — UI-индикация, 1 сессия.
3. A2.0-5 (5.1→5.2→5.3→5.4) → (5.5→5.6→5.7) → (5.8 + приёмка §6).
   Подзадачи 5.x — отдельными сессиями (правило «один шаг = одна сущность»).

## 8. Бюджеты и константы
| Константа | Значение | Обоснование |
|---|---|---|
| `MAX_OUT_SAMPLES` (scratch) | 65536 (≈8К фр. стерео @44.1кГц) | покрытие любого cpal-буфера |
| `ring_buffer_ms` | 1500 (дефолт), [100..10000] | настройка толерантности |
| Целевая заполненность ring | ≥ 1/4 | защита от стартового underrun |
| Аллокации в колбэке | 0 (детектор-тест, §3.4) | жёсткое требование |
| Средняя latency до ЦАП (ring) | 1/4 заполненность + период | компенсация jitter, см. §5.2 |