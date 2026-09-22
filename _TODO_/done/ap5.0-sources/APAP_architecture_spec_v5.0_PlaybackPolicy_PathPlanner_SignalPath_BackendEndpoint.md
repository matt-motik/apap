# APAP --- Архитектурная спецификация v5.0

## PlaybackPolicy → PathPlanner → SignalPath → BackendEndpoint

**Дата:** 2026-09-22\
**Репозиторий:** `matt-motik/apap`\
**Ветка:** `main`\
**Назначение:** agent-ready спецификация следующего этапа рефакторинга
audio pipeline.

------------------------------------------------------------------------

## 1. Архитектурный контракт

Целевая цепочка строго фиксируется:

``` text
PlaybackPolicy
      ↓
PathPlanner
      ↓
PathPlan / SignalPath
      ↓
BackendEndpoint
      ↓
RT Audio Engine
```

При этом входами Planner являются `SourceFormat` и `DeviceCapabilities`.

Главное правило:

> `PlaybackPolicy` описывает намерение пользователя. `PathPlanner`
> принимает решение. `SignalPath` описывает фактически выбранную
> обработку сигнала. `BackendEndpoint` описывает способ доступа к
> устройству.

`Bit-Perfect` --- свойство конкретного `SignalPath`, а не глобальный
режим плеера.

Технические integrity-инварианты из предыдущей спецификации сохраняются,
но становятся нижним уровнем архитектуры.

------------------------------------------------------------------------

# 2. PlaybackPolicy

``` rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackPolicy {
    Shared,
    BestPossible,
    Strict,
}
```

### Shared --- «Играем всё и всегда вместе с другими программами»

-   Shared/non-exclusive access.
-   На Linux: PipeWire/default endpoint.
-   Raw `hw:*` запрещён.
-   Допускается system mixer/audio-server conversion.
-   Цель --- непрерывность и совместимость.
-   DAC-level Bit-Perfect не гарантируется.

### BestPossible --- «Стремимся к лучшему из возможного»

-   Exclusive access.
-   Сначала exact/native path.
-   Если source format аппаратно недоступен --- разрешён качественный
    SRC/format conversion.
-   Трек продолжает играть.
-   Conversion означает `DspPcm`, а не Bit-Perfect.

Пример: `24/192 → high-quality SRC → 24/96 → Exclusive`.

### Strict --- «Только лучшее»

-   Exclusive access.
-   Нет SRC, sample-format conversion, channel conversion и DSD→PCM.
-   Exact/native path обязателен.
-   Несовместимый трек получает `TrackRejection` и пропускается.
-   Если все треки rejected --- агрегированное сообщение, а не
    бесконечный retry.

Ключевой инвариант:

> `TrackRejected` не равен `PlaylistFailure`.

------------------------------------------------------------------------

# 3. BackendAccess и BackendEndpoint

``` rust
pub enum BackendAccess {
    Shared,
    Exclusive,
}
```

Соответствие policy:

``` text
Shared       → Shared
BestPossible → Exclusive
Strict       → Exclusive
```

Endpoint --- отдельный слой:

``` rust
pub enum BackendEndpoint {
    Shared(SharedEndpoint),
    Exclusive(ExclusiveEndpoint),
}
```

На Linux:

``` text
Shared     → PipeWire / pcm.pipewire / PIPEWIRE_NODE
Exclusive  → raw ALSA hw:CARD=...,DEV=...
```

`Shared` **никогда** не должен вызывать `snd_pcm_open("hw:*")`.

------------------------------------------------------------------------

# 4. PhysicalDevice

Один физический DAC может иметь несколько representations:

``` rust
pub struct PhysicalDevice {
    pub id: PhysicalDeviceId,
    pub name: String,
    pub shared: Option<SharedEndpoint>,
    pub exclusive: Option<ExclusiveEndpoint>,
    pub capabilities: DeviceCapabilities,
}
```

Нельзя использовать правило «среди одинаковых имён raw `hw:*` всегда
выигрывает».

Это напрямую связано с подтверждённой Xonar DX/PipeWire regression:
открытие raw ALSA отнимало устройство у PipeWire, node исчезал из graph
и возвращался только после WirePlumber restart.

------------------------------------------------------------------------

# 5. SignalPath

