# SPEC: Audio Integrity & Strict Bit-Perfect Architecture v4.1

**Status:** Proposed  
**Date:** 2026-09-18  
**Related:** Code Review 3.0, Code Review 3.1  
**Based on:** `docs/spec_audio_integrity_v4.0.md` + review corrections  
**Replaces:** `docs/spec_audio_integrity_v4.0.md`  
**Scope:** signal-integrity refactor over the existing A2.0 RT audio foundation

---

## 1. Executive Summary

Эта спецификация определяет архитектуру целостности аудиосигнала для `apap` с акцентом на:

1. строгий Bit-Perfect для integer PCM;
2. typed PCM transport от decoder до CPAL callback;
3. строгую проверку hardware/backend capabilities;
4. bounded real-time callback;
5. отдельные Native DSD и DoP paths;
6. worker-side visualizer tap;
7. корректную границу DSP/resampler;
8. golden/negative tests и software/hardware verification.

### Ключевой принцип

> **`f32` — representation для DSP, но не универсальный transport format.**

Строгий Bit-Perfect path не должен выполнять:

```text
integer PCM → f32 → integer PCM
```

Даже если для части значений такая конвертация математически обратима, сама архитектура не гарантирует exact integer sample identity.

Целевая архитектура:

```text
                         ┌──────────────────────┐
                         │       Decoder        │
                         │ Symphonia / source   │
                         └──────────┬───────────┘
                                    │
                         decoded PCM representation
                                    │
              ┌─────────────────────┼─────────────────────┐
              │                     │                     │
              ▼                     ▼                     ▼
      ┌────────────────┐    ┌────────────────┐    ┌────────────────┐
      │ Bit-Perfect    │    │ DSP PCM        │    │ DSD            │
      │ I16/I24/I32    │    │ f32            │    │ Native / DoP   │
      └───────┬────────┘    └───────┬────────┘    └───────┬────────┘
              │                     │                     │
              │                     ▼                     │
              │              resampler / DSP              │
              │                     │                     │
              └─────────────────────┼─────────────────────┘
                                    │
                              Playback Worker
                               /           \
                              /             \
                             ▼               ▼
                      Visualizer Tap    Playback Ring
                                             │
                                      lock-free SPSC
                                             │
                                      RtConsumer<T>
                                             │
                                      CPAL callback
                                             │
                                           Device
```

Главное изменение v4.1 по сравнению с v4.0:

```text
BitPerfectPcm(I16)
    → typed owned transport
    → typed ring
    → typed RtConsumer
    → matching CPAL callback
    → matching device stream
```

Нельзя остановиться на `AudioFrame::PcmI16(&[i16])`, а затем снова свести данные к `[f32]` ниже по stack.

---

# 2. Normative Language

Ключевые слова:

- **MUST / ОБЯЗАН** — обязательное требование.
- **MUST NOT / НЕ ДОЛЖЕН** — запрещённое поведение.
- **SHOULD / СЛЕДУЕТ** — архитектурно предпочтительное решение.
- **MAY / МОЖЕТ** — допустимый вариант.

Если реализация не может выполнить MUST-требование, она не может заявлять соответствующий режим как поддерживаемый.

---

# 3. Definitions

## 3.1 Strict Bit-Perfect

В этой спецификации **Bit-Perfect PCM** означает:

> Каждый decoded integer PCM sample, включая знак, bit depth и channel position, должен попасть на device stream без изменения sample value и без signal-processing transformation.

Для PCM:

```text
decoded sample
    ==
transport sample
    ==
CPAL callback sample
    ==
device stream sample
```

При условии, что hardware/backend действительно использует тот же формат, sample rate, channel layout и stream semantics.

### Важная граница

Bit-Perfect относится к **decoded PCM sample identity**, а не к byte identity исходного compressed file.

Например:

```text
FLAC bytes
    ↓ decode
PCM samples
    ↓ exact transport
device
```

Нельзя требовать:

```text
device bytes == FLAC file bytes
```

Это разные representation layers.

---

## 3.2 DSP Mode

DSP mode разрешает преобразования:

```text
PCM → f32
PCM → resampler → f32
channel mixing
software volume
dither
EQ / filters / DSP
```

DSP mode никогда не должен отображаться UI как Bit-Perfect.

---

## 3.3 Native DSD

Native DSD — отдельный signal path, в котором DSD payload не переводится в PCM/f32.

```text
DSD
 ↓
typed DSD transport
 ↓
native DSD backend/device
```

---

## 3.4 DoP

DoP (DSD over PCM) — специальный transport protocol.

DoP нельзя трактовать как обычный PCM audio signal и нельзя реализовывать через:

```text
DSD → f32 → integer → DoP
```

Payload и marker должны быть типизированы отдельно.

---

# 4. Signal-Path Model

## 4.1 Source of Truth

Вместо набора независимых boolean flags:

```rust
bit_perfect: bool
dsp: bool
resampler: bool
dither: bool
native_dsd: bool
```

используется типизированный signal-path state:

```rust
pub enum SignalPath {
    BitPerfectPcm(PcmFormat),
    DspPcm(DspFormat),
    NativeDsd(DsdFormat),
    DoP(DopFormat),
}
```

Например:

```rust
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub layout: ChannelLayout,
    pub sample_format: PcmSampleFormat,
}

pub enum PcmSampleFormat {
    I16,
    I24,
    I32,
}
```

DSP:

```rust
pub struct DspFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub layout: ChannelLayout,
}
```

### Invariant

`SignalPath::BitPerfectPcm(_)` автоматически означает:

```text
no resampler
no mixer
no software volume
no dither
no DSP
no f32 transport
```

Это предпочтительнее, чем проверять десять boolean conditions в разных местах.

---

# 5. Audio Representation

## 5.1 Problem

Текущий API вида:

```rust
trait AudioSource {
    fn next_frames(&mut self) -> Option<&[f32]>;
}
```

не подходит для strict integer PCM.

Пример:

```text
original i16 = -32768

normalize to f32 = -1.0
restore:
-1.0 * 32767 = -32767

result != original
```

Для i32 проблема ещё более фундаментальна:

```text
f32 mantissa ≈ 24 bits
i32 PCM = 32 bits
```

Поэтому все integer values не могут быть представлены exactly.

---

## 5.2 Typed Representation

Целевой abstraction:

```rust
pub enum AudioFrame<'a> {
    PcmI16(&'a [i16]),
    PcmI24(&'a [I24]),
    PcmI32(&'a [i32]),
    PcmF32(&'a [f32]),
    NativeDsd(&'a [DsdSample]),
    Dop(&'a [DopFrame]),
}
```

Или generic source:

```rust
pub trait PcmSource {
    type Sample;

    fn next_frames(&mut self) -> Result<Option<&[Self::Sample]>, DecodeError>;
}
```

`PcmF32` допустим для DSP path.

`PcmI16/I24/I32` обязаны оставаться typed до output boundary в Bit-Perfect path.

---

# 6. Ownership and Lifetime Contract

Это обязательное дополнение к v4.0.

## 6.1 Borrowed Decoder Frames

Decoder MAY возвращать borrowed frames:

```rust
AudioFrame<'a>
```

но lifetime такого frame ограничен текущей decoder operation.

Worker MUST NOT:

- помещать borrowed decoder slice в long-lived ring;
- передавать decoder-owned reference в RT callback;
- хранить reference после следующего decode operation;
- строить self-referential ownership между decoder и playback queue.

Пример:

```rust
let frame = decoder.next_frames()?;

// frame valid only while decoder guarantees its lifetime.
```

Нельзя:

```rust
ring.push(frame.samples); // forbidden if ring outlives frame
```

---

## 6.2 Owned Transport Storage

Worker должен копировать данные в заранее выделенное typed storage:

```text
decoder borrowed frame
        ↓
owned/preallocated worker buffer
        ↓
typed playback ring
        ↓
RT consumer
```

Например:

```rust
struct OwnedPcmBlock<T> {
    samples: Box<[T]>,
    frames: usize,
}
```

Но для hot path предпочтительно bounded reuse/pool:

```rust
struct PcmBlockPool<T> {
    free: Vec<OwnedPcmBlock<T>>,
}
```

Pool создаётся до playback.

После старта playback запрещается динамическое расширение storage.

---

## 6.3 Forbidden Ownership Models

НЕ использовать для Bit-Perfect transport:

```rust
Arc<Mutex<Vec<i16>>>
Arc<Mutex<Vec<i32>>>
Arc<Mutex<AudioBuffer>>
```

как основной audio transport.

Причины:

- скрытая synchronization;
- unpredictable contention;
- ownership complexity;
- потенциальная allocation;
- плохая RT boundary.

---

# 7. Typed Transport Must Reach CPAL

Критическое правило v4.1:

> Typed transport должен сохраняться до самого CPAL callback.

Цепочка для i16:

```text
Decoder
  ↓
PcmI16
  ↓
OwnedPcmBlock<i16>
  ↓
RingBuffer<i16>
  ↓
RtConsumer<i16>
  ↓
cpal callback(&mut [i16])
  ↓
device
```

Для i32:

```text
Decoder
  ↓
PcmI32
  ↓
RingBuffer<i32>
  ↓
RtConsumer<i32>
  ↓
cpal callback(&mut [i32])
```

Для DSP:

```text
Decoder
  ↓
PcmI16/I24/I32
  ↓
DSP conversion
  ↓
f32
  ↓
resampler
  ↓
DSP
  ↓
f32 ring
  ↓
cpal f32 callback
```

### Forbidden

```text
BitPerfect I16
    ↓
f32 ring
    ↓
i16 callback
```

или:

```text
BitPerfect I32
    ↓
f32
    ↓
i32 callback
```

---

# 8. I16 Representation

Прямой transport:

```rust
fn write_i16(
    output: &mut [i16],
    consumer: &mut RtConsumer<i16>,
) {
    for sample in output {
        *sample = consumer.pop().unwrap_or(0);
    }
}
```

В production RT code предпочтительно использовать API без allocation/error construction:

```rust
for dst in output.iter_mut() {
    *dst = match consumer.pop() {
        Some(sample) => sample,
        None => 0,
    };
}
```

Никакого:

```rust
sample as f32
sample * 32767.0
sample.round()
sample.clamp(...)
```

в strict path.

---

# 9. I24 Representation

## 9.1 Canonical Type

```rust
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct I24(i32);
```

Invariant:

```text
-8_388_608 <= value <= 8_388_607
```

Constructor обязан проверять диапазон вне RT hot path:

```rust
pub fn try_new(value: i32) -> Result<Self, I24Error>
```

Внутри proven-valid hot path допустим:

```rust
unsafe { I24::new_unchecked(value) }
```

только если invariant доказан локально.

---

## 9.2 CPAL 24-bit Boundary

Нельзя считать:

```text
I24 == i32
```

автоматически.

Нужно явно определить:

```text
source 24-bit PCM
        ↓
canonical I24
        ↓
CPAL selected sample format
        ↓
backend representation
        ↓
device representation
```

Если backend представляет 24-bit audio в 32-bit container, mapping должен быть документирован и покрыт golden test.

Пример:

```text
24-bit payload:
[ b23 ... b0 ]

32-bit container:
[ sign extension / padding / alignment ]
```

Точная endian/alignment semantics определяется конкретным backend и не должна угадываться.

---

# 10. I32 Representation

I32 MUST remain `i32`.

Forbidden:

```rust
i32 → f32 → i32
```

Golden vectors должны включать:

```text
i32::MIN
i32::MAX
-1
0
1
2^24
2^24 + 1
2^25 + 1
random values
```

Особенно:

```text
16_777_216
16_777_217
```

потому что такие adjacent integers демонстрируют ограничение f32 integer precision.

---

# 11. Decoder Boundary

