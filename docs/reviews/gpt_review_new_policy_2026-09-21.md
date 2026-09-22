# APAP — новое архитектурное code review
## Rust + Slint High-End Audio Player
### Ревью с учётом новой концепции трёх режимов воспроизведения

**Репозиторий:** `matt-motik/apap`  
**Ветка:** `main`  
**Дата ревью:** 2026-09-21  
**Профиль ревью:** Senior Rust Developer / System Architect / High-End Audio

---

# 1. Executive Summary

После повторного анализа текущего `main` и сопоставления его с новой концепцией воспроизведения вывод такой:

> **Текущая архитектура уже содержит хороший фундамент для RT-safe audio engine, но модель выбора аудиопути всё ещё построена вокруг старой идеи “Bit-Perfect как главный режим”, а не вокруг пользовательской политики воспроизведения.**

Это принципиально важно.

Предыдущая работа над `spec_audio_integrity_v4.1.md` была полезна в части технической строгости:

- RT callback должен быть максимально простым;
- декодирование и ресемплинг должны находиться вне callback;
- обмен с callback должен идти через SPSC ring;
- DSP не должен проникать в строгий Bit-Perfect путь;
- DoP должен формироваться только там, где действительно выбран DoP path;
- UI не должен выполнять тяжёлую аудиообработку;
- capability negotiation должна быть отделена от фактического stream opening.

**Эти принципы сохраняются.**

Но продуктовая политика должна быть поднята на уровень выше.

Нужны два независимых понятия:

```text
PlaybackPolicy       = ЧТО пользователь хочет получить
SignalPath            = ЧТО реально произошло с аудиосигналом
BackendAccess         = КАК поток попал в устройство
```

Предлагаемая модель:

```text
PlaybackPolicy
├── Shared
│   └── играем вместе с другими программами
│       → best effort
│       → без монополизации устройства
│       → Bit-Perfect не обещается
│
├── BestPossible
│   └── Exclusive
│       ├── native / exact → Bit-Perfect
│       └── unsupported → high-quality SRC/conversion → продолжаем играть
│
└── Strict
    └── Exclusive
        ├── native / exact → Bit-Perfect
        └── unsupported → этот трек не играем → skip → следующий трек
```

При этом:

```text
PlaybackPolicy != SignalPath
```

Например:

```text
BestPossible + 24/192 + DAC max 96
    ↓
SignalPath = DspPcm(96 kHz)
    ↓
High-quality SRC
    ↓
PLAY
```

А:

```text
Strict + 24/192 + DAC max 96
    ↓
BitPerfectUnavailable
    ↓
TrackRejected
    ↓
Skip to next track
```

Это ключевой архитектурный вывод настоящего ревью.

---

# 2. Вердикт по архитектуре

## Общая оценка

**Сильные стороны:**

- UI и основной audio producer уже существенно разделены.
- Есть отдельный `PlaybackWorker`.
- Используется `rtrb` SPSC ring.
- RT consumer имеет заранее выделенный scratch buffer.
- В callback нет decoder / filesystem / logging / обычного Mutex.
- Seek реализован через generation handshake.
- Resampler вынесен из callback.
- Есть чистая функция `choose_output()`, которую можно тестировать без live backend.
- Есть `DeviceInfo`, `OutputRequest`, `ChosenOutput`, то есть задел для capability negotiation уже присутствует.
- UI device enumeration вынесен в worker thread.
- UI обновления частично delta-based.
- В проекте уже есть тестируемые чистые функции выбора output.

**Главный архитектурный дефект:**

```text
старое:
Settings → bit_perfect/exclusive/fallback/resampler → output

нужно:
PlaybackPolicy
      ↓
Path Planner
      ↓
SignalPath
      ↓
Backend Endpoint
      ↓
RT Engine
```

Сейчас эти уровни частично смешаны.

### Итог

**UI/worker/RT разделение — в целом хорошее.**

**Audio format integrity — пока недостаточно строгое для настоящего Bit-Perfect.**

**Device/backend selection — архитектурно опасное место.**

**Playback policy — требует переработки.**

---

# 3. КРИТИЧЕСКАЯ ПРОБЛЕМА №1
# Текущий audio engine не является настоящим Bit-Perfect PCM pipeline

Это самая важная техническая проблема ревью.

В `src/audio/worker.rs` worker производит:

```rust
f32
```

и кладёт именно `f32` в:

```rust
rtrb::RingBuffer<f32>
```

`RtConsumer` также работает с:

```rust
Consumer<f32>
scratch: Vec<f32>
```

То есть фактический pipeline сейчас:

```text
source PCM
   ↓
decoder
   ↓
f32
   ↓
resampler
   ↓
rtrb<f32>
   ↓
scratch<f32>
   ↓
i16/i32/F32 callback
```

Это **не является строгим integer Bit-Perfect pipeline**.

Особенно явно это видно в:

