use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    /// Human-readable label (currently unused by the UI which uses icons).
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "Off",
            RepeatMode::All => "All",
            RepeatMode::One => "One",
        }
    }

    pub fn next(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

/// Per-column configuration: display title, default weight, min/max
/// constraints, visibility and optional column type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ColumnCfg {
    /// Display title shown in the playlist header and settings dialog.
    #[serde(default = "default_cfg_title")]
    pub title: String,
    /// Default weight for proportional width distribution (0..100 scale,
    /// normalised across visible columns). Replaced by user-dragged
    /// `column_widths` when present.
    #[serde(default)]
    pub priority: f32,
    /// Hard minimum width in physical pixels.
    #[serde(default)]
    pub min_width: f32,
    /// Absolute pixel cap (mutually exclusive with `max_width_percent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<f32>,
    /// Relative cap as a fraction of the container width (mutually exclusive
    /// with `max_width`). When both are present the pixel cap wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width_percent: Option<f32>,
    /// Whether the column is visible by default.
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Optional column type override. `None` / `"data"` — normal metadata
    /// column; `"now-playing"` — narrow marker showing ▶ for the current row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_type: Option<String>,
    /// User-dragged width as a percentage (0..100). `None` when the user has
    /// never dragged this column — falls back to `priority` for distribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
}

fn default_cfg_title() -> String {
    String::new()
}

fn default_true() -> bool {
    true
}

impl ColumnCfg {
    /// Convert to the layout constraint used by [`playlist_layout::resolve_widths`].
    pub fn to_limit(&self) -> crate::playlist_layout::ColumnLimit {
        crate::playlist_layout::ColumnLimit {
            min_px: self.min_width,
            max_px: self.max_width,
            max_pct: self.max_width_percent,
        }
    }
}

/// Identifiers for the playlist columns (order matches the display order in
/// the playlist table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
pub enum ColumnId {
    #[default]
    NowPlaying,
    TrackNumber,
    Title,
    Artist,
    Album,
    Genre,
    Year,
    Format,
    Bitrate,
    BitDepth,
    SampleRate,
    Duration,
    FileName,
    FilePath,
}

impl ColumnId {
    pub const ALL: [ColumnId; 14] = [
        ColumnId::NowPlaying,
        ColumnId::TrackNumber,
        ColumnId::Title,
        ColumnId::Artist,
        ColumnId::Album,
        ColumnId::Genre,
        ColumnId::Year,
        ColumnId::Format,
        ColumnId::Bitrate,
        ColumnId::BitDepth,
        ColumnId::SampleRate,
        ColumnId::Duration,
        ColumnId::FileName,
        ColumnId::FilePath,
    ];

    /// Column key used for persistence matching (stable across renames).
    pub fn key(self) -> &'static str {
        match self {
            ColumnId::NowPlaying => "now_playing",
            ColumnId::TrackNumber => "track",
            ColumnId::Title => "title",
            ColumnId::Artist => "artist",
            ColumnId::Album => "album",
            ColumnId::Genre => "genre",
            ColumnId::Year => "year",
            ColumnId::Format => "format",
            ColumnId::Bitrate => "bitrate",
            ColumnId::BitDepth => "bit_depth",
            ColumnId::SampleRate => "sample_rate",
            ColumnId::Duration => "duration",
            ColumnId::FileName => "file_name",
            ColumnId::FilePath => "file_path",
        }
    }

    /// Resolve a column from its stable key, or `None` if unknown.
    pub fn from_key(key: &str) -> Option<ColumnId> {
        ColumnId::ALL.iter().copied().find(|c| c.key() == key)
    }
}

/// Источники обложек альбома. Порядок объявления = порядок по умолчанию.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum CoverSource {
    /// Файл обложки в папке с альбомом (cover.jpg, folder.png, ...).
    #[default]
    Folder,
    /// Встроенная обложка (APIC/covr/pictures) внутри самого файла.
    Embedded,
    /// Поиск по iTunes Search API в интернете.
    Internet,
}

impl CoverSource {
    pub const ALL: [CoverSource; 3] = [
        CoverSource::Folder,
        CoverSource::Embedded,
        CoverSource::Internet,
    ];

