//! Подсистема визуализации аудио (ТЗ 5.1).
//!
//! Этот модуль содержит настройки визуализации и общие модели. Три режима
//! («осциллограмма», «спектрограмма», «анализатор спектра»), независимые
//! настройки для каждого типа, стерео/моно. Обработка (FFT, воркеры) — в
//! последующих этапах (6.2+).
//!
//! Настройки персистентны в `config.toml` как `[visualization]` и
//! вложенные `[visualization.<type>]` (см. ТЗ §10). Значения по умолчанию —
//! из ТЗ §5. TOML не поддерживает `#[serde(flatten)]`, поэтому общие поля
//! (§5.1) дублируются в каждом типе.

use serde::{Deserialize, Serialize};

/// Режим визуализации.
///
/// Сериализуется в нижнем регистре (`off`/`oscilloscope`/`spectrogram`/
/// `spectrum`), как в примере конфига ТЗ §10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VisualizationMode {
    /// Плейсхолдер (артист/альбом/формат).
    #[default]
    Off,
    /// Полнотрековая: envelope сигнала по всему треку.
    Oscilloscope,
    /// Полнотрековая: частота × время, цветовая карта.
    Spectrogram,
    /// Мгновенная: полосы по частотам в текущий момент.
    Spectrum,
}

impl VisualizationMode {
    pub const ALL: [Self; 4] = [
        Self::Off,
        Self::Oscilloscope,
        Self::Spectrogram,
        Self::Spectrum,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Oscilloscope => "oscilloscope",
            Self::Spectrogram => "spectrogram",
            Self::Spectrum => "spectrum",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "oscilloscope" => Self::Oscilloscope,
            "spectrogram" => Self::Spectrogram,
            "spectrum" => Self::Spectrum,
            _ => Self::Off,
        }
    }

    /// Порядковый номер для Slint (`viz-mode`): 0 off, 1 osc, 2 spectro, 3 spectrum.
    pub fn index(&self) -> i32 {
        match self {
            Self::Off => 0,
            Self::Oscilloscope => 1,
            Self::Spectrogram => 2,
            Self::Spectrum => 3,
        }
    }

    pub fn from_index(i: i32) -> Self {
        Self::ALL.iter().copied().find(|m| m.index() == i).unwrap_or(Self::Off)
    }

    /// Полнотрековые визуализации (строятся один раз и кэшируются).
    pub fn is_fulltrack(&self) -> bool {
        matches!(self, Self::Oscilloscope | Self::Spectrogram)
    }

    /// Мгновенная визуализация (обновляется непрерывно).
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Spectrum)
    }
}

/// Режим каналов для всех типов визуализации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChannelMode {
    /// L и R суммируются.
    Mono,
    /// L и R раздельно.
    #[default]
    Stereo,
}

/// Общие дефолты полей всех типов (ТЗ §5.1).
pub mod defaults {
    pub const BG_COLOR: &str = "#11111b";
    pub const FG_COLOR: &str = "#cba6f7";
    pub fn color() -> String {
        BG_COLOR.to_string()
    }
    pub fn fg_color() -> String {
        FG_COLOR.to_string()
    }
    pub fn sensitivity() -> f32 {
        1.0
    }
    pub fn line_width() -> f32 {
        1.2
    }
    pub fn max_columns() -> u32 {
        2000
    }
    pub fn max_frames() -> u32 {
        4000
    }
    pub const fn fft_size() -> u32 {
        2048
    }
    pub const fn freq_min() -> u32 {
        20
    }
    pub const fn freq_max() -> u32 {
        22_000
    }
    pub fn gain_db() -> f32 {
        20.0
    }
    pub fn range_db() -> f32 {
        80.0
    }
    pub fn high_boost_db() -> f32 {
        6.0
    }
    pub const fn bands() -> u32 {
        32
    }
    pub fn smoothing() -> f32 {
        0.7
    }
    pub const fn peak_decay_ms() -> u32 {
        400
    }
    pub const fn bar_gap() -> u32 {
        2
    }
    pub const fn bar_radius() -> u32 {
        2
    }
    pub const fn true_() -> bool {
        true
    }
}