``` rust
pub enum SignalPath {
    BitPerfectPcm(PcmFormat),
    DspPcm(PcmFormat),
    NativeDsd(DsdFormat),
    DoP(DopFormat),
}
```

## BitPerfectPcm

Разрешён только если сохраняются:

-   sample rate;
-   sample format;
-   channels/layout;
-   sample representation;
-   нет SRC;
-   нет DSP;
-   нет volume scaling;
-   нет dither;
-   нет f32 roundtrip.

## DspPcm

Любой путь с SRC, volume processing, channel conversion, DSD→PCM или
иным преобразованием.

DSD→PCM **никогда** не маркируется Bit-Perfect.

## NativeDsd

Нативная передача DSD без PCM conversion.

## DoP

DSD-over-PCM. DoP marker/packing должны быть typed и принадлежать
`DopFormat`, а не `is_dop: bool`.

------------------------------------------------------------------------

# 6. PathPlan

``` rust
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

`PathPlan` создаётся до запуска RT stream и после создания не должен
динамически менять критические свойства.

------------------------------------------------------------------------

# 7. PathPlanner

Новый модуль:

``` text
src/audio/planner.rs
```

Целевой API:

``` rust
pub fn plan(
    source: &SourceFormat,
    device: &DeviceCapabilities,
    policy: PlaybackPolicy,
) -> Result<PathPlan, TrackRejection>;
```

Planner:

-   не открывает устройство;
-   не вызывает ALSA/CPAL stream creation;
-   не делает DSP;
-   не меняет UI;
-   не принимает RT decisions.

Он отвечает только на вопрос:

> «Какой путь допустим для этого source + device + policy?»

### Shared

``` text
Shared
 → SharedEndpoint
 → совместимый player-side path
 → PipeWire
```

### BestPossible

``` text
BestPossible
 → exact/native, если возможно
 → иначе best supported output + conversion
 → PLAY
```

### Strict

``` text
Strict
 → exact/native
 → иначе TrackRejection
```

------------------------------------------------------------------------

# 8. ConversionPlan

``` rust
enum ConversionPlan {
    None,
    Resample(ResamplerConfig),
    ConvertPcm(PcmConversion),
    DsdToPcm(DsdPcmConfig),
}
```

`BitPerfectPcm` совместим только с `ConversionPlan::None`.

Следовательно, состояния вроде:

``` text
bit_perfect = true + resampler_enabled = true
```

не должны существовать как валидная модель.

Resampler является частью `DspPcm`, а не независимым глобальным
переключателем.

------------------------------------------------------------------------

# 9. TrackRejection

Минимально:

``` rust
enum TrackRejection {
    SampleRateUnsupported,
    SampleFormatUnsupported,
    ChannelsUnsupported,
    ExclusiveUnavailable,
    NativeDsdUnavailable,
    DopUnavailable,
    RequiresConversion,
    NoValidSignalPath,
}
```

Конкретные варианты можно расширить данными (`requested`, `supported`,
`reason`).

Обязательно различать:

``` text
TrackRejected
BackendError
DecoderError
DeviceError
```

------------------------------------------------------------------------

# 10. DSD/DoP policy

### Shared

Если Shared endpoint не предоставляет native DSD/DoP:

``` text
DSD → PCM → PipeWire
```

Это `SignalPath::DspPcm`.

### BestPossible

При наличии capabilities предпочтительный порядок:

``` text
Native DSD > DoP > high-quality DSD→PCM
```

### Strict

Разрешены только:

``` text
NativeDsd
DoP
```

DSD→PCM запрещён.

------------------------------------------------------------------------

# 11. Typed PCM transport --- P0

Текущий проект использует в `src/audio/worker.rs`:

``` rust
RingBuffer<f32>
Consumer<f32>
scratch: Vec<f32>
```

а `src/audio/player.rs` переводит `f32` обратно в `i16/i32` через
scaling/rounding.

Это блокирует доказуемый Strict Bit-Perfect PCM path.

Целевой принцип:

``` text
BitPerfect:
source integer → typed integer transport → typed output