`src/audio/player.rs`

```rust
let mut x = src.clamp(-1.0, 1.0) as f64
    * vol as f64
    * I32_MAX;
*dst = x.round()... as i32;
```

и для i16:

```rust
let mut x = src.clamp(-1.0, 1.0)
    * vol
    * 32767.0;
*dst = x.round()... as i16;
```

То есть даже если:

```rust
bit_perfect == true
```

данные уже прошли через `f32`.

## Почему это блокирующее

Для настоящего Bit-Perfect необходимо сохранять исходное integer sample representation до output boundary.

Например:

```text
24-bit PCM source
    ↓
I24/I32 typed transport
    ↓
ring
    ↓
I24/I32 callback
    ↓
device
```

а не:

```text
I24
 ↓
f32
 ↓
I24
```

Даже если многие значения представимы точно в `f32`, сам контракт Bit-Perfect становится неверным:

- формат источника потерян;
- scaling semantics меняются;
- clipping/rounding semantics появляются;
- integer domain больше не является источником истины;
- невозможно доказать сохранение sample identity простой проверкой buffer equality.

## Требуемое решение

Нужен типизированный transport.

Минимально:

```rust
enum PcmBuffer {
    I16(...),
    I24(...),
    I32(...),
}
```

или более производительный compile-time вариант:

```rust
PcmFormat::I16
PcmFormat::I24
PcmFormat::I32
```

и отдельные ring instances для выбранного format.

При этом DSP path может продолжать использовать:

```text
f32
```

Но только:

```text
SignalPath::DspPcm
```

---

# 4. КРИТИЧЕСКАЯ ПРОБЛЕМА №2
# `bit_perfect` сейчас является runtime flag, а не доказанным свойством path

В `RtShared` есть:

```rust
bit_perfect: AtomicBool
```

и callback делает:

```rust
let bit_perfect = consumer.shared().bit_perfect();
```

Это опасная семантика.

Сейчас можно получить состояние:

```text
bit_perfect = true
resampler_enabled = true
```

Сам проект это даже явно допускает:

```rust
pub fn bit_perfect_resampled(&self) -> bool {
    self.bit_perfect() && self.resampler_enabled
}
```

То есть система знает, что:

```text
BitPerfect + Resampled
```

одновременно существуют.

Это не должно быть допустимым состоянием модели.

## Правильная модель

Не:

```rust
bit_perfect: bool
```

а:

```rust
SignalPath
```

например:

```rust
enum SignalPath {
    BitPerfectPcm(PcmFormat),
    DspPcm(DspFormat),
    NativeDsd(DsdFormat),
    DoP(DopFormat),
}
```

Тогда callback не спрашивает:

```rust
if bit_perfect { ... }
```

а получает уже подготовленный immutable stream contract.

---

# 5. КРИТИЧЕСКАЯ ПРОБЛЕМА №3
# `collapse_same_name()` сохраняет старую ошибочную модель выбора raw hw

В `src/audio/output.rs`:

```rust
/// ... preferring `hw:*`.
fn collapse_same_name(...)
```

и:

```rust
if is_raw_hardware_id(&info.id) {
    ...
    if !is_raw_hardware_id(&kept.id) {
        *kept = info;
    }
}
```

Это было оправдано старой задачей:

> Bit-Perfect важнее всего → среди одинаковых имён выбираем raw `hw:*`.

Но для новой концепции это неправильно.

## Почему

Для Shared режима raw `hw:*` вообще не должен становиться автоматически выбранным endpoint.

Нужны разные representations одного физического устройства:

```text
PhysicalDevice
├── SharedEndpoint
│   └── PipeWire / system mixer
│
└── ExclusiveEndpoint
    └── raw ALSA hw:*
```

То есть:

```text
один DAC
    ↓
PhysicalDevice
    ├── Shared endpoint
    └── Exclusive endpoint
```

а не:

```text
один DAC
    ↓
одна строка
    ↓
hw:* всегда побеждает
```

Это особенно важно с учётом уже подтверждённого Xonar DX/PipeWire бага проекта.

---

# 6. КРИТИЧЕСКАЯ ПРОБЛЕМА №4
# Shared mode пока не является настоящим Shared backend policy

Текущий `select_output()` создаёт:

```rust
exclusive: ExclusiveMode::Off,
fallback: FallbackPolicy::Nearest,
resampler: ResamplerMode::Auto,
```

и затем вызывает:

```rust
select_output_for(&req)
```

Но endpoint resolution всё ещё происходит через общий `CpalHost`.

То есть:

```text
Shared
 ↓
CpalHost.devices()
 ↓
collapse_same_name()
 ↓
raw hw:* может стать выбранным
```

Это прямо противоречит новой политике:

> Shared никогда не открывает raw `hw:*`.

## Требуемое правило

Для Linux/PipeWire:

```text
Shared PCM
    → pcm.pipewire
    → PIPEWIRE_NODE
```

и:

```text
Shared
    → forbidden:
       snd_pcm_open("hw:*")
```

Exclusive:

```text
Exclusive PCM
    → raw ALSA hw:*
```

Exclusive DoP:

```text
Exclusive DoP
    → raw ALSA hw:*
```

Это должно быть не рекомендацией, а invariant.

---

# 7. КРИТИЧЕСКАЯ ПРОБЛЕМА №5
# `choose_output()` смешивает product policy и технический fallback

Сейчас `choose_output()` одновременно решает:

- exclusive;
- channels;
- sample rate;
- fallback;
- resampling;
- sample format;
- buffer size.

Это удобно как первая реализация, но для трёх режимов становится слишком плоским.

Нужно разделить:

```text
1. Policy planner
2. Capability planner
3. Signal-path builder
4. Backend resolver
5. Stream builder
```

Предлагаемая цепочка:

```text
TrackInfo
   +
DeviceCapabilities
   +
PlaybackPolicy
   ↓
PathPlanner
   ↓
PathPlan
   ├── access
   ├── signal_path
   ├── output_format
   ├── conversion
   └── rejection reason
   ↓
BackendResolver
   ↓
OutputEndpoint
   ↓
CPAL / ALSA / PipeWire
```

---

# 8. КРИТИЧЕСКАЯ ПРОБЛЕМА №6
# Strict mode пока не реализует “ошибка трека → следующий трек”

В `src/app/playback_manager.rs`:

```rust
match self.player.open(&path) {
    Ok(info) => {
        ...
        self.player.play();
    }
    Err(e) => {
        self.status = format!("Cannot play {title}: {e}").into();
        self.player.stop();
    }
}
```

То есть ошибка превращается в:

```text
Cannot play
→ stop
```

Но новая политика требует:

```text
Strict
track N unsupported
    ↓
record rejection
    ↓
skip track N
    ↓
try track N+1
```

Это принципиально разные semantics.

## Правильное разделение

```text
TrackFailure
```

не равно:

```text
PlaylistFailure
```

Нужны как минимум:

```rust
enum TrackOpenResult {
    Started,
    Rejected(TrackRejection),
    Fatal(PlaybackError),
}
```

И playlist engine должен уметь:

```text
Rejected
→ next candidate
```

При этом настоящий fatal error:

```text
device disappeared
backend crashed
decoder infrastructure broken
```

может остановить playback.

---

# 9. КРИТИЧЕСКАЯ ПРОБЛЕМА №7
# Shared и BestPossible должны иметь разные guarantees

Новая модель требует очень чёткого UI/engine contract.

## Mode 1 — Shared

Название пользовательского режима:

> Играем всё и всегда вместе с другими программами

Guarantee:

```text
Player guarantees:
    - не монополизирует устройство;
    - старается воспроизвести любой поддерживаемый lossless source;
    - не ломает системный audio path.

Player does NOT guarantee:
    - DAC-level Bit-Perfect;
    - отсутствие SRC в PipeWire/Windows/macOS mixer;
    - физический hardware sample rate.
```

Это важно.

Нельзя показывать:

```text
Bit-Perfect
```

только потому, что player сам не делал SRC.

Правильнее:

```text
Shared
24/192 source
→ system audio path
→ actual DAC conversion unknown
```

---

# 10. Mode 2 — BestPossible

Пользовательская семантика:

> Стремимся к лучшему из возможного

Это **exclusive mode**.

Алгоритм:

```text
track format
      ↓
exact hardware support?
      ├── YES → native
      │          → BitPerfect
      │
      └── NO
           ↓
       best supported output
           ↓
       high-quality SRC/conversion
           ↓
       DspPcm
           ↓
       PLAY
```

Пример:

```text
source: 24/192
DAC:    max 96
```

Результат:

```text
24/192
 ↓
high-quality SRC
 ↓
24/96
 ↓
exclusive
 ↓
PLAY
```

UI:

```text
Exclusive · 96 kHz
Resampled from 192 kHz
```

Но **не**:

```text
Bit-Perfect
```

---

# 11. Mode 3 — Strict

Пользовательская семантика:

> Только лучшее

Алгоритм:

```text
source
 ↓
exact native capability?
 ├── YES → BitPerfect
 │
 └── NO → reject track
             ↓
          skip
             ↓
          next track
```

Например:

```text
playlist:

24/96
24/192
DSD256
16/44.1
```

DAC:

```text
max 96 kHz
```

Strict:

```text
24/96   → PLAY
24/192  → SKIP
DSD256  → SKIP
16/44.1 → PLAY
```

При этом skipped tracks должны иметь reason:

```text
24/192 — device does not support 192 kHz
DSD256 — native/DoP path unavailable
```

Если вся playlist несовместима:

```text
No playable tracks under Strict policy
```