    pub fn key(self) -> &'static str {
        match self {
            CoverSource::Folder => "folder",
            CoverSource::Embedded => "embedded",
            CoverSource::Internet => "internet",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CoverSource::Folder => "From folder (cover.jpg, ...)",
            CoverSource::Embedded => "Embedded in file (APIC)",
            CoverSource::Internet => "From internet (iTunes)",
        }
    }

    pub fn from_key(key: &str) -> Option<CoverSource> {
        CoverSource::ALL.iter().copied().find(|c| c.key() == key)
    }
}

/// Имена файлов обложек, искомых в папке с альбомом (по умолчанию).
pub fn default_cover_folder_names() -> Vec<String> {
    [
        "cover.jpg",
        "cover.png",
        "cover.jpeg",
        "folder.jpg",
        "folder.png",
        "folder.jpeg",
        "album.jpg",
        "album.png",
        "album.jpeg",
        "front.jpg",
        "front.png",
        "front.jpeg",
        "art.jpg",
        "art.png",
        "art.jpeg",
        "scan.jpg",
        "scan.png",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Default relative widths as percentages (0..100). The percent values sum to
/// 100 across the canonical (all-visible) column set; they are renormalised at
/// runtime to sum to 100 over whichever columns are currently visible.
pub fn default_column_width(id: ColumnId) -> f32 {
    match id {
        ColumnId::NowPlaying => 1.0,
        ColumnId::TrackNumber => 3.0,
        ColumnId::Title => 22.0,
        ColumnId::Artist => 15.0,
        ColumnId::Album => 15.0,
        ColumnId::Genre => 5.0,
        ColumnId::Year => 4.0,
        ColumnId::Format => 4.0,
        ColumnId::Bitrate => 4.0,
        ColumnId::BitDepth => 4.0,
        ColumnId::SampleRate => 5.0,
        ColumnId::Duration => 5.0,
        ColumnId::FileName => 8.0,
        ColumnId::FilePath => 4.0,
    }
}

/// Default column configurations (title, priority, limits, visibility).
type ColumnDef = (ColumnId, &'static str, f32, f32, Option<f32>, Option<f32>, bool, Option<&'static str>);

pub fn default_columns() -> std::collections::HashMap<String, ColumnCfg> {
    let defs: Vec<ColumnDef> = vec![
        (ColumnId::NowPlaying,  "\u{25B6}",    0.01,  24.0, Some(36.0),  None,                   true,  Some("now-playing")),
        (ColumnId::TrackNumber, "\u{2116}",    0.03,  44.0, Some(84.0),  None,                   true,  None),
        (ColumnId::Title,       "Название",    0.22, 120.0, None,        Some(0.60),             true,  None),
        (ColumnId::Artist,      "Исполнитель", 0.15,  90.0, None,        Some(0.45),             true,  None),
        (ColumnId::Album,       "Альбом",      0.15,  90.0, None,        Some(0.45),             true,  None),
        (ColumnId::Genre,       "Жанр",        0.05,  80.0, None,        Some(0.30),             false, None),
        (ColumnId::Year,        "Год",         0.04,  44.0, Some(72.0),  None,                   false, None),
        (ColumnId::Format,      "Формат",      0.04,  52.0, Some(96.0),  None,                   false, None),
        (ColumnId::Bitrate,     "Битрейт",     0.04,  70.0, Some(130.0), None,                   false, None),
        (ColumnId::BitDepth,    "Глубина",     0.04,  64.0, Some(110.0), None,                   false, None),
        (ColumnId::SampleRate,  "Частота",     0.05,  90.0, Some(140.0), None,                   false, None),
        (ColumnId::Duration,    "Длит.",       0.05,  56.0, Some(100.0), None,                   true,  None),
        (ColumnId::FileName,    "Имя файла",   0.08, 110.0, None,        Some(0.60),             false, None),
        (ColumnId::FilePath,    "Путь",        0.04, 130.0, None,        Some(0.70),             false, None),
    ];

    defs.into_iter()
        .map(|(id, title, pri, min_w, max_w, max_pct, vis, col_type)| {
            (
                id.key().to_string(),
                ColumnCfg {
                    title: title.to_string(),
                    priority: pri,
                    min_width: min_w,
                    max_width: max_w,
                    max_width_percent: max_pct,
                    visible: vis,
                    column_type: col_type.map(|s| s.to_string()),
                    width: None,
                },
            )
        })
        .collect()
}

/// Stable keys of the track-info panel labels (order = display order in UI).
pub const INFO_LABEL_KEYS: [&str; 13] = [
    "artist",
    "track",
    "title",
    "duration",
    "year",
    "album",
    "disc",
    "genre",
    "format",
    "bitrate",
    "bit_depth",
    "sample_rate",
    "channels",
];

/// Default (English) labels for the track-info panel. Editing `info_labels`
/// in the config file overrides these values.
pub(crate) fn default_info_labels() -> std::collections::HashMap<String, String> {
    [
        ("artist", "Artist"),
        ("track", "Track"),
        ("title", "Title"),
        ("duration", "Duration"),
        ("year", "Year"),
        ("album", "Album"),
        ("disc", "Disc"),
        ("genre", "Genre"),
        ("format", "Format"),
        ("bitrate", "Bitrate"),
        ("bit_depth", "Bit depth"),
        ("sample_rate", "Sample rate"),
        ("channels", "Channels"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Режим вывода DSD (ТЗ 5.1 §8.2). `Pcm` — единственный реализованный;
/// `Native`/`DoP` зарезервированы под этап 6.6 (bit-perfect / native / DoP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DsdMode {
    #[default]
    Pcm,
    Native,
    DoP,
}

impl DsdMode {
    /// Порядковый индекс для ComboBox-модели `.slint` (0=Pcm, 1=Native, 2=DoP).
    pub const fn index(self) -> i32 {
        match self {
            DsdMode::Pcm => 0,
            DsdMode::Native => 1,
            DsdMode::DoP => 2,
        }
    }

    /// Обратное отображение из индекса UI (валидные значения 0..=2).
    pub fn from_index(i: i32) -> Option<Self> {
        match i {
            0 => Some(DsdMode::Pcm),
            1 => Some(DsdMode::Native),
            2 => Some(DsdMode::DoP),
            _ => None,
        }
    }
}

/// Битность конвертации DSD → PCM (ТЗ 5.1 §8.2). Внутри плеера PCM всегда
/// f32; значение используется на этапе 6.6 (dither/bit-perfect) и в FullTrackWorker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TargetBitDepth {
    #[serde(rename = "16")]
    Bits16,
    #[default]
    #[serde(rename = "24")]
    Bits24,
    #[serde(rename = "32")]
    Bits32Int,
    #[serde(rename = "32float")]
    Bits32Float,
}

/// Целевая частота DSD → PCM (ТЗ 5.1 §8.2). `Auto` — выход CIC-децимации
/// (44.1/48/88.2/96/176.4/192 кГц в зависимости от кратности DSD), дальше всё
/// доводит универсальный ресемплер до частоты устройства (§8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TargetSampleRate {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "44100")]
    Hz44100,
    #[serde(rename = "48000")]
    Hz48000,
    #[serde(rename = "88200")]
    Hz88200,
    #[serde(rename = "96000")]
    Hz96000,
    #[serde(rename = "176400")]
    Hz176400,
    #[serde(rename = "192000")]
    Hz192000,
}

/// Алгоритм универсального ресемплинга PCM → устройство (ТЗ 5.1 §8.3).
/// Применяется ко всем трекам (PCM и DSD), где частоты не совпадают.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResamplerAlgorithm {
    /// Линейная интерполяция (дёшево, низкое качество).
    Linear,
    /// Кубическая Catmull-Rom (4 точки).
    Cubic,
    /// Windowed-sinc, 32 тапа.
    SincFast,
    /// Windowed-sinc, 64 тапа (дефолт).
    #[default]
    SincMedium,
    /// Windowed-sinc, 128 тапов.
    SincSlow,
}

/// Дизеринг при ресемплинге (ТЗ 5.1 §8.2). Пока не применяется в горячем
/// пути — зарезервирован под этап 6.6 (bit-perfect/dither).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResamplerDither {
    #[default]
    Tpdf,
    Triangular,
    Off,
}