Decoder должен сообщать реальную decoded representation:

```rust
enum DecodedAudio {
    I16,
    I24,
    I32,
    F32,
    Dsd,
}
```

Если Symphonia выдаёт другой внутренний representation, conversion должен быть explicit и иметь ownership/precision contract.

### Rule

Для strict Bit-Perfect:

```text
decoded integer sample
    ↓
same integer sample
```

Если decoder API не может предоставить необходимую integer representation без precision loss, strict Bit-Perfect для этого source format MUST be rejected.

Нельзя silently claim Bit-Perfect после lossy conversion.

---

# 12. Strict Hardware Capability Negotiation

## 12.1 Required Conditions

Bit-Perfect разрешается только если одновременно:

| Requirement | Condition |
|---|---|
| Sample rate | `device_rate == source_rate` |
| Sample format | exact supported mapping |
| Channels | exact channel count |
| Layout | exact channel positions |
| Resampler | disabled |
| DSP | disabled |
| Software volume | unity |
| Dither | disabled |
| Mixer | disabled |
| Backend mode | native/exclusive as required |
| Stream callback type | exact matching representation |

---

## 12.2 Capability Result

Не возвращать просто:

```rust
bool
```

Использовать диагностический result:

```rust
pub enum BitPerfectAvailability {
    Available {
        format: PcmFormat,
    },
    Unavailable(BitPerfectRejection),
}
```

Причины:

```rust
pub enum BitPerfectRejection {
    SampleRateMismatch {
        source: u32,
        device: u32,
    },
    SampleFormatMismatch,
    ChannelCountMismatch,
    ChannelLayoutMismatch,
    BackendDoesNotGuaranteeNativePath,
    ExclusiveModeUnavailable,
    NativeDsdUnavailable,
    DeviceStreamMismatch,
}
```

Это позволяет UI показать реальную причину.

---

# 13. CPAL Format Negotiation and Verification

Это обязательное дополнение.

Перед созданием stream необходимо зафиксировать:

```text
requested representation
selected CPAL stream config
callback sample type
backend stream representation
```

Invariant:

```text
source representation
    ==
selected CPAL representation
    ==
callback representation
    ==
device stream representation
```

Для i16:

```text
PcmI16
 → Ring<i16>
 → callback(&mut [i16])
 → CPAL i16 stream
```

Для i32:

```text
PcmI32
 → Ring<i32>
 → callback(&mut [i32])
 → CPAL i32 stream
```

Для I24:

```text
PcmI24
 → typed transport
 → explicit backend mapping
 → CPAL/backend representation
```

Если exact mapping невозможно доказать:

```text
BitPerfect = unavailable
```

---

# 14. Strict Bit-Perfect Selection Algorithm

```text
requested mode = BitPerfect
        │
        ▼
decode representation known?
        │
       no ─────→ reject/fallback DSP
        │
       yes
        ▼
native sample rate?
        │
       no ─────→ reject/fallback DSP
        │
       yes
        ▼
exact sample format?
        │
       no ─────→ reject/fallback DSP
        │
       yes
        ▼
exact channel count/layout?
        │
       no ─────→ reject/fallback DSP
        │
       yes
        ▼
backend/native/exclusive path?
        │
       no ─────→ reject/fallback DSP
        │
       yes
        ▼
callback representation exact?
        │
       no ─────→ reject/fallback DSP
        │
       yes
        ▼
BitPerfectPcm(...)
```

---

# 15. Fallback Policy

```rust
pub enum BitPerfectFallback {
    Dsp,
    Reject,
}
```

Flow:

```text
BitPerfect requested
       │
       ├── available → BitPerfect
       │
       └── unavailable
              │
              ├── Dsp → DSP mode
              │
              └── Reject → playback rejected
```

UI MUST display actual mode:

```text
Requested: Bit-Perfect
Actual: DSP
Reason: device does not support 44.1 kHz native output
```

Never:

```text
Requested: Bit-Perfect
Actual: DSP
UI: "Bit-Perfect"
```

---

# 16. Channel Layout Integrity

Bit-Perfect MUST preserve:

```text
channel count
channel order
channel identity
channel positions
```

Forbidden:

```text
stereo → mono
mono → stereo
5.1 → stereo
stereo → 5.1
L/R reorder
```

even when transformation appears harmless.

`mix_rel()` and equivalent channel conversion are DSP operations.

Example negative tests:

```rust
#[test]
fn stereo_to_mono_is_not_bitperfect() {}

#[test]
fn mono_to_stereo_is_not_bitperfect() {}

#[test]
fn surround_to_stereo_is_not_bitperfect() {}

#[test]
fn channel_reorder_is_not_bitperfect() {}
```

---

# 17. Software Volume

Strict Bit-Perfect requires:

```text
software volume == unity
```

Therefore:

```text
0.5 × PCM → NOT BitPerfect
```

Hardware volume may be permitted only if the platform/backend semantics are explicitly defined as outside the software signal-integrity path.

If the implementation cannot guarantee that hardware volume preserves the required semantics, the mode must not be advertised beyond what can be proven.

---

# 18. Dither

Dither is forbidden in Bit-Perfect:

```text
SignalPath::BitPerfectPcm(_)
    → no dither
```

Dither MAY exist in DSP conversion path.

Invariant:

```text
BitPerfectPcm && dither_enabled
```

must be impossible at the typed state level where practical.

---

# 19. RT Callback Contract

## 19.1 Core Rule

RT safety is not equivalent to:

```text
lock-free
allocation-free
```

The callback must also have **bounded execution time**.

---

## 19.2 Allowed Operations

RT callback MAY:

```text
atomic load/store
bounded arithmetic
typed SPSC ring operations
copy from preallocated memory
write device buffer
bounded state transition
```

---

## 19.3 Forbidden Operations

RT callback MUST NOT:

```text
Mutex / RwLock
allocation
deallocation
Vec growth
String creation
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
unbounded loops
```

---

# 20. RT Boundedness

Callback complexity must be proportional to output buffer size:

```text
O(output_buffer_len)
```

It MUST NOT depend on:

```text
ring occupancy
seek distance
decoder state
file size
visualizer backlog
queue backlog
number of dropped frames
number of stale generations
```

Forbidden:

```rust
while ring.pop().is_ok() {}
```

because worst-case work is proportional to ring occupancy.

---

# 21. Seek Invalidation: Generation/Epoch

## 21.1 Problem

After seek, stale queued audio must not be played.

Naive solution:

```rust
while ring.pop().is_ok() {}
```

is unbounded.

---

## 21.2 Generation Model

Use:

```rust
struct PlaybackGeneration {
    current: AtomicU64,
}
```

Worker increments generation:

```text
generation 41
    ↓ seek
generation 42
```

New blocks carry:

```rust
struct AudioBlock<T> {
    generation: u64,
    samples: ...
}
```

Consumer checks generation.

Conceptually:

```rust
if block.generation != current_generation {
    discard_block();
}
```

The discard operation itself must remain bounded.

---

## 21.3 Important Implementation Constraint

Generation tags alone do not magically remove stale data from an SPSC ring.

The implementation must define one of:

1. O(1) producer-side reset;
2. generation-aware ring cursor reset;
3. bounded stale-block rejection;
4. ring replacement outside the RT callback;
5. another formally bounded invalidation protocol.

The chosen mechanism MUST be documented and tested.

---

# 22. RT Scheduling and Priority

Это обязательное дополнение.

RT correctness includes scheduling behavior.

The architecture must document:

```text
callback thread priority
callback scheduling policy
worker priority
priority inversion risks
CPU starvation risks
backend scheduling guarantees
```

Requirements:

- callback MUST NOT wait for worker mutex;
- worker MUST NOT wait for callback progress to release critical resources;
- worker may prepare data ahead of playback;
- callback must remain functional under temporary worker stalls;
- visualization load must not compete with callback in a way that causes avoidable underruns.

Hardware/backend-specific scheduling guarantees must be documented separately.

---

# 23. RT Failure Behavior

When ring has insufficient data:

```text
missing samples → write deterministic silence
```

The callback must not:

```text
block
wait for decoder
invoke UI
request synchronous decode
allocate recovery buffer
```

Underrun counters may be updated atomically:

```rust
underruns.fetch_add(1, Ordering::Relaxed);
```

but logging/reporting happens outside RT.

---

# 24. DSD Architecture

## 24.1 Native DSD

```text
Decoder
  ↓
DsdSample / DsdBlock
  ↓
typed DSD transport
  ↓
native DSD backend
  ↓
device
```

No:

```text
DSD → f32 → PCM
```

for Native DSD.

---

# 25. DoP Architecture

DoP is an explicit typed protocol.

Example:

```rust
#[repr(C)]
pub struct DopFrame {
    pub payload: Dsd24,
    pub marker: DopMarker,
}
```

or equivalent strongly typed representation.

Encoder:

```rust
fn encode_dop_frame(
    payload: Dsd24,
    marker: DopMarker,
) -> DopFrame
```

must explicitly define:

- payload width;
- marker bytes;
- endian;
- frame alignment;
- channel interleave;
- marker sequence;
- block boundaries.

DoP MUST NOT be implemented as arbitrary f32 normalization.

---

# 26. DoP Golden Example

Given known DSD payload:

```text
payload = 0x12_34_56
marker  = defined DoP marker
```

expected encoded representation must be fixed:

```text
byte 0 = ...
byte 1 = ...
byte 2 = ...
byte 3 = ...
```

The exact byte sequence must come from the chosen DoP specification/backend contract and be encoded in golden tests.

Tests:

```text
payload packing
marker placement
marker alternation
endian
channel interleave
frame boundary
```

---

# 27. Native DSD Capability

Native DSD availability must be negotiated separately from PCM.

If unavailable:

```text
Native DSD requested
      ↓
hardware/backend cannot provide it
      ↓
fallback according to explicit policy
```

Never silently convert to another path while continuing to label it Native DSD.

---

# 28. Visualizer Architecture

## 28.1 Tap Position

The visualizer tap remains:

```text
worker
  ↓
resampler / DSP
  ↓
visualizer tap
  ↓
playback ring
```

The tap MUST NOT be inside the CPAL callback.

This preserves the existing A2.0 RT foundation.

---

# 29. VizTap Ownership

Preferred:

```rust
struct PlaybackWorker {
    viz_producer: Option<Producer<f32>>,
    viz_active: Arc<AtomicBool>,
}
```

The worker owns the producer.

UI/manager creates the ring and transfers producer ownership to the worker.

Avoid:

```rust
Arc<Mutex<Option<Producer<f32>>>>
```

for ownership coordination unless there is a concrete reason that cannot be solved by ownership transfer.

The current worker-side Mutex is not itself an RT violation because it is outside the callback, but direct ownership is simpler and easier to reason about.

---

# 30. Visualizer Backpressure

Visualization is best-effort.

Required behavior:

```text
visualizer slow
    ↓
drop visualization data
    ↓
playback continues
```

Forbidden:

```text
visualizer slow
    ↓
worker blocks
    ↓
playback starves
```

Visualizer queues must therefore have bounded capacity.

---

# 31. Slint UI Integration

Heavy computation remains worker-side:

```text
FFT
spectrogram
waveform analysis
cache work
decode
resampling
DSP
```

Do not run these in Slint event loop.

---

# 32. Latest-Value UI Model

Instead of:

```text
FFT block
 ↓
invoke_from_event_loop()
 ↓
FFT block
 ↓
invoke_from_event_loop()
```

use:

```text
worker
  ↓
latest-value buffer
  ↓
Slint timer 30–60 Hz
  ↓
consume latest frame
```

This intentionally drops intermediate visualizer frames.

Audio playback must never wait for UI consumption.

---

# 33. UI Ownership

Worker must not hold a strong UI reference.

Preferred:

```rust
let weak = ui.as_weak();
```

