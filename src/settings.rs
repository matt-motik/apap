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

/// Duration column is the only numeric with a fixed-ish width among the
/// defaults; widths below are the initial values applied at first run.
pub fn default_column_width(id: ColumnId) -> f32 {
    match id {
        ColumnId::Index => 30.0,
        ColumnId::TrackNumber => 55.0,
        ColumnId::Title => 220.0,
        ColumnId::Artist => 150.0,
        ColumnId::Album => 150.0,
        ColumnId::Genre => 90.0,
        ColumnId::Year => 60.0,
        ColumnId::Format => 70.0,
        ColumnId::Bitrate => 70.0,
        ColumnId::BitDepth => 80.0,
        ColumnId::SampleRate => 90.0,
        ColumnId::Duration => 70.0,
        ColumnId::FileName => 200.0,
        ColumnId::FilePath => 300.0,
    }
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
        }
    }
}

impl Settings {
    /// Resolved column width for a given id, falling back to the default.
    pub fn column_width(&self, id: ColumnId) -> f32 {
        self.column_widths
            .get(id.key())
            .copied()
            .filter(|w| *w > 0.0)
            .unwrap_or_else(|| default_column_width(id))
    }

    /// Resolved visibility for a given id (default: visible).
    pub fn column_visible(&self, id: ColumnId) -> bool {
        self.column_visibility
            .get(id.key())
            .copied()
            .unwrap_or(true)
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
}

pub struct SettingsStore {
    pub settings: Settings,
    pub path: PathBuf,
}

impl SettingsStore {
    pub fn load() -> Self {
        let dir = config_dir();
        let path = dir.join("settings.toml");
        let settings = match fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
            Err(_) => Settings::default(),
        };
        Self { settings, path }
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
}