/// Количество тапов windowed-sinc фильтра для алгоритма.
impl ResamplerAlgorithm {
    pub const fn sinc_taps(self) -> u32 {
        match self {
            ResamplerAlgorithm::Linear | ResamplerAlgorithm::Cubic => 0,
            ResamplerAlgorithm::SincFast => 32,
            ResamplerAlgorithm::SincMedium => 64,
            ResamplerAlgorithm::SincSlow => 128,
        }
    }
}

/// Режим exclusive-доступа к устройству (ТЗ A3.0 §2.1). На ALSA/cpal
/// «exclusive» означает предпочтение raw-ноды `hw:*`; отдельного флага
/// exclusive-access у cpal нет, режим задаётся на этапе выбора устройства.
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

impl ExclusiveMode {
    /// Порядковый индекс для ComboBox-модели `.slint` (0=Off, 1=Auto, 2=Strict).
    pub const fn index(self) -> i32 {
        match self {
            ExclusiveMode::Off => 0,
            ExclusiveMode::Auto => 1,
            ExclusiveMode::Strict => 2,
        }
    }

    /// Обратное отображение из индекса UI (валидные значения 0..=2).
    pub fn from_index(i: i32) -> Option<Self> {
        match i {
            0 => Some(ExclusiveMode::Off),
            1 => Some(ExclusiveMode::Auto),
            2 => Some(ExclusiveMode::Strict),
            _ => None,
        }
    }
}

