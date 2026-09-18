Привет. Я сделал именно **жёсткий code review текущего `main`**, а не повторил предыдущий разбор документации. Смотрел `AGENTS.md`, `ROADMAP.md`, `Cargo.toml`, `audio/player.rs`, `audio/worker.rs`, `audio/output.rs`, `audio/decoder.rs`, `audio/visualizer.rs` и спецификацию V5.1/A2.0/A3.0. Текущая версия проекта — `0.3.0`; в roadmap уже заявлено RT-hardening: `0` аллокаций в callback, Producer/Consumer, seek-handshake, DoP и `ring_buffer_ms`. 

# Вердикт по архитектуре

**Архитектурно разделение UI / decoder / worker / RT callback сейчас хорошее и существенно лучше старой версии. Но я пока не назвал бы audio core "high-end bit-perfect".**

Моя оценка:

| Область                   |                                    Оценка |
| ------------------------- | ----------------------------------------: |
| RT callback isolation     |                                  **9/10** |
| Lock-free transport       |                                  **9/10** |
| RT allocation discipline  |                                  **9/10** |
| Worker/decoder separation |                                  **9/10** |
| Seek protocol             |                                  **8/10** |
| Error containment         |                                  **7/10** |
| Slint architecture        |                                  **8/10** |
| True bit-perfect PCM      |                                  **4/10** |
| DoP                       | **7/10**, hardware verification ещё нужна |
| Testability               |                                  **8/10** |
| High-end audio readiness  |                                  **7/10** |

Главный сюрприз code review:

> **Real-time архитектура уже довольно хорошая. Главная проблема теперь не RT thread, а семантика `bit-perfect`.**

---

# 1. Что сделано действительно хорошо

Текущая схема:

```text
                    UI
                     │
              control / commands
                     │
                     ▼
             PlaybackWorker
          ┌────────────────────┐
          │ Decoder            │
          │ Resampler          │
          │ staging buffer     │
          └─────────┬──────────┘
                    │
                 rtrb SPSC
                    │
                    ▼
              RtConsumer
                    │
                    ▼
              cpal callback
                    │
                    ▼
                   DAC
```

Это правильное направление.

`PlaybackWorker` владеет `Box<dyn AudioSource>` и `Resampler`, а callback получает только `RtConsumer`, ring и атомики. Сам код явно фиксирует инвариант: callback не должен видеть decoder, `Mutex`, allocation или logging. 

Это уже намного ближе к правильной audio-engine архитектуре, чем классический:

```text
cpal callback
    ↓
decoder.next()
    ↓
resampler
    ↓
Vec
    ↓
Mutex
```

который был бы категорически неприемлем.

---

# 2. Критический плюс: `rtrb` действительно используется правильно

`RtConsumer` владеет:

```rust
ring: rtrb::Consumer<f32>,
shared: Arc<RtShared>,
scratch: Vec<f32>,
```

`scratch` создаётся заранее:

```rust
scratch: vec![0.0f32; MAX_OUT_SAMPLES],
```

а `MAX_OUT_SAMPLES = 1 << 16`.

В callback новые buffers не создаются. `pull_f32()` пишет непосредственно в CPAL buffer, а integer callbacks используют заранее выделенный scratch. 

Это именно то, что хочется увидеть.

---

# 3. Хорошо сделан atomic control plane

`RtShared` не содержит `Mutex`.

Там:

```rust
AtomicU32 volume_bits
AtomicBool muted
AtomicBool playing
AtomicBool bit_perfect
AtomicU8 dither
AtomicU64 pos_frames
AtomicU64 seek_generation
AtomicU64 seek_target_frames
AtomicU64 seek_done_generation
AtomicBool eof
AtomicBool natural_end
```

То есть:

```text
UI ────────────────┐
                   │
                   ▼
               Atomics
                   ▲
                   │
Worker ────────────┘

RT callback ───────► read-only/control atomics
```

Это правильная модель. 

Особенно хорошо, что `f32` volume не хранится через `Atomic<f32>`, а кодирует bits:

```rust
AtomicU32
f32::to_bits()
f32::from_bits()
```