Это уже понятное пользовательское поведение.

---

# 12. Рекомендованная модель Rust

## PlaybackPolicy

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackPolicy {
    /// Shared system audio path.
    /// Never monopolizes the device.
    Shared,

    /// Exclusive access; preserve native format when possible,
    /// otherwise use the best available conversion.
    BestPossible,

    /// Exclusive access; no format/rate conversion.
    /// Unsupported tracks are rejected and skipped.
    Strict,
}
```

## SignalPath

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalPath {
    BitPerfectPcm(PcmFormat),
    DspPcm(PcmFormat),
    NativeDsd(DsdFormat),
    DoP(DopFormat),
}
```

## Access

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendAccess {
    Shared,
    Exclusive,
}
```

## PathPlan

```rust
pub struct PathPlan {
    pub policy: PlaybackPolicy,
    pub access: BackendAccess,
    pub signal_path: SignalPath,
    pub source_format: SourceFormat,
    pub output_format: OutputFormat,
    pub conversion: ConversionPlan,
}
```

## Rejection

```rust
pub enum TrackRejection {
    SampleRateUnsupported {
        requested: u32,
        supported: String,
    },

    SampleFormatUnsupported,

    ChannelsUnsupported,

    ExclusiveUnavailable,

    NativeDsdUnavailable,

    DopUnavailable,

    NoValidSignalPath,
}
```

---

# 13. Как должен работать planner

Главный принцип:

> Planner не открывает устройство.

Он только отвечает:

```text
“Какой путь допустим?”
```

Пример:

```rust
pub fn plan(
    track: &TrackFormat,
    device: &DeviceCapabilities,
    policy: PlaybackPolicy,
) -> Result<PathPlan, TrackRejection>
```

### Shared

```text
PlaybackPolicy::Shared
    ↓
BackendAccess::Shared
    ↓
choose shared endpoint
    ↓
best player-side PCM representation
```

### BestPossible

```text
PlaybackPolicy::BestPossible
    ↓
try native
    ↓
if unavailable:
    choose best supported output
    ↓
SRC
```

### Strict

```text
PlaybackPolicy::Strict
    ↓
require native exact path
    ↓
otherwise Err(TrackRejection)
```

---

# 14. Resampler: текущая реализация

`Resampler` находится в `src/audio/output.rs`.

Сильные стороны:

- preallocated `Vec`;
- bounded capacity;
- несколько алгоритмов;
- 1:1 path может bypass;
- sinc window coefficients создаются при construction;
- resampler не находится внутри CPAL callback.

Это хороший фундамент.

Но есть несколько проблем.

## 14.1 `Vec::drain()` на worker — допустим, но не RT invariant

В resampler:

```rust
self.buf.drain(..n_frames * self.src_ch);
```

Это не callback, поэтому это не RT allocation problem.

Но `drain` двигает большой объём памяти.

Для long-running playback лучше рассмотреть:

```text
fixed circular source buffer
```

вместо:

```text
Vec + drain
```

Особенно для sinc128.

---

# 15. Resampler и High-End policy

Текущий default:

```rust
ResamplerAlgorithm::SincMedium
```

это разумный default для BestPossible.

Но алгоритм должен быть частью:

```text
DspPcm path
```

и никогда не должен быть доступен через:

```text
BitPerfectPcm
```

То есть:

```rust
if signal_path.is_bit_perfect() {
    assert!(!resampler.enabled());
}
```

лучше заменить архитектурой, в которой такое состояние невозможно.

---

# 16. Dither

Текущий dither применяется в callback.

Это допустимо с точки зрения RT, потому что:

- состояние маленькое;
- нет allocation;
- нет lock;
- вычисление дешёвое.

Но семантически dither должен быть частью:

```text
DspPcm
```

или explicit quantization stage.

Для:

```text
BitPerfectPcm
```

dither запрещён.

Нельзя делать:

```text
BitPerfect + Resample + Dither
```

и затем просто показывать warning.

В Strict policy такой path должен быть rejected.

---

# 17. DoP

Текущий `audio_callback_i32_dop_rt()` делает:

```rust
*dst = ((src.clamp(0.0, 16_777_215.0) as u32) << 8) as i32;
```

Сам принцип packing разумен для 24-bit DoP payload.

Но архитектурно DoP должен быть отдельным path:

```text
SignalPath::DoP
```

а не:

```text
f32 transport
+
is_dop bool
```

`is_dop` в `OutputSpec` — слабый тип.

Лучше:

```rust
OutputFormat::Dop24 {
    container_rate: u32,
}
```

Так невозможно случайно использовать обычный PCM callback для DoP.

---

# 18. RT callback — что уже сделано хорошо

`RtConsumer`:

```rust
ring: rtrb::Consumer<f32>,
scratch: Vec<f32>,
```

создаётся до запуска stream.

Это хорошо.

В callback нет:

```text
Vec::new
Box::new
String
filesystem
decoder
network
normal Mutex
```

Это соответствует основному RT принципу.

Seek generation handshake тоже хорошо спроектирован:

```text
UI
 ↓