/// Политика фолбека при несовпадении параметров потока (ТЗ A3.0 §2.1).
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

impl FallbackPolicy {
    /// Порядковый индекс для ComboBox-модели `.slint` (0=Nearest, 1=DeviceDefault, 2=Fail).
    pub const fn index(self) -> i32 {
        match self {
            FallbackPolicy::Nearest => 0,
            FallbackPolicy::DeviceDefault => 1,
            FallbackPolicy::Fail => 2,
        }
    }

    /// Обратное отображение из индекса UI (валидные значения 0..=2).
    pub fn from_index(i: i32) -> Option<Self> {
        match i {
            0 => Some(FallbackPolicy::Nearest),
            1 => Some(FallbackPolicy::DeviceDefault),
            2 => Some(FallbackPolicy::Fail),
            _ => None,
        }
    }
}

/// Политика ресемплинга (ТЗ A3.0 §2.1).
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

impl ResamplerMode {
    /// Порядковый индекс для ComboBox-модели `.slint` (0=Auto, 1=Native, 2=Fixed).
    pub const fn index(self) -> i32 {
        match self {
            ResamplerMode::Auto => 0,
            ResamplerMode::Native => 1,
            ResamplerMode::Fixed => 2,
        }
    }

    /// Обратное отображение из индекса UI (валидные значения 0..=2).
    pub fn from_index(i: i32) -> Option<Self> {
        match i {
            0 => Some(ResamplerMode::Auto),
            1 => Some(ResamplerMode::Native),
            2 => Some(ResamplerMode::Fixed),
            _ => None,
        }
    }
}

/// Частотное семейство для выбора fallback-рейта (ТЗ A3.0 §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClockFamily {
    #[default]
    Auto,
    /// 44.1 / 88.2 / 176.4 / 352.8
    Family44k,
    /// 48 / 96 / 192 / 384
    Family48k,
}

impl ClockFamily {
    /// Порядковый индекс для ComboBox-модели `.slint` (0=Auto, 1=44k, 2=48k).
    pub const fn index(self) -> i32 {
        match self {
            ClockFamily::Auto => 0,
            ClockFamily::Family44k => 1,
            ClockFamily::Family48k => 2,
        }
    }

    /// Обратное отображение из индекса UI (валидные значения 0..=2).
    pub fn from_index(i: i32) -> Option<Self> {
        match i {
            0 => Some(ClockFamily::Auto),
            1 => Some(ClockFamily::Family44k),
            2 => Some(ClockFamily::Family48k),
            _ => None,
        }
    }
}

/// Политика выбора fallback-рейта при недоступности native (ТЗ A3.0 §2.1).
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

