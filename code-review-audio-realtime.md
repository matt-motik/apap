# Код-ревью: аудио-движок (Real-Time) и Slint UI

Ревьюер: Senior Rust / High-End Audio (DSP, ALSA/ASIO/WASAPI через cpal)
Объём: `src/audio/{output.rs, player.rs, worker.rs, decoder.rs, dsd.rs}`, `src/app/{mod.rs, ui_manager.rs, playback_manager.rs}`, `src/main.rs`

---

## Вердикт по архитектуре

Разделение UI/Audio выполнено **грамотно и по учебнику Producer/Consumer**:

- `cpal`-колбэк (`audio_callback_f32_rt` / `_i16_rt` / `_u8_rt` / `_i32_pcm_rt` / `_i32_dop_rt` в [player.rs](src/audio/player.rs:961)) — чистый consumer: читает только `RtConsumer` (SPSC-кольцо `rtrb` + предвыделенный `scratch: Vec<f32>`, созданный один раз при открытии потока) и атомики `RtShared` ([worker.rs:34](src/audio/worker.rs:34)). Ни одного `Mutex`, ни одной аллокации, ни одного `unwrap()` в этом пути — это именно то, что требуется для hard-real-time аудио-колбэка.
- Декодирование + ресемплинг вынесены в отдельный поток `PlaybackWorker` ([worker.rs:380](src/audio/worker.rs:380)), который пушит PCM в кольцо и *не* является RT-потоком — там `Mutex` на визуализаторном тапе (`VizTap`) уместен и явно прокомментирован как допустимый компромисс.
- Управление (volume/mute/bit-perfect/dither/seek) идёт через атомики `RtShared` с корректной моделью памяти: `seek_generation`/`seek_target_frames` пишутся на UI-потоке, транспортируются воркеру через `mpsc`-канал (что даёт полноценный happens-before), а подтверждение `seek_done_generation` синхронизировано парой `Release` (воркер) / `Acquire` (consumer, [worker.rs:285](src/audio/worker.rs:285)) — по факту это учебный пример корректного release-acquire хэндшейка, без гонок.
- TPDF-дизеринг генерируется собственным zero-alloc LCG/xorshift32 (`TpdfRng`, [player.rs:28](src/audio/player.rs:28)) — явно прокомментировано, почему `rand::thread_rng()` запрещён. Полное соответствие пункту 3 регламента AGENTS.md.
- DoP-путь (`audio_callback_i32_dop_rt`, [player.rs:1114](src/audio/player.rs:1114)) вообще не трогает volume/mute/dither — корректно, т.к. любое масштабирование DSD-over-PCM исказит служебные маркеры кадра.

**Итог:** RT-путь спроектирован так, как должен быть спроектирован high-end аудио-движок. Основные риски находятся не в самом hot-path, а на границах (переполнение фиксированного scratch-буфера) и в UI-слое (реентерабельность `RefCell`).

---

## Критические проблемы (блокирующие)

### 1. Тихое усечение аудио при `data.len() > MAX_OUT_SAMPLES` — потенциальный глитч без диагностики

`RtConsumer::scratch` выделяется один раз ёмкостью `MAX_OUT_SAMPLES = 1 << 16` (65536 сэмплов, [worker.rs:26](src/audio/worker.rs:26)). Это корректный zero-alloc паттерн — **но** ни в `pull_scratch` ([worker.rs:321](src/audio/worker.rs:321)), ни в `build_stream_rt` ([output.rs:1569](src/audio/output.rs:1569)) нет проверки, что запрошенный cpal-буфер `data.len()` не превышает эту ёмкость.

`pull_scratch` тихо делает `let n = len.min(cap);` — если реальный колбэк-буфер окажется больше 65536 сэмплов (например, эксклюзивный WASAPI/ASIO с `BufferSize::Default`, который в общем случае выбирает драйвер, а не приложение — см. [output.rs:1538](src/audio/output.rs:1538), где явно есть путь `BufferSize::Default` без верхней границы), остаток буфера `data` будет молча заполнен тишиной (`for s in data.iter_mut().skip(produced) { *s = 0 }`). Для пользователя это звучит как периодические щелчки/провалы на ровном месте, и диагностировать такое крайне тяжело — нет ни лога, ни счётчика, ни паники, которая бы указала на причину.