Then:

```rust
let _ = weak.upgrade_in_event_loop(move |ui| {
    ui.set_spectrum(...);
});
```

Invariant:

```text
UI owns UI lifetime
worker does not keep UI alive
```

---

# 34. Slint Property Efficiency

Prefer a single logical frame:

```rust
struct SpectrumFrame {
    bins: Box<[f32]>,
    peak: f32,
    rms: f32,
}
```

over many independent updates:

```text
set_bin_0
set_bin_1
...
set_bin_31
set_peak
set_rms
```

The exact Slint representation may differ, but update batching is preferred.

---

# 35. Decoder Error Semantics

## 35.1 Seek

Forbidden:

```rust
let time = Time::try_new(...).unwrap_or(Time::ZERO);
```

because:

```text
invalid seek
    ↓
silent seek to zero
```

Instead:

```rust
fn seek(...) -> Result<(), DecodeError>
```

and propagate the error.

---

# 36. Decode Errors

Errors must be classified.

Example:

```rust
enum DecodeSeverity {
    Recoverable,
    Fatal,
}
```

Recoverable:

```text
bad packet
    ↓
skip packet
    ↓
increment error counter
    ↓
continue
```

Fatal:

```text
decoder state invalid
    ↓
stop playback
    ↓
report error
```

Forbidden:

```rust
DecodeError(_) => continue
```

without classification, accounting, or observable status.

---

# 37. Error Reporting Boundary

RT callback MUST NOT format or log errors.

Instead:

```text
RT atomic counter/state
       ↓
worker/main thread
       ↓
structured error/event
       ↓
UI/logging
```

This keeps diagnostics outside the RT path.

---

# 38. Unwrap/Expect Policy

Do not perform a mechanical global replacement.

## Allowed

Tests:

```rust
unwrap()
expect()
```

are allowed where useful.

Internal invariant:

```rust
.expect("invariant: ring capacity is non-zero")
```

is acceptable only when:

1. invariant is locally established;
2. violation means programming error;
3. external input/device/file state is not the cause.

## Forbidden

External conditions:

```text
device unavailable
invalid seek
malformed file
unsupported format
missing configuration
backend failure
```

must return/report `Result` or explicit state.

---

# 39. Resampler Scope

The resampler is a DSP component, not a Bit-Perfect component.

It MUST NOT execute in:

```text
SignalPath::BitPerfectPcm(_)
```

It MAY execute in:

```text
SignalPath::DspPcm(_)
```

Do not rewrite the existing resampler merely because the architecture changes.

First measure current behavior.

---

# 40. Resampler Validation Matrix

Required conversions:

```text
44.1 → 48 kHz
48 → 44.1 kHz
96 → 44.1 kHz
192 → 48 kHz
```

Signals:

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

---

# 41. Resampler Metrics

Measure:

```text
passband ripple
stopband attenuation
alias rejection
frequency response
latency
CPU ns/sample
allocations
```

For each conversion, record:

```text
input rate
output rate
input frequency
output amplitude
error
alias components
CPU time
allocation count
```

---

# 42. DSP Acceptance Criteria

Acceptance thresholds MUST be written numerically before declaring the resampler validated.

Example table:

| Metric | 44.1→48 | 48→44.1 | 96→44.1 | 192→48 |
|---|---:|---:|---:|---:|
| Passband ripple | TBD | TBD | TBD | TBD |
| Stopband attenuation | TBD | TBD | TBD | TBD |
| Alias rejection | TBD | TBD | TBD | TBD |
| CPU ns/sample | measured | measured | measured | measured |
| Allocations | 0 hot-path | 0 hot-path | 0 hot-path | 0 hot-path |

`TBD` must be replaced by measured/approved thresholds during implementation.

Hardware THD+N is outside software-stage acceptance.

---

# 43. Golden PCM Tests

## 43.1 I16

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

Property:

```rust
assert_eq!(input, output);
```

---

# 44. I24

Vectors:

```text
0
1
-1
8_388_607
-8_388_608
random
```

Check:

```text
payload
sign
range
packing
alignment
endian
```

---

# 45. I32

Vectors:

```text
i32::MIN
i32::MAX
-1
0
1
2^24
2^24 + 1
-(2^24 + 1)
random
```

Property:

```rust
assert_eq!(input, output);
```

This test must fail if an f32 roundtrip is introduced.

---

# 46. Transport Property Tests

For each typed PCM format:

```text
generate valid samples
    ↓
encode into transport
    ↓
consume
    ↓
compare
```

Required:

```text
output[i] == input[i]
```

for all valid samples.

Use deterministic random seeds so failures are reproducible.

---

# 47. DoP Golden Tests

Test:

```text
known payload
→ encoder
→ expected frame bytes
```

Required coverage:

```text
zero payload
all-one payload
alternating payload
marker boundaries
channel interleave
endian
block boundary
```

---

# 48. Negative Bit-Perfect Tests

Every condition below must produce:

```text
BitPerfectUnavailable
```

or another explicit non-BitPerfect state:

```text
sample rate mismatch
sample format mismatch
channel count mismatch
channel layout mismatch
resampler enabled
DSP enabled
software volume != 1
dither enabled
channel mixer enabled
backend cannot guarantee native path
callback type mismatch
device stream mapping unknown
exclusive/native mode unavailable when required
```

---

# 49. State Machine Invariants

The implementation should make invalid states hard or impossible to construct.

Conceptually:

```text
BitPerfectPcm
    ├── no DSP
    ├── no resampler
    ├── no mixer
    ├── no dither
    ├── unity software volume
    ├── exact rate
    ├── exact channels
    ├── exact layout
    └── exact transport representation
```

If a field must be stored for diagnostics, it must not permit contradictory runtime state.

---

# 50. Performance Tests

Benchmark:

```text
typed ring push
typed ring pop
RT callback copy
generation handling
DSP f32 callback
visualizer tap
```

Metrics:

```text
p50
p95
p99
p99.9
max
```