impl FallbackRatePolicy {
    /// Порядковый индекс для ComboBox-модели `.slint` (0=Nearest, 1=SameFamily, 2=NeverDownsample).
    pub const fn index(self) -> i32 {
        match self {
            FallbackRatePolicy::Nearest => 0,
            FallbackRatePolicy::SameFamily => 1,
            FallbackRatePolicy::NeverDownsample => 2,
        }
    }

    /// Обратное отображение из индекса UI (валидные значения 0..=2).
    pub fn from_index(i: i32) -> Option<Self> {
        match i {
            0 => Some(FallbackRatePolicy::Nearest),
            1 => Some(FallbackRatePolicy::SameFamily),
            2 => Some(FallbackRatePolicy::NeverDownsample),
            _ => None,
        }
    }
}

/// Настройки `[dsd]` (ТЗ 5.1 §8.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DsdCfg {
    #[serde(default)]
    pub mode: DsdMode,
    #[serde(default)]
    pub target_bit_depth: TargetBitDepth,
    #[serde(default)]
    pub target_sample_rate: TargetSampleRate,
}

impl Default for DsdCfg {
    fn default() -> Self {
        Self {
            mode: DsdMode::Pcm,
            target_bit_depth: TargetBitDepth::Bits24,
            target_sample_rate: TargetSampleRate::Auto,
        }
    }
}

/// Настройки `[audio.resampler]` (ТЗ 5.1 §8.2, A3.0 §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioResamplerCfg {
    #[serde(default)]
    pub algorithm: ResamplerAlgorithm,
    #[serde(default)]
    pub dither: ResamplerDither,
    /// Политика ресемплинга (ТЗ A3.0 §2.2).
    #[serde(default)]
    pub mode: ResamplerMode,
    /// 0 = auto по семейству (используется только при `mode = Fixed`).
    #[serde(default)]
    pub fixed_rate: u32,
    /// Предпочитаемое частотное семейство (ТЗ A3.0 §2.2).
    #[serde(default)]
    pub prefer_family: ClockFamily,
    /// Политика fallback-рейта при недоступности native (ТЗ A3.0 §2.2).
    #[serde(default)]
    pub fallback_rate: FallbackRatePolicy,
}

impl Default for AudioResamplerCfg {
    fn default() -> Self {
        Self {
            algorithm: ResamplerAlgorithm::SincMedium,
            dither: ResamplerDither::Tpdf,
            mode: ResamplerMode::Auto,
            fixed_rate: 0,
            prefer_family: ClockFamily::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
        }
    }
}

/// Default depth of the producer/consumer ring in milliseconds (ТЗ A2.0 §5.2).
pub const RING_BUFFER_MS_DEFAULT: u32 = 1500;
/// Lower bound of the configurable ring depth (ТЗ A2.0 §5.2).
pub const RING_BUFFER_MS_MIN: u32 = 100;
/// Upper bound of the configurable ring depth (ТЗ A2.0 §5.2).
pub const RING_BUFFER_MS_MAX: u32 = 10000;

fn default_ring_buffer_ms() -> u32 {
    RING_BUFFER_MS_DEFAULT
}

/// Clamp a ring depth into the supported `[MIN..MAX]` range (ТЗ A2.0 §5.2).
pub fn clamp_ring_buffer_ms(ms: u32) -> u32 {
    ms.clamp(RING_BUFFER_MS_MIN, RING_BUFFER_MS_MAX)
}

/// Настройки `[audio]` (ТЗ 5.1 §8.2, A3.0 §2.2): bit-perfect, ресемплер,
/// exclusive/fallback и фильтры списка устройств.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioCfg {
    #[serde(default)]
    pub bit_perfect: bool,
    /// Глубина ring-буфера «задержка vs устойчивость», мс (ТЗ A2.0 §5.2).
    #[serde(default = "default_ring_buffer_ms")]
    pub ring_buffer_ms: u32,
    #[serde(default)]
    pub resampler: AudioResamplerCfg,
    /// Режим exclusive-доступа (ТЗ A3.0 §2.2).
    #[serde(default)]
    pub exclusive: ExclusiveMode,
    /// Политика фолбека при несовпадении параметров (ТЗ A3.0 §2.2).
    #[serde(default)]
    pub fallback: FallbackPolicy,
    /// Показывать в списке только аппаратные устройства (ТЗ A3.0 §2.2).
    #[serde(default)]
    pub filter_hardware_only: bool,
    /// Показывать в списке только стерео-устройства (ТЗ A3.0 §2.2).
    #[serde(default)]
    pub filter_stereo_only: bool,
}

