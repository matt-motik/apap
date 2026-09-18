--- docs/spec_audio_integrity_v4.0.md (原始)


+++ docs/spec_audio_integrity_v4.0.md (修改后)
# SPEC: Audio Integrity & Strict Bit-Perfect Architecture v4.0

**Status:** Proposed
**Date:** 2025
**Related:** Code Review 3.0, Code Review 3.1
**Replaces:** N/A (new architecture spec)

---

## 1. Executive Summary

Эта спецификация определяет архитектуру целостности аудиосигнала для apap player с акцентом на **строгий Bit-Perfect режим**, **typed PCM transport**, и **RT safety**.

**Ключевой принцип:**

> **`f32` — representation для DSP, а не универсальный transport format.**

Текущая архитектура использует `AudioSource::next_frames() -> Option<&[f32]>`, что делает невозможным настоящий bit-perfect transport для integer PCM (потери при конвертации i16/i24/i32 → f32 → i16/i24/i32).

Цель: разделить signal paths так, чтобы integer PCM транспортировался без конвертации через f32 в Bit-Perfect режиме.

---

## 2. Target Architecture

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

---

## Audio Buffer Ownership

Invariant:

1. borrowed decoder frames are valid only during current worker iteration;
2. PlaybackWorker must copy into owned/preallocated transport storage
   before decoder buffer is reused;
3. RT consumer never holds decoder references;
4. no self-referential structures;
5. no Arc<Mutex<Vec<_>> as audio transport;
6. buffer reuse must be explicit and bounded.

## 3. Bit-Perfect Definition

### 3.1 Bit-Perfect PCM

Bit-Perfect означает identity decoded PCM samples
между decoder output и hardware output representation.

Это не означает byte-for-byte identity с compressed file.

FLAC file bytes
↓
Symphonia decode
↓
PCM samples
↓
BitPerfect transport
↓
DAC

**Разрешено:**

```text
decode → exact PCM samples → transport → device
```

**Запрещено в Bit-Perfect режиме:**

```text
PCM → f32 → PCM          (конвертация с потерями)
PCM → resampler          (изменение sample rate)
PCM → channel mixer      (downmix/upmix)
PCM → volume             (software volume != 1.0)
PCM → dither             (dithering)
PCM → DSP                (любая обработка)
```

### 3.2 Hardware Requirements

Bit-Perfect разрешается **только если одновременно**:

| Требование                          | Описание                                    |
| ----------------------------------- | ------------------------------------------- |
| `sample_rate == source_rate`        | Частота дискретизации устройства = исходной |
| `sample_format == source_format`    | Формат (i16/i24/i32) = исходному            |
| `channel_count == source_channels`  | Количество каналов = исходному              |
| `channel_layout == source_layout`   | Раскладка каналов = исходной                |
| `DSP == disabled`                   | Обработка отключена                         |
| `software_volume == 1.0`            | Громкость 100%                              |
| `dither == disabled`                | Dither отключен                             |
| `resampler == disabled`             | Resampler отключен                          |
| `hardware_mode == exclusive/native` | Требуется эксклюзивный/нативный режим       |

**Если хотя бы одно условие невозможно:**

```text
BitPerfect unavailable
        │
        └── fallback allowed → DSP mode
```

**Важно:** `DSP mode ≠ BitPerfect`. Нельзя сообщать UI что включён Bit-Perfect если используется resampling или другая обработка.

---

## 4. Audio Representation

### 4.1 Текущая проблема

```rust
trait AudioSource {
    fn next_frames(&mut self) -> Option<&[f32]>;
}
```

Затем в callback:

```rust
let sample = src * 32767.0;
let value = sample.round() as i16;
```

**Пример потери:**

```text
original i16: -32768
normalized:   -1.0
* 32767:      -32767  ← ПОТЕРЯ!
```

### 4.2 Целевой API

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

Или через generic trait:

```rust
trait PcmSource {
    type Sample;

    fn next_frames(&mut self) -> Option<&[Self::Sample]>;
}
```

