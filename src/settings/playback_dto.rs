//! Файловое представление `ModeSettings` — раздел `[playback]` в
//! `settings.toml` (§6.27, 02 §2.4, §2.6).
//!
//! Каждое поле режима хранится как `Option<toml::Value>`, а не как типизированное
//! перечисление: так ошибка в значении одного ключа не делает нечитаемым весь
//! раздел `[playback]` (02 §2.3, вариант В) — проверка допустимости значения и
//! подстановка значения по умолчанию выполняются позже, в `load_mode_settings`
//! (§6.27), которой в этом шаге нет. Здесь только обратная операция — запись
//! актуального `ModeSettings` в DTO (02 §2.6: "обратная функция к
//! `load_mode_settings`, которой в 01 нет").
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::audio::backend::SharedDeviceId;
use crate::audio::format::SampleRate;
use crate::persist::keys::{KeyPath, LoadNote, LoadNoteKind};

use super::playback::{
    buffers_allowed, BufferMs, CompatibleOpts, DeviceBuffer, Dither, DsdAboveDac, DsdFilter,
    ModeSettings, OptimalOpts, RateFallbackRule, SharedDeviceChoice, SrcFilter, StrictOpts,
};

/// Раздел `[playback]` целиком (§6.27, 02 §2.4). `schema` — версия формата
/// раздела: отсутствие или значение, отличное от `2`, делает раздел
/// нераспознанным (§6.27 п. 2). `unknown` хранит незнакомые ключи верхнего
/// уровня раздела — у `playback_dto` он всегда пуст (02 §2.6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaybackDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatible: Option<CompatibleDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optimal: Option<OptimalDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<StrictDto>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, toml::Value>,
}

/// Подраздел `[playback.compatible]` (§6.27, §2.2 Совместимый режим).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompatibleDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_rate: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_filter: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dither: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsd_filter: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsd_gain_comp: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer: Option<toml::Value>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, toml::Value>,
}

/// Подраздел `[playback.optimal]` (§6.27, §2.2 Оптимальный режим).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OptimalDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_fallback: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_filter: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dither: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_lock: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsd_above_dac: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsd_filter: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsd_gain_comp: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_buffer_ms: Option<toml::Value>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, toml::Value>,
}

/// Подраздел `[playback.strict]` (§6.27, §2.2 Строгий режим; И-Т1 — только
/// `device`, `buffer`, `device_buffer`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StrictDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_buffer_ms: Option<toml::Value>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, toml::Value>,
}

/// Строковая форма `SrcFilter` (§6.27): `"steep"` / `"slow"` / `"very_slow"`.
fn src_filter_key(v: SrcFilter) -> &'static str {
    match v {
        SrcFilter::Steep => "steep",
        SrcFilter::Slow => "slow",
        SrcFilter::VerySlow => "very_slow",
    }
}

/// Числовая форма `DsdFilter` (§6.27): `24` / `30` / `50` (кГц начала полосы
/// подавления).
fn dsd_filter_khz(v: DsdFilter) -> i64 {
    match v {
        DsdFilter::K24 => 24,
        DsdFilter::K30 => 30,
        DsdFilter::K50 => 50,
    }
}

/// Строковая форма `Dither`: `"tpdf"` / `"off"`.
fn dither_key(v: Dither) -> &'static str {
    match v {
        Dither::Tpdf => "tpdf",
        Dither::Off => "off",
    }
}

/// Строковая форма `RateFallbackRule`: `"same_family"` / `"nearest"` /
/// `"no_downsample"` (§2.2, ОВ-3).
fn rate_fallback_key(v: RateFallbackRule) -> &'static str {
    match v {
        RateFallbackRule::SameFamily => "same_family",
        RateFallbackRule::Nearest => "nearest",
        RateFallbackRule::NoDownsample => "no_downsample",
    }
}

/// Строковая форма `DsdAboveDac`: единственное реализуемое значение —
/// `"convert_to_pcm"` (решение 6, §6.27: `"remodulate"` недопустимо).
fn dsd_above_dac_key(v: DsdAboveDac) -> &'static str {
    match v {
        DsdAboveDac::ConvertToPcm => "convert_to_pcm",
    }
}

