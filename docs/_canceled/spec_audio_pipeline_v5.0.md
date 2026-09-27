# APAP — Аудио-тракт v5.0: политика воспроизведения и целостность сигнала

**Префикс:** `AP5.0`  
**Статус:** На согласовании  
**Дата:** 2026-09-22  
**Опирается на:** `A2.0` (RT-ядро: worker → rtrb → RtConsumer → CPAL), `A3.0` (настройки аудио)  
**Заменяет:** все промежуточные документы по этой теме — см. §25  
**Назначение:** единая agent-ready спецификация следующего этапа рефакторинга аудио-тракта. В документе только окончательные решения; как менялась концепция и откуда что взято — §25.

Ключевые слова: **ОБЯЗАН / MUST** — обязательно; **НЕ ДОЛЖЕН / MUST NOT** — запрещено; **СЛЕДУЕТ / SHOULD** — предпочтительно; **МОЖЕТ / MAY** — допустимо. Если реализация не выполняет MUST-требование режима, она не имеет права заявлять этот режим как поддерживаемый.

---

## 1. Цель и принципы

RT-архитектура A2.0 (изоляция колбэка, lock-free транспорт, 0 аллокаций, seek-хендшейк) признана хорошей и **не переписывается**. Проблем две:

- **семантика сигнала:** «bit-perfect» сейчас проходит через `f32` (`decoder → f32 → rtrb<f32> → i16/i32 callback`), поэтому строгий контракт не выполняется;
- **модель режимов:** режим задаётся набором независимых флагов (`bit_perfect`, `exclusive`, `fallback`, `resampler`, `dsd.mode`), из которых собираются противоречивые состояния (например, `bit_perfect_resampled()`), а технический инвариант Bit-Perfect был превращён в глобальный пользовательский режим.

Решение: пользователь выбирает **политику воспроизведения**, движок подбирает максимально качественный допустимый путь, а требования целостности сигнала становятся **условными контрактами конкретного пути**, а не ограничением всего продукта.

Принципы:

1. **Пользователь выбирает политику, Planner выбирает путь, runtime исполняет.**
   `PlaybackPolicy` — намерение пользователя; `PathPlanner` — единственная точка решения; `SignalPath` — фактическая обработка; `BackendEndpoint` — способ доступа к устройству; RT-движок только исполняет готовый `PathPlan`.
2. **Bit-Perfect — свойство конкретного `SignalPath`, а не глобальный режим плеера.**
3. **`f32` — представление для DSP, но не универсальный транспорт.** Строгий путь не выполняет `integer PCM → f32 → integer PCM`, даже если для части значений это обратимо.
4. **Недопустимые состояния невыразимы типами**, а не отлавливаются runtime-проверками в разных местах.
5. **Программные заявления, заявления бэкенда и физическое железо проверяются раздельно** (§21.9).

Сохраняется без переписывания: `PlaybackWorker`, `rtrb` SPSC, seek generation handshake, декодирование вне RT, существующие алгоритмы ресемплера, корректный DoP-фрейминг, разделение Slint/ядро.

---

## 2. Термины

**Bit-Perfect PCM** — каждый декодированный целочисленный PCM-сэмпл (со знаком, разрядностью и позицией канала) попадает в поток устройства без изменения значения и без обработки:

```text
decoded sample == transport sample == CPAL callback sample == device stream sample
```

при условии, что бэкенд/железо действительно используют тот же формат, частоту, раскладку каналов и семантику потока. Bit-Perfect относится к **идентичности декодированных сэмплов**, а не к идентичности байтов исходного сжатого файла (`device bytes == FLAC bytes` не требуется).

**DSP-путь** — любой путь с преобразованием: `PCM → f32`, SRC, смешивание каналов, программная громкость, дизеринг, фильтры, DSD→PCM. DSP-путь никогда не отображается как Bit-Perfect.

**Native DSD** — DSD-поток передаётся устройству без перевода в PCM/`f32`.

**DoP (DSD over PCM)** — транспортный протокол: DSD-payload + маркеры в PCM-контейнере. Не является обычным PCM-сигналом; реализация через `DSD → f32 → integer → DoP` запрещена.

---

## 3. Архитектура

```text
                         SourceFormat
                              │
                     DeviceCapabilities
                              │
              ┌───────────────┴───────────────┐
              │                               │
      PlaybackPolicy                  AdvancedPreferences
       HARD RULES                    USER PREFERENCES (§5)
              │                               │
              └───────────────┬───────────────┘
                              ▼
                         PathPlanner (§8)
                              │
                 ┌────────────┴────────────┐
                 ▼                         ▼
             PathPlan                TrackRejectionInfo (§9)
        ┌────────┼─────────┐               │
   SignalPath ConversionPlan BackendEndpoint   next track (§15)
                 │
                 ▼
     typed worker transport (§10)
                 │
          RT callback (§13)
                 │
           BackendEndpoint
       ├── Shared    → PipeWire
       └── Exclusive → ALSA hw:*
```

Слои и владельцы:

| Слой | Модуль | Отвечает за | НЕ делает |
|---|---|---|---|
| Настройки | `settings.rs` | хранение и миграцию `PlaybackPolicy` + `AdvancedPreferences` | не выбирает путь |
| Политика | `audio/policy.rs` | жёсткие ограничения политик | — |
| Возможности | `audio/capabilities.rs` | `DeviceCapabilities`, `PhysicalDevice` | — |
| Planner | `audio/planner.rs` | единственный выбор `PathPlan` | не открывает устройство, не создаёт stream, не делает DSP, не трогает UI, не принимает RT-решений |
| Путь | `audio/signal_path.rs` | `SignalPath`, `ConversionPlan`, `PathPlan` | — |
| Endpoint | `audio/endpoint.rs` | Shared/Exclusive endpoints | — |
| Вывод | `audio/output.rs` | создание stream, жизненный цикл бэкенда, привязка колбэка | не принимает policy-решений |
| Плеер | `audio/player.rs` | исполняет `PathPlan`/`StreamContract` | не решает Shared/Exclusive/SRC/Strict |
| Приложение | `app/playback_manager.rs` | потребитель Planner на уровне плейлиста | не интерпретирует `AdvancedPreferences` |

---