**Invariant:** Integer PCM must not be converted through f32 on the strict Bit-Perfect path.

### 4.3 SignalPath Enum

Ввести явное состояние вместо множества boolean флагов:

```rust
enum SignalPath {
    BitPerfectPcm(PcmFormat),
    DspPcm(DspFormat),
    NativeDsd(DsdFormat),
    DoP(DopFormat),
}
```

Где `SignalPath` становится **source of truth** для определения текущего режима.

---

## 5. Typed PCM Transport

### 5.1 I16 Transport

**Target implementation:**

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

**Никакого:**

```rust
// ЗАПРЕЩЕНО в Bit-Perfect path
f32 → multiply → round → clamp → i16
```

### 5.2 I24 Transport

24-bit требует отдельного representation:

```rust
#[repr(transparent)]
struct I24(i32);
```

**Invariant:**

```text
-8_388_608 <= value <= 8_388_607
```

**Важно:** Если CPAL backend представляет 24-bit output через 32-bit container, transport должен быть явно описан:

```text
source 24-bit PCM
       ↓
device's expected 24-bit-in-container representation
```

Нельзя просто считать любой i32 автоматически bit-perfect.

### 5.3 I32 Transport

Текущий код использует:

```rust
src * 2147483647.0  // ← ПОТЕРЯ ТОЧНОСТИ!
```

**Проблема:** `f32` не способен точно представить все значения 32-bit integer PCM.

**Тест:**

```text
i32 input → transport → output
```

Должен давать:

```text
output == input
```

для edge/random vectors.

---

## 6. Strict Bit-Perfect Hardware Selection

### 6.1 Правило несовместимости

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

**А не:**

```text
BitPerfect + resampler  ← НЕДОПУСТИМО
```

### 6.2 Fallback Policy

```rust
enum BitPerfectFallback {
    Dsp,      // Разрешить fallback в DSP режим
    Reject,   // Отказать, если BitPerfect невозможен
}
```

**Flow:**

```text
BitPerfect requested
       │
       ├── exact path available → BitPerfect
       │
       └── impossible
             │
             ├── Dsp → DSP path (UI показывает "DSP mode")
             │
             └── Reject → Unsupported
```

UI должен показывать **реальный режим**, а не запрошенный.

---

## 7. Channel Layout Integrity

### 7.1 Запрет конвертации в Bit-Perfect

Текущий `Resampler` содержит `mix_rel()` для downmix/upmix.

Это нормально для **DSP mode**, но:

```text
BitPerfect + mix_rel() = ЗАПРЕЩЕНО
```

### 7.2 Тесты

```rust
#[test]
fn bitperfect_rejects_channel_conversion() {
    // stereo → mono = not bit-perfect
    // mono → stereo = not bit-perfect
    // 5.1 → stereo = not bit-perfect
    // stereo → 5.1 = not bit-perfect
}
```

Даже если mathematically transformation кажется безобидной.

---

## 8. RT Callback Safety

### 8.1 Проблема: O(N) Seek Drain

Текущий код:

```rust
// reconcile_seek()
while ring.pop().is_ok() {}
```

Это lock-free и allocation-free, но:

> **lock-free ≠ bounded-time.**

Если ring большой, callback может выполнить большое количество `pop`.

### 8.2 Решение: Generation/Epoch Mechanism

```rust
struct PlaybackEpoch {
    generation: AtomicU64,
}
```

**Producer:**

```text
generation = 42  // маркирует данные
```

**Seek:**

```text
generation = 43  // инкремент при seek
```

**Consumer:** видит mismatch и перестаёт считать старые frames валидными.

### 8.3 RT Safety Contract

**Запрещено в RT callback:**

```text
Mutex, RwLock
allocation, deallocation
Vec growth, String
format!, println!, eprintln!
filesystem, network
decoder, resampler
FFT, Slint
blocking syscall, sleep
unbounded loop
```

**Разрешено:**

```text
atomic load/store
bounded arithmetic
preallocated memory
SPSC ring operations
direct device buffer writes
```