/// Настройки осциллограммы (полнотрековая, ТЗ §5.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OscilloscopeCfg {
    #[serde(default)]
    pub channels: ChannelMode,
    #[serde(default = "defaults::color")]
    pub bg_color: String,
    #[serde(default = "defaults::fg_color")]
    pub fg_color: String,
    #[serde(default = "defaults::sensitivity")]
    pub sensitivity: f32,
    /// Толщина линии, 0.5–4.0.
    #[serde(default = "defaults::line_width")]
    pub line_width: f32,
    /// Рисовать осевую линию.
    #[serde(default)]
    pub draw_center_line: bool,
    /// Максимум колонок (min/max на колонку), 512–8192.
    #[serde(default = "defaults::max_columns")]
    pub max_columns: u32,
    /// Держать изображение в RAM.
    #[serde(default = "defaults::true_")]
    pub cache_in_memory: bool,
    /// Сохранять в `~/.cache/<app>/viz/`.
    #[serde(default = "defaults::true_")]
    pub cache_on_disk: bool,
}

impl Default for OscilloscopeCfg {
    fn default() -> Self {
        Self {
            channels: ChannelMode::Stereo,
            bg_color: defaults::color(),
            fg_color: defaults::fg_color(),
            sensitivity: defaults::sensitivity(),
            line_width: defaults::line_width(),
            draw_center_line: false,
            max_columns: defaults::max_columns(),
            cache_in_memory: true,
            cache_on_disk: true,
        }
    }
}

/// Шкала частот спектрограммы/анализатора.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FreqScale {
    Linear,
    #[default]
    Log,
    Mel,
}

/// Тип окна БПФ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WindowType {
    #[default]
    Hann,
    Hamming,
    Blackman,
}

/// Цветовая палитра спектрограммы (ТЗ §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Palette {
    Solid,
    #[default]
    Magma,
    Viridis,
    Plasma,
    Inferno,
    Gray,
    Thermal,
    Rainbow,
}

/// Настройки спектрограммы (полнотрековая, ТЗ §5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectrogramCfg {
    #[serde(default)]
    pub channels: ChannelMode,
    #[serde(default = "defaults::color")]
    pub bg_color: String,
    #[serde(default = "defaults::fg_color")]
    pub fg_color: String,
    #[serde(default = "defaults::sensitivity")]
    pub sensitivity: f32,
    /// Размер окна БПФ (512/1024/2048/4096/8192).
    #[serde(default = "defaults::fft_size")]
    pub fft_size: u32,
    #[serde(default)]
    pub window_type: WindowType,
    #[serde(default)]
    pub freq_scale: FreqScale,
    /// Нижняя частота, Гц.
    #[serde(default = "defaults::freq_min")]
    pub freq_min: u32,
    /// Верхняя частота, Гц.
    #[serde(default = "defaults::freq_max")]
    pub freq_max: u32,
    /// Усиление, дБ (−40…+100).
    #[serde(default = "defaults::gain_db")]
    pub gain_db: f32,
    /// Динамический диапазон, дБ (1–200).
    #[serde(default = "defaults::range_db")]
    pub range_db: f32,
    /// Подъём ВЧ, дБ/декада (0–60).
    #[serde(default = "defaults::high_boost_db")]
    pub high_boost_db: f32,
    #[serde(default)]
    pub palette: Palette,
    /// Максимум столбцов (адаптивный hop), 512–8192.
    #[serde(default = "defaults::max_frames")]
    pub max_frames: u32,
    /// Компенсация спада CIC для DSD.
    #[serde(default = "defaults::true_")]
    pub dsd_cic_compensation: bool,
    #[serde(default = "defaults::true_")]
    pub cache_in_memory: bool,
    #[serde(default = "defaults::true_")]
    pub cache_on_disk: bool,
}

impl Default for SpectrogramCfg {
    fn default() -> Self {
        Self {
            channels: ChannelMode::Stereo,
            bg_color: defaults::color(),
            fg_color: defaults::fg_color(),
            sensitivity: defaults::sensitivity(),
            fft_size: defaults::fft_size(),
            window_type: WindowType::Hann,
            freq_scale: FreqScale::Log,
            freq_min: defaults::freq_min(),
            freq_max: defaults::freq_max(),
            gain_db: defaults::gain_db(),
            range_db: defaults::range_db(),
            high_boost_db: defaults::high_boost_db(),
            palette: Palette::Magma,
            max_frames: defaults::max_frames(),
            dsd_cic_compensation: true,
            cache_in_memory: true,
            cache_on_disk: true,
        }
    }
}