/// Числовая форма `DeviceBuffer` в миллисекундах — ключ `device_buffer_ms`
/// (§6.27): `20` / `40` / `100` / `200` / `400`.
fn device_buffer_ms_value(v: super::playback::DeviceBuffer) -> i64 {
    i64::from(v.ms())
}

fn compatible_dto(o: &CompatibleOpts) -> CompatibleDto {
    let device = match &o.device {
        SharedDeviceChoice::SystemDefault => None,
        SharedDeviceChoice::Named(id) => Some(toml::Value::String(id.as_str().to_owned())),
    };
    CompatibleDto {
        device,
        fixed_rate: o
            .fixed_rate
            .map(|r| toml::Value::Integer(i64::from(r.hz()))),
        src_filter: Some(toml::Value::String(
            src_filter_key(o.src_filter).to_owned(),
        )),
        dither: Some(toml::Value::String(dither_key(o.dither).to_owned())),
        dsd_filter: Some(toml::Value::Integer(dsd_filter_khz(o.dsd_filter))),
        dsd_gain_comp: Some(toml::Value::Boolean(o.dsd_gain_comp)),
        buffer: Some(toml::Value::Integer(i64::from(o.buffer.get()))),
        unknown: BTreeMap::new(),
    }
}

fn optimal_dto(o: &OptimalOpts) -> OptimalDto {
    OptimalDto {
        device: o
            .device
            .as_ref()
            .map(|id| toml::Value::String(id.clone())),
        rate_fallback: Some(toml::Value::String(
            rate_fallback_key(o.rate_fallback).to_owned(),
        )),
        src_filter: Some(toml::Value::String(
            src_filter_key(o.src_filter).to_owned(),
        )),
        dither: Some(toml::Value::String(dither_key(o.dither).to_owned())),
        volume_lock: Some(toml::Value::Boolean(o.volume_lock)),
        dsd_above_dac: Some(toml::Value::String(
            dsd_above_dac_key(o.dsd_above_dac).to_owned(),
        )),
        dsd_filter: Some(toml::Value::Integer(dsd_filter_khz(o.dsd_filter))),
        dsd_gain_comp: Some(toml::Value::Boolean(o.dsd_gain_comp)),
        buffer: Some(toml::Value::Integer(i64::from(o.buffer.get()))),
        device_buffer_ms: Some(toml::Value::Integer(device_buffer_ms_value(
            o.device_buffer,
        ))),
        unknown: BTreeMap::new(),
    }
}

fn strict_dto(o: &StrictOpts) -> StrictDto {
    StrictDto {
        device: o
            .device
            .as_ref()
            .map(|id| toml::Value::String(id.clone())),
        buffer: Some(toml::Value::Integer(i64::from(o.buffer.get()))),
        device_buffer_ms: Some(toml::Value::Integer(device_buffer_ms_value(
            o.device_buffer,
        ))),
        unknown: BTreeMap::new(),
    }
}

/// Файловое представление `ModeSettings`: обратная функция к
/// `load_mode_settings` (§6.27), которой в этом шаге нет (02 §2.6). `schema`
/// всегда записывается равным `2`; поля `unknown` всех уровней всегда пусты —
/// эта функция строит DTO заново, а не переносит незнакомые ключи файла.
pub fn playback_dto(m: &ModeSettings) -> PlaybackDto {
    PlaybackDto {
        schema: Some(2),
        compatible: Some(compatible_dto(&m.compatible)),
        optimal: Some(optimal_dto(&m.optimal)),
        strict: Some(strict_dto(&m.strict)),
        unknown: BTreeMap::new(),
    }
}

/// Строковая форма -> `SrcFilter`, обратная к `src_filter_key`: перебирает те
/// же варианты, которыми пишет писатель, так что строки не могут разойтись
/// (§6.27). Минимально-фазовые легаси-строки (`"steep_short_delay"`,
/// `"slow_short_delay"`) здесь не совпадают ни с одним вариантом — это
/// недопустимое значение (ТЗ-130, И-Т19), обрабатывается как любое другое.
fn parse_src_filter(s: &str) -> Option<SrcFilter> {
    [SrcFilter::Steep, SrcFilter::Slow, SrcFilter::VerySlow]
        .into_iter()
        .find(|v| src_filter_key(*v) == s)
}