**Почему это критично для high-end аудио:** именно такие «тихие» деградации хуже честного краха — плеер продолжает работать, репортует bit-perfect в UI, но реально дропает сэмплы.

**Рефакторинг (было / стало):**

```rust
// БЫЛО — output.rs, build_stream_rt: буфер устройства не проверяется
// относительно ёмкости RtConsumer::scratch.
pub fn build_stream_rt(
    spec: &OutputSpec,
    consumer: RtConsumer,
    error_flag: Option<Arc<AtomicBool>>,
) -> Result<cpal::Stream, String> {
    match spec.sample_format {
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U8 | SampleFormat::I32 => {}
        other => return Err(format!("Unsupported output sample format: {other:?}")),
    }
    // ... сразу строим поток
}
```

```rust
// СТАЛО — валидация на границе открытия потока (не в RT-пути!),
// плюс диагностический счётчик, который UI/bp-report может показать.
pub fn build_stream_rt(
    spec: &OutputSpec,
    consumer: RtConsumer,
    error_flag: Option<Arc<AtomicBool>>,
) -> Result<cpal::Stream, String> {
    match spec.sample_format {
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U8 | SampleFormat::I32 => {}
        other => return Err(format!("Unsupported output sample format: {other:?}")),
    }

    // Верхняя граница периода, которую реально может запросить cpal-колбэк
    // для этой конфигурации (BufferSize::Fixed уже ограничен раньше;
    // BufferSize::Default может прийти от драйвера — оцениваем worst-case
    // по частоте и защищаемся заранее, а не молчим в RT-колбэке).
    let max_frames_hint = worst_case_callback_frames(&spec.config);
    let max_samples_hint = max_frames_hint * spec.config.channels as usize;
    if max_samples_hint > crate::audio::worker::MAX_OUT_SAMPLES {
        return Err(format!(
            "Device period ({max_samples_hint} samples) exceeds the RT scratch capacity \
             ({}); refusing to open a stream that would silently drop samples",
            crate::audio::worker::MAX_OUT_SAMPLES
        ));
    }
    // ... строим поток как раньше
}
```

```rust
// worker.rs — RtConsumer::pull_scratch: превращаем тихое усечение
// в наблюдаемый факт (атомарный счётчик, не паника — RT-поток паниковать
// не должен), который bp-report/диагностика могут показать пользователю.
pub fn pull_scratch(&mut self, len: usize) -> usize {
    let cap = self.scratch.len();
    let n = len.min(cap);
    if n < len {
        // Не паникуем в RT-пути — только считаем; счётчик читается
        // не-RT потоком в диагностике.
        self.shared.scratch_overrun.fetch_add(1, Ordering::Relaxed);
    }
    // ... остальное без изменений
}
```

Это не «теоретическая придирка»: given, что проект уже поддерживает multichannel (до 6+ каналов, судя по тестам в output.rs) и высокие частоты дискретизации DSD/PCM, произведение `frames × channels` может подойти к границе быстрее, чем кажется, особенно в эксклюзивном режиме, где период диктует драйвер, а не приложение.

---

### 2. `Rc<RefCell<MusicApp>>` + ~60 колбэков + 3 независимых таймера = риск `BorrowMutError` при реентерабельном вызове

В [main.rs:30-68](src/main.rs:30) три независимых `slint::Timer` (`tick` каждые 100мс, `reflow` каждые 16мс, `viz_push` на частоте FPS) держат по своей копии `Rc<RefCell<MusicApp>>` и делают `app_for_tick.borrow_mut()` в каждом тике. Плюс в [mod.rs:640](src/app/mod.rs:640) `bind_callbacks` регистрирует порядка 60 Slint-колбэков (`on_play_pause`, `on_seek`, `on_settings_*` и т.д.), каждый из которых тоже берёт `this.borrow_mut()` на `Rc<RefCell<Self>>`.