DSP:
source → f32 DSP → f32 transport/output as required
```

Возможный тип:

``` rust
enum PcmBlock {
    I16(...),
    I24(...),
    I32(...),
}
```

или generic equivalent.

Тест Strict должен сравнивать sample bytes, а не approximate f32 values.

------------------------------------------------------------------------

# 12. Volume, dither и RT

Volume scaling не является Bit-Perfect operation. Для `BitPerfectPcm`
unity-only; при необходимости обработки путь становится `DspPcm`, а
Strict должен reject такой path.

Dither относится к DSP/quantization path и запрещён для `BitPerfectPcm`,
`NativeDsd` и `DoP`.

RT callback не должен:

-   allocate/deallocate;
-   lock Mutex;
-   делать filesystem/network I/O;
-   decode;
-   создавать/resample;
-   log;
-   создавать String;
-   clone/drop `Arc` внутри callback;
-   менять stream configuration;
-   принимать policy decisions.

Callback исполняет уже выбранный `PathPlan`/`StreamContract`.

------------------------------------------------------------------------

# 13. Сопоставление с текущими файлами

## `src/audio/output.rs`

**Сейчас:** device discovery, `DeviceInfo`, output selection, fallback,
exclusive semantics, resampler, output config, DoP-related state.

**Рефакторинг:**

-   вынести policy → `policy.rs`;
-   capabilities → `capabilities.rs`;
-   signal path → `signal_path.rs`;
-   endpoint model → `endpoint.rs`;
-   path decision → `planner.rs`;
-   оставить actual stream creation/backend lifecycle/CPAL callback
    binding.

**Критично:** удалить семантику `collapse_same_name()`, где raw `hw:*`
побеждает среди одинаковых имён.

Resampler оставить как algorithm/runtime implementation, но его выбор
должен приходить из `PathPlan`.

------------------------------------------------------------------------

## `src/audio/player.rs`

**Сейчас:** Player управляет decoder/worker/output/volume и содержит
callback conversions и `bit_perfect` runtime state.

**Рефакторинг:**

-   Player получает готовый `PathPlan`/`StreamContract`;
-   не решает Shared/Exclusive/SRC/Strict самостоятельно;
-   убрать `bit_perfect: AtomicBool` как источник истины;
-   убрать f32→integer conversion из Strict path;
-   callback dispatch определяется typed `SignalPath`.

------------------------------------------------------------------------

## `src/audio/worker.rs`

**Сейчас:** `rtrb::RingBuffer<f32>` и `RtConsumer<f32>`.

**Рефакторинг:**

-   сохранить worker + SPSC/rtrb;
-   сделать transport typed для integer Bit-Perfect;
-   оставить f32 для DSP path;
-   visualizer сделать best-effort и никогда не позволять ему
    блокировать audio producer.

------------------------------------------------------------------------

## `src/audio/decoder.rs`

**Сейчас:** `AudioSource::next_frames()` отдаёт `&[f32]`, из-за чего f32
становится универсальным representation.

**Рефакторинг:** разделить source representation и DSP representation.
Decoder должен сохранять sample rate/channels/bit depth/PCM-vs-DSD и
предоставлять representation, достаточный для Strict integer path.

Если конкретный decoder может выдать только f32, это должно быть явным
ограничением path, а не скрытой конверсией с последующим Bit-Perfect
claim.

------------------------------------------------------------------------

## `src/audio/dsd.rs`

DSD→PCM явно соответствует `SignalPath::DspPcm`.

Raw DSD → `NativeDsd`.

DoP preparation → `DoP`.

`dsd.rs` не принимает пользовательскую policy --- policy принадлежит
Planner.

------------------------------------------------------------------------

## `src/audio/dop.rs`

Оставить marker generation, byte packing и validation. Результат должен
быть typed `DopFormat`; убрать архитектурную зависимость от
`is_dop: bool`.

------------------------------------------------------------------------

## `src/audio/mod.rs`

Добавить:

``` rust
pub mod policy;
pub mod planner;
pub mod signal_path;
pub mod capabilities;
pub mod endpoint;
```

Целевая структура:

``` text
src/audio/
├── policy.rs
├── planner.rs
├── signal_path.rs
├── capabilities.rs
├── endpoint.rs
├── output.rs
├── worker.rs
├── player.rs
├── decoder.rs
├── dsd.rs
├── dop.rs
└── resampler.rs
```

------------------------------------------------------------------------

## `src/app/playback_manager.rs`

Это playlist/application-level consumer нового Planner.

Текущая семантика `open() -> Err → status + stop()` должна быть
разделена.

Целевое поведение:

``` text
Plan/Open
 ├─ Started → play
 ├─ TrackRejected → record reason → next eligible track
 └─ Fatal error → report → stop/recover