/// Числовая форма -> `DsdFilter`, обратная к `dsd_filter_khz` (§6.27).
fn parse_dsd_filter(khz: i64) -> Option<DsdFilter> {
    [DsdFilter::K24, DsdFilter::K30, DsdFilter::K50]
        .into_iter()
        .find(|v| dsd_filter_khz(*v) == khz)
}

/// Строковая форма -> `Dither`, обратная к `dither_key` (§6.27).
fn parse_dither(s: &str) -> Option<Dither> {
    [Dither::Tpdf, Dither::Off]
        .into_iter()
        .find(|v| dither_key(*v) == s)
}

/// Строковая форма -> `RateFallbackRule`, обратная к `rate_fallback_key` (§6.27).
fn parse_rate_fallback(s: &str) -> Option<RateFallbackRule> {
    [
        RateFallbackRule::SameFamily,
        RateFallbackRule::Nearest,
        RateFallbackRule::NoDownsample,
    ]
    .into_iter()
    .find(|v| rate_fallback_key(*v) == s)
}

/// Строковая форма -> `DsdAboveDac`, обратная к `dsd_above_dac_key`. Легаси-
/// строка `"remodulate"` не совпадает с единственным вариантом — недопустимое
/// значение, подстановка `ConvertToPcm` с заметкой (ТЗ-34, §6.27).
fn parse_dsd_above_dac(s: &str) -> Option<DsdAboveDac> {
    [DsdAboveDac::ConvertToPcm]
        .into_iter()
        .find(|v| dsd_above_dac_key(*v) == s)
}

/// Числовая форма (мс) -> `DeviceBuffer`, обратная к `device_buffer_ms_value`
/// (§6.27).
fn parse_device_buffer_ms(ms: i64) -> Option<DeviceBuffer> {
    [
        DeviceBuffer::Ms20,
        DeviceBuffer::Ms40,
        DeviceBuffer::Ms100,
        DeviceBuffer::Ms200,
        DeviceBuffer::Ms400,
    ]
    .into_iter()
    .find(|v| device_buffer_ms_value(*v) == ms)
}

/// Согласование буфера устройства с буфером плеера (И-Р22, ТЗ-131, §6.27 п. 4):
/// если `buffers_allowed(buffer, device_buffer)` — ошибка, подстановка
/// `Ms40`; если и `Ms40` недопустим — наименьшее допустимое значение (при
/// `BufferMs ≥ 100` это всегда не хуже `Ms40`, ветвь оставлена ради полноты).
/// `buffer` не меняется.
fn reconcile_device_buffer(
    buffer: BufferMs,
    device_buffer: DeviceBuffer,
    key: &str,
    notes: &mut Vec<LoadNote>,
) -> DeviceBuffer {
    if buffers_allowed(buffer, device_buffer).is_ok() {
        return device_buffer;
    }
    let corrected = if buffers_allowed(buffer, DeviceBuffer::Ms40).is_ok() {
        DeviceBuffer::Ms40
    } else {
        [
            DeviceBuffer::Ms20,
            DeviceBuffer::Ms40,
            DeviceBuffer::Ms100,
            DeviceBuffer::Ms200,
            DeviceBuffer::Ms400,
        ]
        .into_iter()
        .find(|d| buffers_allowed(buffer, *d).is_ok())
        .unwrap_or(DeviceBuffer::Ms20)
    };
    notes.push(LoadNote {
        key: KeyPath::new(key.to_owned()),
        kind: LoadNoteKind::Adjusted {
            reason: format!(
                "буфер устройства {} мс больше половины буфера плеера {} мс — установлено {} мс",
                device_buffer.ms(),
                buffer.get(),
                corrected.ms()
            )
            .into(),
        },
    });
    corrected
}

/// Заметка «недопустимое значение» (§6.27 п. 3, ТЗ-34, ТЗ-130).
fn invalid_note(key: &str, found: &toml::Value, allowed: &'static str) -> LoadNote {
    LoadNote {
        key: KeyPath::new(key.to_owned()),
        kind: LoadNoteKind::Invalid {
            found: found.to_string().into(),
            allowed,
        },
    }
}

/// Заметка «неизвестный ключ, игнорируется» (§6.27 п. 3, ТЗ-94).
fn unknown_note(key: String) -> LoadNote {
    LoadNote {
        key: KeyPath::new(key),
        kind: LoadNoteKind::Unknown,
    }
}