seek_generation
 ↓
worker
 ↓
seek_done_generation
 ↓
RT consumer
```

И использование:

```rust
Ordering::Acquire
Ordering::Release
```

на завершении seek — правильное направление.

---

# 19. Но `Arc` в callback contract требует дополнительного внимания

`RtConsumer` содержит:

```rust
shared: Arc<RtShared>
```

Сам callback не делает:

```rust
Arc::clone()
```

что хорошо.

Но архитектурно лучше, чтобы RT object владел ссылкой на immutable/shared state без необходимости refcount operations во время callback.

Текущая реализация приемлема, потому что `Arc` создаётся заранее.

Invariant должен быть документирован:

```text
NO Arc::clone/drop INSIDE callback
```

---

# 20. Visualizer tap

В `worker.rs`:

```rust
pub type VizTap = Arc<Mutex<Option<rtrb::Producer<f32>>>>;
```

Worker может брать:

```rust
viz_tap.lock()
```

Это **не RT callback**, поэтому сам по себе Mutex здесь не блокирующий RT defect.

Это правильнее, чем использовать Mutex непосредственно в callback.

Однако visualizer должен быть explicitly best-effort.

Если visualizer slow/full:

```text
audio must never wait for visualizer
```

Сейчас worker может удерживать mutex и выполнять push.

Рекомендуется:

```text
audio worker
    ↓
non-blocking visualizer ring
    ↓
visualizer consumer
```

и визуализатор никогда не должен влиять на audio producer.

---

# 21. UI architecture

В `main.rs`:

```rust
Rc<RefCell<MusicApp>>
```

и несколько timers:

```text
100 ms
16 ms
visualizer period
```

Это не критическая проблема.

`as_weak()` используется правильно:

```rust
let weak = ui.as_weak();
```

и callback проверяет:

```rust
weak.upgrade().is_none()
```

Это хорошая практика для Slint.

---

# 22. UI property churn

Есть хорошая delta optimization в:

```text
sync_playback_state_to_ui()
```

Например:

```rust
if playing != cur.playing {
    self.ui.set_playing(playing);
}
```

Это правильный подход.

Но другие места всё ещё строят `Vec`, `String`, `ModelRc` на UI tick.

Это допустимо, но надо следить за:

```text
100 ms tick
+
playlist rows
+
audio device capabilities
+
cache statistics
```

Особенно:

```rust
sync_track_info_to_ui()
```

создаёт:

```rust
Vec<String>
```

и несколько `format!`.

Это не RT problem.

Но не следует вызывать такую функцию на каждый 16 ms reflow.

---

# 23. Slint: `expect/unwrap`

В `main.rs` есть:

```rust
MusicApp::new(...)

ui.show().unwrap();

slint::run_event_loop_until_quit().unwrap();
```

Для production это не соответствует вашему собственному review rule:

> no unwrap/expect in production.

`main()` должен возвращать ошибку или переводить startup failure в контролируемое сообщение.

Например концептуально:

```rust
fn main() -> Result<(), AppError>
```

и:

```rust
ui.show()?;
```

Это не audio RT blocker, но это production correctness issue.

---

# 24. `drain_cover()`

Есть:

```rust
slint::Image::load_from_path(p).unwrap_or_default()
```

Здесь panic нет, потому что используется `unwrap_or_default`.

Это безопаснее.

Но потеря ошибки происходит silently.

Лучше:

```text
cover load failure
→ diagnostic
→ no cover
```

а не молчаливый default.

---

# 25. Device capabilities: сильная часть, но модель пока недостаточна

`DeviceInfo` содержит:

```text
supported rates
supported formats
channels
exclusive_capable
category
```

Это хороший фундамент.

Однако:

```rust
exclusive_capable:
    is_raw_hardware_id(&id) && category == Hardware
```

слишком сильно связывает:

```text
“raw hw”
```

с:

```text
“exclusive”
```

и не учитывает:

```text
backend endpoint semantics
```

Нужно:

```rust
DeviceCapabilities {
    physical_id,
    shared_endpoint,
    exclusive_endpoint,
    rates,
    formats,
    channels,
    dsd,
    dop,
}
```

---

# 26. Главный архитектурный рефакторинг

Сейчас:

```text
Settings
  ↓
OutputRequest
  ↓
choose_output()
  ↓
OutputSpec
```

Нужно:

```text
Settings
  ↓
PlaybackPolicy
  ↓
PathPlanner
  ↓
PathPlan
  ↓
EndpointResolver
  ↓
OutputSpec
  ↓