---

## 9. Dither Policy

Dither должен быть строго:

```text
DSP / integer conversion path
```

**Но не:**

```text
BitPerfect
```

**Target:**

```rust
match signal_path {
    SignalPath::BitPerfectPcm(_) => {
        // direct copy, NO dither
    }

    SignalPath::DspPcm(_) => {
        // conversion / dither if configured
    }
}
```

**Invariant:** Невозможно получить состояние `bit_perfect == true && dither == true`.

---

## 10. DSD & DoP Transport

### 10.1 DoP (DSD over PCM)

DoP нельзя концептуально считать обычным PCM `f32`.

**Текущий код:**

```rust
((src.clamp(0, 16777215) as u32) << 8) as i32
```

**Заменить на явно типизированный DoP representation:**

```rust
struct DopFrame {
    payload: u32,
}

fn encode_dop_frame(
    dsd_payload: Dsd24,
    marker: DopMarker,
) -> DopFrame
```

### 10.2 Native DSD

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

**Никакого:**

```text
DSD → f32 → PCM  ← ДЛЯ NATIVE DSD
```

Для неподдерживаемого hardware:

```text
Native DSD unavailable → fallback согласно policy
```

---

## 11. Visualizer Architecture

### 11.1 Текущее состояние

Фактическая реализация уже делает tap в **worker после resampler и до playback ring**, а не внутри CPAL callback. Это правильный вариант.

**Target:**

```text
worker
  │
  ├── playback ring
  │
  └── visualization ring
```

### 11.2 VizTap Ownership

**Сейчас:**

```rust
Arc<Mutex<Option<Producer<f32>>>>
```

Mutex находится **не в RT callback**, а worker-side. Это не critical RT violation, но архитектурно лучше:

```rust
struct PlaybackWorker {
    viz_producer: Option<Producer<f32>>,
    viz_active: Arc<AtomicBool>,
}
```

Worker владеет producer напрямую. UI/manager только:

```text
start visualizer
    ↓
create ring
    ↓
give producer to worker
```

### 11.3 Backpressure Invariant

```text
visualizer slow
      ↓
drop visualization samples
      ↓
playback continues normally
```

**Никогда:**

```text
visualizer slow
      ↓
worker blocked
      ↓
playback underrun
```

---

## 12. Slint UI Integration

### 12.1 Update Rate Separation

Heavy processing остаётся worker-side:

```text
FFT, decode, spectrogram, waveform, cache
```

**Не в Slint.**

**Предотвратить:**

```text
FFT worker
   ↓
invoke_from_event_loop()  ← на каждый FFT block
invoke_from_event_loop()
invoke_from_event_loop()
...
```

**Target pipeline:**

```text
FFT worker
    ↓
latest-value buffer
    ↓
Slint timer 30–60 Hz
    ↓
consume latest frame
```

UI rendering rate отделён от DSP rate.

### 12.2 Weak UI Handles

Worker не должен владеть сильной ссылкой на UI.

**Target pattern:**

```rust
let weak = ui.as_weak();

worker.on_update(move |data| {
    let weak = weak.clone();

    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_spectrum(...);
    });
});
```

**Invariant:**

```text
UI owns UI
worker does not keep UI alive
```

### 12.3 Property Efficiency

**Плохо:**

```text
every FFT frame
    ↓
10–20 individual Slint property updates
```

**Лучше:**

```rust
SpectrumFrame {
    bins: ...,
    peak: ...,
    rms: ...,
}
```

и одна логическая update operation.

Особенно для: spectrum, waveform, playback position, device state, visualizer status.

---

## 13. Decoder Error Handling

### 13.1 Seek Errors

**Сейчас:**

```rust
let time = Time::try_new(...).unwrap_or(Time::ZERO);
```

**Проблема:**

```text
invalid seek
    ↓
seek to zero  ← silent corruption
```

**Target:**

```rust
fn seek(...) -> Result<(), DecodeError>
```

и явный propagation.

### 13.2 Decode Error Policy