и callback делает всего один relaxed load.

---

# 4. Seek handshake — хорошая работа

Вот эта часть уже выглядит серьёзно:

```text
UI
 │
 │ begin_seek(target)
 ▼
seek_generation++
 │
 ▼
WorkerCmd::Seek
 │
 ▼
worker:
 decoder.seek()
 resampler.reset()
 publish_seek_done()
 │
 ▼
callback:
 reconcile_seek()
 drain stale ring
 rebase position
```

Особенно важна пара:

```rust
store(..., Ordering::Release)
```

и

```rust
load(..., Ordering::Acquire)
```

для `seek_done_generation`.

Это правильнее, чем просто гонять несколько relaxed atomic и надеяться, что всё «как-нибудь увидится». 

---

# 5. Но теперь самое важное: `bit-perfect` здесь НЕ bit-perfect

Вот это **блокирующая проблема для high-end режима**.

`Decoder` принципиально декодирует всё в:

```rust
Option<&[f32]>
```

и Symphonia buffer копируется:

```rust
self.scratch.resize(total, 0.0);
buf.copy_to_slice_interleaved(&mut self.scratch);
```

То есть исходный integer PCM превращается в `f32` ещё до audio output. 

Затем для I16:

```rust
let mut x = src.clamp(-1.0, 1.0) * vol * 32767.0;
*dst = x.round().clamp(-32768.0, 32767.0) as i16;
```

Даже когда:

```rust
bit_perfect == true
```

volume просто становится:

```rust
vol = 1.0
```

Но преобразование:

```text
integer PCM
    ↓
f32
    ↓
f32 * 32767
    ↓
round
    ↓
i16
```

**не гарантирует identity transform.** 

---

## Конкретный пример

Для signed 16-bit PCM:

```text
original:
-32768

normalised f32:
-1.0

current output:
-1.0 * 32767 = -32767
```

То есть:

```text
0x8000 → 0x8001
```

Это уже не bit-perfect.

И это не теоретическая придирка. Это буквально изменение sample value.

---

# 6. Ещё хуже для 24-bit / I32

Для I32 сейчас:

```rust
src * 2_147_483_647.0
```

и потом:

```rust
round() as i32
```

Та же проблема.

А для 24-bit source → I32:

```text
24-bit integer
   ↓
f32
   ↓
scale
   ↓
i32
```

может быть lossless для самих 24-bit значений **при правильном диапазоне**, но текущая универсальная f32-нормализация всё равно делает architecture unnecessarily lossy.

Для 32-bit PCM уже тем более нельзя считать f32 transport гарантированно bit-exact для всех integer values: `f32` имеет 24 effective bits of precision.

---

# 7. DoP — ещё один особый случай

DoP callback:

```rust
*dst = ((src.clamp(0.0, 16_777_215.0) as u32) << 8) as i32;
```

То есть здесь f32 используется фактически как:

```text
f32 = integer container
```

для 24-bit DoP word. 

Для 24-bit integer диапазона это потенциально приемлемо, потому что все значения `0..2^24-1` точно представимы в f32.

Но архитектурно это всё равно неправильная abstraction boundary.

DoP — это **битовый транспорт**, поэтому лучше:

```rust
RingBuffer<u32>
```

или специализированный:

```rust
enum AudioPayload {
    PcmF32(f32),
    PcmI32(i32),
    Dop(u32),
}
```

а не:

```rust
RingBuffer<f32>
```

с условием:

> «в некоторых режимах f32 на самом деле integer».

---

# 8. Как я бы исправил bit-perfect

Я бы разделил pipeline на два режима.

### Normal

```text
decoder
 ↓
f32
 ↓
resampler
 ↓
volume
 ↓
dither
 ↓
device
```

### Bit-perfect PCM

```text
decoder
 ↓
native integer samples
 ↓
NO volume
 ↓
NO DSP
 ↓
NO resampler
 ↓
native device format
```

То есть тип данных должен быть частью режима.

Например:

```rust
pub enum SampleBuffer {
    F32(AudioBlock<f32>),
    I32(AudioBlock<i32>),
    I24(AudioBlock<I24>),
}
```

Но ещё лучше — не тащить enum через hot loop, а выбрать typed pipeline при открытии stream.

---

# 9. Рефакторинг №1

### Было

```rust
pub trait AudioSource: Send {
    fn next_frames(&mut self) -> Option<&[f32]>;
}
```

и:

```rust
let mut x = src.clamp(-1.0, 1.0) * 32767.0;
*dst = x.round().clamp(-32768.0, 32767.0) as i16;
```

### Стало

```rust
pub trait AudioSource: Send {
    type Sample;

    fn next_frames(&mut self) -> Option<&[Self::Sample]>;
}
```

И для bit-perfect PCM:

```rust
pub struct I16Source {
    // decoder state
}

impl AudioSource for I16Source {
    type Sample = i16;

    #[inline]
    fn next_frames(&mut self) -> Option<&[i16]> {
        self.decode_next_native()
    }
}
```

А callback:

```rust
pub fn audio_callback_i16_bitperfect(
    consumer: &mut RtConsumerI16,
    data: &mut [i16],
) {
    if !consumer.shared().is_playing() {
        data.fill(0);
        return;
    }

    if !consumer.reconcile_seek() {
        data.fill(0);
        return;
    }

    let produced = consumer.pull(data);

    data[produced..].fill(0);

    if produced < data.len() {
        finish_if_eof(consumer, produced, data.len());
    }
}
```

Это уже действительно:

```text
sample → sample
```

без математического round-trip.

---

# 10. Вторая серьёзная проблема — `bit-perfect` определяется слишком высокоуровнево

Сейчас `RtShared` считает:

```rust
bit_perfect == true
```

и отдельно:

```rust
resampler_enabled == false
```

есть статус:

```rust
bit_perfect_resampled()
```

Но настоящий bit-perfect contract должен учитывать **весь chain**:

```text
source format
source bit depth
decoder conversion
channel conversion
resampler
volume
mute
dither
sample-format conversion
device backend
exclusive/shared
server node
DoP/native
```

Именно поэтому `A3.0` уже пытается классифицировать:

```text
BitPerfect
Degraded
Unsupported
```

Это хорошее направление. 

Но runtime должен использовать тот же formal result.

Я бы ввёл:

```rust
pub enum SignalIntegrity {
    BitPerfect,
    Converted {
        reasons: SmallVec<[ConversionReason; 4]>,
    },
}
```

и **единственный источник истины** для badge + runtime policy.

---

# 11. Сейчас есть опасная семантическая ловушка

В `open_pcm()`:

```rust
let spec = select_output_for(&req)?;
```

а затем:

```rust
let resampler =
    Resampler::with_algo(src_rate, out_rate, ...);
```

и `RtShared` получает:

```rust
resampler.is_enabled()
```

Если пользователь включил `bit_perfect`, но backend не способен работать на native rate, код допускает поток с resampling и лишь пишет warning:

```text
device does not support native rate ... resampling ...
under Bit-perfect mode
```

Это прямо видно в `player.rs`. 

Для настоящего **strict bit-perfect mode** это неправильная семантика.

Если пользователь говорит:

```text
bit_perfect = true
```

варианты должны быть:

```text
A. native exact → play
B. impossible → don't play
```

а не:

```text
C. resample + warning
```

Иначе название настройки вводит в заблуждение.

---

# 12. Ещё одна важная проблема: channel conversion ломает bit-perfect

`Resampler::mix_rel()` умеет:

```rust
out_ch == 1
```

и делает:

```rust
sum / src_ch
```

или:

```rust
src_ch == 1 → duplicate
```

То есть:

```text
stereo → mono
```

и:

```text
mono → stereo
```

считаются частью output negotiation. 

Но channel remix — это DSP/conversion.

Следовательно:

```text
bit-perfect + channels mismatch
```

должно быть:

```text
Unsupported
```

а не:

```text
BitPerfect = true
```

Если политика действительно требует raw PCM.

---

# 13. Resampler: архитектурно хорошо, но есть high-end вопрос