AudioEngine
```

---

# 27. Рекомендуемая новая структура модулей

```text
src/audio/
    policy.rs
    capabilities.rs
    planner.rs
    signal_path.rs
    endpoint.rs
    output.rs
    worker.rs
    player.rs
    decoder.rs
    dsd.rs
    dop.rs
    resampler.rs
```

### `policy.rs`

```rust
PlaybackPolicy
```

### `capabilities.rs`

```rust
DeviceCapabilities
```

### `signal_path.rs`

```rust
SignalPath
PcmFormat
DsdFormat
DopFormat
```

### `planner.rs`

```rust
plan()
```

### `endpoint.rs`

```rust
SharedEndpoint
ExclusiveEndpoint
```

### `output.rs`

Только:

```text
actual stream creation
```

а не product policy.

---

# 28. Было / Стало: Bit-Perfect flag

## Было

```rust
pub struct RtShared {
    bit_perfect: AtomicBool,
    resampler_enabled: bool,
}
```

и:

```rust
let bit_perfect = consumer.shared().bit_perfect();
```

## Стало

```rust
pub struct StreamContract {
    pub signal_path: SignalPath,
    pub output_format: OutputFormat,
}
```

И stream создаётся уже с неизменяемым contract.

Для strict:

```text
SignalPath::BitPerfectPcm
```

Для fallback:

```text
SignalPath::DspPcm
```

---

# 29. Было / Стало: playback policy

## Было

```rust
bit_perfect: bool
exclusive: ExclusiveMode
fallback: FallbackPolicy
resampler: ResamplerMode
```

Несколько независимых switches создают множество противоречивых комбинаций.

## Стало

```rust
pub enum PlaybackPolicy {
    Shared,
    BestPossible,
    Strict,
}
```

Дополнительные настройки остаются только там, где они действительно являются параметрами выбранной policy.

Например:

```text
BestPossible
    + resampler algorithm
    + fallback rate preference

Strict
    + no conversion settings at all
```

---

# 30. Было / Стало: track failure

## Было

```rust
Err(e) => {
    self.status = format!("Cannot play {title}: {e}").into();
    self.player.stop();
}
```

## Стало

Концептуально:

```rust
match self.player.open(&path) {
    Ok(info) => start(info),

    Err(TrackOpenError::Rejected(reason))
        if self.policy == PlaybackPolicy::Strict =>
    {
        self.record_skip(index, reason);
        self.play_next_eligible(index);
    }

    Err(error) => {
        self.report_fatal(error);
        self.player.stop();
    }
}
```

Ключевой момент:

```text
Rejected track ≠ fatal playback failure
```

---

# 31. Было / Стало: device representation

## Было

```text
name
 ↓
collapse_same_name()
 ↓
raw hw wins
```

## Стало

```rust
struct PhysicalDevice {
    id: PhysicalDeviceId,
    name: String,
    shared: Option<SharedEndpoint>,
    exclusive: Option<ExclusiveEndpoint>,
}
```

Тогда planner решает:

```text
Shared
→ physical.shared

BestPossible
→ physical.exclusive

Strict
→ physical.exclusive
```

---

# 32. Новый state machine

Рекомендованный state machine:

```text
Idle
  ↓
Planning
  ↓
Opening
  ↓
Playing
  ├── EOF → NextTrack
  ├── Seek → Seeking
  ├── DeviceLost → Recover/Stop
  └── TrackRejected → NextTrack
```

Для playlist:

```text
NextTrack
   ↓
Plan(track)
   ├── Playable → Opening
   └── Rejected → NextTrack
```

Таким образом Strict не ломает playlist.

---

# 33. Особенность смены sample rate

В BestPossible и Strict надо учитывать, что разные треки могут требовать разные hardware rates.

Например:

```text
Track A: 44.1
Track B: 96
Track C: 192
```

Exclusive DAC может потребовать:

```text
close stream
reconfigure
open stream
```

между треками.

Это не должно считаться ошибкой.

Нужно явно иметь:

```text
Track transition
→ plan
→ compare current OutputContract
→ if changed:
     stop stream
     reconfigure
     restart
```

Для gapless playback это отдельная задача.

---

# 34. Gapless и policy

В новой модели gapless нельзя проектировать отдельно от policy.

Если:

```text
BestPossible
```

и следующий трек имеет другой native rate:

```text
44.1 → 96
```

то exclusive backend может потребовать reconfiguration.

В Strict это нормальный hardware transition.

Нельзя ради gapless автоматически вставлять SRC — это нарушит Strict.

---

# 35. DSD policy

Новая модель должна различать:

```text
Native DSD
DoP
DSD → PCM
```

### Strict

Допустимо:

```text
Native DSD
DoP
```

Недопустимо:

```text
DSD → PCM
```

### BestPossible

Можно:

```text
Native DSD
→ DoP
→ high-quality DSD → PCM
```

в зависимости от capabilities.

### Shared

Возможен:

```text
DSD → PCM
→ PipeWire/shared
```

потому что главная гарантия Shared — совместимость, а не DAC-level bit-perfect.

---

# 36. Важный вывод по старой `spec_audio_integrity_v4.1`

Не нужно выбрасывать эту спецификацию.

Её техническая часть должна стать **нижним уровнем invariant'ов**.

То есть:

```text
Playback Policy
       ↑
       │
