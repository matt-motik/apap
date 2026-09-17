# Спека A3.0 — Audio Settings: устройства, capabilities, валидация, bit-perfect report

**Префикс (ID):** `A3.0`
**Область:** `settings.rs`, `audio/output.rs`, `audio/player.rs`, `app/mod.rs`,
`app/ui_manager.rs`, `app/playback_manager.rs`, `app/bp_report.rs` (новый),
`ui/settings.slint`, `ui/bp_report.slint` (новый), `ui/main.slint`, `ui/status.slint`
**Статус:** финал (зафиксирован 2026-09-17, решения по §14 приняты)
**Предпосылки:** аудио-бэкенд (cpal + ALSA), `Player` с Producer/Consumer,
cpal probe, DoP, CIC decode, существующая вкладка Audio.

---

## 1. Scope

### 1.1 Входит

- Расширение `DeviceInfo`: категория, поддерживаемые rate/format, exclusive-capable.
- Фильтры списка устройств: **Hardware only**, **Stereo only**.
- Панель **Capabilities** выбранного устройства (type, channels, rates, formats, exclusive, DSD/DoP).
- Панель **Validation** — статическая матрица «типовой источник × текущие настройки» (11 строк).
- Настройки: `exclusive`, `fallback`, `resampler.mode`, `resampler.fixed_rate`,
  `resampler.prefer_family`, `resampler.fallback_rate`, `dsd_mode` (с цепочкой
  фолбеков), `filter_hardware_only`, `filter_stereo_only`.
- **DSD preference chain** — три фиксированных режима с автоматическими фолбеками: `Native → DoP → PCM`, `DoP → PCM`, `PCM`.
- **Clock family** — политика выбора fallback rate: `Nearest | SameFamily | NeverDownsample`.
- Отдельное окно **Bit-perfect report** (клик по badge в статус-баре).
- Расширение `StreamDesc` — источник, активный поток, причина фолбека.

#### Семантика «Exclusive» на ALSA/cpal (ключевое решение §14.0)

cpal для ALSA **не имеет флага exclusive-access** (это понятие WASAPI). Поэтому:

- `exclusive` в этой спеке означает **предпочтение raw-ноды `hw:*`** (открытие
  напрямую, минуя dmix/plughw — как при текущем выборе raw-ноды).
- `exclusive_capable` = `is_raw_hardware_id(id)` && `category == Hardware`
  (детерминированно, без обращения к бэкенду).
- `build_stream_rt` **новых API не получает** — режим задаётся на этапе выбора
  устройства, не на этапе постройки потока.
- `ExclusiveMode::Auto` (дефолт) = «предпочесть raw-ноду, иначе shared» —
  это официально сохраняет текущее поведение выбора.

### 1.2 Не входит

- DSD Native backend (`DSD_U32_BE` через FFI) — заглушка `Err`.
- Curated-таблица DAC.
- Кнопка «Протестировать устройство».
- Per-device профили.
- Мультиканал (>2 ch).
- Integer Mode, PCM→DSD, Realtime priority.
- Миграция `settings.toml` — плеер в разработке, старые поля игнорируются.
- Локализация — весь новый UI на **русском** (консистентность с интерфейсом).

---

## 2. Модель данных (Rust)