```

Strict:

``` text
A rejected → skip
B playable → play
C rejected → skip
D playable → play
```

Не использовать бесконечную рекурсию для skip; ограничивать число
кандидатов размером playlist/итеративно искать следующий eligible track.

При полном reject:

``` text
No playable tracks under Strict policy
```

------------------------------------------------------------------------

## `src/settings.rs`

Добавить единый сериализуемый `playback_policy`.

Старые `bit_perfect`, `exclusive`, `fallback` не должны остаться вторым
источником истины.

Нужна migration старого TOML в новую policy model.

Параметры resampler/dither остаются DSP parameters, но не определяют
policy.

`audio_device` должен идентифицировать physical device, а не заставлять
все режимы открывать один конкретный `hw:*` endpoint.

------------------------------------------------------------------------

## UI / `src/app/ui_manager.rs` и связанные UI updates

UI не должен вычислять Bit-Perfect из boolean комбинаций.

Нужен готовый snapshot:

``` rust
pub struct PlaybackStatus {
    pub policy: PlaybackPolicy,
    pub access: BackendAccess,
    pub signal_path: SignalPath,
    pub source_format: SourceFormat,
    pub output_format: OutputFormat,
    pub conversion: Option<ConversionInfo>,
}
```

Отображать:

``` text
Policy: Shared / Best Possible / Strict
Actual path: Shared / Bit-Perfect PCM / DSP / Native DSD / DoP
Backend: PipeWire / ALSA Exclusive / ...
Conversion: None / 192→96 / DSD→PCM / ...
```

Не отображать просто `Bit-Perfect: ON`.

------------------------------------------------------------------------

## `src/main.rs`

Не является частью основного audio refactor. P2: заменить startup
`unwrap()` на контролируемое error propagation; это production
correctness, не RT blocker.

------------------------------------------------------------------------

## `src/app/fulltrack_manager.rs`, `src/app/playlist_manager.rs`

Сохранить их текущую специализацию. Проверить точки, где track
lifecycle/playlist advancement предполагают, что любая ошибка
`Player::open()` останавливает playback. Strict rejection должен
возвращаться на уровень `PlaybackManager`, не превращаясь в fatal
playlist error.

------------------------------------------------------------------------

# 14. Запрещённые комбинации

Архитектура не должна допускать:

``` text
Shared + raw hw:*
Strict + SRC
Strict + DSD→PCM
BitPerfectPcm + f32 roundtrip
BitPerfectPcm + volume scaling
BitPerfectPcm + dither
BitPerfectPcm + ConversionPlan != None
```

Предпочтительно сделать эти состояния невозможными
типами/конструкторами, а не только runtime asserts.

------------------------------------------------------------------------

# 15. State machine

``` text
Idle
 ↓
Planning
 ↓
Opening
 ↓
Playing
 ├─ EOF → NextTrack
 ├─ Seek → Seeking
 ├─ DeviceLost → Recover/Stop
 └─ TrackRejected → NextTrack
```

`NextTrack`:

``` text
Plan(track)
 ├─ Playable → Opening
 └─ Rejected → next candidate