`Resampler` заранее резервирует:

```rust
MAX_BUFFERED_FRAMES = 8192
```

и sinc window создаётся при construction. Это хорошо: allocation не происходит во время обработки. 

Но есть проблема другого уровня:

```text
f64 pos
+
windowed sinc
+
manual implementation
```

для high-end audio я бы не доверял качеству только потому, что есть `sinc_medium/slow`.

Нужно обязательно иметь объективные tests:

```text
passband ripple
stopband attenuation
THD+N
alias rejection
impulse response
frequency response
```

Для:

```text
44.1 → 48
44.1 → 96
96 → 44.1
192 → 44.1
352.8 → 44.1
```

и отдельно:

```text
88.2 ↔ 176.4
96 ↔ 192
```

Сейчас unit tests проверяют в основном selection/policy, а не **DSP quality**. Это серьёзный пробел именно для high-end аудио.

---

# 14. Ring buffer: хороший, но есть latency trade-off

Сейчас capacity рассчитывается примерно так:

```rust
samples =
    out_rate * channels * ring_buffer_ms / 1000
```

и минимум:

```rust
max(
    requested_depth,
    2 * callback_period,
    4096
)
```



Это разумно для drop-out resistance.

Но:

> **ring buffer — это не бесплатно.**

Если `ring_buffer_ms = 200`, то система может иметь:

```text
decoder → ring → DAC
```

с потенциально огромной latency.

Для playback это терпимо.

Для:

* pause;
* seek;
* stop;
* next;
* device switch

уже появляется perceptible delay.

Я бы рекомендовал разделить:

```text
decode-ahead
```

и

```text
RT safety buffer
```

и измерять:

```text
actual buffered frames
```

в runtime.

---

# 15. Самая неприятная потенциальная проблема RT: `reconcile_seek()`

В callback:

```rust
while self.ring.pop().is_ok() {}
```

Это lock-free, allocation-free.

Но это **потенциально O(N) работа внутри RT callback**.

Если ring большой и произошёл seek:

```text
callback
 ↓
drain 100k samples
 ↓
callback budget exhausted
 ↓
xruns
```

Это не allocation проблема.

Это **RT latency problem**.

В идеале seek должен инвалидировать ring generation, а не физически вычитывать всё в callback.

---

# 16. Я бы изменил seek architecture

Сейчас:

```rust
while self.ring.pop().is_ok() {}
```

Лучше сделать:

```text
producer generation
consumer generation
```

или использовать resettable SPSC queue / epoch.

Например:

```rust
struct RingEpoch {
    generation: AtomicU64,
}
```

После seek:

```text
UI:
generation++

worker:
flush producer state
publish ready

consumer:
if generation != local_generation:
    output silence
    swap/reset ring
```

Тогда callback:

```rust
if generation != local_generation {
    data.fill(0);
    return;
}
```

и:

```text
O(1)
```

вместо:

```text
O(buffer_size)
```

---

# 17. Visualizer tap сделан лучше, чем ожидалось

Есть важная деталь.

Спецификация говорит про tap рядом с callback, но реализация фактически делает:

```text
Decoder
 ↓
Resampler
 ↓
staging
 ├──────────────→ VizTap
 ↓
Playback ring
 ↓
CPAL callback
```

То есть tap выполняется **в worker**, а не в RT callback. 

С точки зрения RT safety это даже лучше:

```text
RT callback
    │
    └── ничего лишнего
```

А visualizer получает post-resampler PCM.

Это хороший trade-off.

Но документацию надо привести в соответствие с фактической архитектурой.

Сейчас V5.1 всё ещё описывает:

> `audio_callback_*` дополнительно пишет ... в lock-free ring.

а код этого уже не делает. Спека говорит одно, implementation — другое. 

Для agent-driven проекта это **traceability defect**.

---

# 18. VizTap использует `Mutex`, но это не RT bug

```rust
pub type VizTap =
    Arc<Mutex<Option<rtrb::Producer<f32>>>>;
```

и:

```rust
if let Ok(mut guard) = viz_tap.lock() {
```