**Сейчас:**

```rust
DecodeError(_) => continue  // ← silently skipped
```

**Определить policy:**

**Recoverable:**

```text
recoverable packet error
    ↓
continue
    +
error counter/status
```

**Fatal:**

```text
fatal decode error
    ↓
stop
    +
reported error
```

Никакого silent corruption.

---

## 14. Unwrap/Expect Policy

### 14.1 Production Code

**Запрещено без доказуемого invariant:**

```rust
unwrap()
expect()
```

### 14.2 Tests

**Разрешено.**

### 14.3 Internal Invariant

**Можно:**

```rust
debug_assert!
```

или:

```rust
.expect("invariant: ...")
```

**Только если:**

1. invariant локально доказан;
2. нарушение действительно является programming bug;
3. это не внешний input/device/file condition.

---

## 15. Resampler DSP Validation

### 15.1 Test Cases

**Sample rate conversions:**

```text
44.1 → 48 kHz
48 → 44.1 kHz
96 → 44.1 kHz
192 → 48 kHz
```

**Test signals:**

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

### 15.2 Metrics

```text
passband ripple
stopband attenuation
alias rejection
frequency response
latency
CPU ns/sample
allocations
```

**Не принимать решение «resampler high-end quality» на основании только unit tests.**

---

## 16. Golden Tests

### 16.1 16-bit PCM

**Vectors:**

```text
0, 1, -1
32767, -32768
0x5555, 0xAAAA
random
```

**Проверка:**

```rust
assert_eq!(input, output);
```

### 16.2 24-bit PCM

**Vectors:**

```text
0, 1, -1
8388607, -8388608
random
```

### 16.3 32-bit PCM

**Особенно:**

```text
i32::MIN
i32::MAX
2^24, 2^24 + 1, ...
```

Это поймает потерю точности через `f32`.

### 16.4 DoP Golden Tests

```text
known DSD payload
        ↓
DoP encoder
        ↓
expected byte sequence
```

**Тестировать:**

- marker
- frame boundary
- endian
- channel interleave
- payload packing
- block alignment

### 16.5 Negative Tests

Не только positive tests. Должно быть:

```text
native rate mismatch → NOT BitPerfect
channel mismatch     → NOT BitPerfect
resampler enabled    → NOT BitPerfect
volume != 1          → NOT BitPerfect
dither enabled       → NOT BitPerfect
DSP enabled          → NOT BitPerfect
software mixer       → NOT BitPerfect
```

### 16.6 State Machine Invariants

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

**Лучше:** типовая модель вообще не должна позволять такие состояния.

---

## 17. Performance & Allocation Tests

### 17.1 Benchmark Hot Path

```text
callback
 ↓
RtConsumer
 ↓
ring
```

**Метрики:**

```text
allocations = 0
locks = 0
syscalls = 0
unbounded loops = 0
```

### 17.2 Callback Latency

```text
p50, p95, p99, p99.9, max
```

особенно на больших buffer/ring configurations.

---

## 18. Hardware Verification

### 18.1 Matrix

**PCM:**

```text
16/44.1, 16/48
24/44.1, 24/48, 24/96, 24/192
32/44.1, 32/96
```

если hardware поддерживает.

**DSD:**

```text
DSD64, DSD128, DSD256
```

где доступно.

### 18.2 Verification Checklist

Для каждого проверить:

```text
requested format
actual stream format
actual sample rate
actual channel layout
reported mode
```

И физически через **DAC/input analyzer** для подтверждения native/DoP/DSD.

### 18.3 Hardware Verification Document

Создать `docs/hardware_verification_audio_integrity.md`:

| Source | Device | Rate | Format | Mode | Result | Evidence |
| ------ | ------ | ---- | ------ | ---- | ------ | -------- |
| ...    | ...    | ...  | ...    | ...  | ...    | ...      |

Особенно для:

- native PCM
- DoP
- native DSD
- unsupported native rate
- fallback to DSP

---

## 19. Implementation Roadmap

