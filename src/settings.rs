use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
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

/// Порядок источников обложек по умолчанию: диск → встроенная → интернет.
pub fn default_cover_priority() -> Vec<String> {
    CoverSource::ALL.iter().map(|c| c.key().to_string()).collect()
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

/// Значение по умолчанию для `Settings::cover_online`.
fn default_cover_online() -> bool {
    true
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
pub fn default_columns() -> std::collections::HashMap<String, ColumnCfg> {
    let defs: Vec<(ColumnId, &str, f32, f32, Option<f32>, Option<f32>, bool, Option<&str>)> = vec![
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub volume: f32,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub last_dir: String,
    #[serde(default)]
    pub minimize_to_tray: bool,
    #[serde(default)]
    pub repeat: RepeatMode,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub audio_device: String,
    /// Unified column configuration (per-column title, priority, limits,
    /// visibility, type). Migrated from the old separate fields on first load.
    #[serde(default = "default_columns")]
    pub columns: std::collections::HashMap<String, ColumnCfg>,
    /// Left-to-right column order as stable keys. Source of truth for ordering;
    /// empty = canonical order.
    #[serde(default)]
    pub column_order: Vec<String>,
    #[serde(default)]
    pub sorted_col: Option<ColumnId>,
    #[serde(default)]
    pub sort_desc: bool,
    /// Auto-scroll the playlist to the currently playing track when it changes.
    #[serde(default = "default_true")]
    pub scroll_to_playing: bool,
    /// Сторона обложки (пиксели) — задаёт высоту верхней панели и размер
    /// квадратных кнопок управления. Диапазон 100..=400, по умолчанию 200.
    #[serde(default)]
    pub cover_size: f32,
    /// Ширина колонки информации о треке (пиксели). Диапазон 200..=400,
    /// по умолчанию 240. При переносе строк колонка скролится.
    #[serde(default)]
    pub col_info_w: f32,
    /// Горизонтальный отступ между колонками верхней панели (пиксели),
    /// применяется по обе стороны вертикального разделителя.
    /// Диапазон 0..=20, по умолчанию 12.
    #[serde(default)]
    pub col_gap: f32,
    /// Позиция окна на экране (физические пиксели, включая рамку). `None` —
    /// не сохранено, окну позицию выбирает оконный менеджер.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win_y: Option<i32>,
    /// Размер окна в физических пикселях (без рамки). `None` — размер по умолчанию.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win_w: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win_h: Option<u32>,
    /// Источники обложек в порядке приоритета (ключи `CoverSource::key`).
    /// Пусто — дефолтный порядок (диск → встроенная → интернет).
    #[serde(default)]
    pub cover_priority: Vec<String>,
    /// Имена файлов обложек, которые ищем в папке с альбомом.
    #[serde(default)]
    pub cover_folder_names: Vec<String>,
    /// Искать обложки в интернете (iTunes Search API).
    #[serde(default = "default_cover_online")]
    pub cover_online: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            volume: 0.8,
            muted: false,
            last_dir: String::new(),
            minimize_to_tray: false,
            repeat: RepeatMode::Off,
            shuffle: false,
            audio_device: String::new(),
            columns: default_columns(),
            column_order: Vec::new(),
            sorted_col: None,
            sort_desc: false,
            scroll_to_playing: true,
            cover_size: 200.0,
            col_info_w: 240.0,
            col_gap: 12.0,
            win_x: None,
            win_y: None,
            win_w: None,
            win_h: None,
            cover_priority: default_cover_priority(),
            cover_folder_names: default_cover_folder_names(),
            cover_online: true,
        }
    }
}

impl Settings {
    /// Get the `ColumnCfg` for a given column id.
    pub fn column_cfg(&self, id: ColumnId) -> Option<&ColumnCfg> {
        self.columns.get(id.key())
    }

    /// Get the display title for a column, falling back to the key if not in config.
    pub fn column_title(&self, id: ColumnId) -> String {
        self.columns
            .get(id.key())
            .map(|c| c.title.clone())
            .unwrap_or_else(|| id.key().to_string())
    }

    /// Resolved visibility for a given id (default: visible).
    pub fn column_visible(&self, id: ColumnId) -> bool {
        self.columns
            .get(id.key())
            .map(|c| c.visible)
            .unwrap_or(true)
    }

    /// Visible columns in display order.
    pub fn visible_columns(&self) -> Vec<ColumnId> {
        self.ordered_columns()
            .into_iter()
            .filter(|c| self.column_visible(*c))
            .collect()
    }

    /// Resolved relative width (percent, 0..100) for a column, falling back to
    /// the config `width` → `priority` → legacy `default_column_width`.
    /// Percent values are normalised to sum to 100 across the currently visible
    /// columns; call [`Settings::normalize_visible_pct`] whenever visibility
    /// changes before applying.
    pub fn column_width_pct(&self, id: ColumnId) -> f32 {
        if let Some(cfg) = self.columns.get(id.key()) {
            // User-dragged width takes highest priority.
            if let Some(w) = cfg.width {
                if w.is_finite() && w > 0.0 {
                    return w;
                }
            }
            // Config priority (default weight).
            if cfg.priority > 0.0 && cfg.priority.is_finite() {
                return cfg.priority;
            }
        }
        default_column_width(id)
    }

    /// Renormalise stored percentages so that the currently visible columns sum
    /// to 100. Hidden/unknown columns are skipped; if no stored widths are
    /// present, defaults (of the visible columns) are written instead.
    /// Widths of hidden columns are preserved (e.g. 0%) and NOT dropped.
    pub fn normalize_visible_pct(&mut self) {
        let visible = self.visible_columns();
        if visible.is_empty() {
            return;
        }
        let has_stored = visible.iter().any(|c| {
            self.columns
                .get(c.key())
                .and_then(|cfg| cfg.width)
                .is_some_and(|w| w.is_finite() && w > 0.0)
        });
        let values: Vec<f32> = visible
            .iter()
            .map(|c| {
                if has_stored {
                    self.columns
                        .get(c.key())
                        .and_then(|cfg| cfg.width)
                        .filter(|w| w.is_finite() && *w > 0.0)
                        .unwrap_or_else(|| self.column_width_pct(*c))
                } else {
                    self.column_width_pct(*c)
                }
            })
            .collect();
        let sum: f32 = values.iter().sum();
        for (c, v) in visible.iter().zip(values) {
            let pct = if sum > 0.0 { (v / sum) * 100.0 } else { 100.0 / visible.len() as f32 };
            if let Some(cfg) = self.columns.get_mut(c.key()) {
                cfg.width = Some(pct);
            }
        }
    }

    /// Enable a column: give it the average of the current visible set,
    /// then renormalise all visible columns to 100%.
    pub fn enable_column(&mut self, id: ColumnId) {
        let old_n = self.visible_columns().len() as f32;
        if let Some(cfg) = self.columns.get_mut(id.key()) {
            cfg.visible = true;
        }
        let avg = if old_n > 0.0 { 100.0 / old_n } else { 100.0 };
        if let Some(cfg) = self.columns.get_mut(id.key()) {
            cfg.width = Some(avg);
        }
        self.normalize_visible_pct();
    }

    /// Disable a column: set its width to 0%, hide it, then renormalise
    /// the remaining visible columns to 100%.
    pub fn disable_column(&mut self, id: ColumnId) {
        if let Some(cfg) = self.columns.get_mut(id.key()) {
            cfg.width = Some(0.0);
            cfg.visible = false;
        }
        self.normalize_visible_pct();
    }

    /// All columns in the user's left-to-right order (falls back to canonical).
    pub fn ordered_columns(&self) -> Vec<ColumnId> {
        if self.column_order.is_empty() {
            return ColumnId::ALL.to_vec();
        }
        let mut out: Vec<ColumnId> = self
            .column_order
            .iter()
            .filter_map(|k| ColumnId::from_key(k))
            .collect();
        // Include any canonical columns not present (e.g. after adding new ones)
        // appended after the stored order.
        for c in ColumnId::ALL {
            if !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    /// Move a column from `from` to `to` in the stored order, materialising
    /// the canonical order first if it was empty.
    pub fn move_column(&mut self, from: usize, to: usize) {
        let mut order = self.ordered_columns();
        if from >= order.len() {
            return;
        }
        let col = order.remove(from);
        let to = to.min(order.len());
        order.insert(to, col);
        self.column_order = order.iter().map(|c| c.key().to_string()).collect();
    }

    /// Cover sources in the user's priority order (falls back to the default:
    /// folder → embedded → internet). Unknown stored keys are dropped, missing
    /// canonical sources are appended.
    pub fn cover_priority_ordered(&self) -> Vec<CoverSource> {
        if self.cover_priority.is_empty() {
            return CoverSource::ALL.to_vec();
        }
        let mut out: Vec<CoverSource> = self
            .cover_priority
            .iter()
            .filter_map(|k| CoverSource::from_key(k))
            .collect();
        for c in CoverSource::ALL {
            if !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    /// Folder cover file names to look for, falling back to the default list
    /// when none are stored. Empty/whitespace entries are dropped.
    pub fn cover_folder_names_list(&self) -> Vec<String> {
        let names: Vec<String> = self
            .cover_folder_names
            .iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        if names.is_empty() {
            default_cover_folder_names()
        } else {
            names
        }
    }

    /// Move a cover source within the stored priority order.
    pub fn move_cover(&mut self, from: usize, to: usize) {
        let mut order: Vec<String> = self
            .cover_priority_ordered()
            .iter()
            .map(|c| c.key().to_string())
            .collect();
        if from >= order.len() {
            return;
        }
        let item = order.remove(from);
        let to = to.min(order.len());
        order.insert(to, item);
        self.cover_priority = order;
    }
}

/// Loaded settings plus the canonical on-disk path they were read from /
/// written back to.
pub struct SettingsStore {
    pub settings: Settings,
    pub path: PathBuf,
}

/// Legacy settings format (pre-unified-columns) used only for one-time migration.
#[derive(Debug, Deserialize, Default)]
struct LegacySettings {
    #[serde(default)]
    column_widths: std::collections::HashMap<String, f32>,
    #[serde(default)]
    column_visibility: std::collections::HashMap<String, bool>,
    #[serde(default)]
    column_order: Vec<String>,
}

impl SettingsStore {
    /// Read settings from `config_dir()/settings.toml` (or defaults if
    /// missing/corrupt), migrate legacy fields and write the result back.
    pub fn load() -> Self {
        let dir = config_dir();
        let path = dir.join("settings.toml");
        let raw = fs::read_to_string(&path).ok();
        let mut settings = match &raw {
            Some(contents) => toml::from_str(contents).unwrap_or_default(),
            None => Settings::default(),
        };
        // One-time migration from old format (separate column_widths/visibility/order
        // fields) to the unified `columns` HashMap.
        if settings.columns.is_empty() {
            if let Some(contents) = &raw {
                if contents.contains("column_widths")
                    || contents.contains("column_visibility")
                    || contents.contains("column_order")
                {
                    if let Ok(legacy) = toml::from_str::<LegacySettings>(contents) {
                        migrate_legacy_columns(&mut settings, &legacy);
                    }
                }
            }
            // Ensure columns exist even without legacy data (fresh install).
            if settings.columns.is_empty() {
                settings.columns = default_columns();
            }
        }
        // Ensure `column_order` has entries for all known columns.
        if settings.column_order.is_empty() {
            settings.column_order = ColumnId::ALL.iter().map(|c| c.key().to_string()).collect();
        }
        let store = Self { settings, path };
        // Persist migration results back to disk.
        let mut s = store;
        s.save();
        s
    }

    /// Persist the current settings to disk.
    pub fn save(&mut self) {
        if let Ok(contents) = toml::to_string(&self.settings) {
            if fs::create_dir_all(self.path.parent().unwrap_or(&self.path)).is_ok() {
                let _ = fs::write(&self.path, contents);
            }
        }
    }
}

/// Migrate old `column_widths` / `column_visibility` / `column_order` into the
/// unified `columns` HashMap.
fn migrate_legacy_columns(settings: &mut Settings, legacy: &LegacySettings) {
    // Merge old column_order (preserve it, append missing at the end).
    if !legacy.column_order.is_empty() {
        settings.column_order = legacy
            .column_order
            .iter()
            .filter(|k| ColumnId::from_key(k).is_some())
            .cloned()
            .collect();
        // Append canonical columns not in legacy order.
        for c in ColumnId::ALL {
            if !settings.column_order.contains(&c.key().to_string()) {
                settings.column_order.push(c.key().to_string());
            }
        }
    }

    // Migrate visibility and width into per-column configs.
    let defaults = default_columns();
    for c in ColumnId::ALL {
        let key = c.key().to_string();
        let cfg = settings.columns.entry(key.clone()).or_insert_with(|| {
            defaults.get(&key).cloned().unwrap_or_default()
        });

        if let Some(&vis) = legacy.column_visibility.get(c.key()) {
            cfg.visible = vis;
        }

        if let Some(&w) = legacy.column_widths.get(c.key()) {
            if w > 0.0 && w.is_finite() {
                cfg.width = Some(w);
            }
        }
    }
}

/// Resolve (and cache) the per-user config directory: `$XDG_CONFIG_HOME/
/// music_player` (or `./music_player` when no config dir exists).
pub fn config_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("music_player")
    })
    .clone()
}

/// Playlist file lives next to `settings.toml`.
pub fn playlist_path() -> PathBuf {
    config_dir().join("playlist.m3u")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_columns_falls_back_to_canonical() {
        let s = Settings::default();
        assert_eq!(s.ordered_columns(), ColumnId::ALL.to_vec());
    }

    #[test]
    fn ordered_columns_uses_stored_order_and_appends_new() {
        let mut s = Settings::default();
        s.column_order = vec!["title".into(), "genre".into(), "artist".into()];
        let ord = s.ordered_columns();
        assert_eq!(ord[0], ColumnId::Title);
        assert_eq!(ord[1], ColumnId::Genre);
        assert_eq!(ord[2], ColumnId::Artist);
        // Remaining canonical columns appended in canonical order.
        assert!(ord.contains(&ColumnId::NowPlaying));
        assert!(ord.contains(&ColumnId::SampleRate));
        assert_eq!(ord.len(), ColumnId::ALL.len());
        // No duplicates.
        let mut set = std::collections::HashSet::new();
        for c in &ord {
            assert!(set.insert(*c), "duplicate {c:?}");
        }
    }

    #[test]
    fn move_column_reorders_stored_keys() {
        let mut s = Settings::default();
        s.column_order = vec![
            "title".into(),
            "genre".into(),
            "artist".into(),
            "year".into(),
        ];
        s.move_column(0, 2); // title -> position 2
                             // The full canonical order is materialised; the reorder is reflected
                             // in the leading positions.
        assert!(s.column_order.starts_with(&[
            "genre".to_string(),
            "artist".to_string(),
            "title".to_string(),
            "year".to_string(),
        ]));
        assert_eq!(s.column_order.len(), ColumnId::ALL.len());
    }

    #[test]
    fn from_key_and_key_roundtrip() {
        for c in ColumnId::ALL {
            assert_eq!(ColumnId::from_key(c.key()), Some(c));
        }
        assert_eq!(ColumnId::from_key("nope"), None);
    }

    #[test]
    fn column_cfg_title_applies() {
        let s = Settings::default();
        assert_eq!(s.column_title(ColumnId::NowPlaying), "\u{25B6}");
        assert_eq!(s.column_title(ColumnId::Title), "Название");
        assert_eq!(s.column_title(ColumnId::Artist), "Исполнитель");
    }

    #[test]
    fn column_width_pct_uses_priority() {
        let s = Settings::default();
        // Title has priority 0.22 in defaults.
        let w = s.column_width_pct(ColumnId::Title);
        assert!((w - 0.22).abs() < 0.001, "expected 0.22, got {w}");
    }

    #[test]
    fn column_width_pct_uses_user_width_over_priority() {
        let mut s = Settings::default();
        if let Some(cfg) = s.columns.get_mut("title") {
            cfg.width = Some(33.0);
        }
        let w = s.column_width_pct(ColumnId::Title);
        assert!((w - 33.0).abs() < 0.001, "expected 33.0, got {w}");
    }

    #[test]
    fn cover_priority_default_order() {
        let s = Settings::default();
        assert_eq!(
            s.cover_priority_ordered(),
            vec![CoverSource::Folder, CoverSource::Embedded, CoverSource::Internet]
        );
    }

    #[test]
    fn cover_priority_filters_unknown_and_appends_missing() {
        let mut s = Settings::default();
        s.cover_priority = vec!["internet".into(), "bogus".into()];
        let ord = s.cover_priority_ordered();
        assert_eq!(ord[0], CoverSource::Internet);
        // Missing canonical sources appended in canonical order.
        assert_eq!(ord[1], CoverSource::Folder);
        assert_eq!(ord[2], CoverSource::Embedded);
        assert_eq!(ord.len(), CoverSource::ALL.len());
    }

    #[test]
    fn cover_move_reorders_stored_keys() {
        let mut s = Settings::default();
        s.move_cover(0, 2); // folder -> last
        let ord = s.cover_priority_ordered();
        assert_eq!(
            ord,
            vec![CoverSource::Embedded, CoverSource::Internet, CoverSource::Folder]
        );
    }

    #[test]
    fn cover_folder_names_fallback_and_trim() {
        let s = Settings::default();
        assert_eq!(s.cover_folder_names_list(), default_cover_folder_names());
        assert_eq!(default_cover_folder_names().len(), 17);

        let mut s = Settings::default();
        s.cover_folder_names = vec!["front.jpg".into(), "   ".into(), "art.png".into()];
        assert_eq!(s.cover_folder_names_list(), vec!["front.jpg", "art.png"]);

        // All-blank stored names fall back to the default list.
        let mut s = Settings::default();
        s.cover_folder_names = vec![" ".into()];
        assert_eq!(s.cover_folder_names_list(), default_cover_folder_names());
    }

    #[test]
    fn migrate_legacy_columns_merges_old_fields() {
        let legacy = LegacySettings {
            column_widths: [("title".into(), 30.0), ("artist".into(), 20.0)].into(),
            column_visibility: [("title".into(), true), ("genre".into(), false)].into(),
            column_order: vec!["title".into(), "artist".into()],
        };
        let mut settings = Settings::default();
        // Clear defaults so migration has something to populate.
        settings.columns.clear();
        migrate_legacy_columns(&mut settings, &legacy);

        // Visibility migrated.
        assert!(settings.column_visible(ColumnId::Title));
        assert!(!settings.column_visible(ColumnId::Genre));
        // Width migrated.
        let title_cfg = settings.columns.get("title").unwrap();
        assert!((title_cfg.width.unwrap() - 30.0).abs() < 0.001);
        // Order migrated.
        assert_eq!(settings.column_order[0], "title");
        assert_eq!(settings.column_order[1], "artist");
        // All canonical columns present in order.
        assert_eq!(settings.column_order.len(), ColumnId::ALL.len());
    }
}