Test under:

```text
small output buffer
medium output buffer
large output buffer
ring near empty
ring near full
normal steady state
seek transition
visualizer active
visualizer inactive
```

---

# 51. Allocation Tests

RT callback:

```text
allocations = 0
deallocations = 0
locks = 0
syscalls = 0
```

Worker:

```text
allocations allowed only during preparation or controlled non-RT phases
```

Hot playback path SHOULD use preallocated/reused buffers.

---

# 52. Hardware Verification: Three Truth Layers

Hardware validation MUST be split into three layers.

## Layer 1 — Software Truth

Verify:

```text
requested format
selected format
selected rate
selected channel layout
SignalPath
callback type
fallback state
```

This proves what the application believes it requested.

## Layer 2 — Backend/Driver Truth

Verify:

```text
actual stream format
backend conversion
shared/exclusive/native behavior
driver mixer/resampler behavior
```

This proves what the OS/backend actually created.

## Layer 3 — Physical Truth

Where applicable, verify with DAC loopback/input analyzer:

```text
native sample rate
DoP marker/payload
native DSD mode
unexpected conversion
channel mapping
```

Physical measurement is required to claim end-to-end hardware proof.

Software tests alone do not prove DAC-level bit-perfect output.

---

# 53. Hardware Verification Matrix

PCM examples:

```text
16/44.1
16/48
24/44.1
24/48
24/96
24/192
32/44.1
32/96
```

where supported by hardware.

DSD:

```text
DSD64
DSD128
DSD256
```

where supported.

Record:

| Source | Device | Requested | Actual stream | Layout | SignalPath | Backend | Physical evidence | Result |
|---|---|---|---|---|---|---|---|---|
| ... | ... | ... | ... | ... | ... | ... | ... | ... |

---

# 54. Hardware Verification Document

Create:

```text
docs/hardware_verification_audio_integrity.md
```

For each device record:

```text
OS
backend
device
driver
source
sample rate
sample format
channels
layout
requested mode
actual mode
CPAL config
backend behavior
physical evidence
date
result
```

Unsupported native rate must be explicitly tested:

```text
requested BitPerfect
device lacks 44.1
→ BitPerfect unavailable
→ DSP fallback or Reject
→ UI reports actual state
```

---

# 55. CPAL Boundary Tests

Where backend testing is possible, verify:

```text
selected CPAL SampleFormat
    ==
callback sample type
```

Examples:

```text
SampleFormat::I16
→ callback(&mut [i16])

SampleFormat::I32
→ callback(&mut [i32])

SampleFormat::F32
→ callback(&mut [f32])
```

For I24/containerized formats, add backend-specific assertions and golden mapping tests.

---

# 56. Visualizer Validation

Verify:

```text
tap position = after resampler/DSP and before playback ring
```

Tests should establish that:

```text
visualizer does not modify playback samples
visualizer backpressure cannot block playback
visualizer can be disabled without changing signal path
```

---

# 57. Documentation Synchronization

The actual implementation must match:

```text
docs/spec_visualizer_v5.1.md
docs/spec_audio_core_v2.0.md
docs/spec_audio_settings_v3.0.md
```

If `docs/spec_rt_audio_v1.0.md` is created, it should contain reusable RT rules shared by other audio features.

If RT rules are only specific to this architecture, keeping them in this spec is acceptable and avoids unnecessary documentation fragmentation.

---

# 58. Implementation Strategy

This is a refactor over the existing A2.0 RT foundation.

Do NOT rewrite the audio engine from scratch.

---

# 59. Phase 0 — Baseline

Agent MUST:

```text
read _STATE_.md
check git status
verify continuation whitelist
cargo check --quiet
cargo test
cargo clippy
```

If baseline is broken:

```text
read-only diagnostic mode
```

Do not mix unrelated fixes into the task.

---

# 60. Phase 1 — Representation

Introduce:

```text
PcmSampleFormat
I24
AudioFrame
PcmSource
SignalPath
```

At this stage:

- no UI redesign;
- no resampler rewrite;
- no RT rewrite.

Add unit tests for representation invariants.

---

# 61. Phase 2 — Typed Worker Transport

Replace:

```text
worker → f32-only playback transport
```

with:

```text
worker → typed transport
```

Start with I16.

Then I24.

Then I32.

Each step must have:

```text
implementation
unit tests
golden vectors
cargo check
cargo test
cargo clippy
commit
STATE update
```

---

# 62. Phase 3 — Typed CPAL Boundary

Implement:

```text
Ring<T>
RtConsumer<T>
callback<T>
```

only where CPAL/backend representation is proven compatible.

Do not create a generic abstraction that hides sample-type mismatches.

A concrete implementation such as:

```rust
fn write_i16(output: &mut [i16], consumer: &mut RtConsumer<i16>)
```

is preferable to an abstraction that silently converts.

---

# 63. Phase 4 — Strict Hardware Policy

Implement:

```rust
BitPerfectAvailability
BitPerfectRejection
BitPerfectFallback
```

Selection must reject:

```text
wrong rate
wrong format
wrong channels
wrong layout
unknown backend mapping
non-native path
```

before entering BitPerfect state.

---

# 64. Phase 5 — RT Seek Boundedness

Replace O(N) drain with generation/epoch or another formally bounded mechanism.

Acceptance:

```text
callback work does not depend on stale ring occupancy
```

Add benchmark covering worst-case ring occupancy.

---

# 65. Phase 6 — DSD / DoP

Implement:

```text
typed DoP
golden vectors
native DSD capability
fallback semantics
```

No f32 disguised DoP transport.

---

# 66. Phase 7 — Decoder Robustness

Implement:

```text
seek Result propagation
recoverable/fatal decode errors
error counters/status
unwrap/expect audit
```

Do not mechanically replace every unwrap.

---

# 67. Phase 8 — Visualizer/UI

Implement:

```text
worker-owned VizTap
bounded visualization queue
latest-value UI state
30–60 Hz timer
Weak UI handles
batched property updates
```