/// Строковое перечисление: отсутствие ключа — значение по умолчанию без
/// заметки; недопустимое значение — значение по умолчанию и заметка (§6.27 п. 3).
fn read_str_enum<T: Copy>(
    value: Option<&toml::Value>,
    parse: fn(&str) -> Option<T>,
    default: T,
    key: &str,
    allowed: &'static str,
    notes: &mut Vec<LoadNote>,
) -> T {
    let Some(v) = value else { return default };
    match v.as_str().and_then(parse) {
        Some(parsed) => parsed,
        None => {
            notes.push(invalid_note(key, v, allowed));
            default
        }
    }
}

/// Числовое перечисление, симметрично `read_str_enum` (§6.27 п. 3).
fn read_int_enum<T: Copy>(
    value: Option<&toml::Value>,
    parse: fn(i64) -> Option<T>,
    default: T,
    key: &str,
    allowed: &'static str,
    notes: &mut Vec<LoadNote>,
) -> T {
    let Some(v) = value else { return default };
    match v.as_integer().and_then(parse) {
        Some(parsed) => parsed,
        None => {
            notes.push(invalid_note(key, v, allowed));
            default
        }
    }
}

/// Булев ключ (`dsd_gain_comp`, `volume_lock`): §6.27 п. 3.
fn read_bool(value: Option<&toml::Value>, default: bool, key: &str, notes: &mut Vec<LoadNote>) -> bool {
    let Some(v) = value else { return default };
    match v.as_bool() {
        Some(b) => b,
        None => {
            notes.push(invalid_note(key, v, "true или false"));
            default
        }
    }
}

/// Буфер декодирование→вывод: целое в допустимом диапазоне `BufferMs`
/// (`u16::try_from` — без потери точности при приведении `i64` toml, §6.27 п. 3).
fn read_buffer_ms(
    value: Option<&toml::Value>,
    default: BufferMs,
    key: &str,
    notes: &mut Vec<LoadNote>,
) -> BufferMs {
    let Some(v) = value else { return default };
    let parsed = v
        .as_integer()
        .and_then(|i| u16::try_from(i).ok())
        .and_then(BufferMs::new);
    match parsed {
        Some(b) => b,
        None => {
            notes.push(invalid_note(key, v, "целое 100..=10 000"));
            default
        }
    }
}

/// Устройство Совместимого режима: строка -> `Named`, отсутствие ключа ->
/// `SystemDefault`, недопустимое значение -> `SystemDefault` и заметка (§6.27 п. 3).
fn read_shared_device(
    value: Option<&toml::Value>,
    key: &str,
    notes: &mut Vec<LoadNote>,
) -> SharedDeviceChoice {
    let Some(v) = value else {
        return SharedDeviceChoice::SystemDefault;
    };
    match v.as_str() {
        Some(s) => SharedDeviceChoice::Named(SharedDeviceId::new(s)),
        None => {
            notes.push(invalid_note(key, v, "строка — идентификатор устройства"));
            SharedDeviceChoice::SystemDefault
        }
    }
}

/// Устройство Оптимального/Строгого режима (временная `String`-замена
/// `HwDeviceId`, см. doc-комментарий `OptimalOpts::device`; §6.27 п. 3).
fn read_device_string(
    value: Option<&toml::Value>,
    key: &str,
    notes: &mut Vec<LoadNote>,
) -> Option<String> {
    let v = value?;
    match v.as_str() {
        Some(s) => Some(s.to_owned()),
        None => {
            notes.push(invalid_note(key, v, "строка — идентификатор устройства"));
            None
        }
    }
}

/// Принудительная частота вывода Совместимого режима (§6.27 п. 3, ОВ-4).
fn read_fixed_rate(
    value: Option<&toml::Value>,
    key: &str,
    notes: &mut Vec<LoadNote>,
) -> Option<SampleRate> {
    let v = value?;
    let parsed = v
        .as_integer()
        .and_then(|i| u32::try_from(i).ok())
        .and_then(SampleRate::new);
    match parsed {
        Some(r) => Some(r),
        None => {
            notes.push(invalid_note(key, v, "целое — частота в Гц"));
            None
        }
    }
}