## 4. PlaybackPolicy

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackPolicy {
    Shared,
    BestPossible,
    Strict,
}
```

| Политика | Для пользователя | Доступ | Преобразования | Несовместимый трек |
|---|---|---|---|---|
| `Shared` | «Всегда воспроизводить» — вместе с другими программами | Shared (на Linux PipeWire/default); raw `hw:*` запрещён | допускаются, включая микшер/конвертацию аудиосервера; DAC-level Bit-Perfect не гарантируется | играет |
| `BestPossible` | «Лучшее возможное качество» | Exclusive (см. §5.3) | сначала exact/native; если формат аппаратно недоступен — качественный SRC/конвертация (`DspPcm`, не Bit-Perfect) | играет. Пример: `24/192 → SRC → 24/96 → Exclusive` |
| `Strict` | «Только без преобразований» | Exclusive | запрещены SRC, конвертация формата сэмплов, каналов и DSD→PCM; exact/native обязателен | `TrackRejection` → пропуск |

Гарантии `Shared`: плеер не монополизирует устройство, старается воспроизвести любой поддерживаемый источник и не ломает системный аудиотракт. Плеер **не** гарантирует bit-perfect на уровне ЦАП, отсутствие SRC в микшере PipeWire/Windows/macOS и физическую частоту железа.

Инварианты:

- `SignalPath::BitPerfectPcm` возможен **только** при `BackendAccess::Exclusive`. При `Shared` UI не показывает «Bit-Perfect», даже если сам плеер ничего не преобразовывал: «Shared · 24/192 → системный аудиотракт · преобразование на стороне сервера неизвестно».
- `TrackRejected` ≠ `PlaylistFailure`: отказ одного трека не останавливает плейлист (§15).
- Если все треки отклонены — одно агрегированное сообщение, без бесконечных повторов.

---

## 5. AdvancedPreferences («Дополнительно»)

Пункт UI «Дополнительно» сохраняется, но это **слой предпочтений для Planner**, а не набор низкоуровневых переключателей и не четвёртая политика.

- **Простой режим:** пользователь выбирает только политику, остальное автоматически. Ему не нужно знать про PipeWire, ALSA, DoP и SignalPath.
- **Экспертный режим:** «Дополнительно» даёт полный контроль в рамках выбранной политики.

### 5.1 Правило пересечения

```text
AllowedPaths = PolicyAllowedPaths ∩ DeviceSupportedPaths ∩ UserAllowedPaths
```

Затем Planner выбирает из `AllowedPaths` наиболее предпочтительный путь: для DSD — в фиксированном порядке Native → DoP → PCM (§8.4), для PCM — по `SampleRatePreference` и качеству. Приоритет: (1) инварианты политики, (2) возможности устройства, (3) предпочтения пользователя, (4) выбор конкретного пути.

`AdvancedPreferences` **только сужают** множество (и выбирают целевую частоту при преобразовании); они **никогда не расширяют** множество, допустимое политикой. Пустое пересечение → отказ с `RejectionSource::Preference` (§9).

### 5.2 Модель

```rust
pub struct AdvancedPreferences {
    pub access: AccessPreference,
    pub pcm: PcmPreferences,
    pub sample_rate: SampleRatePreference,
    pub dsd: DsdPathPreference,
    pub compatibility: CompatibilityPreferences,
}

pub enum AccessPreference { Automatic, PreferShared, PreferExclusive }

pub struct PcmPreferences {
    pub allow_resampling: bool,
    pub allow_sample_format_conversion: bool,
    pub allow_channel_conversion: bool,
}

pub enum SampleRatePreference { SourceIfPossible, BestSupported, MaximumSupported }

pub enum DsdRoute {
    Native, // SignalPath::NativeDsd
    DoP,    // SignalPath::DoP
    Pcm,    // ConversionPlan::DsdToPcm → SignalPath::DspPcm
}

/// Какие DSD-пути разрешены. Порядок перебора фиксирован и
/// пользователем не меняется: Native → DoP → PCM (§8.4).
/// Хотя бы один путь разрешён — гарантируется конструктором
/// (`DsdPathPreference::new(native, dop, pcm) -> Result`).
pub struct DsdPathPreference {
    native: bool,
    dop: bool,
    pcm: bool,
}

pub struct CompatibilityPreferences {
    pub use_pipewire_for_shared: bool,
    pub allow_fallback: bool, // семантика — §24, вопрос 2
}
```

Отдельных `allow_dsd_to_pcm: bool` и `DopPreference` вне `DsdPathPreference` **нет**: разрешение каждого DSD-пути хранится в одном месте, поэтому противоречие вроде «только DoP» + «DoP запрещён» невыразимо. Приоритет путей не настраивается.

Флаги диагностики («Показывать фактический SignalPath», «Показывать причину преобразования») — настройки отображения, не входы Planner; хранятся отдельно.

### 5.3 Доступ к устройству

`BackendAccess` выводится из политики; `AccessPreference` учитывается **только при `BestPossible`**:

| Политика | `AccessPreference` | Итог | UI |
|---|---|---|---|
| `Shared` | любое | `Shared` | только чтение: «Доступ к устройству: Shared» |
| `Strict` | любое | `Exclusive` | только чтение: «Доступ к устройству: Exclusive» |
| `BestPossible` | `Automatic`, `PreferExclusive` | `Exclusive` | редактируемо |
| `BestPossible` | `PreferShared` | `Shared` (пути ограничены возможностями Shared endpoint) | редактируемо |

Даже при ошибочном UI-состоянии Planner обязан отфильтровать недопустимое: `Shared` никогда не превращается в raw `hw:*`, `Strict` всегда Exclusive.

### 5.4 Примеры

| Политика | Предпочтения | Ситуация | Результат |
|---|---|---|---|
| `Strict` | `allow_resampling = true` | 24/192, ЦАП max 96 | `RequiresConversion` (Policy) — SRC запрещён политикой |
| `BestPossible` | `allow_resampling = false` | 24/192, ЦАП max 96 | `RequiresConversion` (Preference) |
| `BestPossible` | DSD `{Native, DoP, Pcm}` | Native нет, DoP есть | `SignalPath::DoP` |
| `BestPossible` | DSD `{Native}` | Native нет | `NativeDsdUnavailable` (Preference) — пользователь сам сузил выбор, `BestPossible` не сломан |
| `Shared` | `PreferExclusive` | — | `BackendEndpoint::Shared` |

### 5.5 UI

```text
Воспроизведение
────────────────────────
Политика:  [ Лучшее возможное качество ▼ ]
[ Дополнительно... ]

Дополнительно
────────────────────────
Доступ к устройству          (редактируемо только при «Лучшее возможное»)
  ○ Автоматически
  ○ Shared / PipeWire
  ○ Exclusive / ALSA

PCM
  ☑ Разрешить изменение частоты дискретизации
  ☑ Разрешить изменение разрядности
  ☑ Разрешить изменение числа каналов

При необходимости преобразования
  Частота:  ○ Исходная, если возможно
            ○ Наилучшая поддерживаемая
            ○ Максимальная
  SRC:      [ алгоритм ресемплера ]

DSD — разрешённые пути (порядок фиксирован: Native → DoP → PCM)
  ☑ Native DSD
  ☑ DoP                Формат: [ ... ]   (§24, вопрос 1)
  ☑ DSD → PCM

Совместимость
  ☑ Использовать PipeWire для Shared
  ☑ Разрешить fallback

Диагностика
  ☑ Показывать фактический SignalPath
  ☑ Показывать причину преобразования
```

UI МОЖЕТ отключать или пояснять параметры, не имеющие смысла при текущей политике (например, PCM-преобразования при `Strict`). Все новые строки UI — на русском.

---

## 6. Устройства и endpoints

```rust
pub enum BackendAccess { Shared, Exclusive }

pub enum BackendEndpoint {
    Shared(SharedEndpoint),       // Linux: PipeWire / pcm.pipewire / PIPEWIRE_NODE
    Exclusive(ExclusiveEndpoint), // Linux: raw ALSA hw:CARD=...,DEV=...
}

pub struct PhysicalDevice {
    pub id: PhysicalDeviceId,
    pub name: String,
    pub shared: Option<SharedEndpoint>,
    pub exclusive: Option<ExclusiveEndpoint>,
    pub capabilities: DeviceCapabilities,
}
```

- Один физический ЦАП имеет несколько представлений; `PhysicalDevice` их объединяет. Planner выбирает: `Shared → physical.shared`, `BestPossible`/`Strict → physical.exclusive` (с учётом §5.3).
- `DeviceCapabilities` содержит: `physical_id`, `shared_endpoint`, `exclusive_endpoint`, частоты, форматы, каналы, поддержку DSD и DoP. Способность к exclusive определяется семантикой endpoint бэкенда, а не тем, что ID похож на raw `hw:*` (`exclusive_capable = is_raw_hardware_id(..) && Hardware` — устаревшая связка).
- `Shared` **никогда** не вызывает `snd_pcm_open("hw:*")`.
- Правило «среди одинаковых имён всегда выигрывает raw `hw:*`» (`collapse_same_name()`) **удаляется**. Причина — подтверждённая регрессия Xonar DX/PipeWire: открытие raw ALSA отнимало устройство у PipeWire, узел исчезал из графа и возвращался только после перезапуска WirePlumber.
- `settings.audio_device` идентифицирует физическое устройство, а не конкретный `hw:*` endpoint для всех режимов.
- Native DSD согласуется отдельно от PCM; при недоступности путь выбирается по §8.4, но никогда не маркируется как Native DSD.

---

## 7. SignalPath, ConversionPlan, PathPlan

```rust
pub enum SignalPath {
    BitPerfectPcm(PcmFormat),
    DspPcm(DspFormat),
    NativeDsd(DsdFormat),
    DoP(DopFormat),
}

pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub layout: ChannelLayout,
    pub sample_format: PcmSampleFormat,
}
pub enum PcmSampleFormat { I16, I24, I32 }

pub struct DspFormat { pub sample_rate: u32, pub channels: u16, pub layout: ChannelLayout }

pub enum ConversionPlan {
    None,
    Resample(ResamplerConfig),
    ConvertPcm(PcmConversion),
    DsdToPcm(DsdPcmConfig),
}

pub struct PathPlan {
    pub policy: PlaybackPolicy,
    pub access: BackendAccess,
    pub signal_path: SignalPath,
    pub source_format: SourceFormat,
    pub output_format: OutputFormat,
    pub conversion: ConversionPlan,
    pub endpoint: BackendEndpoint,
}
```

Инварианты:

- `BitPerfectPcm` ⇔ `ConversionPlan::None`, а также: нет SRC, микшера, программной громкости ≠ 1, дизеринга, DSP, транспорта через `f32`; сохранены частота, формат сэмплов, число и раскладка каналов.
- `NativeDsd` ⇔ нет PCM-преобразования. `DoP` ⇔ нет DSD→PCM.
- `DspPcm` — любой путь с SRC, громкостью, преобразованием каналов, DSD→PCM или иной обработкой. **DSD→PCM никогда не маркируется Bit-Perfect.**
- Ресемплер — часть `DspPcm`, а не глобальный переключатель; состояние `bit_perfect = true + resampler_enabled = true` не существует как валидная модель.
- DoP-маркер и упаковка принадлежат типу `DopFormat`, а не флагу `is_dop: bool`; выходной формат DoP — отдельный вариант (`OutputFormat::Dop24 { container_rate }`), поэтому обычный PCM-колбэк невозможно случайно использовать для DoP.
- `PathPlan` создаётся до запуска RT stream и после создания не меняет критических свойств.

Stream создаётся с неизменяемым контрактом вместо runtime-флага `RtShared.bit_perfect: AtomicBool`:

```rust
pub struct StreamContract {
    pub signal_path: SignalPath,
    pub output_format: OutputFormat,
}
```

Колбэк не спрашивает `if bit_perfect { .. }` — он собран под конкретный контракт.

---

## 8. PathPlanner

### 8.1 API

```rust
pub fn plan(
    source: &SourceFormat,
    device: &DeviceCapabilities,
    policy: PlaybackPolicy,
    preferences: &AdvancedPreferences,
) -> Result<PathPlan, TrackRejectionInfo>;
```

Planner отвечает только на вопрос «какой путь допустим для этого source + device + policy + preferences». Чистая функция: без устройства, без I/O, тестируется без живого бэкенда.

### 8.2 Алгоритм

1. Построить множество путей, допустимых политикой (§4) и не нарушающих запреты §20.
2. Пересечь с возможностями устройства.
3. Пересечь с `AdvancedPreferences` (§5.1).
4. Пусто → `TrackRejectionInfo` (источник по §9). Иначе — лучший путь: DSD — первый доступный в фиксированном порядке Native → DoP → PCM; PCM — exact/native, иначе преобразование с целевой частотой по `SampleRatePreference`.

### 8.3 Проверка кандидата `BitPerfectPcm`

Кандидат допустим, только если одновременно выполнено всё:

| Условие | Требование |
|---|---|
| Представление декодера | известно и целочисленное без потерь (§10.2) |
| Частота | `device_rate == source_rate` |
| Формат сэмплов | точное поддерживаемое соответствие |
| Каналы / раскладка | точное число и позиции |
| Ресемплер, DSP, микшер, дизеринг | выключены |
| Программная громкость | единица |
| Режим бэкенда | native/exclusive, где требуется |
| Тип колбэка / поток устройства | точно совпадает с представлением (§10.5) |

Проверка не возвращает `bool`: невыполненное условие — это конкретная причина из `TrackRejection` (§9). Что делать, если кандидат `BitPerfectPcm` недопустим, решает политика: `Strict` → отказ с этой причиной, `BestPossible` → DSP-путь (если разрешён предпочтениями), `Shared` → Shared-путь. Отдельных типов «доступность Bit-Perfect» и «fallback Bit-Perfect» нет — пользователь запрашивает политику, а не Bit-Perfect.

### 8.4 DSD и DoP

| Политика | Допустимые пути | Порядок (фиксирован) |
|---|---|---|
| `Shared` | если Shared endpoint не даёт Native/DoP: `DSD → PCM → PipeWire` (`DspPcm`) | — |
| `BestPossible` | Native, DoP, DSD→PCM | `Native → DoP → PCM`; пользователь может только отключить пути через `DsdPathPreference`, но не переставить их |
| `Strict` | только Native и DoP | `Native → DoP` из разрешённых пользователем; DSD→PCM запрещён независимо от предпочтений |

Исключение пользователем всех доступных путей → отказ (`Preference`), а не скрытый fallback.

---

## 9. Отказы и классы ошибок

```rust
pub enum TrackRejection {
    SampleRateUnsupported { requested: u32, supported: Vec<u32> },
    SampleFormatUnsupported,
    ChannelsUnsupported,
    ChannelLayoutUnsupported,
    /// Нельзя доказать, что тип колбэка / поток устройства совпадает с представлением (§10.5).
    OutputRepresentationUnproven,
    /// Декодер не выдаёт целочисленное представление без потерь (§10.2).
    SourceRepresentationLossy,
    ExclusiveUnavailable,
    NativeDsdUnavailable,
    DopUnavailable,
    RequiresConversion,
    NoValidSignalPath,
}

pub enum RejectionSource {
    /// Путь запрещён политикой (например, Strict + SRC).
    Policy,
    /// Путь допустим политикой, но отсечён AdvancedPreferences.
    Preference,
}

pub struct TrackRejectionInfo {
    pub reason: TrackRejection,
    pub source: RejectionSource,
}
```

- Варианты МОГУТ нести данные (`requested`, `supported`, `reason`); причина показывается пользователю по-человечески: «24/192 — устройство не поддерживает 192 кГц», «DSD256 — нет пути Native/DoP».
- Если отказ вызван и политикой, и предпочтением — источник `Policy` (политика приоритетнее).
- UI («Показывать причину преобразования») ОБЯЗАН показывать источник, чтобы пользователь понимал, что чинить: политику или своё «Дополнительно».
- ОБЯЗАТЕЛЬНО различать: `TrackRejected`, `BackendError`, `DecoderError`, `DeviceError` — у них разная обработка (§15). Результат открытия трека:

```rust
pub enum TrackOpenResult {
    Started,
    Rejected(TrackRejectionInfo),
    Fatal(PlaybackError), // устройство пропало, бэкенд упал, инфраструктура декодера сломана
}
```

---

## 10. Представление сэмплов и транспорт

### 10.1 Проблема

`AudioSource::next_frames() -> Option<&[f32]>` делает `f32` универсальным представлением. Это ломает строгий путь: `i16 = -32768 → -1.0 → -32767`; у `f32` мантисса ≈ 24 бита, поэтому не все `i32` представимы точно.

### 10.2 Граница декодера

Декодер сообщает реальное декодированное представление и сохраняет частоту, число каналов, разрядность, PCM-vs-DSD:

```rust
pub enum DecodedAudio { I16, I24, I32, F32, Dsd }