/// Шкала громкости анализатора.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LevelScale {
    Linear,
    #[default]
    Log,
}

/// Настройки анализатора спектра (мгновенный, ТЗ §5.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectrumCfg {
    #[serde(default)]
    pub channels: ChannelMode,
    #[serde(default = "defaults::color")]
    pub bg_color: String,
    #[serde(default = "defaults::fg_color")]
    pub fg_color: String,
    #[serde(default = "defaults::sensitivity")]
    pub sensitivity: f32,
    /// Количество полос, 4–128.
    #[serde(default = "defaults::bands")]
    pub bands: u32,
    #[serde(default)]
    pub freq_scale: FreqScale,
    #[serde(default)]
    pub level_scale: LevelScale,
    /// Сглаживание (0 = выкл), 0.0–0.99.
    #[serde(default = "defaults::smoothing")]
    pub smoothing: f32,
    /// Удержание пиков.
    #[serde(default)]
    pub peak_hold: bool,
    /// Время спада пиков, мс.
    #[serde(default = "defaults::peak_decay_ms")]
    pub peak_decay_ms: u32,
    /// Зазор между полосами, px (0–10).
    #[serde(default = "defaults::bar_gap")]
    pub bar_gap: u32,
    /// Скругление полос, px (0–12).
    #[serde(default = "defaults::bar_radius")]
    pub bar_radius: u32,
    /// Градиент по высоте полосы.
    #[serde(default = "defaults::true_")]
    pub gradient: bool,
    /// Компенсация спада CIC для DSD.
    #[serde(default = "defaults::true_")]
    pub dsd_cic_compensation: bool,
}

impl Default for SpectrumCfg {
    fn default() -> Self {
        Self {
            channels: ChannelMode::Stereo,
            bg_color: defaults::color(),
            fg_color: defaults::fg_color(),
            sensitivity: defaults::sensitivity(),
            bands: defaults::bands(),
            freq_scale: FreqScale::Log,
            level_scale: LevelScale::Log,
            smoothing: defaults::smoothing(),
            peak_hold: false,
            peak_decay_ms: defaults::peak_decay_ms(),
            bar_gap: defaults::bar_gap(),
            bar_radius: defaults::bar_radius(),
            gradient: true,
            dsd_cic_compensation: true,
        }
    }
}

/// Корневые настройки визуализации (`[visualization]` в config.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualizerSettings {
    #[serde(default)]
    pub mode: VisualizationMode,
    /// Не строить полнотрековые визуализации для DSD.
    #[serde(default = "defaults::true_")]
    pub skip_fulltrack_for_dsd: bool,
    #[serde(default)]
    pub oscilloscope: OscilloscopeCfg,
    #[serde(default)]
    pub spectrogram: SpectrogramCfg,
    #[serde(default)]
    pub spectrum: SpectrumCfg,
}

impl Default for VisualizerSettings {
    fn default() -> Self {
        Self {
            mode: VisualizationMode::Off,
            skip_fulltrack_for_dsd: true,
            oscilloscope: OscilloscopeCfg::default(),
            spectrogram: SpectrogramCfg::default(),
            spectrum: SpectrumCfg::default(),
        }
    }
}

/// Лёгкий снимок настроек для фоновых воркеров (по образцу `CoverConfig`).
#[derive(Debug, Clone)]
pub struct VisualizerConfig {
    pub mode: VisualizationMode,
    pub skip_fulltrack_for_dsd: bool,
    pub oscilloscope: OscilloscopeCfg,
    pub spectrogram: SpectrogramCfg,
    pub spectrum: SpectrumCfg,
}