/// `[playback.compatible]` -> `CompatibleOpts` (§6.27 п. 3).
fn parse_compatible(dto: CompatibleDto, notes: &mut Vec<LoadNote>) -> CompatibleOpts {
    let default = CompatibleOpts::default();
    let opts = CompatibleOpts {
        device: read_shared_device(dto.device.as_ref(), "playback.compatible.device", notes),
        fixed_rate: read_fixed_rate(dto.fixed_rate.as_ref(), "playback.compatible.fixed_rate", notes),
        src_filter: read_str_enum(
            dto.src_filter.as_ref(),
            parse_src_filter,
            default.src_filter,
            "playback.compatible.src_filter",
            "\"steep\" / \"slow\" / \"very_slow\"",
            notes,
        ),
        dither: read_str_enum(
            dto.dither.as_ref(),
            parse_dither,
            default.dither,
            "playback.compatible.dither",
            "\"tpdf\" / \"off\"",
            notes,
        ),
        dsd_filter: read_int_enum(
            dto.dsd_filter.as_ref(),
            parse_dsd_filter,
            default.dsd_filter,
            "playback.compatible.dsd_filter",
            "24, 30 или 50",
            notes,
        ),
        dsd_gain_comp: read_bool(
            dto.dsd_gain_comp.as_ref(),
            default.dsd_gain_comp,
            "playback.compatible.dsd_gain_comp",
            notes,
        ),
        buffer: read_buffer_ms(dto.buffer.as_ref(), default.buffer, "playback.compatible.buffer", notes),
    };
    for key in dto.unknown.keys() {
        notes.push(unknown_note(format!("playback.compatible.{key}")));
    }
    opts
}

/// `[playback.optimal]` -> `OptimalOpts` (§6.27 п. 3).
fn parse_optimal(dto: OptimalDto, notes: &mut Vec<LoadNote>) -> OptimalOpts {
    let default = OptimalOpts::default();
    let opts = OptimalOpts {
        device: read_device_string(dto.device.as_ref(), "playback.optimal.device", notes),
        rate_fallback: read_str_enum(
            dto.rate_fallback.as_ref(),
            parse_rate_fallback,
            default.rate_fallback,
            "playback.optimal.rate_fallback",
            "\"same_family\" / \"nearest\" / \"no_downsample\"",
            notes,
        ),
        src_filter: read_str_enum(
            dto.src_filter.as_ref(),
            parse_src_filter,
            default.src_filter,
            "playback.optimal.src_filter",
            "\"steep\" / \"slow\" / \"very_slow\"",
            notes,
        ),
        dither: read_str_enum(
            dto.dither.as_ref(),
            parse_dither,
            default.dither,
            "playback.optimal.dither",
            "\"tpdf\" / \"off\"",
            notes,
        ),
        volume_lock: read_bool(
            dto.volume_lock.as_ref(),
            default.volume_lock,
            "playback.optimal.volume_lock",
            notes,
        ),
        dsd_above_dac: read_str_enum(
            dto.dsd_above_dac.as_ref(),
            parse_dsd_above_dac,
            default.dsd_above_dac,
            "playback.optimal.dsd_above_dac",
            "\"convert_to_pcm\"",
            notes,
        ),
        dsd_filter: read_int_enum(
            dto.dsd_filter.as_ref(),
            parse_dsd_filter,
            default.dsd_filter,
            "playback.optimal.dsd_filter",
            "24, 30 или 50",
            notes,
        ),
        dsd_gain_comp: read_bool(
            dto.dsd_gain_comp.as_ref(),
            default.dsd_gain_comp,
            "playback.optimal.dsd_gain_comp",
            notes,
        ),
        buffer: read_buffer_ms(dto.buffer.as_ref(), default.buffer, "playback.optimal.buffer", notes),
        device_buffer: read_int_enum(
            dto.device_buffer_ms.as_ref(),
            parse_device_buffer_ms,
            default.device_buffer,
            "playback.optimal.device_buffer_ms",
            "20, 40, 100, 200 или 400",
            notes,
        ),
    };
    for key in dto.unknown.keys() {
        notes.push(unknown_note(format!("playback.optimal.{key}")));
    }
    opts
}