pub enum AudioFrame<'a> {
    PcmI16(&'a [i16]),
    PcmI24(&'a [I24]),
    PcmI32(&'a [i32]),
    PcmF32(&'a [f32]),   // только DSP-путь
    NativeDsd(&'a [DsdSample]),
    Dop(&'a [DopFrame]),
}
```

(или эквивалентный generic `PcmSource { type Sample; fn next_frames(&mut self) -> Result<Option<&[Self::Sample]>, DecodeError>; }`).

Если декодер может выдать только `f32` или не может выдать целочисленное представление без потерь — это явное ограничение пути: строгий Bit-Perfect для такого формата **отклоняется**, а не заявляется после конверсии.

### 10.3 Владение и время жизни

- Декодер МОЖЕТ возвращать заимствованные кадры, но их время жизни ограничено текущей операцией декодера.
- Worker НЕ ДОЛЖЕН: класть заимствованный срез в долгоживущее кольцо, передавать ссылку декодера в RT-колбэк, хранить её после следующего decode, строить самоссылающееся владение декодер ↔ очередь.
- Worker копирует данные в заранее выделенное типизированное хранилище (`OwnedPcmBlock<T>`, пул блоков `PcmBlockPool<T>`); пул создаётся до воспроизведения, после старта рост хранилища запрещён.
- `Arc<Mutex<Vec<T>>>` / `Arc<Mutex<AudioBuffer>>` как основной транспорт Bit-Perfect запрещены.

### 10.4 Типизированный транспорт доходит до CPAL

```text
Bit-Perfect I16:  PcmI16 → OwnedPcmBlock<i16> → RingBuffer<i16> → RtConsumer<i16> → callback(&mut [i16]) → device
Bit-Perfect I32:  PcmI32 → RingBuffer<i32> → RtConsumer<i32> → callback(&mut [i32])
DSP:              PcmI16/I24/I32 → f32 → resampler/DSP → f32 ring → callback(&mut [f32])
```

Запрещено: `BitPerfect I16 → f32 ring → i16 callback`, `BitPerfect I32 → f32 → i32 callback`, а также держать типизированный PCM только на границе декодера и затем снова сводить к `f32`. Worker + SPSC/rtrb сохраняются; `f32` остаётся для DSP-пути.

Конкретные функции (`fn write_i16(output: &mut [i16], consumer: &mut RtConsumer<i16>)`) предпочтительнее обобщённой абстракции, которая может молча конвертировать. В строгом пути нет `as f32`, масштабирования, `round`, `clamp`.

### 10.5 Форматы и граница CPAL

- **I16, I32** — остаются `i16`/`i32` на всём пути.
- **I24** — канонический тип `#[repr(transparent)] struct I24(i32)` с инвариантом `-8_388_608 ≤ v ≤ 8_388_607`; конструктор `try_new` проверяет диапазон вне RT; `unsafe new_unchecked` допустим только при локально доказанном инварианте. `I24 == i32` автоматически не предполагается: отображение 24-бит → контейнер CPAL/бэкенда (знаковое расширение, выравнивание, endian) ОБЯЗАНО быть задокументировано и покрыто golden-тестом; оно не угадывается.
- Перед созданием stream фиксируются: запрошенное представление, выбранный конфиг CPAL, тип колбэка, представление потока бэкенда. Инвариант: все четыре совпадают. **Если точное соответствие нельзя доказать — Bit-Perfect недоступен.**
- Выбор `SampleFormat` в CPAL сам по себе не доказывает физический формат устройства (§21.9).

---

## 11. Каналы, громкость, дизеринг

- **Каналы.** Bit-Perfect сохраняет число, порядок, идентичность и позиции каналов. Запрещены даже «безобидные» stereo↔mono, 5.1↔stereo, перестановка L/R. `mix_rel()` и аналоги — DSP-операции.
- **Громкость.** Bit-Perfect требует программной громкости = 1 (`0.5 × PCM` — не Bit-Perfect); при необходимости обработки путь становится `DspPcm`, а `Strict` отклоняет такой путь. Аппаратная громкость допустима, только если её семантика явно определена как вне программного тракта; иначе режим не заявляется шире доказанного.
- **Дизеринг** запрещён для `BitPerfectPcm`, `NativeDsd`, `DoP`; допустим в DSP-пути. `BitPerfectPcm && dither` невыразимо на уровне типов, где это практично. Seed дизеринга СЛЕДУЕТ считать стабильным хешем (FNV-1a/xxhash), а не `DefaultHasher`, чтобы шум не менялся при обновлении тулчейна.

---

## 12. DSD и DoP

- **Native DSD:** `Decoder → DsdSample/DsdBlock → typed DSD transport → native backend`; без `DSD → f32 → PCM`.
- **DoP** — явный типизированный протокол:

  ```rust
  #[repr(C)]
  pub struct DopFrame { pub payload: Dsd24, pub marker: DopMarker }
  fn encode_dop_frame(payload: Dsd24, marker: DopMarker) -> DopFrame;
  ```

  Явно определяются: ширина payload, байты маркера (0x05/0xFA), endian, выравнивание кадра, чередование каналов, последовательность маркеров, границы блоков. Точная последовательность байтов берётся из выбранной спецификации DoP / контракта бэкенда и фиксируется golden-тестами. DoP НЕ ДОЛЖЕН реализовываться через `f32`-нормализацию; архитектурная зависимость от `is_dop: bool` удаляется (`dop.rs` сохраняет генерацию маркеров, упаковку и валидацию).
- `dsd.rs` не принимает пользовательскую политику: DSD→PCM = `DspPcm`, raw DSD = `NativeDsd`, подготовка DoP = `DoP`.

---

## 13. RT-контракт

### 13.1 Разрешено / запрещено

Колбэк МОЖЕТ: atomic load/store, ограниченную арифметику, операции типизированного SPSC-кольца, копирование из заранее выделенной памяти, запись в буфер устройства, ограниченный переход состояния.

Колбэк НЕ ДОЛЖЕН: `Mutex`/`RwLock`, аллокации и деаллокации, рост `Vec`, создание `String`, `format!`/`println!`/`eprintln!`, файловая система и сеть, декодирование, ресемплинг, FFT, Slint, блокирующие syscall, `sleep`, неограниченные циклы, clone/drop `Arc`, изменение конфигурации stream, policy-решения. Колбэк исполняет уже выбранный `PathPlan`/`StreamContract`.

### 13.2 Ограниченность времени

RT-безопасность ≠ «lock-free + без аллокаций»; время выполнения тоже ограничено. Работа колбэка — `O(output_buffer_len)` и НЕ зависит от заполненности кольца, дистанции seek, состояния декодера, размера файла, очереди визуализатора, числа потерянных кадров или устаревших поколений. `while ring.pop().is_ok() {}` в колбэке запрещён.

### 13.3 Инвалидация при seek

Устаревшие данные после seek не должны воспроизводиться, но текущий O(N)-drain в `reconcile_seek()` заменяется ограниченным механизмом: generation/epoch (`PlaybackGeneration { current: AtomicU64 }`, блоки несут `generation`) плюс одно из — O(1)-сброс на стороне producer, сброс курсора кольца с учётом поколения, ограниченное отбрасывание устаревших блоков, замена кольца вне RT, иной формально ограниченный протокол. Выбранный механизм ОБЯЗАН быть задокументирован и покрыт тестом/бенчмарком худшего случая.

### 13.4 Границы буферов

Если запрошенный CPAL-буфер (`data.len()`) больше ёмкости заранее выделенного scratch (`MAX_OUT_SAMPLES`), текущая реализация молча дописывает тишину — это недопустимо. Ёмкость проверяется при построении stream (буфер устройства ограничивается или stream не создаётся с явной ошибкой), а в колбэке переполнение увеличивает атомарный счётчик, видимый в UI. Минимальная глубина кольца (сейчас литерал `4096`) выносится в именованную константу рядом с `MAX_OUT_SAMPLES`.

### 13.5 Планирование и отказы

- Документируются: приоритет и политика планирования потока колбэка и worker, риски инверсии приоритетов и голодания CPU, гарантии бэкенда.
- Колбэк не ждёт mutex worker'а; worker не ждёт прогресса колбэка для освобождения критических ресурсов; колбэк работоспособен при временной остановке worker'а; визуализация не конкурирует с колбэком так, чтобы вызывать избежимые underrun.
- Нехватка данных → детерминированная тишина; колбэк не блокируется, не ждёт декодер, не вызывает UI, не запрашивает синхронный decode, не выделяет буфер восстановления. Счётчик underrun — `fetch_add(1, Relaxed)`.
- Ошибки из RT: атомарный счётчик/состояние → worker/основной поток → структурированное событие → UI/лог. Error-callback CPAL тоже не печатает напрямую, а взводит `error_flag`, логирование — в `tick()`.
- Backoff worker'а при полном кольце (сейчас `sleep(1 ms)`) СЛЕДУЕТ сделать адаптивным для высоких частот (DSD256+, 768 kHz).

---

## 14. Декодер: ошибки и `unwrap`

- **Seek:** `Time::try_new(..).unwrap_or(Time::ZERO)` запрещён (невалидный seek → молчаливый переход в 0); `seek(..) -> Result<(), DecodeError>` с распространением ошибки.
- **Классификация:** `DecodeSeverity { Recoverable, Fatal }`. Recoverable (битый пакет) → пропустить, увеличить счётчик, продолжить. Fatal → остановить воспроизведение, сообщить. `DecodeError(_) => continue` без классификации, учёта и наблюдаемого статуса запрещён.
- **`unwrap`/`expect`:** без механической глобальной замены. Допустимы в тестах и для внутреннего инварианта (`expect("invariant: ...")`), если инвариант установлен локально и его нарушение — ошибка программиста. Внешние условия (устройство недоступно, невалидный seek, битый файл, неподдерживаемый формат, отсутствующая конфигурация, сбой бэкенда) возвращают `Result`/явное состояние. Startup-`unwrap()` в `main.rs` (`ui.show().unwrap()`, `run_event_loop_until_quit().unwrap()`) — P2: `main() -> Result<(), AppError>`. Ошибки, которые сейчас теряются молча (`Image::load_from_path(..).unwrap_or_default()` в загрузке обложек), дают диагностику, а не тихий default.

---

## 15. Плейлист и конечный автомат

```text
Idle → Planning → Opening → Playing
                               ├─ EOF           → NextTrack
                               ├─ Seek          → Seeking
                               ├─ DeviceLost    → Recover/Stop
                               └─ TrackRejected → NextTrack

NextTrack: Plan(track) ├─ Playable → Opening
                       └─ Rejected → следующий кандидат
```

`PlaybackManager` разделяет результаты открытия:

```text
Plan/Open ├─ Started       → play
          ├─ TrackRejected → записать причину → следующий подходящий трек
          └─ Fatal error   → сообщить → stop/recover
```

- `Strict`: `A rejected → skip, B → play, C rejected → skip, D → play`.
- Поиск следующего подходящего трека — итеративный, ограниченный размером плейлиста; без рекурсии.
- Все треки отклонены → одно сообщение «Нет треков, воспроизводимых при политике Strict».
- Смена частоты между треками: переход трека = `plan → сравнить с текущим StreamContract → если изменился: stop stream → reconfigure → restart`. Для exclusive это нормальный переход, а не ошибка. Gapless проектируется вместе с политикой (отдельная задача); добавлять SRC ради gapless при `Strict` нельзя.
- `fulltrack_manager.rs`, `playlist_manager.rs` сохраняют специализацию; места, где любая ошибка `Player::open()` останавливает воспроизведение, исправляются — отказ `Strict` возвращается в `PlaybackManager`, а не становится фатальной ошибкой плейлиста.

---

## 16. Визуализатор и UI

### 16.1 Визуализатор

- Точка отвода: `worker → resampler/DSP → visualizer tap → playback ring`; не внутри колбэка CPAL.
- `VizTap`: producer принадлежит worker'у (`viz_producer: Option<Producer<f32>>`, `viz_active: Arc<AtomicBool>`); менеджер создаёт кольцо и передаёт producer во владение. `Arc<Mutex<Option<Producer>>>` не использовать без конкретной причины.
- Визуализация best-effort: медленный визуализатор → данные визуализации отбрасываются → воспроизведение продолжается. Очереди ограничены по ёмкости; worker никогда не блокируется визуализатором.
- Тяжёлые вычисления (FFT, спектрограмма, анализ осциллограммы, кэш, decode, ресемплинг, DSP) — вне event loop Slint.
- Модель «последнего значения»: `worker → latest-value buffer → таймер Slint 30–60 Гц → забрать последний кадр` (промежуточные кадры отбрасываются намеренно) вместо `invoke_from_event_loop` на каждый блок FFT.
- Worker не держит сильную ссылку на UI (`ui.as_weak()`, `upgrade_in_event_loop`); UI владеет своим временем жизни.
- Обновления свойств пакетируются (один логический `SpectrumFrame`, а не 32 отдельных `set_bin_*`).
- Delta-обновления (`if playing != cur.playing { set_playing }`) — образец; функции, строящие `Vec`/`String`/`ModelRc` (`sync_track_info_to_ui`), не вызываются на каждом 16-мс кадре (P2: снизить аллокации UI).

### 16.2 Статус воспроизведения

UI не вычисляет Bit-Perfect из булевых комбинаций. Источник — готовый снимок:

```rust
pub struct PlaybackStatus {
    pub policy: PlaybackPolicy,
    pub access: BackendAccess,
    pub signal_path: SignalPath,
    pub source_format: SourceFormat,
    pub output_format: OutputFormat,
    pub conversion: Option<ConversionInfo>,
}
```

Отображается:

```text
Политика:        Shared / Лучшее возможное / Strict
Фактический путь: Shared / Bit-Perfect PCM / DSP / Native DSD / DoP
Бэкенд:          PipeWire / ALSA Exclusive / ...
Преобразование:  нет / 192→96 / DSD→PCM / ...
Причина:         (при отказе/преобразовании, с источником Policy/Preference)
```

Примеры: «Лучшее возможное · Exclusive · 96 кГц · SRC из 192 кГц: устройство поддерживает максимум 96 кГц»; «Strict · трек пропущен · устройство не поддерживает 192 кГц».

Не отображать просто «Bit-Perfect: ON». «Bit-Perfect» показывается тогда и только тогда, когда фактический путь — `BitPerfectPcm`. Фактический путь показывается независимо от предпочтений.

### 16.3 Состояние приложения

`Rc<RefCell<MusicApp>>` + ~60 колбэков + 3 таймера — системный риск `BorrowMutError` при реентерабельном вызове. Тело каждого `move || { ... }` выносится в метод менеджера (`fn on_xxx(&mut self, ...)`), в замыкании остаётся один вызов; точки, вызывающие `ui.set_*` внутри заимствования, устраняются.

---

## 17. Ресемплер (DSP)

- Ресемплер — DSP-компонент; НЕ ДОЛЖЕН выполняться в `BitPerfectPcm`, МОЖЕТ — в `DspPcm`. Его выбор приходит из `PathPlan`.
- Не переписывать без измерений: сначала измерить, менять только при подтверждённом дефекте.
- Обязательная матрица: `44.1→48`, `48→44.1`, `96→44.1`, `192→48`. СЛЕДУЕТ дополнительно: `44.1→96`, `192→44.1`, `352.8→44.1`, `88.2↔176.4`, `96↔192`.
- Сигналы: импульс; синус 1 кГц, 10 кГц, 18 кГц, около Найквиста; тишина; полная шкала; многоканальный импульс.
- Метрики: неравномерность АЧХ в полосе пропускания, подавление в полосе задерживания, подавление алиасинга, АЧХ, задержка, CPU нс/сэмпл, аллокации (0 в горячем пути). Для каждой конверсии фиксируются входная/выходная частота, частота сигнала, амплитуда, ошибка, компоненты алиасинга, CPU, число аллокаций.
- **Числовые пороги ОБЯЗАНЫ быть записаны до объявления ресемплера валидированным** (§24, вопрос 6). THD+N железа — вне программной приёмки.
- P2: кольцевой буфер вместо `Vec::drain()` в ресемплере.
- P2: разделить «декодирование впрок» и «RT-страховочный буфер» (`ring_buffer_ms` сейчас смешивает оба и даёт заметную задержку pause/seek/next/смены устройства); измерять фактическое число буферизованных кадров во время работы.

---

## 18. Настройки и миграция

- `settings.rs` хранит единый `playback_policy` и `advanced_preferences`; загружает, мигрирует, но **не выбирает** `SignalPath`:

  ```text
  settings.toml → Settings → PlaybackPolicy + AdvancedPreferences → PathPlanner
  ```

- Старые `bit_perfect`, `exclusive`, `fallback`, `resampler`, `dsd.mode` и поля A3.0 (`ExclusiveMode`, `FallbackPolicy`, `ResamplerMode`, `ClockFamily`, `FallbackRatePolicy`) не остаются вторым источником истины. Миграция отображает их в новую модель, где это осмысленно, остальное — дефолты; старый конфиг читается без потери намерений пользователя (таблица — §24, вопрос 3).
- Параметры ресемплера и дизеринга остаются DSP-параметрами и не определяют политику.

---

## 19. Модули

```text
src/audio/
├── policy.rs          PlaybackPolicy, жёсткие ограничения            (новый)
├── planner.rs         PathPlanner — единственный выбор PathPlan       (новый)
├── signal_path.rs     SignalPath, ConversionPlan, PathPlan, форматы   (новый)
├── capabilities.rs    DeviceCapabilities, PhysicalDevice              (новый)
├── endpoint.rs        Shared/Exclusive endpoints, EndpointResolver   (новый)
├── output.rs          создание stream, бэкенд, привязка колбэка       (из него выносится policy/capabilities/endpoint/planner)
├── worker.rs          worker + типизированный SPSC; f32 для DSP
├── player.rs          исполняет PathPlan; без bit_perfect: AtomicBool как источника истины
├── decoder.rs         раздельные представления источника и DSP
├── dsd.rs, dop.rs     типизированные DSD/DoP
└── resampler.rs       алгоритмы ресемплера
```

Цепочка вместо нынешней `Settings → OutputRequest → choose_output() → OutputSpec` (где одна функция смешивает политику и технический fallback):

```text
Settings → PlaybackPolicy + AdvancedPreferences → PathPlanner → PathPlan
         → EndpointResolver → OutputSpec/StreamContract → AudioEngine
```

---

## 20. Запреты

Недопустимые состояния (СЛЕДУЕТ сделать невыразимыми типами/конструкторами, а не только runtime-проверками):

```text
Shared + raw hw:*
Shared + BitPerfectPcm
Strict + SRC / sample-format conversion / channel conversion / DSD→PCM
BitPerfectPcm + f32 roundtrip / volume ≠ 1 / dither / ConversionPlan ≠ None
AdvancedPreferences меняют BackendAccess в обход политики
AdvancedPreferences расширяют множество путей сверх допустимого политикой
DsdPathPreference без единого разрешённого пути
```

Анти-паттерны (НЕ делать):

1. Чинить Bit-Perfect добавлением булевых флагов.
2. Пропускать целочисленный PCM через `f32` и называть это Bit-Perfect.
3. Держать типизированный PCM только на границе декодера.
4. Прятать несовпадение типов CPAL за обобщённой конверсией.
5. `Mutex`/`RwLock`/аллокации/неограниченный drain/FFT/визуализацию в колбэке.
6. Молча ресемплировать, переставлять каналы или менять громкость, сообщая Bit-Perfect.
7. Считать DoP обычным `f32` PCM; заявлять Native DSD после конверсии.
8. Механически удалять все `unwrap`/`expect`.
9. Переписывать ресемплер без измерений.
10. Позволять визуализатору блокировать воспроизведение; позволять worker'у владеть временем жизни UI.
11. Делать заявления о железе только по состоянию приложения; считать, что выбор формата CPAL доказывает физический формат.
12. Самоссылающееся владение декодер/транспорт; `Arc<Mutex<Vec<T>>>` как основной транспорт.
13. Работа колбэка, зависящая от заполненности кольца, дистанции seek или очереди.
14. Один гигантский многофайловый непроверяемый коммит.

---

## 21. Тестирование

Все тесты — детерминированные (фиксированные seed), без живого бэкенда, если не сказано иное.

### 21.1 Planner: политики

| Политика | Источник | Устройство | Ожидание |
|---|---|---|---|
| Shared | 16/44.1 | max 96 | играет через Shared |
| Shared | 24/192 | max 96 | играет |
| BestPossible | 24/192 | поддерживает 192 | `BitPerfectPcm` |
| BestPossible | 24/192 | max 96 | `DspPcm` 96 |
| Strict | 24/192 | max 96 | отказ |
| Strict | 16/44.1 | поддерживает 44.1 | `BitPerfectPcm` |
| BestPossible | DSD256 | DoP | `DoP` |
| Strict | DSD256 | нет Native/DoP | отказ |
| Shared | DSD256 | нет Native | `DspPcm` / Shared |

### 21.2 Planner: предпочтения

| Политика | Предпочтения | Условие | Ожидание |
|---|---|---|---|
| Strict | `allow_resampling = true` | 24/192 на max 96 | отказ (Policy) |
| Strict | DSD `{Native, DoP, Pcm}` | нет Native/DoP | отказ (Policy), не DSD→PCM |
| Strict | DSD `{Native}` | есть только DoP | отказ (Preference) |
| Shared | `PreferExclusive` | — | Shared endpoint |
| BestPossible | `allow_resampling = false` | 24/192 на max 96 | отказ (Preference) |
| BestPossible | DSD `{Native, DoP, Pcm}` | Native нет, DoP есть | `DoP` |
| BestPossible | DSD `{Native, Pcm}` | Native нет, DoP есть | `DspPcm` (DSD→PCM) |
| BestPossible | DSD `{Native}` | Native нет | отказ (Preference) |
| BestPossible | `PreferShared` | exclusive доступен | Shared endpoint |
| BestPossible | DSD `{Native, DoP, Pcm}` | доступны все три | `NativeDsd` — порядок фиксирован, не зависит от настроек |

**Property-тест** на всей матрице `Policy × AdvancedPreferences × DeviceCapabilities × SourceFormat`: множество путей с предпочтениями — всегда подмножество множества без них; запреты §20 не нарушаются.

### 21.3 Плейлист

`A несовместим, B совместим, C несовместим, D совместим` при `Strict` → играют B и D. Все несовместимы → «Нет воспроизводимых треков». Поиск кандидата ограничен размером плейлиста.

### 21.4 Golden PCM и свойства транспорта

Для каждого формата: `вход → транспорт → consumer → колбэк`, требование `output[i] == input[i]` (сравниваются значения/байты сэмплов, не приближённые `f32`):

- **I16:** `0, 1, -1, 32767, -32768, 0x5555, 0xAAAA`, случайные.
- **I24:** `0, 1, -1, 8_388_607, -8_388_608`, случайные; проверяются payload, знак, диапазон, упаковка, выравнивание, endian.
- **I32:** `i32::MIN, i32::MAX, -1, 0, 1, 2^24, 2^24+1, -(2^24+1), 2^25+1`, случайные. Тест ОБЯЗАН падать, если появится `f32`-roundtrip (`16_777_216` / `16_777_217`).
- Сквозной приёмочный тест: 16/44.1 стерео, `Strict`, устройство 44.1 native → выход колбэка побитово равен декодированному PCM.

### 21.5 DoP golden

Известный payload → кодер → ожидаемые байты кадра. Покрытие: нулевой, все единицы, чередующийся payload; границы маркеров и их чередование; чередование каналов; endian; границы блоков.

### 21.6 Негативные Bit-Perfect

Для каждого условия Planner не выдаёт `BitPerfectPcm`: при `Strict` — `TrackRejection` с соответствующей причиной, при `BestPossible` — `DspPcm`: несовпадение частоты, формата, числа каналов, раскладки; включены ресемплер, DSP, микшер, дизеринг; громкость ≠ 1; бэкенд не гарантирует native-путь; несовпадение типа колбэка; неизвестное отображение потока устройства; exclusive/native недоступен, когда требуется. Отдельно: `stereo→mono`, `mono→stereo`, `surround→stereo`, перестановка каналов — не Bit-Perfect.

### 21.7 RT: аллокации и производительность

- Колбэк во всех ветках: аллокаций = 0, деаллокаций = 0, блокировок = 0, syscall = 0 (существующие `tests/rt_zero_alloc.rs` расширяются на типизированные колбэки).
- Worker: аллокации только на подготовке и в контролируемых не-RT фазах.
- Бенчмарки: push/pop типизированного кольца, копирование в колбэке, обработка поколений, DSP-колбэк `f32`, отвод визуализатора; метрики p50/p95/p99/p99.9/max; условия: малый/средний/большой буфер, кольцо почти пустое/почти полное, установившийся режим, переход seek, визуализатор вкл/выкл. Seek — худший случай заполненности кольца: работа колбэка не зависит от неё.
- Переполнение scratch (§13.4) не происходит молча: тест на `data.len() > MAX_OUT_SAMPLES`.

### 21.8 Граница CPAL, бэкенд, визуализатор

- `SampleFormat::I16 → callback(&mut [i16])`, `I32 → &mut [i32]`, `F32 → &mut [f32]`; для I24/контейнеров — assertions конкретного бэкенда и golden-отображение.
- Бэкенд: Shared никогда не открывает `hw:*`; Exclusive — `hw:*` разрешён; DoP — Exclusive + корректные маркер/упаковка. Отдельно `PhysicalDevice`, Shared и Exclusive endpoints — регрессия Xonar DX/PipeWire не возвращается.
- Визуализатор: отвод после DSP и до кольца; не изменяет сэмплы воспроизведения; его backpressure не блокирует воспроизведение; его отключение не меняет `SignalPath`.

### 21.9 Проверка на железе — три слоя

1. **Программа:** запрошенный и выбранный формат, частота, раскладка, `SignalPath`, тип колбэка, состояние fallback — что приложение *считает*, что запросило.
2. **Бэкенд/драйвер:** фактический формат потока, конверсия бэкенда, поведение shared/exclusive/native, микшер/ресемплер драйвера — что *создала* ОС.
3. **Физика** (где есть оборудование): петля ЦАП / анализатор — native-частота, DoP-маркер/payload, режим native DSD, неожиданная конверсия, раскладка каналов. Только этот слой даёт сквозное доказательство; программные тесты не доказывают bit-perfect на уровне ЦАП.

Матрица (где поддерживается железом): PCM 16/44.1, 16/48, 24/44.1, 24/48, 24/96, 24/192, 32/44.1, 32/96; DSD64, DSD128, DSD256. Результаты — в `docs/hardware_verification_audio_integrity.md` (ОС, бэкенд, устройство, драйвер, источник, частота, формат, каналы, раскладка, запрошенный и фактический режим, конфиг CPAL, поведение бэкенда, физическое свидетельство, дата, результат). Отдельно фиксируется сценарий «устройство без 44.1 кГц: `BestPossible` → `DspPcm` с SRC, `Strict` → трек пропущен; UI сообщает фактическое состояние и причину».

### 21.10 Связь с приёмкой

Критерии §23 оформляются пакетом `docs/acceptance/ap5.0/criteria.yaml` и прогоняются `tools/acceptance.py` на Linux и Windows после завершения рефакторинга. Проверки слоёв 2–3 (§21.9) — ручные критерии со `scope: any`: достаточно одной машины с нужным оборудованием.

---

## 22. Порядок внедрения

Рефакторинг поверх A2.0, не с нуля. Каждый этап — серия микро-шагов по `AGENTS.md` (при расхождении действуют правила `AGENTS.md`): один шаг — одна сущность, 1–2 файла, отдельный проверяемый коммит; `cargo check` / `test` / `clippy` по регламенту.

| Этап | Содержание | Приоритет |
|---|---|---|
| 0 | Базовая линия; исправление молчаливого переполнения scratch (§13.4) — дефект текущего кода | P0 |
| 1 | Модель: `PlaybackPolicy`, `AdvancedPreferences`, `SignalPath`, `ConversionPlan`, `PcmSampleFormat`, `I24`, `TrackRejection`/`RejectionSource`, `TrackOpenResult`, `StreamContract` — чистые типы + тесты инвариантов; без UI, без изменений RT | P0 |
| 2 | Устройства: `PhysicalDevice`, Shared/Exclusive endpoints, запрет Shared → `hw:*`, удаление `collapse_same_name()` (регрессия Xonar DX) | P0 |
| 3 | `PathPlanner` вне `output.rs` + тесты §21.1–21.2 и property-тест | P1 |
| 4 | Интеграция: `Player` получает `PathPlan`; удаление `bit_perfect: AtomicBool` как источника истины; `PlaybackManager`: отказ → пропуск, конечный автомат §15; миграция настроек §18 | P0/P1 |
| 5 | Типизированный PCM-транспорт до CPAL: I16 → I24 → I32 (каждый — транспорт, golden-векторы, колбэк), проверка формата CPAL §10.5 | P0 |
| 6 | RT: ограниченная инвалидация seek §13.3, бенчмарк худшего случая, error-callback без `eprintln!` | P1 |
| 7 | DSD/DoP: типизированный DoP, golden-векторы, согласование Native DSD | P1 |
| 8 | Декодер: `Result` у seek, классификация ошибок, счётчики, аудит `unwrap`/`expect` | P2 |
| 9 | UI: `PlaybackStatus`, «Дополнительно» §5.5, визуализатор/UI §16.1, `RefCell`-реентерабельность §16.3 | P1 (статус, «Дополнительно») / P2 (остальное) |
| 10 | DSP: измерительный стенд ресемплера, АЧХ, алиасинг, CPU/аллокации; кольцевой буфер в ресемплере | P2 |
| 11 | Верификация: полный golden-набор, RT-бенчмарки, матрица железа, синхронизация документации (V5.1, A2.0, A3.0), приёмка §21.10 | — |

---

## 23. Критерии приёмки

Рефакторинг завершён, когда выполнены все пункты. Каждый пункт — отдельный критерий пакета приёмки.

**Архитектура**

1. `PlaybackPolicy` существует как отдельный тип; `AdvancedPreferences` — отдельный тип и не являются вторым источником истины.
2. `PathPlanner` — единственная точка выбора пути; чистая функция, покрыта тестами §21.1–21.2.
3. `SignalPath` описывает фактическую обработку; `BackendEndpoint` отделён от пути.
4. `bit_perfect + resampler` (и любые запреты §20) не существуют как валидное состояние.
5. Старый `settings.toml` мигрирует без потери намерений пользователя.

**Политики и предпочтения**

6. Shared никогда не открывает raw `hw:*` — ни через политику, ни через UI, ни через `settings.toml`.
7. Strict никогда не выполняет преобразований; DSD→PCM никогда не называется Bit-Perfect.
8. BestPossible выполняет преобразование только по решению Planner.
9. `AdvancedPreferences` не расширяют множество путей политики — property-тест.
10. «Доступ к устройству» редактируем только при `BestPossible`.
11. Отказ различает `RejectionSource::Policy` и `Preference`; источник виден в UI.
12. «Дополнительно» сохранено в UI (§5.5).

**Плейлист**

13. Несовместимый трек при `Strict` автоматически пропускается, плейлист продолжается; при полном отказе — одно сообщение.

**Целостность сигнала**

14. Декодированный целочисленный PCM сохраняет идентичность: I16, I24, I32 — точный транспорт до колбэка CPAL (golden §21.4, сквозной тест).
15. Строгий путь не проходит через `f32`; тип колбэка и поток устройства совпадают с представлением или Bit-Perfect недоступен.
16. В Bit-Perfect нет ресемплера, преобразования каналов, громкости ≠ 1, дизеринга, DSP (негативные тесты §21.6).
17. Native DSD и DoP — отдельные типизированные пути; DoP golden-векторы проходят.

**RT**

18. В колбэке 0 аллокаций, 0 блокировок, 0 syscall, нет декодера/FFT/UI.
19. Работа колбэка — `O(output_buffer_len)`; seek-инвалидация не зависит от заполненности кольца (бенчмарк).
20. Переполнение scratch и underrun наблюдаемы, не молчаливы; underrun не блокирует.
21. Контракт планирования потоков задокументирован.

**Декодер**

22. Нет молчаливого seek в 0; ошибки декодера классифицированы, фатальные сообщаются, счётчики наблюдаемы; production-`unwrap`/`expect` проаудированы.

**UI и визуализатор**

23. UI показывает фактический путь, бэкенд, преобразование и причину — независимо от предпочтений; никогда «Bit-Perfect» при DSP.
24. Визуализатор: отвод после DSP и до кольца, ограниченная очередь, не блокирует воспроизведение; модель последнего значения 30–60 Гц; `Weak`-ссылки на UI; пакетные обновления свойств.

**DSP**

25. Ресемплер измерен по обязательной матрице §17; числовые пороги утверждены и выполнены; 0 аллокаций в горячем пути.

**Бэкенд и железо**

26. Выбор Shared/Exclusive endpoint покрыт тестами бэкенда; регрессия Xonar DX/PipeWire устранена.
27. Проверены программный слой и слой бэкенда; физический слой — где есть оборудование; создан `docs/hardware_verification_audio_integrity.md`.
28. Документация (V5.1, A2.0, A3.0) синхронизирована с реализацией.

---

## 24. Открытые вопросы

1. **«Формат DoP»** в «Дополнительно»: что выбирает пользователь — контейнерную частоту, разрядность контейнера (24/32) или иное?
2. **`allow_fallback`**: предложение — при `BestPossible` разрешает Planner перейти к следующему endpoint/пути, если *открыть* выбранный не удалось (ошибка выполнения, а не отказ по возможностям); при `Strict` игнорируется.
3. **Таблица миграции** старых полей `settings.toml` (включая A3.0) — составляется на этапе 4.
4. **Расширения предпочтений** («предпочитаю 44.1-кратные частоты», «PCM не ниже 24 bit»): ввести `clock_family` / `min_bit_depth` сейчас или в P2? (`ClockFamily`/`FallbackRatePolicy` уже есть в A3.0.)
5. **Выбор целевой частоты** при SRC для `SourceIfPossible` vs `BestSupported` — зафиксировать правило.
6. **Числовые пороги ресемплера** (§17) — утверждаются по результатам измерений этапа 10.
7. **Отображение I24 → контейнер** конкретного бэкенда (ALSA, WASAPI) — определяется и документируется на этапе 5.
8. **`SignalPath` при `Shared` без преобразований в плеере.** `BitPerfectPcm` при Shared запрещён (§4), `DspPcm` неточен (преобразования нет). Предложение: отдельный вариант `SignalPath::SharedPcm(PcmFormat)` — «плеер передал без изменений, дальше системный аудиотракт».

---

## 25. Как менялась концепция и что заменяет этот документ

Номера версий и даты в исходных документах ненадёжны (например, у «v4.0» в истории правок стоит «2025»), поэтому этапы восстановлены по содержанию. При расхождении действует решение более позднего этапа.

| Этап | Концепция | Документы |
|---|---|---|
| 1 | Настройки bit-perfect противоречат тому, что делает код; главная проблема — транспорт через `f32`. Вывод: «strict bit-perfect должен отказывать, а не ресемплировать» | code review GPT — `docs/reviews/gpt_review_bitperfect_float.md` |
| 2 | Жёсткий режим Strict Bit-Perfect: строгие определения, типизированный транспорт до CPAL, владение памятью, RT-контракт, DSD/DoP, golden-тесты, три слоя проверки железа | спека «Audio Integrity & Strict Bit-Perfect Architecture» (две редакции, «v4.0» и «v4.1»; вторая полностью заменяет первую) |
| 3 | Политики воспроизведения (Shared / BestPossible / Strict): Bit-Perfect — свойство пути, а не режим плеера; техника этапа 2 становится нижним уровнем инвариантов; регрессия Xonar DX/PipeWire → endpoints | code review 2026-09-21 — `docs/reviews/gpt_review_new_policy_2026-09-21.md`; архитектурная спека «v5.0» |
| 4 | Политики + тонкие настройки: «Дополнительно» как слой предпочтений, только сужающий выбор Planner; отказ от `f32` остаётся сквозным требованием | дополнения «Advanced Preferences» к спеке и к ревью (2026-09-22) — `docs/reviews/gpt_review_addendum_advanced_preferences_2026-09-22.md`; решения пользователя 2026-09-22 |

Отказ от `f32` как универсального транспорта — не отдельный этап, а нить через все четыре: главная проблема на этапе 1, типизированный транспорт на этапе 2, P0 на этапах 3–4.

Отдельно учтено RT-ревью текущего кода — `docs/reviews/claude_review_rt_audio_2026-09-22.md` (переполнение scratch, `RefCell`, seed дизеринга, backoff worker'а, error-callback).

### Решения при расхождении этапов

- Пользователь выбирает политику, а не «Bit-Perfect»: из этапа 2 не переносятся `BitPerfectFallback { Dsp, Reject }` и `BitPerfectAvailability`/`BitPerfectRejection` — их роль играют политика и `TrackRejection` (§8.3, §9).
- Техника этапа 2 (представление, транспорт, владение, CPAL, RT, DSD/DoP, тесты, проверка железа) сохраняется целиком как нижний уровень (§10–14, §21).
- Порядок работ — по этапу 3 (политика и модель раньше транспорта), но типизированный транспорт остаётся P0.
- «Дополнительно» (этап 4) принят с уточнениями пользователя: доступ к устройству редактируем только при `BestPossible`; DSD-предпочтение — набор разрешённых путей с фиксированным порядком Native → DoP → PCM (переставлять нельзя) вместо 6 вариантов enum + отдельного `DopPreference`; источник отказа (`RejectionSource`) обязателен.
- Идентификаторы микро-шагов `A4.x.y` из этапа 2 не используются; задачи заводятся в `ROADMAP.md` с префиксом `AP5.0`.

### Удаляются после утверждения

каталог `_TODO_/done/ap5.0-sources/` целиком (v4.0, v4.1, базовая v5.0, дополнение «Advanced Preferences», план `wiggly-baking-rivest.md`; лежат в `done/`, чтобы Шаг 2 AGENTS.md не разбирал их как новые задачи); судьба ревью в `docs/reviews/` — по решению пользователя.