Slint выполняет и таймеры, и колбэки на одном (UI) потоке, так что классической data race тут нет. Но **реентерабельность** — да: если внутри `tick()`/`reflow()`/любого `on_*`-обработчика происходит синхронный вызов в Slint (`ui.set_...()`, `invoke_...()`), который **синхронно** триггерит другой колбэк, привязанный к тому же `Rc<RefCell<MusicApp>>` (например, `changed`-хэндлер на свойстве, которое меняется внутри `tick()`), — второй `borrow_mut()` **запаникует** (`already borrowed: BorrowMutError`), уронив всё приложение, включая активно играющий аудио-поток.

При 60+ точках входа и постоянно растущем UI (`viz_settings_manager.rs`, `fulltrack_manager.rs` и т.д.) гарантировать отсутствие такого сценария вручную с каждым новым PR практически невозможно — это системный риск, а не разовый баг.

**Рефакторинг (было / стало):**

```rust
// БЫЛО — main.rs: паника при реентерабельном borrow_mut() убивает процесс,
// включая активный аудио-поток.
timer.start(
    slint::TimerMode::Repeated,
    std::time::Duration::from_millis(TICK_INTERVAL_MS),
    move || {
        if weak.upgrade().is_none() {
            return;
        }
        app_for_tick.borrow_mut().tick();
    },
);
```

```rust
// СТАЛО — try_borrow_mut: при реентерабельном вызове тик молча
// пропускается (следующий тик через 100мс всё равно досчитает состояние),
// вместо падения всего приложения. Плюс лог для диагностики в разработке.
timer.start(
    slint::TimerMode::Repeated,
    std::time::Duration::from_millis(TICK_INTERVAL_MS),
    move || {
        if weak.upgrade().is_none() {
            return;
        }
        match app_for_tick.try_borrow_mut() {
            Ok(mut app) => app.tick(),
            Err(_) => {
                #[cfg(debug_assertions)]
                eprintln!("tick(): re-entrant borrow skipped — investigate call chain");
            }
        }
    },
);
```

Это симптоматическое лечение (не даёт крашнуться), а не устранение первопричины. Правильное решение по архитектуре — либо гарантировать, что ни один Slint-колбэк, вызываемый из `tick()`/`reflow()`/`viz_push()`, не совершает синхронных property-set’ов, триггерящих другой колбэк на том же `MusicApp` (это надо доказывать при код-ревью каждого нового `ui.set_*` внутри обработчиков), либо — если проект наберёт больше веса — рассмотреть переход на паттерн с явной пере-энтерабельной защитой (`Cell<bool>` "in progress" флаг рядом с `RefCell`, который дешевле полного `RefCell::try_borrow` дебага). Как минимум `try_borrow_mut` в трёх точках входа таймеров — дешёвая страховка уже сейчас.

---

## Оптимизация High-End (аудио-данные, DSP, bit-perfect)

1. **Bit-perfect уже корректно замкнут в разных слоях**: `bit_perfect()` форсит `vol = 1.0` и отключает дизеринг при `!resampler_enabled` ([player.rs:938](src/audio/player.rs:938), [player.rs:970](src/audio/player.rs:970)); в `ui_manager.rs` (по вашему собственному регламенту AGENTS.md §4) UI обязан блокировать программные слайдеры при активном bit-perfect. Стоит добавить unit-тест, который явно проверяет **инвариант**: при `bit_perfect=true && resampler_enabled=false` выходной сэмпл **побитово равен** входному после прохождения через `audio_callback_f32_rt` (сейчас тесты в `worker.rs` проверяют позиционирование/ring, но не побитовую идентичность сигнала на выходе колбэка). Это關 недорогой тест, который бы формально фиксировал главное УТП продукта.

2. **`ring_capacity`** ([player.rs:97](src/audio/player.rs:97)) корректно считает пол в `max(4096, 2×buffer_frames)` — защита от андеррана продумана. Рекомендую вынести magic-константу `4096` в именованную константу рядом с `MAX_OUT_SAMPLES`, чтобы связь между «минимальной глубиной кольца» и «максимальной ёмкостью scratch» (см. проблему №1) была явной в коде, а не только в голове у автора.

3. **`push_all`** ([worker.rs:563](src/audio/worker.rs:563)) при заполненном кольце делает `thread::sleep(Duration::from_millis(1))` в цикле. Для воркер-потока это нормально (не RT), но при высоких PCM-частотах (DSD256+/768kHz) 1мс — это довольно грубый шаг бэкоффа относительно объёма данных, который нужно протолкнуть за этот период; можно рассмотреть `park_timeout` с меньшим интервалом или адаптивный бэкофф (начинать с более короткого сна и увеличивать), чтобы worker быстрее реагировал на освобождение места в кольце при очень высоких частотах — не критично, но заметно на профилировании при экстремальных DSD-режимах.