Это допустимо, потому что lock находится в `PlaybackWorker`, а не CPAL callback. 

Но я бы всё равно убрал `Mutex`.

Причина не RT, а архитектурная:

```text
Worker
   ↓
Mutex
   ↓
optional Producer
```

создаёт ненужную синхронизацию на каждом chunk.

Лучше:

```rust
Option<rtrb::Producer<f32>>
```

владеть непосредственно worker'ом.

А включение/выключение:

```rust
AtomicBool
```

уже есть.

Тогда:

```rust
if shared.viz_tap_active() {
    if let Some(tap) = viz_tap.as_mut() {
        ...
    }
}
```

без mutex.

---

# 19. Это особенно важно потому, что worker и так single-owner

`PlaybackWorker` владеет:

```rust
producer
source
resampler
staging
```

Поэтому `VizTap` логически должен быть частью этого ownership model.

Сейчас `Player` держит:

```rust
Arc<Mutex<Option<Producer>>>
```

только потому, что producer нельзя clone'ить.

Это решает проблему, но немного нарушает красивую ownership-модель.

---

# 20. Ошибки: стало лучше, но `unwrap()` всё ещё есть

Production audio code в основном аккуратен.

Но repository policy говорит:

> исключить `unwrap()`

при этом production code содержит как минимум безопасные по контексту места, а tests активно используют `unwrap/expect`. Например decoder tests. 

Это не blocker само по себе.

Но я бы ввёл policy:

```text
production:
    unwrap/expect = forbidden

tests:
    unwrap/expect = allowed
```

И явно закрепил это в `clippy.toml`/CI.

---

# 21. `Decoder::seek()` имеет questionable fallback

Вот:

```rust
let time = Time::try_new(whole, nanos).unwrap_or(Time::ZERO);
```

Это production code.

Да, практически диапазоны там нормальные.

Но:

```rust
try_new(...)
    .unwrap_or(Time::ZERO)
```

превращает потенциальную ошибку времени seek в:

```text
seek to 0
```

Это очень плохая failure semantic.

Пользователь запросил:

```text
seek 3:42
```

а если что-то не так:

```text
seek 0:00
```

без ошибки.

Я бы сделал:

```rust
let time = Time::try_new(whole, nanos)
    .map_err(|e| format!("Invalid seek position: {e}"))?;
```

Это намного безопаснее.

---

# 22. Ещё одна логическая проблема Decoder

При:

```rust
Err(SymError::DecodeError(_)) => continue,
```

код **молча пропускает decode errors**. 

Для damaged FLAC/MP3:

```text
packet 1 OK
packet 2 ERROR → silently skip
packet 3 OK
```

получится audio gap.

Это может быть сознательно.

Но high-end player должен различать:

```text
recoverable decode error
```

и:

```text
corrupted stream
```

и как минимум иметь diagnostic counter:

```rust
decode_errors: AtomicU32
```

или worker-side diagnostic.

Не нужно ломать playback из-за одного bad packet, но пользователь/лог должны знать.

---

# 23. UI architecture: концептуально правильная

Спека очень чётко требует:

```text
UI
  ↓ commands
workers
  ↓ data
UI
```

и запрещает FFT/decode в Slint. 

Это правильная архитектура.

`invoke_from_event_loop` — нормальный механизм для возврата worker results в Slint event loop; это также рекомендуемый способ в документации/обсуждениях Slint. ([GitHub][1])

Но здесь я бы очень внимательно следил за **частотой сообщений worker → UI**.

Если LiveWorker делает:

```text
60 FPS
 ×
invoke_from_event_loop
```

это нормально.

Если он делает:

```text
audio chunk = 1024 frames
44,100 / 1024 ≈ 43 messages/sec
```

тоже нормально.

Но если после каждой FFT операции отправляется отдельное UI update, можно получить:

```text
worker
 → event queue
 → event queue
 → event queue
 → ...
```

Лучше иметь:

```text
latest-value semantics
```

для spectrum data.

---

# 24. Для spectrum я бы не отправлял каждый frame

Лучше:

```text
LiveWorker
    ↓
atomic sequence
    ↓
latest spectrum buffer
    ↓
UI timer 30–60 FPS
    ↓
read latest frame
```

То есть:

```text
worker rate: 100–200 Hz
UI rate:     30–60 Hz
```

UI берёт только последний frame.

Это сильно лучше event queue:

```text
worker → 100 invoke_from_event_loop/sec
```

потому что старые spectrum frames бессмысленны.

---

# 25. Особенно важно для Slint Properties

Если делать:

```rust
ui.set_spectrum(...)
```

каждый раз, когда приходит FFT:

```text
FFT 120 Hz
↓
Slint property invalidation 120 Hz
↓
rendering
```

это лишняя работа.

Лучше:

```text
audio worker
    ↓
latest snapshot
    ↓
Slint Timer 30/60 Hz
    ↓
if sequence != last_sequence:
    property.set(...)
```

Таким образом UI становится consumer с rate limiting.

---

# 26. Кэширование visualizer — правильная идея, но есть опасность

Спека задаёт:

```text
RAM cache = 64 MiB
disk cache = 512 MiB
```

и `VisualizerSettings` уже содержит эти параметры. 

Это хорошо.

Но cache key обязательно должен включать:

```text
file identity
+
visualizer mode
+
dimensions
+
FFT size
+
window
+
freq range
+
gain
+
palette
+
channel mode
+
DSD conversion parameters
```

Особенно хорошо, что `DsdCacheParams` уже выделены отдельно и включаются в configuration snapshot. 

Я бы дополнительно включил:

```text
decoder/version schema
visualizer algorithm version
```

Например:

```rust
cache_key = sha256(
    "viz-v3" ||
    file_identity ||
    settings_hash
)
```

Иначе изменение FFT implementation может оставить старый визуальный cache.

---

# 27. Архитектурный долг: `f32` сейчас слишком глубоко проник в систему

Это, пожалуй, главный structural issue.

Сейчас:

```text
Decoder → f32
Resampler → f32
Ring → f32
VizTap → f32
Consumer → f32
```

То есть вся система implicitly говорит:

> «аудио = f32».

Для обычного music player это нормально.

Для **high-end audio / bit-perfect / DSD / DoP** — нет.

Я бы разделил:

```text
Audio representation
```

и:

```text
DSP representation
```

---

# 28. Правильная архитектура для high-end

Я бы сделал:

```text
                    ┌───────────────┐
                    │ Source PCM    │
                    │ native format │
                    └───────┬───────┘
                            │
                 ┌──────────┴──────────┐
                 │                     │
            bit-perfect             DSP path
                 │                     │
                 │                 f32/f64
                 │                     │
                 │                resampler
                 │                     │
                 │                  volume
                 │                     │
                 └──────────┬──────────┘
                            │
                     Device format
                            │
                         CPAL
```

То есть `f32` — **DSP domain**, а не универсальный transport format.

---

# 29. DSD pipeline тоже стоит разделить

Сейчас:

```text
DSD
 ↓
DsdDecoder
 ↓
f32-ish PCM representation
```

и дальше возможен:

```text
DoP packing
```

Я бы делал:

```text
DSD
 │
 ├── Native
 │      ↓
 │   DSD transport
 │
 ├── DoP
 │      ↓
 │   DoP frame transport
 │
 └── PCM
        ↓
      CIC
        ↓
      DSP PCM
```

Не надо прогонять native/DoP через общий PCM abstraction.

---

# 30. Что с `unsafe`

Пока я не вижу в просмотренных ключевых audio modules опасного `unsafe`, и это хорошо.

Особенно я бы **не добавлял unsafe ради производительности** на данном этапе.

В high-end audio Rust:

```text
safe Rust + lock-free
```

обычно уже даёт достаточно.

`unsafe` нужен только если появится:

```text
ALSA mmap
CoreAudio AudioUnit
ASIO FFI
SIMD intrinsics
```

и тогда каждый блок должен иметь отдельный safety contract.

---

# 31. Что я бы потребовал перед объявлением audio core production-grade

### Blocker 1

Исправить:

```text
bit-perfect != f32 round-trip
```

### Blocker 2

Strict bit-perfect должен **отказывать**, а не resample.

### Blocker 3

Channel conversion не должен называться bit-perfect.

### Blocker 4

DoP transport сделать integer/byte typed, а не f32 disguised integer.

### Blocker 5

Seek drain из callback сделать O(1).

---

# 32. High-End DSP: что добавить в тесты

Я бы добавил отдельный `audio-quality` test suite.

Например:

```text
tests/audio_quality/
    pcm_identity.rs
    resampler_response.rs
    dither.rs
    dop_framing.rs
    channel_mapping.rs
```

### PCM identity

```text
input bytes
     ↓
player
     ↓
output bytes
```

assert:

```rust
assert_eq!(input, output);
```

для:

* 16-bit;
* 24-bit;
* 32-bit.

При:

```text
bit_perfect = true
exclusive = true
native rate
same channels
```

Это будет **самый важный тест проекта**.

---

# 33. Ещё лучше — golden vectors

Например:

```text
fixtures/
  impulse_i16.raw
  impulse_i24.raw
  impulse_i32.raw
  sine_1khz_44k1.raw
  sine_997hz_96k.raw
  dop_dsd64.raw
```

и:

```text
golden/
  ...
```

Тогда CI может проверять:

```text
decoder
→ DSP
→ output
```

без реального DAC.

---

# 34. Resampler quality benchmark

Для каждого algorithm:

```text
linear
cubic
sinc_fast
sinc_medium
sinc_slow
```

нужны:

```text
passband error
stopband attenuation
alias level
THD+N
CPU ns/sample
```

Например:

```text
                    THD+N    Alias      CPU
linear              ...      ...        ...
cubic               ...      ...        ...
sinc_fast           ...      ...        ...
sinc_medium         ...      ...        ...
sinc_slow           ...      ...        ...
```

Вот это уже настоящий **High-End Audio engineering**, а не только API-level testing.

---

# 35. Что мне понравилось в roadmap

Текущий roadmap уже сам фиксирует важный факт:

```text
A2.0 = RT hardening completed
```

и:

```text
0 allocations
Producer/Consumer
seek handshake
DoP
ring_buffer_ms
```

Также есть конкретные реальные баги из эксплуатации:

```text
DoP framer panic
DoP layout
ADI-2 / PipeWire
device enumeration
```

Это очень хороший знак: roadmap уже начинает отражать **реальные hardware failures**, а не только теоретические требования. 

---

# 36. Но roadmap сейчас имеет одну проблему

Он утверждает:

> A2.0 выполнена.

А в code review я нахожу:

```text
bit-perfect semantics
```

которые ещё не полностью соответствуют high-end definition.

То есть:

```text
A2.0:
"RT-hardening"
```

— действительно практически выполнена.

Но:

```text
"bit-perfect"
```

нужно считать отдельным quality contract.

Я бы **не смешивал эти два понятия**.

---

# 37. Я бы добавил отдельную спецификацию

Например:

```text
docs/spec_audio_integrity_v1.0.md
```

с:

```text
AI1.1 PCM identity
AI1.2 Integer transport
AI1.3 Native format preservation
AI1.4 Channel identity
AI1.5 Sample-rate identity
AI1.6 Volume bypass
AI1.7 Dither bypass
AI1.8 DSP bypass
AI1.9 DoP identity
AI1.10 Native DSD identity
```

И acceptance criteria:

```text
[x] 16-bit PCM identity
[x] 24-bit PCM identity
[ ] 32-bit PCM identity
[ ] DoP golden vector
[ ] native DSD hardware validation
[ ] no hidden conversion
```

---

# 38. Рефакторинг №2 — сделать integrity explicit

### Было

```rust
let bit_perfect = consumer.shared().bit_perfect();

let vol = if bit_perfect {
    1.0
} else {
    consumer.shared().volume()
};
```

