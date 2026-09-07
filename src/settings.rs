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

/// Identifiers for the playlist columns (order matches the display order in
/// the playlist table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ColumnId {
    #[default]
    Index,
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
        ColumnId::Index,
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

    pub fn label(self) -> &'static str {
        match self {
            ColumnId::Index => "#",
            ColumnId::TrackNumber => "Track #",
            ColumnId::Title => "Title",
            ColumnId::Artist => "Artist",
            ColumnId::Album => "Album",
            ColumnId::Genre => "Genre",
            ColumnId::Year => "Year",
            ColumnId::Format => "Format",
            ColumnId::Bitrate => "Bitrate",
            ColumnId::BitDepth => "Bit Depth",
            ColumnId::SampleRate => "Sample Rate",
            ColumnId::Duration => "Duration",
            ColumnId::FileName => "File Name",
            ColumnId::FilePath => "File Path",
        }
    }

    /// Column key used for persistence matching (stable across renames).
    pub fn key(self) -> &'static str {
        match self {
            ColumnId::Index => "index",
            ColumnId::TrackNumber => "track_number",
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
        ColumnId::Index => 5.0,
        ColumnId::TrackNumber => 8.0,
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

/// Returns `true` when the stored width values look like absolute pixel widths
/// (sum way above 100) instead of percentages. Used for one-time migration.
fn widths_are_pixels(widths: &std::collections::HashMap<String, f32>) -> bool {
    let sum: f32 = widths.values().copied().filter(|w| *w > 0.0).sum();
    sum > 100.0
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
    #[serde(default)]
    pub column_widths: std::collections::HashMap<String, f32>,
    #[serde(default)]
    pub column_visibility: std::collections::HashMap<String, bool>,
    #[serde(default)]
    /// User-customised left-to-right column order (as stable keys). Empty
    /// means the canonical [`ColumnId::ALL`] order.
    pub column_order: Vec<String>,
    #[serde(default)]
    pub sorted_col: Option<ColumnId>,
    #[serde(default)]
    pub sort_desc: bool,
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
            column_widths: Default::default(),
            column_visibility: Default::default(),
            column_order: Vec::new(),
            sorted_col: None,
            sort_desc: false,
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
    /// Resolved visibility for a given id (default: visible).
    pub fn column_visible(&self, id: ColumnId) -> bool {
        self.column_visibility
            .get(id.key())
            .copied()
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
    /// the default. Percent values are normalised to sum to 100 across the
    /// currently visible columns; call [`Settings::normalize_visible_pct`]
    /// whenever visibility changes before applying.
    pub fn column_width_pct(&self, id: ColumnId) -> f32 {
        self.column_widths
            .get(id.key())
            .copied()
            .filter(|w| w.is_finite() && *w > 0.0)
            .unwrap_or_else(|| default_column_width(id))
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
            self.column_widths
                .get(c.key())
                .is_some_and(|w| w.is_finite() && *w > 0.0)
        });
        let values: Vec<f32> = visible
            .iter()
            .map(|c| {
                if has_stored {
                    self.column_widths
                        .get(c.key())
                        .copied()
                        .filter(|w| w.is_finite() && *w > 0.0)
                        .unwrap_or_else(|| default_column_width(*c))
                } else {
                    default_column_width(*c)
                }
            })
            .collect();
        let sum: f32 = values.iter().sum();
        for (c, v) in visible.iter().zip(values) {
            let pct = if sum > 0.0 { (v / sum) * 100.0 } else { 100.0 / visible.len() as f32 };
            self.column_widths.insert(c.key().to_string(), pct);
        }
    }

    /// Enable a column: give it the average of the current visible set,
    /// then renormalise all visible columns to 100%.
    pub fn enable_column(&mut self, id: ColumnId) {
        let old_n = self.visible_columns().len() as f32;
        self.column_visibility.insert(id.key().to_string(), true);
        let avg = if old_n > 0.0 { 100.0 / old_n } else { 100.0 };
        self.column_widths.insert(id.key().to_string(), avg);
        self.normalize_visible_pct();
    }

    /// Disable a column: set its width to 0%, hide it, then renormalise
    /// the remaining visible columns to 100%.
    pub fn disable_column(&mut self, id: ColumnId) {
        self.column_widths.insert(id.key().to_string(), 0.0);
        self.column_visibility.insert(id.key().to_string(), false);
        self.normalize_visible_pct();
    }

    /// One-time migration: convert stored pixel widths to percentages.
    /// No-op when widths are already percentages or absent.
    pub fn migrate_widths_to_pct(&mut self) {
        if self.column_widths.is_empty() || !widths_are_pixels(&self.column_widths) {
            return;
        }
        let sum: f32 = self.column_widths.values().copied().filter(|w| *w > 0.0).sum();
        if sum <= 0.0 {
            return;
        }
        for (_, w) in self.column_widths.iter_mut() {
            if *w > 0.0 {
                *w = (*w / sum) * 100.0;
            }
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
            .filter_map(|k| {
                let c = ColumnId::from_key(k)?;
                Some(c)
            })
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

pub struct SettingsStore {
    pub settings: Settings,
    pub path: PathBuf,
}

impl SettingsStore {
    pub fn load() -> Self {
        let dir = config_dir();
        let path = dir.join("settings.toml");
        let mut settings = match fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
            Err(_) => Settings::default(),
        };
        settings.migrate_widths_to_pct();
        let store = Self { settings, path };
        // Persist migration results back to disk.
        let mut s = store;
        s.save();
        s
    }

    pub fn save(&mut self) {
        if let Ok(contents) = toml::to_string(&self.settings) {
            if fs::create_dir_all(self.path.parent().unwrap_or(&self.path)).is_ok() {
                let _ = fs::write(&self.path, contents);
            }
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("music_player")
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
        assert!(ord.contains(&ColumnId::Index));
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
}
