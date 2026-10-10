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

use super::playback::{
    CompatibleOpts, Dither, DsdAboveDac, DsdFilter, ModeSettings, OptimalOpts, RateFallbackRule,
    SharedDeviceChoice, SrcFilter, StrictOpts,
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
}
