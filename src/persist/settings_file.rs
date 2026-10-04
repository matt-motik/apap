//! Новые типы `settings.toml` (ТЗ-2, §2.4).
//!
//! Этот модуль — целевая модель файла настроек: поля без `width` (она ушла в
//! `SessionState`, §2.5) и без прежних смешанных ключей состояния. Разбор и
//! сериализация добавляются следующими микро-шагами (§2.4, §2.6); здесь —
//! только типы и их значения по умолчанию, совпадающие с таблицей ключей §2.4.
//! Старый `crate::settings::Settings`/`SettingsStore` продолжает работать —
//! оба набора типов сосуществуют до переключения чтения/записи (§8.1 С3).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::audio::visualizer::VizSettings;
use crate::settings::{AudioCfg, ColumnId, CoverSource, DsdCfg};

/// Содержимое `settings.toml` (ТЗ-2, §2.4): параметры окна настроек, меняются
/// только действием «Сохранить» диалога (решение 1, ТЗ-10).
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub theme: ThemeName,
    pub save_interval: SaveInterval,
    pub minimize_to_tray: bool,
    pub scroll_to_playing: bool,
    pub top_panel: TopPanelLayout,
    pub columns: ColumnsConfig,
    pub covers: CoverSettings,
    pub info_labels: BTreeMap<InfoLabelKey, String>,
    pub visualization: VizSettings,
    /// Раздел `[playback]` (§2.4): до С4 (01_audio_modes) — старые `[audio]`,
    /// `[dsd]`, `audio_device`; далее заменяется на `ModeSettings`/`PlaybackDto`.
    pub playback: LegacyPlayback,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            theme: ThemeName::default(),
            save_interval: SaveInterval::default(),
            minimize_to_tray: false,
            scroll_to_playing: true,
            top_panel: TopPanelLayout::default(),
            columns: ColumnsConfig::default(),
            covers: CoverSettings::default(),
            info_labels: default_info_labels(),
            visualization: VizSettings::default(),
            playback: LegacyPlayback::default(),
        }
    }
}

/// Имя темы = имя файла `themes/<имя>.toml` без расширения (§2.4,
/// `src/settings.rs:77-80`). Непустое, без разделителей пути; существование
/// файла темы проверяется при чтении темы, не при разборе настроек.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ThemeName(Box<str>);

impl ThemeName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ThemeName {
    fn default() -> ThemeName {
        ThemeName("light".into())
    }
}

/// N — интервал отложенной записи (ТЗ-33, ТЗ-8, НФ-1, §2.4). Других значений
/// нет (И-Т3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SaveInterval {
    S10,
    #[default]
    S30,
    S60,
    S120,
}

impl SaveInterval {
    /// Длительность интервала (ТЗ-33).
    pub const fn duration(self) -> Duration {
        Duration::from_secs(self.secs())
    }

    /// Интервал в секундах — значение ключа `save_interval` (§2.4).
    pub const fn secs(self) -> u64 {
        match self {
            SaveInterval::S10 => 10,
            SaveInterval::S30 => 30,
            SaveInterval::S60 => 60,
            SaveInterval::S120 => 120,
        }
    }

    /// Обратное отображение из значения ключа `save_interval`; `None` — вне
    /// допустимого множества {10, 30, 60, 120} (ТЗ-8).
    pub fn from_secs(s: i64) -> Option<SaveInterval> {
        match s {
            10 => Some(SaveInterval::S10),
            30 => Some(SaveInterval::S30),
            60 => Some(SaveInterval::S60),
            120 => Some(SaveInterval::S120),
            _ => None,
        }
    }
}

/// Нижняя/верхняя граница и значение по умолчанию `TopPanelLayout::cover_size`
/// (§2.4, `src/settings.rs:763-766`).
pub const COVER_SIZE_MIN: f32 = 100.0;
pub const COVER_SIZE_MAX: f32 = 400.0;
pub const COVER_SIZE_DEFAULT: f32 = 200.0;