No RT callback changes are required for visualization.

---

# 68. Phase 9 — DSP Validation

Measure existing resampler first.

Only modify it if measurements show an actual defect.

Run:

```text
44.1 → 48
48 → 44.1
96 → 44.1
192 → 48
```

and all required signal/metric tests.

---

# 69. Phase 10 — Verification

Complete:

```text
golden PCM
negative BitPerfect
DoP
state invariants
RT performance
allocation
CPAL mapping
software/backend/physical hardware verification
documentation audit
```

---

# 70. Agent Execution Rules

For this project, the implementation agent MUST follow existing repository workflow.

Before each micro-step:

```text
_STATE_.md
git status
whitelist
baseline
```

Constraints:

```text
one micro-task
≤ 1–2 code files where practical
small reviewable commit
```

After successful step:

```text
cargo check
cargo test
cargo clippy
STATE update
commit
```

After three failed attempts at the same micro-step:

```text
rollback current step
document failure
stop
```

Do not continue by accumulating speculative patches.

---

# 71. Suggested Micro-Steps

Recommended sequence:

```text
A4.0.1 — Add SignalPath types
A4.0.2 — Add PcmSampleFormat
A4.0.3 — Add I24 invariant type
A4.0.4 — Add typed AudioFrame
A4.0.5 — Add I16 owned transport
A4.0.6 — Add I16 golden tests
A4.0.7 — Add I16 CPAL callback
A4.0.8 — Add I24 transport
A4.0.9 — Add I24 golden/packing tests
A4.0.10 — Add I32 transport
A4.0.11 — Add I32 precision vectors
A4.0.12 — Add typed ring/consumer boundary tests
A4.1.1 — Add strict capability result
A4.1.2 — Add rate rejection
A4.1.3 — Add format rejection
A4.1.4 — Add channel/layout rejection
A4.1.5 — Add callback/device representation verification
A4.1.6 — Add fallback state reporting
A4.2.1 — Add generation state
A4.2.2 — Replace O(N) seek drain
A4.2.3 — Benchmark worst-case seek
A4.3.1 — Add typed DoP
A4.3.2 — Add DoP golden vectors
A4.3.3 — Add Native DSD capability
A4.4.1 — Fix seek errors
A4.4.2 — Classify decode errors
A4.4.3 — Audit production unwrap/expect
A4.5.1 — Simplify VizTap ownership
A4.5.2 — Bound visualizer backpressure
A4.5.3 — Add latest-value UI model
A4.5.4 — Synchronize visualizer spec
A4.6.1 — Resampler measurement harness
A4.6.2 — Frequency response tests
A4.6.3 — Alias rejection tests
A4.6.4 — CPU/allocation benchmarks
A4.7.1 — Full golden suite
A4.7.2 — RT benchmark suite
A4.7.3 — Hardware verification matrix
A4.7.4 — Final documentation audit
```

The exact decomposition may change after repository inspection, but each step must remain small and independently verifiable.

---

# 72. Anti-Patterns

DO NOT:

1. Fix Bit-Perfect by adding more boolean flags.
2. Convert integer PCM through f32 and call it Bit-Perfect.
3. Keep typed PCM only at decoder boundary and convert back to f32 later.
4. Hide CPAL sample-type mismatches behind generic conversion.
5. Add Mutex/RwLock to RT callback.
6. Add allocation to RT callback.
7. Drain an unbounded ring from RT callback.
8. Put FFT or visualization processing in callback.
9. Silently resample while reporting BitPerfect.
10. Silently remap channels while reporting BitPerfect.
11. Silently change software volume while reporting BitPerfect.
12. Treat DoP as ordinary f32 PCM.
13. Claim Native DSD after conversion.
14. Mechanically remove all unwrap/expect.
15. Rewrite the resampler without measurements.
16. Let visualizer backpressure block playback.
17. Let worker own UI lifetime.
18. Make hardware claims based only on application state.
19. Assume CPAL format selection proves physical device format.
20. Make one giant multi-file unreviewable commit.
21. Introduce self-referential decoder/transport ownership.
22. Use `Arc<Mutex<Vec<T>>>` as the primary Bit-Perfect transport.
23. Make callback work depend on ring occupancy, seek distance, or backlog.

---

# 73. Definition of Done

## Signal Integrity

- [ ] decoded integer PCM has exact sample identity
- [ ] I16 exact transport
- [ ] I24 exact transport
- [ ] I32 exact transport
- [ ] no integer PCM → f32 → integer conversion in strict path
- [ ] typed ring/consumer reaches CPAL boundary
- [ ] exact callback sample representation
- [ ] exact device stream mapping proven
- [ ] no resampler in BitPerfect
- [ ] no channel conversion
- [ ] no software volume != unity
- [ ] no dither
- [ ] no DSP

## Ownership

- [ ] borrowed decoder frames never outlive decoder contract
- [ ] worker copies into owned/preallocated transport
- [ ] RT never holds decoder references
- [ ] no self-referential audio ownership
- [ ] bounded buffer reuse

## Hardware Policy

- [ ] native sample rate required
- [ ] exact sample format required
- [ ] exact channels required
- [ ] exact layout required
- [ ] native/exclusive requirement enforced where applicable
- [ ] explicit rejection reason
- [ ] explicit DSP fallback
- [ ] UI reports actual signal path

## RT

- [ ] zero allocation in callback
- [ ] zero locks in callback
- [ ] no syscalls
- [ ] no decoder
- [ ] no FFT
- [ ] no UI
- [ ] bounded seek invalidation
- [ ] callback work O(output_buffer_len)
- [ ] no work proportional to ring occupancy
- [ ] scheduling/priority contract documented
- [ ] underrun handling non-blocking

## DSD / DoP

- [ ] typed DoP transport
- [ ] DoP golden vectors
- [ ] Native DSD path
- [ ] Native DSD capability verification
- [ ] explicit fallback semantics

## Visualizer/UI