/// `[playback.strict]` -> `StrictOpts` (§6.27 п. 3; И-Т1 — только `device`,
/// `buffer`, `device_buffer`).
fn parse_strict(dto: StrictDto, notes: &mut Vec<LoadNote>) -> StrictOpts {
    let default = StrictOpts::default();
    let opts = StrictOpts {
        device: read_device_string(dto.device.as_ref(), "playback.strict.device", notes),
        buffer: read_buffer_ms(dto.buffer.as_ref(), default.buffer, "playback.strict.buffer", notes),
        device_buffer: read_int_enum(
            dto.device_buffer_ms.as_ref(),
            parse_device_buffer_ms,
            default.device_buffer,
            "playback.strict.device_buffer_ms",
            "20, 40, 100, 200 или 400",
            notes,
        ),
    };
    for key in dto.unknown.keys() {
        notes.push(unknown_note(format!("playback.strict.{key}")));
    }
    opts
}

/// Загрузка `ModeSettings` из DTO (§6.27 пп. 1–4, ТЗ-97, ТЗ-94, ТЗ-34,
/// ТЗ-130, ТЗ-131, ТЗ-124): нет файла -> `default()` без заметок; нет раздела
/// `[playback]` или `schema ≠ 2` -> `default()` и одна заметка «аудио-
/// настройки файла не распознаны»; иначе — известный ключ с допустимым
/// значением идёт в поле, недопустимое значение заменяется значением по
/// умолчанию с заметкой, неизвестный ключ игнорируется с заметкой. После
/// разбора Оптимального и Строгого режимов `device_buffer` согласуется с
/// `buffer` по И-Р22 (`reconcile_device_buffer`, §6.27 п. 4).
pub fn load_mode_settings(dto: Option<PlaybackDto>) -> (ModeSettings, Vec<LoadNote>) {
    let Some(dto) = dto else {
        return (ModeSettings::default(), Vec::new());
    };

    if dto.schema != Some(2) {
        let found = dto
            .schema
            .map_or_else(|| "отсутствует".to_owned(), |s| s.to_string());
        let note = LoadNote {
            key: KeyPath::new("playback.schema"),
            kind: LoadNoteKind::Invalid {
                found: found.into(),
                allowed: "аудио-настройки файла не распознаны",
            },
        };
        return (ModeSettings::default(), vec![note]);
    }

    let mut notes = Vec::new();
    let compatible = parse_compatible(dto.compatible.unwrap_or_default(), &mut notes);
    let mut optimal = parse_optimal(dto.optimal.unwrap_or_default(), &mut notes);
    let mut strict = parse_strict(dto.strict.unwrap_or_default(), &mut notes);
    optimal.device_buffer = reconcile_device_buffer(
        optimal.buffer,
        optimal.device_buffer,
        "playback.optimal.device_buffer_ms",
        &mut notes,
    );
    strict.device_buffer = reconcile_device_buffer(
        strict.buffer,
        strict.device_buffer,
        "playback.strict.device_buffer_ms",
        &mut notes,
    );
    for key in dto.unknown.keys() {
        notes.push(unknown_note(format!("playback.{key}")));
    }
    notes.sort_by(|a, b| a.key.cmp(&b.key));

    (
        ModeSettings {
            compatible,
            optimal,
            strict,
        },
        notes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `playback_dto` для настроек по умолчанию: `schema = 2`, известные
    /// строковые/числовые формы ключей по §6.27, `unknown` пусты на всех
    /// уровнях.
    #[test]
    fn default_mode_settings_serialize_known_keys() {
        let dto = playback_dto(&ModeSettings::default());
        assert_eq!(dto.schema, Some(2));
        assert!(dto.unknown.is_empty());

        let compatible = dto.compatible.expect("compatible present");
        assert_eq!(compatible.device, None);
        assert_eq!(compatible.fixed_rate, None);
        assert_eq!(
            compatible.src_filter,
            Some(toml::Value::String("steep".to_owned()))
        );
        assert_eq!(compatible.dsd_filter, Some(toml::Value::Integer(30)));
        assert_eq!(compatible.dsd_gain_comp, Some(toml::Value::Boolean(true)));
        assert_eq!(compatible.buffer, Some(toml::Value::Integer(1500)));
        assert!(compatible.unknown.is_empty());

        let optimal = dto.optimal.expect("optimal present");
        assert_eq!(
            optimal.rate_fallback,
            Some(toml::Value::String("same_family".to_owned()))
        );
        assert_eq!(
            optimal.dsd_above_dac,
            Some(toml::Value::String("convert_to_pcm".to_owned()))
        );
        assert_eq!(optimal.device_buffer_ms, Some(toml::Value::Integer(40)));

        let strict = dto.strict.expect("strict present");
        assert_eq!(strict.device_buffer_ms, Some(toml::Value::Integer(40)));
        assert_eq!(strict.buffer, Some(toml::Value::Integer(1500)));
    }

    /// §7.2 `missing_file_starts_compatible` (ТЗ-124): нет файла -> `default()`,
    /// заметок нет.
    #[test]
    fn missing_file_starts_compatible() {
        let (settings, notes) = load_mode_settings(None);
        assert_eq!(settings, ModeSettings::default());
        assert!(notes.is_empty());
    }

    /// §7.2 `legacy_settings_load_defaults_one_note` (ТЗ-97): раздел без
    /// распознанной `schema` (старые файлы, мусор) -> `default()` и ровно одна
    /// заметка; остальные поля DTO не разбираются.
    #[test]
    fn legacy_settings_load_defaults_one_note() {
        let dto = PlaybackDto {
            schema: None,
            compatible: Some(CompatibleDto {
                buffer: Some(toml::Value::Integer(999)),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (settings, notes) = load_mode_settings(Some(dto));
        assert_eq!(settings, ModeSettings::default());
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key, KeyPath::new("playback.schema"));
        assert!(matches!(notes[0].kind, LoadNoteKind::Invalid { .. }));
    }

    /// §7.2 `unknown_key_ignored_logged` (ТЗ-94): ключ `volume` в
    /// `[playback.strict]` -> игнорируется, одна заметка, другие режимы без
    /// изменений.
    #[test]
    fn unknown_key_ignored_logged() {
        let mut strict_unknown = BTreeMap::new();
        strict_unknown.insert("volume".to_owned(), toml::Value::Integer(50));
        let dto = PlaybackDto {
            schema: Some(2),
            strict: Some(StrictDto {
                unknown: strict_unknown,
                ..Default::default()
            }),
            ..Default::default()
        };
        let (settings, notes) = load_mode_settings(Some(dto));
        assert_eq!(settings, ModeSettings::default());
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key, KeyPath::new("playback.strict.volume"));
        assert!(matches!(notes[0].kind, LoadNoteKind::Unknown));
    }

    /// §7.2 `remodulation_loads_as_convert` (ТЗ-34): `dsd_above_dac =
    /// "remodulate"` -> `ConvertToPcm` и заметка.
    #[test]
    fn remodulation_loads_as_convert() {
        let dto = PlaybackDto {
            schema: Some(2),
            optimal: Some(OptimalDto {
                dsd_above_dac: Some(toml::Value::String("remodulate".to_owned())),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (settings, notes) = load_mode_settings(Some(dto));
        assert_eq!(settings.optimal.dsd_above_dac, DsdAboveDac::ConvertToPcm);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key, KeyPath::new("playback.optimal.dsd_above_dac"));
        assert!(matches!(notes[0].kind, LoadNoteKind::Invalid { .. }));
    }

    /// §7.2 `src_filter_default_and_min_phase_not_selectable` (ТЗ-130, И-Т19;
    /// часть про минимально-фазовые значения): `src_filter =
    /// "slow_short_delay"` в файле -> `Steep` и заметка.
    #[test]
    fn src_filter_default_and_min_phase_not_selectable() {
        let dto = PlaybackDto {
            schema: Some(2),
            compatible: Some(CompatibleDto {
                src_filter: Some(toml::Value::String("slow_short_delay".to_owned())),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (settings, notes) = load_mode_settings(Some(dto));
        assert_eq!(settings.compatible.src_filter, SrcFilter::Steep);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key, KeyPath::new("playback.compatible.src_filter"));
        assert!(matches!(notes[0].kind, LoadNoteKind::Invalid { .. }));
    }

    /// §6.27 п. 4 (И-Р22, ТЗ-131): Оптимальный режим, `buffer = 100`,
    /// `device_buffer_ms = 400` (нужно 800 мс) -> `Ms40` (нужно 80 мс, есть) и
    /// одна заметка `Adjusted`.
    #[test]
    fn optimal_device_buffer_reconciled_to_ms40() {
        let dto = PlaybackDto {
            schema: Some(2),
            optimal: Some(OptimalDto {
                buffer: Some(toml::Value::Integer(100)),
                device_buffer_ms: Some(toml::Value::Integer(400)),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (settings, notes) = load_mode_settings(Some(dto));
        assert_eq!(settings.optimal.device_buffer, DeviceBuffer::Ms40);
        assert_eq!(
            settings.optimal.buffer,
            BufferMs::new(100).expect("100 мс в допустимом диапазоне BufferMs")
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].key,
            KeyPath::new("playback.optimal.device_buffer_ms")
        );
        match &notes[0].kind {
            LoadNoteKind::Adjusted { reason } => {
                assert_eq!(
                    reason.as_ref(),
                    "буфер устройства 400 мс больше половины буфера плеера 100 мс — установлено 40 мс"
                );
            }
            other => panic!("ожидалась заметка Adjusted, получено {other:?}"),
        }
    }

    /// §6.27 п. 4 (И-Р22, ТЗ-131): Строгий режим, та же согласующая коррекция.
    #[test]
    fn strict_device_buffer_reconciled_to_ms40() {
        let dto = PlaybackDto {
            schema: Some(2),
            strict: Some(StrictDto {
                buffer: Some(toml::Value::Integer(100)),
                device_buffer_ms: Some(toml::Value::Integer(400)),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (settings, notes) = load_mode_settings(Some(dto));
        assert_eq!(settings.strict.device_buffer, DeviceBuffer::Ms40);
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].key,
            KeyPath::new("playback.strict.device_buffer_ms")
        );
        match &notes[0].kind {
            LoadNoteKind::Adjusted { reason } => {
                assert_eq!(
                    reason.as_ref(),
                    "буфер устройства 400 мс больше половины буфера плеера 100 мс — установлено 40 мс"
                );
            }
            other => panic!("ожидалась заметка Adjusted, получено {other:?}"),
        }
    }

    /// Круговой проход `playback_dto` -> `load_mode_settings` для настроек по
    /// умолчанию: без заметок, `ModeSettings` не меняется (02 §2.6).
    #[test]
    fn round_trip_default_mode_settings() {
        let m = ModeSettings::default();
        let dto = playback_dto(&m);
        assert_eq!(load_mode_settings(Some(dto)), (m, Vec::new()));
    }

    /// Круговой проход для нетривиального `ModeSettings`: несколько полей
    /// каждого режима изменены, буферы уже согласованы (И-Р22) -> заметок нет.
    #[test]
    fn round_trip_non_default_mode_settings() {
        let m = ModeSettings {
            compatible: CompatibleOpts {
                device: SharedDeviceChoice::Named(SharedDeviceId::new("hw:CARD=X,DEV=0")),
                fixed_rate: SampleRate::new(96_000),
                src_filter: SrcFilter::Slow,
                dither: Dither::Off,
                dsd_filter: DsdFilter::K24,
                dsd_gain_comp: false,
                buffer: BufferMs::new(2000).expect("2000 мс в допустимом диапазоне BufferMs"),
            },
            optimal: OptimalOpts {
                device: Some("optimal-dev".to_owned()),
                rate_fallback: RateFallbackRule::Nearest,
                src_filter: SrcFilter::VerySlow,
                dither: Dither::Off,
                volume_lock: true,
                dsd_above_dac: DsdAboveDac::ConvertToPcm,
                dsd_filter: DsdFilter::K50,
                dsd_gain_comp: false,
                buffer: BufferMs::new(800).expect("800 мс в допустимом диапазоне BufferMs"),
                device_buffer: DeviceBuffer::Ms400,
            },
            strict: StrictOpts {
                device: Some("strict-dev".to_owned()),
                buffer: BufferMs::new(400).expect("400 мс в допустимом диапазоне BufferMs"),
                device_buffer: DeviceBuffer::Ms100,
            },
        };
        let dto = playback_dto(&m);
        assert_eq!(load_mode_settings(Some(dto)), (m, Vec::new()));
    }
}