4. **`dither_amplitude`**/**TPDF** — реализация математически верна (сумма двух равномерных распределений даёт треугольное; амплитуда 1 LSB для полного TPDF и 0.5 LSB для «triangular»), сид детерминирован по пути файла ([player.rs:85](src/audio/player.rs:85)) — воспроизводимо между запусками, что хорошо для регрессионного тестирования звука. Единственное замечание: `track_seed` использует `DefaultHasher`, чьи гарантии стабильности между версиями Rust **не даются** stdlib (только «стабилен в рамках одной версии компилятора»). Для аудиофильского продукта, где воспроизводимость дизер-паттерна между сборками может быть частью QA-процесса, стоит заменить на явно зафиксированный алгоритм (например, FNV-1a или xxhash с фиксированной спецификацией), чтобы обновление тулчейна не поменяло шум незаметно для тестов.

---

## Прочие находки (не блокирующие)

- **`eprintln!` в error-callback cpal** ([output.rs:1594](src/audio/output.rs:1594) и еще 3 места) — это не data-колбэк, а отдельный error-callback, вызываемый редко (device error), так что жёсткого нарушения real-time правил тут нет. Тем не менее, если он в принципе может быть вызван с драйверного потока с чужим приоритетом, надёжнее не трогать `stdout` напрямую, а взводить тот же атомарный `error_flag`, который уже прокидывается по коду, и логировать из UI-потока на `tick()`. Это унифицирует обработку ошибок устройства в одном месте вместо двух (`eprintln!` + `error_flag`).
- **Тестируемость RT-ядра — сильная сторона проекта.** `RtConsumer`/`RtShared`/`worker_loop` покрыты содержательными unit-тестами (`worker_fills_ring_with_exact_frames`, `worker_seek_resets_ring_and_position`, `consumer_never_tears_a_partial_frame` и т.д., [worker.rs:643-797](src/audio/worker.rs:643)) — именно так и нужно тестировать lock-free код: через наблюдаемое поведение (позиция, число сэмплов), а не через подглядывание в приватное состояние. Это заметно выше среднего для audio-движков на Rust.
- **Тестируемость UI-слоя ниже** — логика обработчиков в `bind_callbacks` живёт прямо в замыканиях, что затрудняет unit-тестирование конкретной ветки без поднятия Slint-компонента целиком. Судя по вынесению части логики в менеджеры (`playback_manager.rs`, `ui_manager.rs`), тренд правильный — рекомендую продолжать выносить тело каждого `move || { ... }` в отдельный `fn on_xxx(&mut self, ...)` метод менеджера, оставляя в замыкании только один вызов; так закрывается и вопрос тестируемости, и диагностика реентерабельности (проблема №2) становится проще — видно все точки, которые могут дергать `ui.set_*`.

---

## Резюме

| Категория | Оценка |
|---|---|
| Изоляция RT-аудио-потока (аллокации, локи, паники) | Отлично — образцовая реализация |
| Lock-free обмен UI↔Audio | Отлично — корректная модель памяти (release/acquire, SPSC) |
| Bit-perfect корректность | Хорошо, не хватает явного побитового unit-теста |
| Обработка ошибок / unsafe | unwrap/expect только в тестах и на старте — корректно |
| Границы буферов (scratch vs. реальный размер cpal-буфера) | Требует исправления — тихое усечение без диагностики (см. №1) |
| Устойчивость `RefCell`-состояния UI к реентерабельности | Требует внимания — системный риск на масштабе (см. №2) |
| Тестируемость | RT-ядро — отлично; UI-колбэки — средне, есть куда расти |

Общий уровень кода — выше типичного для инди-аудиоплеера на Rust; основной риск сосредоточен не в DSP-математике (она сделана аккуратно), а в двух краевых случаях на границах системы: непроверенный размер cpal-буфера относительно фиксированной ёмкости scratch, и реентерабельность общего `RefCell` у UI-состояния.