Path Planner
       ↑
       │
Signal Path
       ↑
       │
Audio Integrity Invariants
```

Старая спецификация хорошо описывает последний уровень.

Ошибка была не в самой идее Bit-Perfect.

Ошибка была в том, что:

```text
Bit-Perfect invariant
```

стал:

```text
global playback policy
```

---

# 37. Новая формулировка главного принципа проекта

Предлагается закрепить:

> **Bit-Perfect — это свойство конкретного выбранного Signal Path, а не обязательное состояние всего плеера.**

И:

> **Playback Policy определяет, что делать, когда Bit-Perfect path недоступен.**

Три политики:

```text
Shared:
    Play everything.
    Do not monopolize the system audio device.

BestPossible:
    Exclusive.
    Prefer native/Bit-Perfect.
    Convert only when required.
    Continue playback.

Strict:
    Exclusive.
    No conversion.
    Reject incompatible track.
    Continue with next eligible track.
```

---

# 38. RT invariants после рефакторинга

Оставить жёсткими:

```text
RT callback MUST NOT:
    allocate
    deallocate
    lock Mutex
    perform filesystem I/O
    perform network I/O
    decode
    resample
    log
    create String
    clone Arc
    change stream configuration
```

Callback должен делать только:

```text
read atomics
read ring
apply already-planned RT-safe operation
write device buffer
```

Для BitPerfect:

```text
read typed integer ring
copy samples
```

и практически ничего больше.

---

# 39. Рекомендуемый transport architecture

Лучший вариант:

```text
                 ┌───────────────┐
                 │ Decoder       │
                 └──────┬────────┘
                        │
             ┌──────────┴──────────┐
             │                     │
       BitPerfect               DSP
       integer path             f32 path
             │                     │
        typed ring              f32 ring
             │                     │
             └──────────┬──────────┘
                        │
                 Output backend
```

Не:

```text
everything → f32 → everything
```

Это позволит действительно выполнить исходное требование проекта:

> integer-domain preservation wherever possible.

---

# 40. Testing strategy

## Planner tests

Обязательно покрыть матрицу:

| Policy | Source | DAC | Expected |
|---|---|---|---|
| Shared | 16/44.1 | 96 max | PLAY |
| Shared | 24/192 | 96 max | PLAY |
| BestPossible | 24/192 | 192 | BitPerfect |
| BestPossible | 24/192 | 96 | DspPcm 96 |
| Strict | 24/192 | 96 | Reject |
| Strict | 16/44.1 | 96 | BitPerfect |
| BestPossible | DSD256 | DoP capable | DoP |
| Strict | DSD256 | no DoP/native | Reject |
| Shared | DSD256 | no native | PCM/shared fallback |

---

# 41. Track skip tests

Нужны тесты:

```text
playlist:
A incompatible
B compatible
C incompatible
D compatible
```

Strict result:

```text
A skipped
B played
C skipped
D played
```

И:

```text
all incompatible
```

результат:

```text
No playable tracks
```

а не бесконечный retry loop.

---

# 42. RT tests

Для typed PCM transport:

```text
I16 input == I16 output
I24 input == I24 output
I32 input == I32 output
```

бит-в-бит.

Нельзя проверять только:

```text
approximately_equal(f32)
```

Для Strict PCM нужен:

```text
assert_eq!(input_bytes, output_bytes)
```

на уровне sample bytes.

---

# 43. Backend tests

Для Linux/PipeWire:

```text
Shared
→ never open hw:*
```

Для Exclusive:

```text
Exclusive
→ raw hw:* permitted
```

Для DoP:

```text
DoP
→ raw hw:*
→ correct marker bytes
```

Нужно отдельно тестировать:

```text
physical device
shared endpoint
exclusive endpoint
```

чтобы regression вроде Xonar DX не возвращался.

---

# 44. UI requirements

UI должен показывать одновременно:

### User policy

```text
Mode:
    Shared
    Best Possible
    Strict