/// Границы и значение по умолчанию `TopPanelLayout::col_info_w` (§2.4,
/// `src/settings.rs:767-770`).
pub const COL_INFO_W_MIN: f32 = 200.0;
pub const COL_INFO_W_MAX: f32 = 400.0;
pub const COL_INFO_W_DEFAULT: f32 = 240.0;

/// Границы и значение по умолчанию `TopPanelLayout::col_gap` (§2.4,
/// `src/settings.rs:771-775`).
pub const COL_GAP_MIN: f32 = 0.0;
pub const COL_GAP_MAX: f32 = 20.0;
pub const COL_GAP_DEFAULT: f32 = 12.0;

/// Размеры верхней панели, логические px (§2.4, `src/settings.rs:763-775`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TopPanelLayout {
    /// Сторона обложки, px. Диапазон [`COVER_SIZE_MIN`]..=[`COVER_SIZE_MAX`].
    pub cover_size: f32,
    /// Ширина колонки информации о треке, px. Диапазон
    /// [`COL_INFO_W_MIN`]..=[`COL_INFO_W_MAX`].
    pub col_info_w: f32,
    /// Горизонтальный отступ между колонками, px. Диапазон
    /// [`COL_GAP_MIN`]..=[`COL_GAP_MAX`].
    pub col_gap: f32,
}

impl Default for TopPanelLayout {
    fn default() -> TopPanelLayout {
        TopPanelLayout {
            cover_size: COVER_SIZE_DEFAULT,
            col_info_w: COL_INFO_W_DEFAULT,
            col_gap: COL_GAP_DEFAULT,
        }
    }
}

/// Видимость, порядок и описания колонок (Т-2, §2.4). Ширин здесь нет — они в
/// `SessionState` (§2.5).
#[derive(Clone, PartialEq, Debug)]
pub struct ColumnsConfig {
    /// Слева направо; каждая `ColumnId` ровно один раз (проверяется при
    /// разборе, §6.2).
    pub order: Vec<ColumnId>,
    pub defs: BTreeMap<ColumnId, ColumnDef>,
}

impl Default for ColumnsConfig {
    fn default() -> ColumnsConfig {
        ColumnsConfig { order: ColumnId::ALL.to_vec(), defs: default_column_defs() }
    }
}

/// Описание колонки — нынешний `ColumnCfg` (`src/settings.rs:39-69`) без поля
/// `width` (§2.4).
#[derive(Clone, PartialEq, Debug)]
pub struct ColumnDef {
    pub title: String,
    pub priority: f32,
    pub min_width: f32,
    pub max_width: Option<f32>,
    pub max_width_percent: Option<f32>,
    pub visible: bool,
    pub kind: ColumnKind,
}

/// Тип колонки — нынешнее `ColumnCfg::column_type` (§2.4): отсутствие ключа
/// или `"data"` — `Data`, `"now-playing"` — `NowPlaying`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnKind {
    Data,
    NowPlaying,
}

/// Значения по умолчанию `ColumnsConfig::defs` — `default_columns()`
/// (`src/settings.rs:261-296`) без поля `width`, разложенное по `ColumnId`.
fn default_column_defs() -> BTreeMap<ColumnId, ColumnDef> {
    let legacy = crate::settings::default_columns();
    ColumnId::ALL
        .iter()
        .filter_map(|&id| legacy.get(id.key()).map(|cfg| (id, column_def_from_legacy(cfg))))
        .collect()
}

fn column_def_from_legacy(cfg: &crate::settings::ColumnCfg) -> ColumnDef {
    let kind = match cfg.column_type.as_deref() {
        Some("now-playing") => ColumnKind::NowPlaying,
        _ => ColumnKind::Data,
    };
    ColumnDef {
        title: cfg.title.clone(),
        priority: cfg.priority,
        min_width: cfg.min_width,
        max_width: cfg.max_width,
        max_width_percent: cfg.max_width_percent,
        visible: cfg.visible,
        kind,
    }
}