### 2.1 `settings.rs` — новые типы

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExclusiveMode {
    /// Всегда shared (не пытаться вешать raw-ноду как exclusive).
    Off,
    /// Пытаться exclusive (raw-нода); при неудаче один откат к shared.
    #[default]
    Auto,
    /// Требовать exclusive; при неудаче — Err, без retry.
    Strict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPolicy {
    /// Ближайший поддерживаемый rate / clamp channels.
    #[default]
    Nearest,
    /// Дефолтный конфиг устройства.
    DeviceDefault,
    /// Отказ при несовпадении — трек не откроется.
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResamplerMode {
    /// Ресемплить, только если native rate недоступен.
    #[default]
    Auto,
    /// Никогда не ресемплить: exact match или Err.
    Native,
    /// Всегда ресемплить к `fixed_rate` (или к ближайшей из семейства).
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClockFamily {
    #[default]
    Auto,
    Family44k,   // 44.1 / 88.2 / 176.4 / 352.8
    Family48k,   // 48 / 96 / 192 / 384
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FallbackRatePolicy {
    /// Ближайшая поддерживаемая (текущее поведение `nearest_rate`).
    #[default]
    Nearest,
    /// Остаться в семействе источника; если семейство неполное — падать
    /// на `Nearest` (но с пометкой в `FallbackReason`).
    SameFamily,
    /// Никогда не downsampl'ить: ближайшая поддерживаемая ≥ источника,
    /// иначе Err.
    NeverDownsample,
}
```

### 2.2 `settings.rs` — расширения структур

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioResamplerCfg {
    #[serde(default)]
    pub mode: ResamplerMode,
    /// 0 = auto по семейству (используется только при `mode = Fixed`).
    #[serde(default)]
    pub fixed_rate: u32,
    #[serde(default)]
    pub prefer_family: ClockFamily,
    #[serde(default)]
    pub fallback_rate: FallbackRatePolicy,
    #[serde(default)]
    pub algorithm: ResamplerAlgorithm,
    #[serde(default)]
    pub dither: ResamplerDither,
}

impl Default for AudioResamplerCfg {
    fn default() -> Self {
        Self {
            mode: ResamplerMode::Auto,
            fixed_rate: 0,
            prefer_family: ClockFamily::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            algorithm: ResamplerAlgorithm::SincMedium,
            dither: ResamplerDither::Tpdf,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioCfg {
    #[serde(default)]
    pub bit_perfect: bool,
    #[serde(default)]
    pub exclusive: ExclusiveMode,
    #[serde(default)]
    pub fallback: FallbackPolicy,
    #[serde(default)]
    pub filter_hardware_only: bool,
    #[serde(default)]
    pub filter_stereo_only: bool,
    #[serde(default = "default_ring_buffer_ms")]
    pub ring_buffer_ms: u32,
    #[serde(default)]
    pub resampler: AudioResamplerCfg,
}

impl Default for AudioCfg {
    fn default() -> Self {
        Self {
            bit_perfect: false,
            exclusive: ExclusiveMode::Auto,
            fallback: FallbackPolicy::Nearest,
            filter_hardware_only: false,
            filter_stereo_only: false,
            ring_buffer_ms: RING_BUFFER_MS_DEFAULT,
            resampler: AudioResamplerCfg::default(),
        }
    }
}
```

`DsdMode` — без изменений (`Pcm / Native / DoP`), но семантика: **предпочитаемый
режим с автоматической цепочкой фолбеков** (см. §4).

### 2.3 Правила взаимодействия (UI + логика)

| Флаг | Эффект |
|---|---|
| `bit_perfect = true` | UI: показать `audio-exclusive-warn`, если `exclusive = Off`. Не меняет настройки автоматически. |
| `resampler.mode = Native` | UI: `fallback` disabled (всегда должен быть `Fail`); при переключении — auto-выставить `Fail`. |
| `resampler.mode = Fixed` | UI: включить поле `fixed_rate`. Если `fallback == Fail` → **авто-подмена на `Nearest` + баннер** (§8.3). |
| `fallback = Fail` | UI: секция DSD chain — красная подсветка строк, требующих фолбека. |
| `filter_hardware_only` | Список устройств фильтруется по `category == Hardware`. |
| `filter_stereo_only` | Список фильтруется по `channels == 2`. |

---

## 3. Backend: `audio/output.rs`

### 3.1 `DeviceCategory`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceCategory {
    Hardware,      // hw:CARD=…, реальное железо
    ServerProxy,   // plughw:, default, front:, PipeWire, Pulse
    Virtual,       // dmix, dsnoop, softvol, null, ffmpeg loopback
    Loopback,      // ALSA loopback, Monitor of …
    Unknown,
}

pub fn classify_device(id: &str, name: &str) -> DeviceCategory;
```

Правила:
- `id.starts_with("hw:")` || `id.starts_with("hw=")` → `Hardware`
- `id.starts_with("plughw:")` || `id == "default"` || `id.starts_with("front:")`
  || `id.starts_with("surround")` || `id.starts_with("sysdefault")` → `ServerProxy`
- `id.contains("dmix")` || `id.contains("dsnoop")` || `id.contains("softvol")`
  || `id.contains("null")` || `id.contains("ffmpeg")` || `id.contains("loopback")` → `Virtual`
- `name.contains("Monitor of")` || `id.contains("loopback")` → `Loopback`
- `name.contains("PipeWire")` || `name.contains("Pulse")` → `ServerProxy`
- иначе → `Unknown`

### 3.2 `DeviceInfo` — расширение

```rust
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub channels: u16,
    pub default_rate: u32,
    pub default_format: SampleFormat,
    pub buffer_size: SupportedBufferSize,
    pub supported: Vec<RateRange>,

    // NEW:
    pub category: DeviceCategory,
    pub supported_rates: Vec<u32>,       // плоский, сортированный, уникальный
    pub supported_formats: Vec<SampleFormat>,
    pub exclusive_capable: bool,
}

impl DeviceInfo {
    pub fn is_stereo(&self) -> bool { self.channels == 2 }
    pub fn supports_rate(&self, rate: u32) -> bool;
    pub fn supports_format(&self, f: SampleFormat) -> bool;
    /// DoP-контейнерная частота для DSD64/128/256 = byte_rate / 2.
    /// Возвращает первую поддерживаемую из {176400, 352800, 705600}, либо None.
    pub fn dop_container_rate(&self) -> Option<u32>;
    /// "44.1 · 48 · 88.2 · 96 · 176.4 · 192 kHz"
    pub fn rates_desc(&self) -> String;
    /// "I16 · I24 · I32 · F32"
    pub fn formats_desc(&self) -> String;
    /// "44k partial · 48k full" — семейство и его полнота.
    pub fn clock_families_desc(&self) -> String;
}
```

`exclusive_capable` заполняется: `is_raw_hardware_id(&id)` && `category == Hardware`.

`clock_families_desc` — проверяет наличие `{base, 2·base, 4·base}`:
- `Family44k`: base 44100, присутствуют ли 44100, 88200, 176400
- `Family48k`: base 48000, присутствуют ли 48000, 96000, 192000
- Вывод: `"44k partial (44.1, 176.4) · 48k full (48, 96, 192)"`

### 3.3 `ChosenOutput` — расширение

```rust
pub struct ChosenOutput {
    pub device_id: String,
    pub device_name: String,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
    pub exclusive: bool,
    pub resampled: bool,
    pub source_rate: u32,
    pub source_channels: usize,
    pub fallback: Option<FallbackReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    RateUnsupported { requested: u32, chosen: u32, cross_family: bool },
    ChannelsUnsupported { requested: usize, chosen: u16 },
    ExclusiveUnavailable,
    ResamplerForced { requested: u32 },
    ClockFamilyIncomplete { requested: u32, chosen: u32 },
}
```

### 3.4 `choose_output` — обновлённый алгоритм

```
Input: devices, default_name, track_rate, track_channels, preferred,
       ExclusiveMode, FallbackPolicy, ResamplerMode, FallbackRatePolicy,
       ClockFamily, fixed_rate

1. Разрешить устройство (existing: preferred id → preferred name → default name)
2. Exclusive:
   requested = exclusive_mode
   Off    → exclusive = false
   Auto   → exclusive = device.exclusive_capable
   Strict → if !device.exclusive_capable → Err(ExclusiveUnavailable)
3. Channels:
   Fail    → exact match, иначе Err(ChannelsUnsupported)
   иначе   → clamp(min(track_ch, device.ch), 1), fallback if changed
4. Rate:
   mode = Native → только exact, иначе Err(RateUnsupported)   // HARD FAIL (§14)
   mode = Fixed:
     target = fixed_rate != 0 ? fixed_rate
                              : nearest_in_family(device, prefer_family)
     если fixed_rate == 0 и семейство пусто → target = device.default_rate,
        fallback = ClockFamilyIncomplete { requested: track_rate, chosen: default_rate }
        (НЕ silent: причина всегда записывается.)
     resampled = (target != track_rate)
     иначе fallback также = ResamplerForced { requested: track_rate }
   mode = Auto:
     если device.supports_rate(track_rate) → native
     иначе по FallbackPolicy:
       Nearest       → choose by fallback_rate policy (шаг 5)
       DeviceDefault → device.default_rate
       Fail          → Err(RateUnsupported)
5. Fallback rate (только при mode = Auto && !native):
   Nearest          → nearest_rate(device, channels, track_rate)
   SameFamily       → если в семействе track_rate есть rate → ближайший в семействе
                      иначе → nearest_rate, cross_family = true
   NeverDownsample  → только rates ≥ track_rate; если пусто → Err
6. Sample format:
   exclusive → I32 → I24 → I16 → F32 (первый поддерживаемый)
   shared    → F32 (совместимо с ОС-микшером)
7. BufferSize:
   min(target_buffer_frames(rate, min, max), cap)
```

`nearest_in_family(device, prefer_family)` — выбирает из `supported_rates` все,
кратные `base(44100 или 48000)`, берёт максимальную; пусто → `None`
(⇒ «пустое семейство»).

### 3.5 `describe_stream`

```rust
/// "48000 Hz · I32 · Exclusive"
/// "48000 Hz · I32 · Shared · resampled from 96000"
pub fn describe_stream(chosen: &ChosenOutput) -> String;
```

---

## 4. DSD preference chain

### 4.1 Семантика

`AudioCfg.dsd_mode` — **предпочитаемый** режим. Плеер разворачивает его в цепочку попыток:

| `dsd_mode` | Chain |
|---|---|
| `Native` | `[Native, DoP, Pcm]` |
| `DoP`    | `[DoP, Pcm]` |
| `Pcm`    | `[Pcm]` |

`FallbackPolicy` действует **внутри** каждого шага (rate/channels/format), но также
управляет поведением при провале всей цепочки:
- `Fail` → **не пытаться** следующий шаг в цепочке DSD; вернуть Err.
- `Nearest` / `DeviceDefault` → идти по цепочке до первого успеха.

`DsdMode::Native` backend **пока не реализован** — `try_open_dsd(path, Native)`
возвращает `Err("Native DSD not supported by cpal backend")`. Место в коде готово,
цепочка это учитывает. (Мок-флаг для Native не нужен — поведение константно.)

### 4.2 `Player` — рефакторинг `open`

```rust
impl Player {
    pub fn open(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let is_dsd = is_dsd_path(path);
        if is_dsd { self.open_dsd_with_chain(path) } else { self.open_pcm(path) }
    }

    fn open_dsd_with_chain(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let preferred = self.dsd_mode;
        let chain = match preferred {
            DsdMode::Native => &[DsdMode::Native, DsdMode::DoP, DsdMode::Pcm][..],
            DsdMode::DoP    => &[DsdMode::DoP, DsdMode::Pcm][..],
            DsdMode::Pcm    => &[DsdMode::Pcm][..],
        };
        let strict = self.fallback_policy == FallbackPolicy::Fail;
        let mut last_err: Option<String> = None;

        for mode in chain {
            match self.try_open_dsd(path, *mode) {
                Ok(info) => {
                    self.stream_desc.dsd_mode = Some(*mode);
                    self.stream_desc.dsd_preferred = Some(preferred);
                    if *mode != preferred {
                        self.stream_desc.dsd_fallback_reason =
                            Some(format!("{preferred:?} unavailable"));
                    }
                    return Ok(info);
                }
                Err(e) => {
                    last_err = Some(e);
                    if strict { break; }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| "DSD playback failed".into()))
    }

    fn try_open_dsd(&mut self, path: &Path, mode: DsdMode) -> Result<TrackInfo, String> {
        match mode {
            DsdMode::Native => Err("Native DSD not supported by cpal backend".into()),
            DsdMode::DoP    => self.open_dop(path),
            DsdMode::Pcm    => self.open_pcm(path),
        }
    }
}
```

`open_dop` и `open_pcm` — существующие, рефакторятся: не проверяют `self.dsd_mode`,
полагаются на вызывающий код.

### 4.3 `Player::open_pcm` — exclusive retry

`start_engine` при `exclusive = true` и Err от `build_stream_rt`:
- если `ExclusiveMode::Auto` → один retry с `exclusive = false`;
- если `ExclusiveMode::Strict` → Err без retry.

Записывается в `stream_desc.exclusive_fallback: bool`.

### 4.4 `StreamDesc`

```rust
#[derive(Debug, Clone, Default)]
pub struct StreamDesc {
    pub device: String,
    pub rate: u32,
    pub channels: u16,
    pub format: &'static str,
    pub exclusive: bool,
    pub exclusive_fallback: bool,   // exclusive был запрошен, но не выдан
    pub resampled: bool,
    pub source_rate: u32,
    pub source_channels: usize,
    pub dsd_mode: Option<DsdMode>,
    pub dsd_preferred: Option<DsdMode>,
    pub dsd_fallback_reason: Option<String>,
    pub fallback: Option<FallbackReason>,
}

impl Player {
    pub fn stream_desc(&self) -> Option<&StreamDesc>;
}
```

---

## 5. Validation: чистая функция

### 5.1 Типы

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome { BitPerfect, Degraded, Unsupported }

#[derive(Debug, Clone)]
pub struct ValidationRow {
    pub source: String,       // "96k PCM", "DSD64"
    pub outcome: Outcome,
    pub detail: String,       // "native", "resampled → 48k", "unsupported"
}

pub fn validate_audio_settings(device: &DeviceInfo, settings: &Settings) -> Vec<ValidationRow>;
```

Чистая функция: без I/O, без зависимостей от `MusicApp`.

### 5.2 Канонические источники (11 фиксированных)

| # | Label | Rate | Channels | DSD |
|---|---|---|---|---|
| 1 | 44.1k PCM | 44 100 | 2 | – |
| 2 | 48k PCM | 48 000 | 2 | – |
| 3 | 88.2k PCM | 88 200 | 2 | – |
| 4 | 96k PCM | 96 000 | 2 | – |
| 5 | 176.4k PCM | 176 400 | 2 | – |
| 6 | 192k PCM | 192 000 | 2 | – |
| 7 | 352.8k PCM | 352 800 | 2 | – |
| 8 | 384k PCM | 384 000 | 2 | – |
| 9 | DSD64 | 176 400 (DoP slot) | 2 | ✓ |
| 10 | DSD128 | 352 800 | 2 | ✓ |
| 11 | DSD256 | 705 600 | 2 | ✓ |

### 5.3 Алгоритм классификации

Для каждой PCM-строки:
```
chosen = choose_output(device, ..., track_rate = row.rate, ...)
match chosen {
    Ok(c) if !c.resampled && c.exclusive        → BitPerfect
    Ok(c) if !c.resampled                      → Degraded   ("native, shared access")
    Ok(c) if c.resampled                        → Degraded   ("resampled → {c.rate}")
    Err(Fail)                                   → Unsupported("{device does not support {rate} Hz}")
}
```

Дополнительно для `ResamplerMode::Native` + `bit_perfect=true` + несовпадение
частот: строка → `Unsupported`, detail
`"Native mode requires exact rate match; device lacks {rate} Hz"` (HARD FAIL, §14).

Для каждой DSD-строки:
```
chain = expand_chain(settings.dsd_mode)
for mode in chain {
    match try_validate_dsd(device, settings, row, mode) {
        Ok(Some(c)) → BitPerfect (если !c.resampled) / Degraded (иначе)
        Ok(None)    → continue
        Err(_)      → continue
    }
}
если chain исчерпана → Unsupported
```

`try_validate_dsd` для `Native` — всегда Err (backend пока не реализован).
`try_validate_dsd` для `DoP` — проверяет `device.dop_container_rate() == Some(row.rate)`.
`try_validate_dsd` для `Pcm` — вызывает `choose_output` с `track_rate = row.rate / 8` (CIC decimation).

### 5.4 `Outcome` → UI-цвет

| Outcome | Background | Text |
|---|---|---|
| `BitPerfect` | `Colors.surface-selected` | `text-primary` |
| `Degraded` | `Colors.surface-active` | `text-warn` |
| `Unsupported` | `transparent` | `text-error` |

---

## 6. Bit-perfect report

### 6.1 Тип

```rust
// app/bp_report.rs
#[derive(Debug, Clone, PartialEq)]
pub struct BpReason {
    pub title: String,
    pub detail: String,
    pub action_id: i32,       // 0 = нет кнопки
    pub action_label: String, // "" если action_id == 0
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BpReport {
    pub bp_active: bool,
    pub stream_desc: String,
    pub source_desc: String,
    pub reasons: Vec<BpReason>,
    pub positives: Vec<String>,
}

pub fn build_bp_report(app: &MusicApp) -> BpReport;
```

### 6.2 Каталог причин (порядок = вес)

| # | Детект | Title | Detail | Action id / label |
|---|---|---|---|---|
| 1 | `player.volume() < 1.0` | `Программная громкость активна ({v:.2})` | `{db:.1} dB ослабление в аудио-колбэке` | `1` / `Поставить 100%` |
| 2 | `player.muted()` | `Приглушено` | `Приглушено в аудио-колбэке` | `2` / `Включить звук` |
| 3 | `shared.resampler_enabled()` | `Ресемплинг включён` | `{src} → {out} (устройство не поддерживает {src})` | `0` |
| 4 | `!stream_desc.exclusive` && `!exclusive_fallback` | `Общий доступ (shared)` | `Устройство {ServerProxy|default}` | `0` |
| 5 | `stream_desc.exclusive_fallback` | `Exclusive недоступен` | `Устройство отклонило exclusive; используется shared` | `0` |
| 6 | `dither_idx != Off` | `Дизеринг активен` | `Применён {Tpdf|Triangular} дизеринг` | `3` / `Отключить дизеринг` |
| 7 | `stream_desc.dsd_mode == Some(Pcm) && source is DSD` | `Конвертация DSD → PCM` | `Децимация CIC ломает bit-perfect` | `0` |
| 8 | `stream_desc.dsd_fallback_reason.is_some()` | `Фолбек DSD применён` | `Предпочтительно {preferred:?}, используется {actual:?}` | `0` |

Порядок сортировки — по возрастанию `#`. Причины с действиями — выше информационных.

### 6.3 Positives (показываются, если true)

- `✓ Обнаружено устройство с поддержкой exclusive`
- `✓ Выдан exclusive-доступ` (если `stream_desc.exclusive`)
- `✓ Native-частота принята (без ресемплинга)` (если `!resampled`)
- `✓ Принят поток I32` (если format = I32)
- `✓ Дизеринг не применяется` (если dither == Off)
- `✓ Программная громкость не применяется` (если volume == 1.0 && !muted)

### 6.4 Действия

`BpReportAction::from_id(i32)`:

| ID | Действие |
|---|---|
| `1` | `player.set_volume(1.0)` + `settings.volume = 1.0` |
| `2` | `player.set_muted(false)` + `settings.muted = false` |
| `3` | `player.set_dither(Off)` + `settings.audio.resampler.dither = Off` |

После действия — пересобрать `BpReport` и обновить UI.

### 6.5 Триггеры пересборки

- Открытие окна (`bp-report-open: false → true`).
- Каждый UI tick, пока окно открыто (данные меняются live — volume, playing state).
- После `BpReportAction`.

### 6.6 Badge в статус-баре

Существующий badge «Bit-perfect» и «Not bit-perfect (DSD→PCM)» и «Resample (device limit)»:
- Оставляем визуально, но переносим логику в `BpReport`.
- `BpReport::bp_active && reasons.is_empty()` → `status_bp_active`.
- `BpReport::bp_active && !reasons.is_empty()` → `status_bp_degraded`.
- Клик на любой badge → `callback bp-clicked()` → `bp-report-open = true`.

---

## 7. UI

### 7.1 `ui/settings.slint` — вкладка Audio

Структура (сверху вниз):

```
[Устройство]
  Выпадающий список (audio-devices) + Обновить
  ☐ Только железо    ☐ Только стерео
  Активно: {active-device}     {codec-info}
  ⚠ {active-error}  (если не пусто)

[Возможности]
  Тип:          Железо · hw:CARD=4,DEV=0
  Каналы:       2 (стерео)
  Частоты:      44.1 · 48 · 88.2 · 96 · 176.4 · 192 kHz
  Форматы:      I16 · I24 · I32 · F32
  Exclusive:    ✓
  DSD (DoP):    176400 · 352800

[Bit-perfect]
  ☐ Bit-perfect
  ⚠ Для bit-perfect требуется exclusive (если bp && exclusive == Off)

[DSD]
  DSD mode:  [ PCM (CIC) ▾ ]
  Цепочка:   Native → DoP → PCM
  ⓘ  Фолбек автоматический. FallbackPolicy управляет провалом цепочки.

[Проверка]
  44.1k PCM    [native]              (строка, цвет фона)
  48k PCM      [native]
  88.2k PCM    [resampled → 96k]
  ...
  DSD64        [DoP @ 176400]
  (неподдерживаемые строки — с тултипом «ожидаемо при Never downsampling» и т.п.)

[Дополнительно]  ▸ (collapsed by default)
  Exclusive         [ Авто ▾ ]        (Выкл / Авто / Строгий)
  Fallback policy   [ Ближайший ▾ ]   (Ближайший / Дефолт устройства / Ошибка)
  Resampler mode    [ Авто ▾ ]        (Авто / Native / Фиксированная)
  Fixed rate        [ Авто ▾ ]        (включён если mode = Fixed)
  Clock family      [ Авто ▾ ]        (Авто / 44.1 / 48)
  Fallback rate     [ Ближайший ▾ ]   (Ближайший / То же семейство / Не понижать)
  Алгоритм          [ Sinc (64 тапа) ▾ ]
  Дизеринг          [ TPDF ▾ ]
  Кольцевой буфер   [────●─────] 1500 мс
                    значения < 500 мс могут давать underrun при exclusive
  ⚠ баннер: «Fixed несовместим с Fail — используется Nearest» (если конфликт, §8.3)
```

Все контролы (кроме превью Capabilities/Validation) — **draft-only**:
пишутся в `settings_draft`, применяются только по кнопке «Сохранить» (§7.7; паттерн
уже существует для всей вкладки — `settings_draft`/`settings-save`/`settings-close`).

### 7.2 `ui/settings.slint` — новые типы и свойства

```slint
export struct DeviceCapability {
    label: string,
    value: string,
    ok: bool,
}

export struct ValidationRow {
    source: string,
    outcome: int,      // 0=BitPerfect, 1=Degraded, 2=Unsupported
    detail: string,
}

export component Settings inherits Rectangle {
    // ... существующие свойства ...

    in property <[DeviceCapability]> audio-caps: [];
    in property <[ValidationRow]> audio-validation: [];
    in property <bool> audio-filter-hardware: false;
    in property <bool> audio-filter-stereo: false;
    in property <bool> audio-advanced-open: false;
    in property <bool> audio-exclusive-warn: false;
    in property <bool> audio-fixed-fail-warn: false;
    in property <string> audio-dsd-chain-desc: "Native → DoP → PCM";

    in property <int> audio-exclusive-idx: 1;
    in property <int> audio-fallback-idx: 0;
    in property <int> audio-resampler-mode-idx: 0;
    in property <int> audio-fixed-rate-idx: 0;
    in property <int> audio-clock-family-idx: 0;
    in property <int> audio-fallback-rate-idx: 0;
    in property <int> audio-ring-buffer-ms: 1500;

    callback set-audio-filter-hardware(bool);
    callback set-audio-filter-stereo(bool);
    callback set-audio-toggle-advanced(bool);
    callback set-audio-exclusive(int);
    callback set-audio-fallback(int);
    callback set-audio-resampler-mode(int);
    callback set-audio-fixed-rate(int);
    callback set-audio-clock-family(int);
    callback set-audio-fallback-rate(int);
    callback set-audio-ring-buffer-ms(int);
}
```

### 7.3 `ui/settings.slint` — ComboBox значения

| ComboBox | Модель | Индексы |
|---|---|---|
| Exclusive | `["Выкл", "Авто", "Строгий"]` | `0/1/2` |
| Fallback policy | `["Ближайший", "Дефолт устройства", "Ошибка"]` | `0/1/2` |
| Resampler mode | `["Авто", "Native", "Фиксированная"]` | `0/1/2` |
| Fixed rate | `["Авто", "44.1k", "48k", "88.2k", "96k", "176.4k", "192k"]` | `0..6` |
| Clock family | `["Авто", "Семейство 44.1", "Семейство 48"]` | `0/1/2` |
| Fallback rate | `["Ближайший", "То же семейство", "Не понижать"]` | `0/1/2` |
| DSD mode | `["PCM (CIC)", "Native DSD", "DoP"]` | `0/1/2` |

### 7.4 `ui/bp_report.slint` — новый компонент

```slint
import { Colors } from "theme.slint";
import { Button, ScrollView } from "std-widgets.slint";

export struct BpReason {
    title: string,
    detail: string,
    action-id: int,
    action-label: string,
}

export component BpReportDialog inherits Rectangle {
    in property <bool> open: false;
    in property <string> stream-desc: "";
    in property <string> source-desc: "";
    in property <bool> bp-active: false;
    in property <[BpReason]> reasons: [];
    in property <[string]> positives: [];

    callback close();
    callback do-action(int);
    callback open-settings();

    visible: root.open;
    background: Colors.bg-overlay;

    dialog := Rectangle {
        width: 520px;
        height: 420px;
        x: (parent.width - self.width) / 2;
        y: (parent.height - self.height) / 2;
        background: Colors.bg-surface;
        border-radius: 8px;
        border-width: 1px;
        border-color: Colors.border-default;

        VerticalLayout {
            padding: 16px;
            spacing: 10px;

            Text {
                text: root.bp-active ? "Статус bit-perfect" : "Bit-perfect выключен";
                color: Colors.text-primary;
                font-size: 16px;
            }

            HorizontalLayout {
                spacing: 6px;
                Text { text: "Поток:"; color: Colors.text-tertiary; font-size: 12px; min-width: 70px; }
                Text { text: root.stream-desc; color: Colors.text-primary; font-size: 12px; }
            }
            HorizontalLayout {
                spacing: 6px;
                Text { text: "Источник:"; color: Colors.text-tertiary; font-size: 12px; min-width: 70px; }
                Text { text: root.source-desc; color: Colors.text-primary; font-size: 12px; }
            }

            ScrollView {
                vertical-stretch: 1;
                VerticalLayout {
                    spacing: 8px;

                    for reason[i] in root.reasons: Rectangle {
                        height: reason.action-id > 0 ? 60px : 44px;
                        background: Colors.surface-hover;
                        border-radius: 4px;

                        VerticalLayout {
                            padding: 8px;
                            spacing: 2px;

                            Text {
                                text: reason.title;
                                color: Colors.text-warn;
                                font-size: 13px;
                                font-weight: 600;
                            }
                            Text {
                                text: reason.detail;
                                color: Colors.text-secondary;
                                font-size: 11px;
                            }
                            if reason.action-id > 0: HorizontalLayout {
                                alignment: end;
                                Button {
                                    text: reason.action-label;
                                    preferred-height: 22px;
                                    clicked => { root.do-action(reason.action-id); }
                                }
                            }
                        }
                    }

                    for p in root.positives: Text {
                        text: "\u{2713} " + p;
                        color: Colors.text-tertiary;
                        font-size: 11px;
                    }
                }
            }

            HorizontalLayout {
                spacing: 8px;
                alignment: end;
                Rectangle { horizontal-stretch: 1; }
                Button {
                    text: "Настройки аудио";
                    preferred-height: 28px;
                    clicked => { root.open-settings(); }
                }
                Button {
                    text: "Закрыть";
                    preferred-height: 28px;
                    clicked => { root.close(); }
                }
            }
        }
    }
}
```

### 7.5 `ui/main.slint` — интеграция

```slint
in property <bool> bp-report-open: false;
in property <string> bp-stream-desc: "";
in property <string> bp-source-desc: "";
in property <[BpReason]> bp-reasons: [];
in property <[string]> bp-positives: [];

callback bp-report-action(int);
callback bp-report-close();
callback bp-report-open-settings();

// ... в конце иерархии, поверх Settings:
BpReportDialog {
    open: root.bp-report-open;
    stream-desc: root.bp-stream-desc;
    source-desc: root.bp-source-desc;
    bp-active: root.bit-perfect;
    reasons: root.bp-reasons;
    positives: root.bp-positives;
    close => { root.bp-report-close(); }
    do-action(id) => { root.bp-report-action(id); }
    open-settings => { root.bp-report-open-settings(); }
}
```

### 7.6 `ui/status.slint`

Существующие badge:
- `bp-active && !dsd-not-bp && !bp-resample` → зелёный (Bit-perfect)
- `dsd-not-bp` → красный (DSD → PCM)
- `bp-resample` → жёлтый (Resample)

Добавляем `callback bp-clicked()` и `TouchArea` поверх badge'а:

```slint
touch := TouchArea {
    clicked => { root.bp-clicked(); }
}
```

### 7.7 Slint ↔ Rust проброс: **draft-only**

В `app/mod.rs` — колбэки пишут **только в `settings_draft`** (мутация draft,
preview-обновление; реально применяются в `settings-save`):

```rust
ui.on_set_audio_filter_hardware(|v| {
    a.settings_mut().filter_hardware_only = v; a.sync_audio_devices();
});
ui.on_set_audio_filter_stereo(|v| {
    a.settings_mut().filter_stereo_only = v; a.sync_audio_devices();
});
ui.on_set_audio_toggle_advanced(|v| { /* audio_advanced_open = v */ });
ui.on_set_audio_exclusive(|i| {
    a.settings_mut().exclusive = ExclusiveMode::from_index(i);
    a.sync_capabilities_and_validation();
});
ui.on_set_audio_fallback(|i| { a.settings_mut().fallback = FallbackPolicy::from_index(i); });
ui.on_set_audio_resampler_mode(|i| {
    let m = ResamplerMode::from_index(i);
    if m == Fixed && draft.fallback == Fail {
        draft.fallback = Nearest;                     // авто-коррекция §14
        a.show_info_message("Fallback изменён на «Ближайший»: Fixed требует ресемплинга.");
        a.sync_audio_advanced();
    }
    draft.resampler.mode = m; a.sync_capabilities_and_validation();
});
ui.on_set_audio_fixed_rate(|i| { draft.resampler.fixed_rate = FIXED_RATES[i]; });
ui.on_set_audio_clock_family(|i| { draft.resampler.prefer_family = ...; });
ui.on_set_audio_fallback_rate(|i| { draft.resampler.fallback_rate = ...; });
ui.on_set_audio_ring_buffer_ms(|v| { draft.ring_buffer_ms = clamp(v); });

ui.on_bp_report_close(|| { /* bp-report-open = false */ });
ui.on_bp_report_action(|id| { /* bp_report::apply_action; rebuild report */ });
ui.on_bp_report_open_settings(|| { /* bp-report-open = false; settings-open = true */ });
```

В `settings-save`: применить draft → `settings`, применить к `player`
(`set_exclusive_mode`, `set_fallback_policy`, `set_resampler_mode`, `set_fixed_rate`,
`set_prefer_family`, `set_ring_buffer_ms`, …) по тем же правилам, что и для уже
существующих полей (device/dsd/bit-perfect/algorithm/dither).

---

## 8. `app/ui_manager.rs` — расширения

### 8.1 `sync_audio_devices`

После получения `pairs`:
1. Фильтрация по `filter_hardware_only` / `filter_stereo_only` (**из draft** —
   панель — превью, не применяется).
2. Формирование ComboBox-модели (labels).
3. Сохранение полного списка `DeviceInfo` в `MusicApp.audio_device_infos: Vec<DeviceInfo>`
   (нужно для capabilities + validation).

### 8.2 `sync_capabilities_and_validation`

Вызывается:
- При смене устройства в ComboBox.
- При изменении draft-настроек, влияющих на результат (exclusive, resampler.mode,
  fallback, ring…), — как превью.
- При `settings-save` (после применения draft).
- При `open_settings`.

```rust
fn sync_capabilities_and_validation(&mut self) {
    let Some(device) = self.current_audio_device_info() else {
        self.ui.set_audio_caps(ModelRc::from(&[][..]));
        self.ui.set_audio_validation(ModelRc::from(&[][..]));
        return;
    };
    let caps = build_capabilities(device);
    self.ui.set_audio_caps(ModelRc::from(caps.as_slice()));

    let validation = output::validate_audio_settings(device, self.settings_ref());
    let rows: Vec<ValidationRow> = validation.iter().map(|r| ValidationRow {
        source: r.source.clone().into(),
        outcome: r.outcome as i32,
        detail: r.detail.clone().into(),
    }).collect();
    self.ui.set_audio_validation(ModelRc::from(rows.as_slice()));

    let warn = self.settings_ref().audio.bit_perfect
        && self.settings_ref().audio.exclusive == ExclusiveMode::Off;
    self.ui.set_audio_exclusive_warn(warn);

    let fixed_fail = self.settings_ref().audio.resampler.mode == ResamplerMode::Fixed
        && self.settings_ref().audio.fallback == FallbackPolicy::Fail;
    self.ui.set_audio_fixed_fail_warn(fixed_fail);
}
```

`build_capabilities(device) -> Vec<DeviceCapability>`:

```
[Тип, "Железо · hw:CARD=4,DEV=0", true]
[Каналы, "2 (стерео)", true]
[Частоты, device.rates_desc(), true]
[Форматы, device.formats_desc(), true]
[Exclusive, "✓" / "—", device.exclusive_capable]
[DSD (DoP), dop_desc, device.dop_container_rate().is_some()]
```

### 8.3 `sync_dsd_chain_desc`

```rust
fn sync_dsd_chain_desc(&self) {
    let desc = match self.settings_ref().dsd.mode {
        DsdMode::Native => "Native → DoP → PCM",
        DsdMode::DoP    => "DoP → PCM",
        DsdMode::Pcm    => "PCM only",
    };
    self.ui.set_audio_dsd_chain_desc(desc.into());
}
```

---

## 9. `app/bp_report.rs` — новый модуль

### 9.1 Публичный API

```rust
pub fn build_bp_report(app: &MusicApp) -> BpReport;
pub fn apply_action(app: &mut MusicApp, action_id: i32);
```

### 9.2 `build_bp_report` — детекторы

Точные условия — §6.2. Источник `source_desc` — из текущего трека:

```rust
fn source_desc(track: Option<&Track>) -> String {
    match track {
        Some(t) => {
            if t.format.starts_with("DSD") {
                format!("{} {}", t.bit_depth.to_uppercase(), t.format)
            } else {
                format!("{}/{} {}", t.bit_depth, t.sample_rate / 1000, t.format)
            }
        }
        None => "—".into(),
    }
}
```

### 9.3 `apply_action`

```rust
pub fn apply_action(app: &mut MusicApp, action_id: i32) {
    match action_id {
        1 => {
            app.player.set_volume(1.0);
            app.settings.settings.volume = 1.0;
        }
        2 => {
            app.player.set_muted(false);
            app.settings.settings.muted = false;
        }
        3 => {
            app.player.set_dither(ResamplerDither::Off);
            app.settings.settings.audio.resampler.dither = ResamplerDither::Off;
        }
        _ => {}
    }
    // Persisted at exit.
}
```

### 9.4 Обновление из tick

В `MusicApp::tick`:

```rust
if self.ui.get_bp_report_open() {
    let report = bp_report::build_bp_report(self);
    self.push_bp_report_to_ui(report);
}
```

---

## 10. `app/playback_manager.rs` — `stream_desc`

При `play_track`:

```rust
self.player.open(&path)?;
self.stream_desc = self.player.stream_desc().cloned();
// ...
```

`stream_desc` сохраняется в `MusicApp.stream_desc: Option<StreamDesc>`.

В `sync_playback_state_to_ui` — обновление `status_bp_active` / `status_bp_degraded`:

```rust
let bp_active = self.player.bit_perfect();
let degraded = self.stream_desc.as_ref()
    .map(|d| d.resampled || d.dsd_mode == Some(DsdMode::Pcm) || d.exclusive_fallback)
    .unwrap_or(false);
self.ui.set_status_bp_active(bp_active && !degraded);
self.ui.set_status_bp_degraded(bp_active && degraded);
```

Существующие `status_dsd_not_bp` и `status_bp_resample` — снимаются (заменяются
единой badge-логикой; в `status.slint` badge'ы остаются три, но условия упрощаются).

---

## 11. Тесты

### 11.1 `settings.rs`

- `audio_resampler_defaults` — `mode = Auto`, `fixed_rate = 0`, `prefer_family = Auto`, `fallback_rate = Nearest`.
- `audio_defaults_new_fields` — `exclusive = Auto`, `fallback = Nearest`, `filter_* = false`.
- `audio_toml_roundtrip_with_new_fields`.
- `audio_partial_config_preserves_existing` — старый конфиг без новых полей парсится.

### 11.2 `output.rs`

- `classify_device_hw` → `Hardware`.
- `classify_device_plughw` → `ServerProxy`.
- `classify_device_dmix` → `Virtual`.
- `classify_device_loopback` → `Loopback`.
- `supported_rates_sorted_unique`.
- `clock_families_desc_partial_and_full`.
- `dop_container_rate_picks_first_supported`.
- `choose_output_exclusive_strict_on_server_fails`.
- `choose_output_exclusive_auto_on_server_degrades` — `exclusive = false`, `FallbackReason::ExclusiveUnavailable`.
- `choose_output_fallback_fail_returns_error` — `Fail` + rate несовпадение → Err.
- `choose_output_device_default_ignores_track_rate`.
- `choose_output_native_mode_requires_exact`.
- `choose_output_fixed_mode_uses_fixed_rate`.
- `choose_output_fixed_mode_uses_family_if_zero`.
- `choose_output_fixed_mode_auto_empty_family_falls_to_device_default` — устройство
  только `[48000, 96000]`, prefer = 44k family → target 48000 (default),
  `fallback_reason = ClockFamilyIncomplete`.
- `choose_output_fallback_rate_same_family` — 88.2k на `{44.1, 48, 96, 192}` + SameFamily → 44.1.
- `choose_output_fallback_rate_never_downsample` — 192k на `{44.1, 48, 96}` → Err.
- `describe_stream_includes_resampled_marker`.

### 11.3 `validate_audio_settings`

- `validate_pcm_native_when_rate_supported`.
- `validate_pcm_degraded_when_nearest`.
- `validate_pcm_unsupported_when_fail`.
- `validate_pcm_cross_family_detail` — 88.2k → 96k, detail содержит `cross-family`.
- `validate_pcm_native_bp_unsupported` — `ResamplerMode::Native` + `bit_perfect` +
  несовпадение → `Unsupported`, detail про exact-rate (HARD FAIL).
- `validate_dsd_bitperfect_when_dop_slot_supported` — DsdMode::Native на `[..., 176400]` → DSD64 BitPerfect, detail `DoP`.
- `validate_dsd_degraded_when_chain_falls_to_pcm` — DsdMode::DoP на `[44100, 48000]`, FallbackPolicy::Nearest → DSD64 Degraded, detail `PCM @ 44100`.
- `validate_dsd_unsupported_when_fail` — FallbackPolicy::Fail → Unsupported.
- `validate_returns_11_rows`.

### 11.4 `player.rs` (Scoped Seam)

Единственная точка расширения для тестов — **`TestHooks` в `Player`**
(`#[cfg(test)]`):

```rust
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct TestHooks {
    /// Сколько следующих вызовов постройки потока вернут Err (имитация
    /// занятого устройства / отказа exclusive). Потребляются в `start_engine`
    /// до реального `build_stream_rt`.
    pub build_failures: AtomicUsize,
}

// в start_engine, перед каждым обращением к build_stream_rt:
#[cfg(test)]
{
    let rem = self.test_hooks.build_failures.load(Ordering::Relaxed);
    if rem > 0 {
        self.test_hooks.build_failures.store(rem - 1, Ordering::Relaxed);
        return Err("Mock: device busy (exclusive)".into());
    }
}
```

Мок-флаг для `Native` DSD не нужен — `try_open_dsd(Native)` и так всегда Err.

Тесты:
- `open_with_exclusive_strict_on_server_fails` — `ExclusiveMode::Strict`,
  `build_failures = 999` → `open` Err, ровно 1 попытка (без retry).
- `open_auto_falls_back_once_on_exclusive_failure` — `ExclusiveMode::Auto`,
  `build_failures = 1` → первый build Err, retry shared → Ok;
  assert `stream_desc().exclusive == false`.
- `open_dsd_chain_native_prefers_dop_when_native_fails` — `DsdMode::Native`
  при `exclusive = Off` и `build_failures = 1`: DoP build падает → PCM
  проходит; assert `stream_desc.dsd_mode == Pcm`, `dsd_preferred == Native`,
  `dsd_fallback_reason.is_some()`.
- `open_dsd_chain_dop_falls_to_pcm`.
- `open_dsd_chain_fail_policy_stops_chain` — `DsdMode::Native` + `Fallback = Fail`
  → Err, без попытки DoP.

### 11.5 `bp_report.rs`

- `bp_report_volume_reason` — volume 0.8 → reason #1.
- `bp_report_positive_exclusive_capable`.
- `bp_report_reasons_sorted_by_weight`.
- `bp_report_dsd_not_bp`.
- `bp_report_empty_when_all_green` — все условия выполнены → `reasons.is_empty()`, `positives` непусто.

### 11.6 Ручной smoke

| # | Сценарий | Ожидание |
|---|---|---|
| 1 | Вкладка Audio, ADI-2 | Capabilities заполнены, 11 validation-строк |
| 2 | `Только железо` on | PipeWire-нода исчезает из ComboBox |
| 3 | `Только стерео` on | Многоканальные устройства исчезают |
| 4 | `bit-perfect` on, volume 0.8 | Клик на badge → окно, reason #1 + кнопка `Поставить 100%` |
| 5 | В окне `Поставить 100%` | Volume → 1.0, окно обновляется, reason исчезает |
| 6 | Fallback = Ошибка, 352.8k на 44.1/48 | Validation строка красная |
| 7 | Fallback = То же семейство, 88.2k на `[44.1,48,96,192]` | Строка жёлтая, `resampled → 44.1 (same family)` |
| 8 | DSD mode = Native, девайс без DoP-слота | DSD-строки красные, detail `no DoP slot` |
| 9 | DSD mode = DoP, девайс с 176400 | DSD64 зелёный, `DoP @ 176400` |
| 10 | Ring buffer 200 ms + exclusive | Подсказка под слайдером, при воспроизведении — возможен underrun |
| 11 | Fixed при Fail | Авто-подмена на Nearest + баннер в Advanced |
| 12 | Изменения в Advanced без «Сохранить» | Перезапуск — значения не применились (draft отброшен) |

---

## 12. Этапы внедрения

| Этап | Содержание | Файлы | Оценка |
|---|---|---|---|
| **A3.1** | Модель настроек: `ExclusiveMode`, `FallbackPolicy`, `ResamplerMode`, `ClockFamily`, `FallbackRatePolicy`; расширение `AudioResamplerCfg` / `AudioCfg`; тесты round-trip. | `settings.rs` | 0.5 дня |
| **A3.2** | `DeviceInfo` расширение, `classify_device`, `supported_rates` / `supported_formats`, `exclusive_capable`, `rates_desc` / `formats_desc` / `clock_families_desc`, `dop_container_rate`. | `audio/output.rs` | 1 день |
| **A3.3** | `choose_output` под новые политики, `FallbackReason`, `describe_stream`, `ChosenOutput`; `validate_audio_settings` + тесты. | `audio/output.rs` | 1.5 дня |
| **A3.4** | `Player`: рефакторинг `open`, DSD chain, exclusive retry, `StreamDesc`, `try_open_dsd`, `TestHooks`. | `audio/player.rs` | 1 день |
| **A3.5** | Вкладка Audio в `settings.slint`: capabilities, validation, фильтры, Advanced (draft-only); `sync_capabilities_and_validation`, `sync_dsd_chain_desc`. | `ui/settings.slint`, `app/ui_manager.rs`, `app/mod.rs` | 2 дня |
| **A3.6** | `BpReportDialog`, `app/bp_report.rs`, badge click, `stream_desc` в `MusicApp`. | `ui/bp_report.slint`, `ui/main.slint`, `ui/status.slint`, `app/bp_report.rs`, `app/playback_manager.rs`, `app/mod.rs` | 1.5 дня |
| **A3.7** | Ручной smoke, правка дефолтов, документирование в `AGENTS.md`. | — | 0.5 дня |

**Итого: ~8 рабочих дней.**

---

## 13. Критерии приёмки

1. Все тесты §11.1–11.5 зелёные.
2. Smoke §11.6 пройден на ADI-2 (USB) и на PipeWire-ноде.
3. Существующие тесты `output.rs` (nearest_rate, collapse_same_name, label_device_names) — без правок, зелёные.
4. Zero-allocation тесты `player.rs` остаются зелёными — RT-колбэки не тронуты (`TestHooks` — только вне RT).
5. `validate_audio_settings` — чистая функция, без I/O, без зависимостей от `MusicApp`.
6. `BpReport` собирается за < 1 ms (проверяется косвенно: tick не деградирует по latency).
7. Ни одно новое поле `settings.toml` не ломает парсинг старого конфига.
8. Новые контролы Audio — draft-only: без «Сохранить» ничего не применяется; шаблон «Реализация section 6.5: cancel discards draft» сохранён.
9. Все новые UI-строки — русские.

---

## 14. Решённые открытые вопросы (зафиксировано 2026-09-17)

| № | Вопрос | Решение |
|---|---|---|
| 0 | Exclusive на ALSA/cpal | `exclusive` = предпочтение raw-ноды `hw:*`; дефолт `Auto` (сохраняет текущее поведение); новых API в `build_stream_rt` нет. |
| 1 | `FallbackPolicy::Fail` + `ResamplerMode::Fixed` | Авто-подмена `Fail → Nearest` при выборе `Fixed` + баннер в Advanced + баннер при конфликте из прямого TOML. Блокировка комбобокса не используется. |
| 2 | `Fixed` + `fixed_rate=0` + пустое семейство | Жёсткая иерархия: fixed_rate → семейство (max) → `DeviceDefault` + `FallbackReason::ClockFamilyIncomplete`. Без молчаливого поведение — причина всегда явная. |
| 3 | `Native` (ресемплер) + `bit_perfect` + несовпадение rate | HARD FAIL: `Err(RateUnsupported)`, строка Validation = `Unsupported` с detail. Воспроизведение не начнётся (контракт «или bit-perfect, или ничего»). |
| 4 | `NeverDownsample` + validation | Ожидаемое поведение: строка красная, тултип «это ожидаемо при Never downsampling». |
| 5 | Ring buffer и exclusive | Подсказка под слайдером (<500 мс), без блокировки. |
| 6 | `BpReportDialog` при bit-perfect = off | Заголовок «Bit-perfect выключен», кнопка «Настройки аудио» → открывает настройки. |
| 7 | Локализация | Русский для всего нового UI. |
| 8 | Immediate vs Dirty Flag | **Dirty Flag + Apply on Save** (существующий паттерн `settings_draft`): ни один новый контрол не применяется до «Сохранить», Capabilities/Validation — live-превью от draft. |
| 9 | Тесты `Player::open` | Scoped Seam: `TestHooks { build_failures: AtomicUsize }` в `start_engine` (только `#[cfg(test)]`); мок для Native не нужен. |
| 10 | `dsd_fallback_reason` в `StreamDesc` | Строка на русском, без локализации конфига. |

---

## 15. Сводка изменений по файлам

| Файл | Тип изменения |
|---|---|
| `src/settings.rs` | +5 enum, +поля в `AudioCfg` / `AudioResamplerCfg` |
| `src/audio/output.rs` | +`DeviceCategory`, +поля `DeviceInfo`, +`FallbackReason`, +поля `ChosenOutput`, +`describe_stream`, +`validate_audio_settings`, переписать `choose_output`, +`classify_device` |
| `src/audio/player.rs` | Переписать `open`, +`open_dsd_with_chain`, +`try_open_dsd`, +`StreamDesc`, +`stream_desc()`, +политики exclusive/fallback/resampler, +`TestHooks` (cfg(test)) |
| `src/app/mod.rs` | +колбэки Audio (10, draft-only), +колбэки bp-report (3), +поле `stream_desc`, +поле `audio_device_infos`, +`show_info_message` |
| `src/app/ui_manager.rs` | +`sync_capabilities_and_validation`, +`sync_dsd_chain_desc`, +фильтрация `sync_audio_devices` |
| `src/app/playback_manager.rs` | +`stream_desc` при `play_track`, обновление badge-логики |
| `src/app/bp_report.rs` | Новый файл — `BpReport`, `BpReason`, `build_bp_report`, `apply_action` |
| `ui/settings.slint` | +2 struct, +13 свойств, +12 колбэков, переписать вкладку Audio |
| `ui/bp_report.slint` | Новый компонент |
| `ui/main.slint` | +5 свойств, +3 колбэка, +`BpReportDialog` в иерархии |
| `ui/status.slint` | +callback `bp-clicked()`, упростить badge-условия |
| `AGENTS.md` | Раздел A3.0: архитектура, ключевые решения |

---

Конец ТЗ.