- [ ] worker-side tap after DSP/resampler
- [ ] tap before playback ring
- [ ] worker-owned producer
- [ ] bounded visualizer queue
- [ ] visualizer cannot block playback
- [ ] heavy computation worker-side
- [ ] latest-value UI model
- [ ] 30–60 Hz UI update budget
- [ ] Weak UI handles
- [ ] efficient property batching
- [ ] documentation synchronized

## Decoder Robustness

- [ ] no silent seek-to-zero
- [ ] recoverable errors classified
- [ ] fatal errors reported
- [ ] decode errors observable
- [ ] production unwrap/expect audited

## DSP

- [ ] 44.1→48 validated
- [ ] 48→44.1 validated
- [ ] 96→44.1 validated
- [ ] 192→48 validated
- [ ] impulse tested
- [ ] sine tests
- [ ] near-Nyquist tested
- [ ] passband ripple measured
- [ ] stopband attenuation measured
- [ ] alias rejection measured
- [ ] frequency response measured
- [ ] CPU ns/sample measured
- [ ] hot-path allocations measured

## Verification

- [ ] PCM golden suite
- [ ] negative BitPerfect suite
- [ ] state invariants
- [ ] RT benchmark
- [ ] CPAL representation tests
- [ ] software truth verified
- [ ] backend/driver truth verified
- [ ] physical truth verified where hardware is available
- [ ] hardware verification document created

---

# 74. Related Documents

- `docs/spec_audio_core_v2.0.md`
- `docs/spec_audio_settings_v3.0.md`
- `docs/spec_visualizer_v5.1.md`
- `docs/spec_rt_audio_v1.0.md` — create only if RT rules are shared across multiple features
- `_DRAFTS_/code_review_3.md`
- `_DRAFTS_/code_review3.1.md`
- `_STATE_.md`
- `ROADMAP.md`

---

# 75. Priority Order

Do not begin by optimizing RT code.

The fundamental risk is signal semantics:

```text
Decoder → f32 → integer
```

Correct order:

```text
1. Signal semantics
2. Representation and ownership
3. Typed PCM transport
4. CPAL/device format proof
5. Strict BitPerfect hardware policy
6. RT boundedness
7. DSD / DoP
8. Decoder error semantics
9. Visualizer/UI
10. DSP/resampler validation
11. Hardware proof
12. Final documentation audit
```

---

# 76. Architectural Examples

## Example A — True Bit-Perfect i16

```text
FLAC decoder
    ↓
decoded i16
    ↓
AudioFrame::PcmI16
    ↓
OwnedPcmBlock<i16>
    ↓
Ring<i16>
    ↓
RtConsumer<i16>
    ↓
cpal callback(&mut [i16])
    ↓
native i16 stream
    ↓
DAC
```

No f32 exists in this path.

---

## Example B — DSP Fallback

Device does not support source rate:

```text
source = 44.1 kHz i16
device = 48 kHz only

BitPerfect requested
        ↓
native rate unavailable
        ↓
BitPerfectUnavailable::SampleRateMismatch
        ↓
fallback = Dsp
        ↓
i16 → f32
        ↓
resampler 44.1 → 48
        ↓
DSP
        ↓
f32 ring
        ↓
cpal f32 callback
```

UI:

```text
Mode: DSP
Reason: device does not support native 44.1 kHz
```

Never:

```text
Mode: Bit-Perfect
```

---

## Example C — Seek

Before seek:

```text
generation = 41
ring contains blocks tagged 41
```

Seek:

```text
worker increments generation → 42
new blocks tagged 42
```

Old blocks are invalidated through the chosen bounded protocol.

Callback does not execute:

```rust
while pop() {}
```

---

## Example D — Visualizer

```text
decoder
   ↓
DSP/resampler
   ↓
audio samples
   ├────────────→ visualization bounded queue
   │                    ↓
   │                 FFT worker
   │                    ↓
   │              latest-value state
   │                    ↓
   │              Slint timer 30–60 Hz
   │
   └────────────→ playback ring
                        ↓
                    RT callback
```

If FFT stalls:

```text
visualizer samples dropped
playback continues
```

---

## Example E — Hardware Proof

Application says:

```text
SignalPath::BitPerfectPcm(I24)
sample_rate = 96 kHz
```

This proves only software state.

Then backend verification establishes:

```text
CPAL stream = 96 kHz
format = backend's 24-bit representation
exclusive/native path = active
```

Physical analyzer establishes:

```text
actual DAC/input path = 96 kHz
24-bit payload preserved
no unexpected sample-rate conversion
```

Only after these layers can the result be recorded as end-to-end hardware evidence.

---

# 77. Final Architectural Invariants

The implementation is considered conformant only if all of the following hold:

```text
Invariant 1:
BitPerfect means decoded PCM sample identity.

Invariant 2:
Integer PCM never traverses f32 in strict path.

Invariant 3:
Typed transport reaches CPAL callback.

Invariant 4:
Borrowed decoder memory never escapes its lifetime.

Invariant 5:
BitPerfect requires exact rate/format/channels/layout.

Invariant 6:
Unknown CPAL/backend representation means BitPerfect unavailable.

Invariant 7:
RT callback work is bounded by output buffer size.

Invariant 8:
Seek invalidation is not an occupancy-dependent O(N) drain.

Invariant 9:
Visualizer cannot block playback.

Invariant 10:
Native DSD and DoP are separate typed paths.

Invariant 11:
Decode/device/file errors are not silently converted into valid-looking state.

Invariant 12:
Software claims, backend claims, and physical hardware claims are separate verification layers.
```

---

# 78. Revision History

| Version | Date | Changes |
|---|---|---|
| 4.0 | 2025 | Initial Audio Integrity architecture |
| 4.1 | 2026-09-18 | Added formal BitPerfect boundary, ownership/lifetime contract, typed transport through CPAL, CPAL/backend verification, bounded RT contract, scheduling contract, three-layer hardware verification, and expanded implementation examples |

---

**End of SPEC.**
