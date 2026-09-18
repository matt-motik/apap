Да. С учётом твоих решений я бы **не делал один большой refactor**. Для текущего workflow проекта безопаснее разбить работу на серию SPEC → micro-step → verify → commit, причём сначала исправить семантику signal path, затем RT, затем UI/visualizer, затем DSP validation.

Ниже план в форме, которую можно практически напрямую отдать агенту.

---

# План устранения замечаний Code Review

[Репозиторий apap](https://github.com/matt-motik/apap?utm_source=chatgpt.com)

## 0. Целевое состояние архитектуры

Главная цель:

```text
                         ┌──────────────────────┐
                         │      Decoder         │
                         │ Symphonia / source   │
                         └──────────┬───────────┘
                                    │
                         decoded audio representation
                                    │
                ┌───────────────────┼───────────────────┐
                │                   │                   │
                ▼                   ▼                   ▼
        ┌──────────────┐    ┌──────────────┐    ┌──────────────┐
        │ Bit-Perfect  │    │   DSP PCM    │    │     DSD      │
        │ PCM integer  │    │     f32      │    │ native/DoP   │
        └──────┬───────┘    └──────┬───────┘    └──────┬───────┘
               │                   │                   │
               │            resampler / DSP            │
               │                   │                   │
               └───────────────────┼───────────────────┘
                                   │
                             Playback Worker
                                   │
                         ┌─────────┴─────────┐
                         │                   │
                         ▼                   ▼
                   Visualization       Playback Ring
                      Worker                │
                                            │
                                      lock-free SPSC
                                            │
                                      RT audio callback
                                            │
                                          cpal
                                            │
                                         Device
```

Ключевой принцип:

> **`f32` — representation для DSP, а не универсальный transport format.**

Сейчас именно это является самым важным архитектурным долгом: `AudioSource` отдаёт `&[f32]`, после чего integer PCM снова превращается в integer в callback.

---

# SPEC 1 — Audio Integrity / Bit-Perfect Architecture

Создать:

```text
docs/spec_audio_integrity_v1.0.md
```

и добавить в `ROADMAP.md` отдельный registry item, например:

```text
A4.0 — Audio Signal Integrity & Strict Bit-Perfect
```

## Цель SPEC

Формально определить:

### Bit-Perfect PCM

Разрешено:

```text
decode → exact PCM samples → transport → device
```

Запрещено:

```text
PCM → f32 → PCM
PCM → resampler
PCM → channel mixer
PCM → volume
PCM → dither
PCM → DSP
```

при включённом Bit-Perfect.

### Hardware requirements

Bit-Perfect разрешается только если одновременно:

```text
sample rate      == source rate
sample format    == source format
channel count    == source channels
channel layout   == source layout
DSP              == disabled
software volume  == 1.0
dither           == disabled
resampler        == disabled
hardware mode    == required exclusive/native mode
```

Если хотя бы одно условие невозможно:

```text
BitPerfect unavailable
        │
        └── fallback allowed → DSP mode
```

Но:

```text
DSP mode ≠ BitPerfect
```

Это особенно важно, потому что текущий код допускает resampling даже при выбранном Bit-Perfect сценарии, если hardware не поддерживает native rate.

---

# SPEC 2 — Разделение Audio Representation

Это самый большой, но самый важный refactor.

## Текущее

Сейчас концептуально:

```rust
trait AudioSource {
    fn next_frames(&mut self) -> Option<&[f32]>;
}
```

После этого callback делает обратное преобразование:

```rust
let sample = src * 32767.0;
let value = sample.round() as i16;
```

Это **не может считаться строгим bit-perfect transport**.

Например:

```text
original i16: -32768
normalized:   -1.0
* 32767:      -32767
```

---

## Целевой API

Не обязательно буквально использовать этот API, но агент должен прийти к эквивалентной архитектуре.

Например:

```rust
pub enum AudioFrame<'a> {
    PcmI16(&'a [i16]),
    PcmI24(&'a [I24]),
    PcmI32(&'a [i32]),
    PcmF32(&'a [f32]),
    Dsd(&'a [u8]),
    Dop(&'a [u32]),
}
```

Либо, что может оказаться ещё чище, разделить pipeline traits:

```rust
trait PcmSource {
    type Sample;

    fn next_frames(&mut self) -> Option<&[Self::Sample]>;
}
```

и:

```rust
trait DsdSource {
    ...
}
```

### Важно

Не заставлять агента механически реализовывать именно мой пример.

В DoD написать:

> Integer PCM must not be converted through f32 on the strict Bit-Perfect path.

---

# Micro-step 2.1 — Исследовать возможности Symphonia

До написания кода агент должен определить:

1. Какие native decoded sample formats предоставляет текущая версия Symphonia.
2. Как получить:
   - i16;
   - i24;
   - i32;
   - f32.

3. Как безопасно удерживать lifetime decoded buffer.
4. Можно ли избежать дополнительной allocation/copy.
5. Какой формат фактически выдаётся каждым decoder.

Это должен быть **read-only research step**.

Не менять код, пока API не понятен.

---

# Micro-step 2.2 — Introduce AudioRepresentation

Добавить внутренний тип, например:

```rust
enum PcmRepresentation {
    I16,
    I24,
    I32,
    F32,
}
```

и соответствующий decoded buffer.

DoD:

- существующий playback продолжает работать;
- нет изменения UI;
- `cargo check`;
- tests.

---

# Micro-step 2.3 — Raw PCM transport

Добавить отдельный pipeline:

```text
integer PCM
    ↓
PlaybackWorker
    ↓
typed ring
    ↓
typed RT consumer
    ↓
cpal output
```

Для `i16` целевая реализация должна быть примерно такой:

```rust
fn audio_callback_i16_bit_perfect(
    output: &mut [i16],
    consumer: &mut RtConsumerI16,
) {
    for sample in output {
        match consumer.pop() {
            Some(value) => *sample = value,
            None => *sample = 0,
        }
    }
}
```

То есть **никакого**:

```rust
f32 → multiply → round → clamp → i16
```

на этом path.

---

# Micro-step 2.4 — i24

24-bit требует отдельного решения.

Например:

```rust
#[repr(transparent)]
struct I24(i32);
```

с invariant:

```text
-8_388_608 <= value <= 8_388_607
```

Но если CPAL backend представляет 24-bit output через 32-bit container, transport должен быть явно описан:

```text
source 24-bit PCM
       ↓
device's expected 24-bit-in-container representation
```

Нельзя просто считать любой `i32` автоматически bit-perfect.

DoD:

- golden vectors;
- min/max;
- sign extension;
- endian/packing tests;
- device-format mapping tests.

---

# Micro-step 2.5 — i32

Текущий код:

```rust
src * 2147483647.0
```

нужно убрать из strict Bit-Perfect path.

Причина принципиальная:

`f32` не способен точно представить все значения 32-bit integer PCM.

Тест:

```text
i32 input
→ transport
→ output
```

должен давать:

```text
output == input
```

для набора edge/random vectors.

---

# Micro-step 2.6 — BitPerfect SignalPath

Ввести явное состояние:

```rust
enum SignalPath {
    BitPerfectPcm(PcmFormat),
    DspPcm(DspFormat),
    NativeDsd(DsdFormat),
    DoP(DopFormat),
}
```

Это лучше, чем распределять semantics по множеству boolean:

```rust
bit_perfect
dither
resampler
mute
volume
...
```

`SignalPath` должен стать **source of truth**.

---

# 3. Strict Bit-Perfect Hardware Selection

Сейчас `output.rs` уже содержит довольно хорошую основу:

```text
ChosenOutput
resampled
exclusive
fallback reason
validation
BitPerfect / Degraded / Unsupported
```

Но нужно сделать semantics строже.

## Правило

Если:

```text
requested = BitPerfect
```

и:

```text
device.sample_rate != source.sample_rate
```

то:

```rust
BitPerfectUnavailable {
    reason: SampleRateMismatch
}
```

а не:

```text
BitPerfect + resampler
```

---

## Fallback

На уровне policy:

```rust
enum BitPerfectFallback {
    Dsp,
    Reject,
}
```

Тогда:

```text
BitPerfect requested
       │
       ├── exact path → BitPerfect
       │
       └── impossible
             │
             ├── Dsp → DSP path
             │
             └── Reject → Unsupported
```

UI должен показывать реальный режим.

---

# 4. Channel Layout Integrity

Текущий `Resampler` содержит `mix_rel()` для downmix/upmix.

Это нормально для DSP mode.

Но:

```text
BitPerfect + mix_rel()
```

запрещено.

## DoD

Добавить тест:

```rust
#[test]
fn bitperfect_rejects_channel_conversion() {
    ...
}
```

Проверить:

```text
stereo → mono = not bit-perfect
mono → stereo = not bit-perfect
5.1 → stereo = not bit-perfect
stereo → 5.1 = not bit-perfect
```

Даже если mathematically transformation кажется безобидной.

---

# 5. RT Callback — убрать O(N) seek

Это второй серьёзный RT issue.

Сейчас:

```rust
while ring.pop().is_ok() {}
```

вызывается из `reconcile_seek()`.

Это lock-free и allocation-free, но:

> **lock-free ≠ bounded-time.**

Если ring большой, callback может выполнить большое количество `pop`.

## Цель

Seek должен иметь O(1) или практически bounded-time invalidation.

---

## Предлагаемый механизм

Использовать generation/epoch:

```rust
struct PlaybackEpoch {
    generation: AtomicU64,
}
```

Producer маркирует данные:

```text
generation = 42
```

Seek:

```text
generation = 43
```

Consumer видит mismatch и перестаёт считать старые frames валидными.

В зависимости от выбранной реализации можно:

- reset/reinitialize ring вне callback;
- использовать epoch metadata;
- сделать bounded discard;
- передать seek command worker → consumer.

### Главное DoD

В callback **не должно быть unbounded draining loop**.

---

# 6. Formal RT Safety Contract

Создать:

```text
docs/spec_rt_audio_v1.0.md
```

и зафиксировать:

### В RT callback запрещены

```text
Mutex
RwLock
allocation
deallocation
Vec growth
String
format!
println!
eprintln!
filesystem
network
decoder
resampler
FFT
Slint
blocking syscall
sleep
unbounded loop
```

Разрешены:

```text
atomic load/store
bounded arithmetic
preallocated memory
SPSC ring operations
direct device buffer writes
```

---

# 7. RT Tests / Static Guards

Добавить regression tests.

Например:

```rust
#[test]
fn rt_consumer_does_not_allocate() {
    ...
}
```

Но ещё лучше — отделить RT core от CPAL и тестировать его независимо.

Цель:

```text
RtConsumer
    ↓
deterministic test
```

без реального audio device.

---

# 8. Dither

Dither должен быть строго:

```text
DSP / integer conversion path
```

но не:

```text
BitPerfect
```

Текущий callback уже имеет условие относительно bit-perfect.

После refactor должно быть ещё проще:

```rust
match signal_path {
    SignalPath::BitPerfectPcm(_) => {
        // direct copy
    }

    SignalPath::DspPcm(_) => {
        // conversion / dither if configured
    }
}
```

То есть невозможно случайно получить:

```text
bit_perfect == true
dither == true
```

---

# 9. DoP — отдельный transport

DoP нельзя концептуально считать обычным PCM `f32`.

Текущий код делает:

```rust
((src.clamp(0, 16777215) as u32) << 8) as i32
```

Это нужно заменить на явно типизированный DoP representation.

Например:

```rust
struct DopFrame {
    payload: u32,
}
```

и отдельный encoder:

```rust
fn encode_dop_frame(
    dsd_payload: Dsd24,
    marker: DopMarker,
) -> DopFrame
```

Добавить golden vectors:

```text
DSD bytes
→ DoP encoder
→ expected PCM container
```

---

# 10. Native DSD

Native DSD должен иметь отдельный path:

```text
DSD source
   ↓
DSD worker
   ↓
native DSD transport
   ↓
device
```

И никакого:

```text
DSD
 ↓
f32
 ↓
PCM
```

для native DSD.

Для неподдерживаемого hardware:

```text
Native DSD unavailable
```

либо fallback согласно policy.

---

# 11. Visualizer

Здесь архитектуру **не надо ухудшать**.

Фактическая реализация уже делает tap в worker после resampler и до playback ring, а не внутри CPAL callback.

Это хороший вариант.

Оставить:

```text
worker
  │
  ├── playback ring
  │
  └── visualization ring
```

---

# 12. Убрать Mutex из VizTap ownership

Сейчас:

```rust
Arc<Mutex<Option<Producer<f32>>>>
```

но mutex находится **не в RT callback**, а worker-side. Поэтому это не critical RT violation.

Тем не менее архитектурно лучше:

```rust
struct PlaybackWorker {
    viz_producer: Option<Producer<f32>>,
    viz_active: Arc<AtomicBool>,
}
```

То есть worker владеет producer напрямую.

UI/manager только:

```text
start visualizer
    ↓
create ring
    ↓
give producer to worker
```

После этого producer ownership больше не требует mutex.

---

# 13. Visualization Backpressure

Нельзя позволять visualizer тормозить playback.

Целевой invariant:

```text
visualizer slow
      ↓
drop visualization samples
      ↓
playback continues normally
```

Никогда:

```text
visualizer slow
      ↓
worker blocked
      ↓
playback underrun
```

Тест:

```rust
#[test]
fn viz_backpressure_never_blocks_playback() {
    ...
}
```

---

# 14. Slint UI Update Rate

Heavy processing остаётся worker-side:

```text
FFT
decode
spectrogram
waveform
cache
```

не в Slint.

Но нужно предотвратить ситуацию:

```text
FFT worker
   ↓
invoke_from_event_loop()
invoke_from_event_loop()
invoke_from_event_loop()
...
```

на каждый FFT block.

Целевой pipeline:

```text
FFT worker
    ↓
latest-value buffer
    ↓
Slint timer 30–60 Hz
    ↓
consume latest frame
```

То есть UI rendering rate отделён от DSP rate.

---

# 15. Slint Weak/Upgrade

Worker не должен владеть сильной ссылкой на UI.

Целевой pattern:

```rust
let weak = ui.as_weak();

worker.on_update(move |data| {
    let weak = weak.clone();

    // worker result arrives
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_spectrum(...);
    });
});
```

Или эквивалентный существующий idiomatic API проекта.

Invariant:

```text
UI owns UI
worker does not keep UI alive
```

---

# 16. UI Property Efficiency

Проверить все live properties.

Плохо:

```text
every FFT frame
    ↓
10–20 individual Slint property updates
```

Лучше:

```rust
SpectrumFrame {
    bins: ...
    peak: ...
    rms: ...
}
```

и одна логическая update operation.

Особенно:

- spectrum;
- waveform;
- playback position;
- device state;
- visualizer status.

---

# 17. Decoder Error Handling

В `decoder.rs` есть две вещи, которые нужно исправить.

### Seek

Сейчас:

```rust
let time = Time::try_new(...).unwrap_or(Time::ZERO);
```

Это опасная семантика:

```text
invalid seek
    ↓
seek to zero
```

Вместо:

```rust
fn seek(...) -> Result<(), DecodeError>
```

и явного propagation.

---

### DecodeError

Сейчас recoverable decode errors фактически пропускаются:

```rust
DecodeError(_) => continue
```

Нужно определить policy:

```text
recoverable packet error
    ↓
continue
    +
error counter/status
```

или:

```text
fatal decode error
    ↓
stop
    +
reported error
```

Никакого silent corruption.

---

# 18. `unwrap` / `expect`

Не делать бессмысленный глобальный:

```text
replace every unwrap
```

Разделить:

### Production

Запрещено без доказуемого invariant:

```rust
unwrap()
expect()
```

### Tests

Разрешено.

### Internal invariant

Можно:

```rust
debug_assert!
```

или:

```rust
.expect("invariant: ...")
```

только если:

1. invariant локально доказан;
2. нарушение действительно является programming bug;
3. это не внешний input/device/file condition.

---

# 19. FFI / CPAL boundary

Для `output.rs` провести отдельный audit:

```text
Rust
 ↓
cpal
 ↓
platform backend
```

Проверить:

- lifetime stream;
- callback captures;
- Send/Sync assumptions;
- error callback;
- device disappearance;
- stream restart;
- unsupported sample formats;
- zero-length buffers;
- channel count mismatch;
- buffer size changes.

Текущий callback уже достаточно хорошо отделён от error callback; error logging происходит в error handler, а не data callback.

Это сохранить.

---

# 20. Resampler — не переписывать вслепую

Существующий `Resampler` уже имеет preallocated buffer и window.

Поэтому сначала:

```text
benchmark
↓
measure
↓
validate
↓
only then optimize
```

---

# 21. Resampler DSP Validation

Создать отдельный test/benchmark suite.

## Cases

```text
44.1 → 48
48 → 44.1
96 → 44.1
192 → 48
```

## Signals

```text
impulse
1 kHz sine
10 kHz sine
18 kHz sine
near-Nyquist sine
silence
full-scale sine
multichannel impulse
```

## Measure

```text
passband ripple
stopband attenuation
alias rejection
frequency response
latency
CPU ns/sample
allocations
```

Не принимать решение «resampler high-end quality» на основании только unit tests.

---

# 22. Golden Bit-Perfect Tests

Это обязательная часть.

Создать:

```text
tests/audio_integrity/
```

или эквивалентную структуру.

## 16-bit

Vectors:

```text
0
1
-1
32767
-32768
0x5555
0xAAAA
random
```

Проверка:

```rust
assert_eq!(input, output);
```

---

## 24-bit

Обязательно:

```text
0
1
-1
8388607
-8388608
```

плюс random.

---

## 32-bit

Особенно:

```text
i32::MIN
i32::MAX
2^24
2^24 + 1
...
```

Это поймает потерю точности через `f32`.

---

# 23. Golden DoP Tests

Например:

```text
known DSD payload
        ↓
DoP encoder
        ↓
expected byte sequence
```

Тестировать:

- marker;
- frame boundary;
- endian;
- channel interleave;
- payload packing;
- block alignment.

---

# 24. Bit-Perfect Negative Tests

Не только positive tests.

Должно быть:

```text
native rate mismatch → NOT BitPerfect
channel mismatch     → NOT BitPerfect
resampler enabled    → NOT BitPerfect
volume != 1          → NOT BitPerfect
dither enabled       → NOT BitPerfect
DSP enabled          → NOT BitPerfect
software mixer       → NOT BitPerfect
```

Это особенно важно.

---

# 25. Bit-Perfect State Machine

Добавить property-based/invariant tests примерно такого типа:

```text
SignalPath::BitPerfect
```

никогда не может одновременно иметь:

```text
resampler = enabled
volume != unity
dither = enabled
channel conversion = enabled
```

Лучше сделать так, чтобы **типовая модель вообще не позволяла** такие состояния.

---

# 26. Performance / Allocation Tests

Нужно проверить не только callback source code, но и фактический hot path.

Benchmark:

```text
callback
 ↓
RtConsumer
 ↓
ring
```

Метрики:

```text
allocations = 0
locks = 0
syscalls = 0
unbounded loops = 0
```

Для callback latency:

```text
p50
p95
p99
p99.9
max
```

особенно на больших buffer/ring configurations.

---

# 27. Документация Visualizer

Обновить:

```text
docs/spec_visualizer_v5.1.md
```

Сейчас текст описывает tap как находящийся в `audio_callback`, тогда как реализация делает tap worker-side. Это нужно синхронизировать.

Целевая документация:

```text
Decoder
 ↓
Resampler/DSP
 ↓
Worker-side VizTap
 ├── Visualization worker
 └── Playback ring
       ↓
     RT callback
```

---

# 28. Hardware Verification

После software tests — отдельный этап.

Не смешивать с unit tests.

## Матрица

```text
PCM
16/44.1
16/48
24/44.1
24/48
24/96
24/192
32/44.1
32/96
```

если hardware поддерживает.

DSD:

```text
DSD64
DSD128
DSD256
```

где доступно.

---

## Проверить

Для каждого:

```text
requested format
actual stream format
actual sample rate
actual channel layout
reported mode
```

И физически:

```text
DAC/input analyzer
```

для подтверждения native/DoP/DSD.

---

# 29. Hardware Bit-Perfect Verification

Нужен отдельный документ:

```text
docs/hardware_verification_audio_integrity.md
```

Таблица:

```text
Source | Device | Rate | Format | Mode | Result | Evidence
```

Особенно для:

- native PCM;
- DoP;
- native DSD;
- unsupported native rate;
- fallback to DSP.

---

# 30. ROADMAP decomposition

Я бы **не делал один `A4.0`**.

Лучше:

```text
A4.0 Audio Integrity Architecture
A4.1 Typed PCM transport
A4.2 Strict Bit-Perfect device policy
A4.3 RT seek boundedness
A4.4 DoP / Native DSD transport
A4.5 Decoder error semantics
A4.6 Visualizer worker/UI architecture
A4.7 Resampler DSP validation
A4.8 Audio integrity golden tests
A4.9 RT performance validation
A4.10 Hardware verification
A4.11 Documentation / final audit
```

---

# 31. Порядок выполнения агентом

**Очень важно:** не давать агенту выполнять их в произвольном порядке.

### Phase 0 — Baseline

```text
read _STATE_
check git status
cargo check --quiet
cargo test
cargo clippy
```

Если baseline broken → diagnostic mode согласно текущему `AGENTS.md`. Текущий workflow уже требует baseline check и строгий whitelist.

---

### Phase 1 — Architecture

```text
A4.0
 ↓
Symphonia research
 ↓
AudioRepresentation
 ↓
SignalPath
```

**Никаких оптимизаций пока.**

---

### Phase 2 — Bit-Perfect PCM

```text
A4.1
 ↓
I16
 ↓
I24
 ↓
I32
 ↓
golden vectors
```

После каждого micro-step:

```text
cargo check
cargo test
cargo clippy
commit
```

---

### Phase 3 — Hardware policy

```text
A4.2
 ↓
strict native rate
 ↓
strict channel layout
 ↓
strict format
 ↓
DSP fallback
```

---

### Phase 4 — RT

```text
A4.3
 ↓
seek generation
 ↓
remove O(N) drain
 ↓
RT tests
 ↓
latency benchmark
```

---

### Phase 5 — DSD

```text
A4.4
 ↓
DoP typed transport
 ↓
golden vectors
 ↓
Native DSD path
```

---

### Phase 6 — Decoder robustness

```text
A4.5
 ↓
seek errors
 ↓
decode error policy
 ↓
production unwrap audit
```

---

### Phase 7 — Visualizer/UI

```text
A4.6
 ↓
worker-owned VizTap
 ↓
latest-value UI model
 ↓
30–60 Hz UI updates
 ↓
Weak UI handles
 ↓
documentation sync
```

---

### Phase 8 — DSP

```text
A4.7
 ↓
resampler benchmarks
 ↓
frequency response
 ↓
alias rejection
 ↓
CPU
 ↓
allocation
```

---

### Phase 9 — Verification

```text
A4.8
 ↓
golden tests

A4.9
 ↓
RT performance

A4.10
 ↓
hardware

A4.11
 ↓
final architecture audit
```

---

# 32. Что агенту запрещено делать

Это я бы буквально добавил в SPEC/AGENTS instructions для данной серии задач:

```text
DO NOT:

1. Fix bit-perfect by adding more boolean conditions.
2. Convert integer PCM through f32 and call it bit-perfect.
3. Add Mutex to the RT callback.
4. Move FFT/visualizer processing into the RT callback.
5. Drain an unbounded ring from the RT callback.
6. Add allocations to the RT callback.
7. Silently fall back to another sample rate while reporting BitPerfect.
8. Perform channel conversion while reporting BitPerfect.
9. Replace all unwrap()/expect() mechanically.
10. Rewrite the resampler without measurements.
11. Make UI own worker lifetime.
12. Allow visualization backpressure to affect playback.
13. Make a large multi-file unreviewable commit.
```

---

# 33. Definition of Done для всей серии

В конце агент **не должен просто сказать `cargo test passed`**.

Финальный DoD:

### Signal integrity

```text
[ ] 16-bit exact transport
[ ] 24-bit exact transport
[ ] 32-bit exact transport
[ ] no f32 in strict PCM path
[ ] no resampling in BitPerfect
[ ] no channel conversion
[ ] no software volume
[ ] no dither
[ ] no DSP
```

### DSD

```text
[ ] DoP golden vectors
[ ] Native DSD path
[ ] DSD fallback semantics
```

### RT

```text
[ ] zero allocation
[ ] zero locks
[ ] no syscalls
[ ] no decoder
[ ] no FFT
[ ] bounded seek handling
[ ] deterministic RT tests
```

### UI

```text
[ ] workers own heavy computation
[ ] UI only presentation
[ ] Weak UI handles
[ ] latest-value strategy
[ ] 30–60 Hz UI update budget
[ ] visualization cannot block playback
```

### DSP

```text
[ ] resampler frequency-response tests
[ ] alias rejection tests
[ ] CPU benchmarks
[ ] allocation benchmarks
```

### Robustness

```text
[ ] no silent seek-to-zero
[ ] decode errors observable
[ ] production unwrap audit
[ ] CPAL/device-loss cases tested
```

### Hardware

```text
[ ] native PCM verified
[ ] unsupported native rate verified
[ ] DSP fallback verified
[ ] DoP verified
[ ] native DSD verified where hardware supports it
```

---

# Самое важное изменение порядка приоритетов

Я бы **не начинал с RT optimization**, хотя первоначальный review был именно про RT.

Причина:

```text
сейчас
Decoder
  ↓
f32
  ↓
Worker
  ↓
Ring
  ↓
RT
```

А фундаментальная проблема находится **выше RT**:

```text
Decoder → f32
```

Если сначала оптимизировать callback, а потом выяснить, что transport representation неправильная для Bit-Perfect, часть работы придётся переделывать.

Правильный порядок:

```text
        ┌─────────────────────┐
        │ 1. Signal semantics │
        └──────────┬──────────┘
                   ↓
        ┌─────────────────────┐
        │ 2. Typed transport  │
        └──────────┬──────────┘
                   ↓
        ┌─────────────────────┐
        │ 3. Hardware policy  │
        └──────────┬──────────┘
                   ↓
        ┌─────────────────────┐
        │ 4. RT boundedness   │
        └──────────┬──────────┘
                   ↓
        ┌─────────────────────┐
        │ 5. DSD / DoP        │
        └──────────┬──────────┘
                   ↓
        ┌─────────────────────┐
        │ 6. UI / Visualizer  │
        └──────────┬──────────┘
                   ↓
        ┌─────────────────────┐
        │ 7. DSP validation   │
        └──────────┬──────────┘
                   ↓
        ┌─────────────────────┐
        │ 8. Hardware proof   │
        └─────────────────────┘
```

**И ещё один принцип:** агенту не следует пытаться сохранить старую `AudioSource -> &[f32]` abstraction только ради минимального diff. Для high-end player это неправильная точка абстракции. `f32` нужно сохранить как основной **DSP representation**, но убрать его из **strict transport layer**. Это позволит одновременно получить настоящий Bit-Perfect, нормальный DSP pipeline и гораздо более ясную архитектуру.

Текущий `A2.0` уже закрыл значительную часть RT hardening — rtrb SPSC, preallocated scratch, callback без mutex/decoder/allocation и т. п., поэтому новый цикл должен быть именно **signal-integrity refactor поверх существующего RT фундамента**, а не переписывание audio engine с нуля.