/// Настройки обложек альбома (§2.4).
#[derive(Clone, PartialEq, Debug)]
pub struct CoverSettings {
    /// Все источники без повторов: неизвестные ключи удаляются, недостающие
    /// дописываются в порядке по умолчанию (как `cover_priority_ordered`,
    /// `src/settings.rs:1033`).
    pub priority: Vec<CoverSource>,
    pub folder_names: Vec<String>,
    pub online: bool,
}

impl Default for CoverSettings {
    fn default() -> CoverSettings {
        CoverSettings {
            priority: CoverSource::ALL.to_vec(),
            folder_names: crate::settings::default_cover_folder_names(),
            online: true,
        }
    }
}

/// Ключ подписи панели информации: только ключи `default_info_labels()`
/// (`src/settings.rs:299-313`, §2.4).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct InfoLabelKey(&'static str);

impl InfoLabelKey {
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

/// Значения по умолчанию `Settings::info_labels` — `default_info_labels()`
/// (`src/settings.rs:317-336`), разложенные по стабильным ключам
/// `INFO_LABEL_KEYS` (`src/settings.rs:299-313`).
fn default_info_labels() -> BTreeMap<InfoLabelKey, String> {
    let labels = crate::settings::default_info_labels();
    crate::settings::INFO_LABEL_KEYS
        .iter()
        .map(|&key| (InfoLabelKey(key), labels.get(key).cloned().unwrap_or_default()))
        .collect()
}

/// Раздел `[playback]` (§2.4) до С4 (01_audio_modes): старые `[audio]`,
/// `[dsd]`, `audio_device` (`src/settings.rs:694-819`). Заменяется на
/// `ModeSettings`/`PlaybackDto` (ADR-04, ТЗ-8 (01_audio_modes)), когда
/// появится модель режимов.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct LegacyPlayback {
    pub audio: AudioCfg,
    pub dsd: DsdCfg,
    pub audio_device: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_matches_documented_defaults() {
        let s = Settings::default();

        assert_eq!(s.theme.as_str(), "light");
        assert_eq!(s.save_interval, SaveInterval::S30);
        assert_eq!(s.save_interval.secs(), 30);
        assert!(!s.minimize_to_tray);
        assert!(s.scroll_to_playing);

        assert_eq!(s.top_panel.cover_size, COVER_SIZE_DEFAULT);
        assert_eq!(s.top_panel.col_info_w, COL_INFO_W_DEFAULT);
        assert_eq!(s.top_panel.col_gap, COL_GAP_DEFAULT);

        assert_eq!(s.columns.order, ColumnId::ALL.to_vec());
        assert_eq!(s.columns.defs.len(), ColumnId::ALL.len());
        let now_playing = &s.columns.defs[&ColumnId::NowPlaying];
        assert_eq!(now_playing.kind, ColumnKind::NowPlaying);
        let title = &s.columns.defs[&ColumnId::Title];
        assert_eq!(title.kind, ColumnKind::Data);
        assert!(title.visible);

        assert_eq!(s.covers.priority, CoverSource::ALL.to_vec());
        assert!(s.covers.online);
        assert!(!s.covers.folder_names.is_empty());

        assert_eq!(s.info_labels.get(&InfoLabelKey("artist")).map(String::as_str), Some("Artist"));
        assert_eq!(s.info_labels.len(), crate::settings::INFO_LABEL_KEYS.len());

        assert!(!s.playback.audio.bit_perfect);
        assert_eq!(s.playback.audio_device, "");

        assert!(s.visualization.skip_fulltrack_for_dsd);
    }

    #[test]
    fn save_interval_secs_roundtrip() {
        for v in [SaveInterval::S10, SaveInterval::S30, SaveInterval::S60, SaveInterval::S120] {
            assert_eq!(SaveInterval::from_secs(i64::try_from(v.secs()).expect("fits i64")), Some(v));
        }
        assert_eq!(SaveInterval::from_secs(31), None);
    }
}