impl Default for AudioCfg {
    fn default() -> Self {
        Self {
            bit_perfect: false,
            ring_buffer_ms: RING_BUFFER_MS_DEFAULT,
            resampler: AudioResamplerCfg::default(),
            exclusive: ExclusiveMode::Auto,
            fallback: FallbackPolicy::Nearest,
            filter_hardware_only: false,
            filter_stereo_only: false,
        }
    }
}

/// Resolve (and cache) the per-user config directory: `$XDG_CONFIG_HOME/
/// music_player` (or `./music_player` when no config dir exists).
/// Вызывается только в `main` (ТЗ-49, ADR-19): остальной код получает пути
/// из `persist::ConfigPaths`.
pub fn config_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("music_player")
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_and_dsd_defaults() {
        let a = AudioCfg::default();
        // ТЗ §8.3: дефолт ресемплера — sinc_medium.
        assert_eq!(a.resampler.algorithm, ResamplerAlgorithm::SincMedium);
        assert_eq!(a.resampler.dither, ResamplerDither::Tpdf);
        assert!(!a.bit_perfect);
        // ТЗ A2.0 §5.2: дефолтная глубина ring-буфера — 1500 мс.
        assert_eq!(a.ring_buffer_ms, RING_BUFFER_MS_DEFAULT);
        // ТЗ §8.2: DSD по умолчанию «DSD → PCM», 24 бит, auto частота.
        let d = DsdCfg::default();
        assert_eq!(d.mode, DsdMode::Pcm);
        assert_eq!(d.target_bit_depth, TargetBitDepth::Bits24);
        assert_eq!(d.target_sample_rate, TargetSampleRate::Auto);
    }

    #[test]
    fn ring_buffer_ms_roundtrips_and_overrides() {
        let a = AudioCfg::default();
        let toml = toml::to_string(&a).unwrap();
        assert!(toml.contains("ring_buffer_ms"), "missing ring_buffer_ms:\n{toml}");
        let parsed: AudioCfg = toml::from_str(&toml).unwrap();
        assert_eq!(parsed.ring_buffer_ms, RING_BUFFER_MS_DEFAULT);

        let parsed: AudioCfg = toml::from_str("ring_buffer_ms = 250\n").unwrap();
        assert_eq!(parsed.ring_buffer_ms, 250);
    }

    #[test]
    fn clamp_ring_buffer_ms_applies_bounds() {
        assert_eq!(clamp_ring_buffer_ms(0), RING_BUFFER_MS_MIN);
        assert_eq!(clamp_ring_buffer_ms(50), RING_BUFFER_MS_MIN);
        assert_eq!(clamp_ring_buffer_ms(1500), 1500);
        assert_eq!(clamp_ring_buffer_ms(20_000), RING_BUFFER_MS_MAX);
    }

    #[test]
    fn dsd_mode_index_roundtrip() {
        assert_eq!(DsdMode::Pcm.index(), 0);
        assert_eq!(DsdMode::Native.index(), 1);
        assert_eq!(DsdMode::DoP.index(), 2);
        assert_eq!(DsdMode::from_index(0), Some(DsdMode::Pcm));
        assert_eq!(DsdMode::from_index(1), Some(DsdMode::Native));
        assert_eq!(DsdMode::from_index(2), Some(DsdMode::DoP));
        assert_eq!(DsdMode::from_index(3), None);
        assert_eq!(DsdMode::from_index(i32::MAX), None);
    }

    #[test]
    fn audio_resampler_defaults() {
        let a = AudioCfg::default();
        assert_eq!(a.resampler.mode, ResamplerMode::Auto, "A3.0 §11.1");
        assert_eq!(a.resampler.fixed_rate, 0, "A3.0 §11.1");
        assert_eq!(a.resampler.prefer_family, ClockFamily::Auto, "A3.0 §11.1");
        assert_eq!(a.resampler.fallback_rate, FallbackRatePolicy::Nearest, "A3.0 §11.1");
    }

    #[test]
    fn audio_defaults_new_fields() {
        let a = AudioCfg::default();
        assert_eq!(a.exclusive, ExclusiveMode::Auto, "A3.0 §11.1");
        assert_eq!(a.fallback, FallbackPolicy::Nearest, "A3.0 §11.1");
        assert!(!a.filter_hardware_only, "A3.0 §11.1");
        assert!(!a.filter_stereo_only, "A3.0 §11.1");
    }

    #[test]
    fn audio_toml_roundtrip_with_new_fields() {
        let toml = "bit_perfect = true\nexclusive = \"strict\"\nfallback = \"fail\"\nfilter_hardware_only = true\nfilter_stereo_only = true\n\n[resampler]\nmode = \"fixed\"\nfixed_rate = 48000\nprefer_family = \"family48k\"\nfallback_rate = \"never_downsample\"\n";
        let a: AudioCfg = toml::from_str(toml).unwrap();
        assert!(a.bit_perfect);
        assert_eq!(a.exclusive, ExclusiveMode::Strict);
        assert_eq!(a.fallback, FallbackPolicy::Fail);
        assert!(a.filter_hardware_only);
        assert!(a.filter_stereo_only);
        assert_eq!(a.resampler.mode, ResamplerMode::Fixed);
        assert_eq!(a.resampler.fixed_rate, 48000);
        assert_eq!(a.resampler.prefer_family, ClockFamily::Family48k);
        assert_eq!(a.resampler.fallback_rate, FallbackRatePolicy::NeverDownsample);

        let re = toml::to_string(&a).unwrap();
        let parsed: AudioCfg = toml::from_str(&re).unwrap();
        assert_eq!(parsed, a);
    }

    #[test]
    fn audio_partial_config_preserves_existing() {
        let toml = "bit_perfect = false\n";
        let a: AudioCfg = toml::from_str(toml).unwrap();
        assert!(!a.bit_perfect);
        assert_eq!(a.resampler.algorithm, ResamplerAlgorithm::SincMedium);
        assert_eq!(a.resampler.dither, ResamplerDither::Tpdf);
        assert_eq!(a.resampler.mode, ResamplerMode::Auto);
        assert_eq!(a.resampler.fixed_rate, 0);
        assert_eq!(a.resampler.prefer_family, ClockFamily::Auto);
        assert_eq!(a.resampler.fallback_rate, FallbackRatePolicy::Nearest);
        assert_eq!(a.exclusive, ExclusiveMode::Auto);
        assert_eq!(a.fallback, FallbackPolicy::Nearest);
        assert!(!a.filter_hardware_only);
        assert!(!a.filter_stereo_only);
    }

    #[test]
    fn audio_resampler_algorithm_overrides_default() {
        let toml = "algorithm = \"cubic\"\n";
        let r: AudioResamplerCfg = match toml::from_str(toml) {
            Ok(r) => r,
            Err(e) => {
                panic!("parse failed: {e}");
            }
        };
        assert_eq!(r.algorithm, ResamplerAlgorithm::Cubic);
        // Остальные поля — дефолты.
        assert_eq!(r.dither, ResamplerDither::Tpdf);
    }

    #[test]
    fn resampler_algorithm_sinc_taps() {
        assert_eq!(ResamplerAlgorithm::Linear.sinc_taps(), 0);
        assert_eq!(ResamplerAlgorithm::Cubic.sinc_taps(), 0);
        assert_eq!(ResamplerAlgorithm::SincFast.sinc_taps(), 32);
        assert_eq!(ResamplerAlgorithm::SincMedium.sinc_taps(), 64);
        assert_eq!(ResamplerAlgorithm::SincSlow.sinc_taps(), 128);
    }

    #[test]
    fn from_key_and_key_roundtrip() {
        for c in ColumnId::ALL {
            assert_eq!(ColumnId::from_key(c.key()), Some(c));
        }
        assert_eq!(ColumnId::from_key("nope"), None);
    }
}