### Phase 0 — Baseline

```text
read _STATE_
check git status
cargo check --quiet
cargo test
cargo clippy
```

### Phase 1 — Architecture

```text
Symphonia research
 ↓
AudioRepresentation
 ↓
SignalPath
```

### Phase 2 — Bit-Perfect PCM

```text
I16 transport
 ↓
I24 transport
 ↓
I32 transport
 ↓
golden vectors
```

### Phase 3 — Hardware Policy

```text
strict native rate
 ↓
strict channel layout
 ↓
strict format
 ↓
DSP fallback
```

### Phase 4 — RT Boundedness

```text
seek generation
 ↓
remove O(N) drain
 ↓
RT tests
 ↓
latency benchmark
```

### Phase 5 — DSD/DoP

```text
DoP typed transport
 ↓
golden vectors
 ↓
Native DSD path
```

### Phase 6 — Decoder Robustness

```text
seek errors
 ↓
decode error policy
 ↓
production unwrap audit
```

### Phase 7 — Visualizer/UI

```text
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

### Phase 8 — DSP Validation

```text
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

### Phase 9 — Verification

```text
golden tests (A4.8)
 ↓
RT performance (A4.9)
 ↓
hardware (A4.10)
 ↓
final audit (A4.11)
```

---

## 20. Anti-Patterns (DO NOT)

```text
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

## 21. Definition of Done

### Signal Integrity

- [ ] 16-bit exact transport
- [ ] 24-bit exact transport
- [ ] 32-bit exact transport
- [ ] no f32 in strict PCM path
- [ ] no resampling in BitPerfect
- [ ] no channel conversion
- [ ] no software volume
- [ ] no dither
- [ ] no DSP

### DSD

- [ ] DoP golden vectors
- [ ] Native DSD path
- [ ] DSD fallback semantics

### RT

- [ ] zero allocation
- [ ] zero locks
- [ ] no syscalls
- [ ] no decoder
- [ ] no FFT
- [ ] bounded seek handling
- [ ] deterministic RT tests

### UI

- [ ] workers own heavy computation
- [ ] UI only presentation
- [ ] Weak UI handles
- [ ] latest-value strategy
- [ ] 30–60 Hz UI update budget
- [ ] visualization cannot block playback

### DSP

- [ ] resampler frequency-response tests
- [ ] alias rejection tests
- [ ] CPU benchmarks
- [ ] allocation benchmarks

### Robustness

- [ ] no silent seek-to-zero
- [ ] decode errors observable
- [ ] production unwrap audit
- [ ] CPAL/device-loss cases tested

### Hardware

- [ ] native PCM verified
- [ ] unsupported native rate verified
- [ ] DSP fallback verified
- [ ] DoP verified
- [ ] native DSD verified (where hardware supports)

---

## 22. Related Documents

- `docs/spec_audio_core_v2.0.md` — базовая аудио архитектура
- `docs/spec_audio_settings_v3.0.md` — настройки аудио
- `docs/spec_visualizer_v5.1.md` — визуализация (требует обновления)
- `docs/spec_rt_audio_v1.0.md` — RT safety contract (to be created)
- `_DRAFTS_/code_review_3.md` — первоначальное code review
- `_DRAFTS_/code_review3.1.md` — детальный план refactor

---

## 23. Revision History

| Version | Date | Author | Changes                                 |
| ------- | ---- | ------ | --------------------------------------- |
| 4.0     | 2025 | —      | Initial spec based on code_review3.1.md |

---

**Принцип порядка приоритетов:**

**Не начинать с RT optimization.** Фундаментальная проблема находится **выше RT**:

```text
Decoder → f32  ← основная проблема
```

**Правильный порядок:**

```text
1. Signal semantics
2. Typed transport
3. Hardware policy
4. RT boundedness
5. DSD / DoP
6. UI / Visualizer
7. DSP validation
8. Hardware proof
```

**Цель:** signal-integrity refactor поверх существующего RT фундамента (A2.0), а не переписывание audio engine с нуля.