```

При смене sample rate между треками допустим close/reconfigure/open
exclusive stream. Нельзя добавлять SRC только ради gapless, если policy
Strict.

------------------------------------------------------------------------

# 16. Тестовая матрица

Planner unit tests без live backend:

  Policy         Source    Device          Ожидание
  -------------- --------- --------------- ------------------------
  Shared         16/44.1   max 96          Play via Shared
  Shared         24/192    max 96          Play
  BestPossible   24/192    supports 192    BitPerfectPcm
  BestPossible   24/192    max 96          DspPcm 96
  Strict         24/192    max 96          Reject
  Strict         16/44.1   supports 44.1   BitPerfectPcm
  BestPossible   DSD256    DoP capable     DoP
  Strict         DSD256    no native/DoP   Reject
  Shared         DSD256    no native       DspPcm/shared fallback

Strict playlist:

``` text
A incompatible
B compatible
C incompatible
D compatible
→ B, D played
```

All incompatible:

``` text
→ No playable tracks
```

Typed PCM tests:

``` text
I16 input bytes == I16 output bytes
I24 input bytes == I24 output bytes
I32 input bytes == I32 output bytes
```

Backend regression:

``` text
Shared → never open hw:*
Exclusive → hw:* permitted
DoP → Exclusive + correct marker/packing
```

Отдельно тестировать `PhysicalDevice`, Shared endpoint и Exclusive
endpoint, чтобы не вернуть Xonar DX/PipeWire regression.

------------------------------------------------------------------------

# 17. Приоритеты

## P0 --- блокирующие

1.  `PlaybackPolicy`.
2.  `SignalPath`.
3.  `PhysicalDevice` + Shared/Exclusive endpoints.
4.  Shared raw-hw ban.
5.  Strict rejection → skip.
6.  Typed PCM transport для строгого integer path.

## P1

1.  `PathPlanner` вне `output.rs`.
2.  `PathPlan`/`StreamContract` вместо `bit_perfect` boolean.
3.  Typed DSD/DoP.
4.  Сделать невозможными `BitPerfect + Resampler`,
    `BitPerfect + Dither`, `Strict + conversion`.
5.  UI path snapshot.

## P2

1.  circular buffer вместо `Vec::drain()` в resampler;
2.  visualizer optimization;
3.  UI allocation cleanup;
4.  production `unwrap/expect` cleanup.

------------------------------------------------------------------------

# 18. Что не переписывать

Сохранить существующую основу:

-   `PlaybackWorker`;
-   `rtrb` SPSC;
-   seek generation handshake;
-   non-RT decoding;
-   существующие resampler algorithms;
-   корректный DoP framing;
-   Slint separation.

Цель --- не переписать audio engine, а перестроить policy/path layer и
затем адаптировать transport там, где это необходимо для настоящего
Bit-Perfect.

------------------------------------------------------------------------

# 19. Acceptance criteria

Рефакторинг завершён, когда:

1.  `PlaybackPolicy` существует как отдельный тип.
2.  `PathPlanner` --- единственная точка выбора допустимого пути.
3.  `SignalPath` описывает фактическую обработку.
4.  `BackendEndpoint` отделён от signal path.
5.  Shared никогда не открывает raw `hw:*`.
6.  Strict никогда не выполняет conversion.
7.  BestPossible выполняет conversion только по решению Planner.
8.  DSD→PCM никогда не называется Bit-Perfect.
9.  Native DSD и DoP имеют отдельные paths.
10. Strict incompatible track автоматически пропускается.
11. Playlist продолжает воспроизведение.
12. Strict PCM не проходит через f32 roundtrip.
13. `bit_perfect + resampler` не существует как валидное semantic state.
14. UI показывает actual path.
15. Xonar DX/PipeWire regression устранён.
16. Planner покрыт unit tests.
17. Shared/Exclusive endpoint selection покрыт backend tests.

------------------------------------------------------------------------

# 20. Итоговая целевая схема

``` text
Slint UI
   ↓ commands/status
Playback Manager
   ↓
PlaybackPolicy
   ↓
PathPlanner
   ├── PathPlan
   │    ├── SignalPath
   │    ├── ConversionPlan
   │    └── BackendEndpoint
   │
   └── TrackRejection
          ↓
       next track

PathPlan
   ↓
typed worker transport
   ↓
RT callback
   ↓
BackendEndpoint
   ├── Shared → PipeWire
   └── Exclusive → ALSA hw:*
```

Главный принцип, который должен быть закреплён в коде и тестах:

> Пользователь выбирает политику воспроизведения. Planner подбирает
> максимально качественный допустимый путь. SignalPath фиксирует
> фактическую обработку. BackendEndpoint фиксирует способ доступа к
> устройству. RT engine только исполняет уже принятое решение.

------------------------------------------------------------------------

## Источник требований

Спецификация построена на сохранённом code review
`apap_code_review_new_policy_2026-09-21.md`, включая его анализ
`src/audio/output.rs`, `player.rs`, `worker.rs`, `decoder.rs`, `dsd.rs`,
`dop.rs`, `src/app/playback_manager.rs`, `settings.rs` и связанных
application/UI компонентов. Ревью прямо фиксирует необходимость перейти
от модели `Settings → bit_perfect/exclusive/fallback/resampler → output`
к `PlaybackPolicy → PathPlanner → SignalPath → Backend Endpoint`.