### Стало

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalPath {
    BitPerfectPcm,
    DspPcm,
    Dop,
    NativeDsd,
}
```

и stream configuration выбирает конкретный path:

```rust
match path {
    SignalPath::BitPerfectPcm => {
        output_native_pcm(consumer, data);
    }

    SignalPath::DspPcm => {
        output_dsp_pcm(consumer, data);
    }

    SignalPath::Dop => {
        output_dop(consumer, data);
    }

    SignalPath::NativeDsd => {
        output_native_dsd(consumer, data);
    }
}
```

Тогда невозможно случайно сделать:

```text
bit_perfect = true
+
volume multiplication
```

потому что самого понятия `volume` внутри `BitPerfectPcm` path нет.

---

# 39. Итог по четырём вашим критериям

## 1. Баги / логические ошибки

**7/10**

Найдены реальные риски:

* false bit-perfect semantics;
* f32 integer round-trip;
* channel conversion;
* resampling fallback в supposedly bit-perfect mode;
* O(N) seek drain в RT callback;
* silent decoder errors;
* seek `unwrap_or(Time::ZERO)`.

DoP-путь уже явно проходил реальные bug fixes, и roadmap показывает несколько исправленных критических проблем. 

---

## 2. Производительность

**8.5/10**

Очень хорошо:

* SPSC `rtrb`;
* preallocated scratch;
* worker decoding;
* prebuilt sinc window;
* no allocation callback;
* no decoder in callback;
* no mutex in callback.

Главные оставшиеся проблемы:

* seek drain;
* f32 conversion;
* возможная частота UI updates;
* Mutex на VizTap;
* resampler CPU profile пока не подтверждён объективными DSP benchmarks.

---

## 3. Архитектура / читаемость

**8.5/10**

Сейчас код уже имеет хорошее разделение:

```text
decoder.rs
worker.rs
output.rs
player.rs
visualizer.rs
settings
```

и ownership достаточно хорошо выражен.

Главный архитектурный долг:

> `f32` используется как универсальный audio transport.

Для обычного player — отлично.

Для high-end player — нужно исправлять.

---

## 4. Тестируемость

**8.5/10**

Здесь проект сильно вырос.

Особенно хорошо:

* `TestHooks`;
* unit tests для output policy;
* seek tests;
* ring tests;
* allocation tracking;
* settings round-trip;
* device selection tests.

Например, уже есть тесты на `Native + bit_perfect + rate mismatch`, DoP capability и fallback policies. 

Но отсутствует самое главное:

> **end-to-end sample identity test.**

---

# Финальный вердикт

Если смотреть именно как **Senior Rust / High-End Audio review**, я бы сейчас сказал:

> **Audio architecture — хорошая. Real-time discipline — хорошая. Bit-perfect implementation — пока нет.**

То есть я бы **не переписывал RT engine**.

Наоборот — текущий:

```text
Worker
  ↓
rtrb
  ↓
RtConsumer
  ↓
CPAL callback
```

стоит сохранить.

Я бы сделал следующий большой refactoring **не вокруг потоков, а вокруг representation of audio samples**:

```text
                         AudioSource
                              │
             ┌────────────────┼─────────────────┐
             │                │                 │
          PCM int           PCM DSP            DSD
             │                │                 │
             │              f32/f64        ┌────┴────┐
             │                │            │         │
             │             resampler     Native     DoP
             │                │            │         │
             └────────────┬───┘            │         │
                          │                │         │
                     OutputPipeline ───────┴─────────┘
                          │
                     typed transport
                          │
                         CPAL
```

И **первым новым acceptance test** сделал бы:

```text
FLAC/WAV 16-bit
44.1 kHz
stereo
bit-perfect=true
exclusive=true
device=44.1 kHz native

decode → output

assert(output_bytes == source_bytes)
```

Пока этот тест не проходит, я бы не называл режим `bit-perfect`, даже если `resampler=false`, `volume=1.0` и ALSA `hw:` выбраны правильно.

Это, на мой взгляд, сейчас **самая важная техническая точка всего проекта**.

[1]: https://github.com/slint-ui/slint/discussions/4377?utm_source=chatgpt.com "Slint + async Rust = ? · slint-ui slint · Discussion #4377 · GitHub"