```

### Actual path

```text
Shared
Bit-Perfect PCM
DSP / Resampled
Native DSD
DoP
```

### Relevant reason

Например:

```text
Best Possible
24/192 → 24/96
Resampled because device maximum is 96 kHz
```

или:

```text
Strict
Track skipped
Device does not support 192 kHz
```

Не следует показывать просто:

```text
Bit-Perfect: ON
```

потому что это скрывает фактический path.

---

# 45. Предлагаемый UI status model

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

UI получает готовую snapshot structure.

UI не должен вычислять:

```text
“если bit_perfect && !resampled...”
```

Это responsibility audio planner.

---

# 46. Приоритет исправлений

## P0 — блокирующие

### P0.1
Убрать `f32` как универсальный transport для Bit-Perfect.

### P0.2
Ввести `PlaybackPolicy`.

### P0.3
Ввести typed `SignalPath`.

### P0.4
Разделить PhysicalDevice / SharedEndpoint / ExclusiveEndpoint.

### P0.5
Запретить raw `hw:*` в Shared.

### P0.6
Реализовать Strict track rejection → skip → next track.

---

## P1 — высокий приоритет

### P1.1
Вынести planner из `output.rs`.

### P1.2
Сделать невозможными состояния:

```text
BitPerfect + Resampler
BitPerfect + Dither
Strict + conversion
Shared + raw hw
```

желательно на уровне types/state machine.

### P1.3
Разделить DSD / DoP / PCM paths типами.

### P1.4
Сделать UI status snapshot path-based.

---

## P2 — оптимизация

### P2.1
Заменить `Vec::drain()` в resampler на circular buffer.

### P2.2
Оптимизировать visualizer tap.

### P2.3
Снизить лишние UI allocations.

### P2.4
Убрать production `unwrap/expect`.

---

# 47. Что НЕ нужно делать

Не надо:

```text
❌ выбрасывать rtrb
❌ возвращать decoder в callback
❌ делать DSP в UI
❌ использовать Mutex в RT callback
❌ отказаться от worker
❌ отказаться от preallocated scratch
❌ делать Bit-Perfect “опциональным” внутри одного boolean
```

Наоборот:

```text
оставить RT engine
+
перестроить policy/path layer
```

---

# 48. Финальный архитектурный вердикт

Текущий проект находится в хорошем промежуточном состоянии:

```text
RT engine:
    хороший фундамент

Worker:
    хороший фундамент

Ring:
    правильное решение

Seek:
    хорошая архитектурная основа

Slint separation:
    в целом корректная

Device negotiation:
    требует серьёзной переработки

Bit-Perfect:
    концептуально заявлен правильно,
    но фактический f32 transport нарушает строгий контракт

Playback policy:
    требует нового верхнего уровня
```

Главная задача следующего этапа — **не переписывать весь audio engine**, а правильно разделить уровни ответственности.

---

# 49. Итоговая целевая архитектура

```text
                    ┌───────────────────────┐
                    │      Slint UI         │
                    └───────────┬───────────┘
                                │ commands/status
                                ▼
                    ┌───────────────────────┐
                    │   Playback Manager    │
                    └───────────┬───────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │    PlaybackPolicy     │
                    │ Shared / Best / Strict│
                    └───────────┬───────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │      PathPlanner      │
                    └───────────┬───────────┘
                                │
                ┌───────────────┼────────────────┐
                │               │                │
                ▼               ▼                ▼
          BitPerfect        DspPcm            DSD/DoP
          integer           f32 DSP            typed path
                │               │                │
                └───────────────┼────────────────┘
                                ▼
                    ┌───────────────────────┐
                    │    Typed RT Ring      │
                    └───────────┬───────────┘
                                ▼
                    ┌───────────────────────┐
                    │    RT Audio Callback  │
                    └───────────┬───────────┘
                                ▼
                    ┌───────────────────────┐
                    │ Backend Endpoint      │
                    │ Shared / Exclusive   │
                    └───────────────────────┘
```

Именно эту архитектуру следует считать целевой.

---

# 50. Главный вывод

Предыдущее код-ревью не нужно “отменять”.

Нужно исправить его главный архитектурный перекос:

> **мы сделали технический invariant Bit-Perfect главным пользовательским режимом.**

Теперь правильная модель:

> **пользователь выбирает политику воспроизведения, а engine подбирает максимально качественный допустимый Signal Path.**

Таким образом:

```text
Shared
→ музыка всегда + совместимость

BestPossible
→ максимум качества + продолжение воспроизведения

Strict
→ абсолютная целостность + skip несовместимых треков
```

При этом High-End требования не ослабляются.

Они становятся **условными контрактами конкретного Signal Path**, а не глобальным ограничением всего продукта.

---

## Основные файлы, использованные в ревью

- `src/audio/output.rs`
- `src/audio/player.rs`
- `src/audio/worker.rs`
- `src/audio/decoder.rs`
- `src/audio/dsd.rs`
- `src/audio/dop.rs`
- `src/app/playback_manager.rs`
- `src/app/fulltrack_manager.rs`
- `src/app/playlist_manager.rs`
- `src/app/ui_manager.rs`
- `src/settings.rs`
- `Cargo.toml`
- черновик спеки Audio Integrity v4.1 (содержание перенесено в `docs/spec_audio_pipeline_v5.0.md`)

Ревью основано на текущем содержимом ветки `main` репозитория `matt-motik/apap` и на новой концепции трёх режимов воспроизведения.