impl VisualizerConfig {
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        let v = &s.visualization;
        Self {
            mode: v.mode,
            skip_fulltrack_for_dsd: v.skip_fulltrack_for_dsd,
            oscilloscope: v.oscilloscope.clone(),
            spectrogram: v.spectrogram.clone(),
            spectrum: v.spectrum.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::defaults as d;
    use super::*;

    #[test]
    fn mode_round_trip_serde() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Wrap {
            mode: VisualizationMode,
        }
        for m in VisualizationMode::ALL {
            let w = Wrap { mode: m };
            let s = toml::to_string(&w).unwrap();
            let back: Wrap = toml::from_str(&s).unwrap();
            assert_eq!(m, back.mode, "round-trip of {m:?}");
            assert!(s.contains(&format!("mode = \"{}\"", m.as_str())));
        }
    }

    #[test]
    fn mode_str_round_trip() {
        for m in VisualizationMode::ALL {
            assert_eq!(VisualizationMode::parse(m.as_str()), m);
            assert_eq!(VisualizationMode::from_index(m.index()), m);
        }
        assert_eq!(VisualizationMode::parse("wat"), VisualizationMode::Off);
        assert_eq!(VisualizationMode::from_index(99), VisualizationMode::Off);
    }

    #[test]
    fn mode_classification() {
        assert!(VisualizationMode::Oscilloscope.is_fulltrack());
        assert!(VisualizationMode::Spectrogram.is_fulltrack());
        assert!(!VisualizationMode::Spectrum.is_fulltrack());
        assert!(VisualizationMode::Spectrum.is_live());
        assert!(!VisualizationMode::Off.is_live());
    }

    #[test]
    fn defaults_match_tz() {
        let o = OscilloscopeCfg::default();
        assert_eq!(o.line_width, 1.2);
        assert_eq!(o.max_columns, 2000);
        assert!(o.cache_in_memory && o.cache_on_disk);
        let sg = SpectrogramCfg::default();
        assert_eq!(sg.fft_size, 2048);
        assert_eq!(sg.window_type, WindowType::Hann);
        assert_eq!(sg.freq_scale, FreqScale::Log);
        assert_eq!(sg.freq_min, 20);
        assert_eq!(sg.freq_max, 22_000);
        assert_eq!(sg.gain_db, 20.0);
        assert_eq!(sg.range_db, 80.0);
        assert_eq!(sg.high_boost_db, 6.0);
        assert_eq!(sg.palette, Palette::Magma);
        assert_eq!(sg.max_frames, 4000);
        assert!(sg.dsd_cic_compensation);
        let sp = SpectrumCfg::default();
        assert_eq!(sp.bands, 32);
        assert_eq!(sp.level_scale, LevelScale::Log);
        assert_eq!(sp.smoothing, 0.7);
        assert_eq!(sp.peak_decay_ms, 400);
        assert_eq!(sp.bar_gap, 2);
        assert_eq!(sp.bar_radius, 2);
        assert!(sp.gradient);
    }

    #[test]
    fn visualizer_settings_serde_round_trip() {
        let mut v = VisualizerSettings::default();
        v.mode = VisualizationMode::Spectrum;
        v.spectrum.bands = 64;
        v.spectrogram.palette = Palette::Viridis;
        let s = toml::to_string(&v).unwrap();
        let back: VisualizerSettings = toml::from_str(&s).unwrap();
        assert_eq!(back.mode, VisualizationMode::Spectrum);
        assert_eq!(back.spectrum.bands, 64);
        assert_eq!(back.spectrogram.palette, Palette::Viridis);
        assert!(s.contains("[spectrogram]"));
    }

    #[test]
    fn partial_config_falls_back_to_defaults() {
        // Только mode — остальное дефолты (валидация загрузки конфига).
        let v: VisualizerSettings = toml::from_str("mode = \"spectrogram\"").unwrap();
        assert_eq!(v.mode, VisualizationMode::Spectrogram);
        assert_eq!(d::fft_size(), v.spectrogram.fft_size);
        assert_eq!(VisualizerSettings::default().spectrum.bands, v.spectrum.bands);
    }

    #[test]
    fn config_from_settings_snapshot() {
        let s = crate::settings::Settings::default();
        let cfg = VisualizerConfig::from_settings(&s);
        assert_eq!(cfg.mode, VisualizationMode::Off);
        assert_eq!(cfg.spectrum.bands, 32);
    }
}