//! Типы `settings.toml` (ТЗ-2, §2.4).
//!
//! Модель файла настроек: поля без `width` (она в `SessionState`, §2.5) и без
//! смешанных ключей состояния; значения по умолчанию совпадают с таблицей
//! ключей §2.4. Владелец действующего значения — `core::AppCore` (§8.1 С3).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::audio::visualizer::{defaults, OscilloscopeCfg, SpectrogramCfg, SpectrumCfg, VizSettings};
use crate::persist::keys::{walk, FileRead, KeyPath, KeySpec, LoadNote, LoadNoteKind, Parsed};
use crate::persist::{ReferenceText, SerializeError};
use crate::settings::{AudioCfg, ColumnId, CoverSource, DsdCfg, DsdMode};

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
    /// Построить имя темы из произвольной строки, с той же проверкой, что
    /// при разборе ключа `theme` (§2.4): непустая, без `/` и `\`.
    pub fn new(s: impl Into<Box<str>>) -> Option<ThemeName> {
        let s = s.into();
        if s.is_empty() || s.contains('/') || s.contains('\\') {
            None
        } else {
            Some(ThemeName(s))
        }
    }

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

impl ColumnsConfig {
    /// Описание колонки по id (бывший `Settings::column_cfg`,
    /// `src/settings.rs:860`, §2.4).
    pub fn column_def(&self, id: ColumnId) -> Option<&ColumnDef> {
        self.defs.get(&id)
    }

    /// Заголовок колонки, с запасным значением — ключ (бывший
    /// `Settings::column_title`, `src/settings.rs:873`, §2.4).
    pub fn column_title(&self, id: ColumnId) -> String {
        self.defs
            .get(&id)
            .map(|d| d.title.clone())
            .unwrap_or_else(|| id.key().to_string())
    }

    /// Видимость колонки, по умолчанию — видима (бывший
    /// `Settings::column_visible`, `src/settings.rs:901`, §2.4).
    pub fn column_visible(&self, id: ColumnId) -> bool {
        self.defs.get(&id).map(|d| d.visible).unwrap_or(true)
    }

    /// Видимые колонки в порядке отображения (бывший
    /// `Settings::visible_columns`, `src/settings.rs:909`, §2.4).
    pub fn visible_columns(&self) -> Vec<ColumnId> {
        self.ordered_columns()
            .into_iter()
            .filter(|c| self.column_visible(*c))
            .collect()
    }

    /// Сделать колонку видимой. Ширина (бывшая часть `Settings::enable_column`,
    /// `src/settings.rs:977`) — поле `SessionState` (§2.5), здесь не трогается.
    pub fn enable_column(&mut self, id: ColumnId) {
        if let Some(def) = self.defs.get_mut(&id) {
            def.visible = true;
        }
    }

    /// Скрыть колонку. Ширина (бывшая часть `Settings::disable_column`,
    /// `src/settings.rs:991`) — поле `SessionState` (§2.5), здесь не трогается.
    pub fn disable_column(&mut self, id: ColumnId) {
        if let Some(def) = self.defs.get_mut(&id) {
            def.visible = false;
        }
    }

    /// Колонки в пользовательском порядке слева направо (бывший
    /// `Settings::ordered_columns`, `src/settings.rs:1000`, §2.4). В отличие от
    /// прежней версии не достраивает отсутствующие id и не фильтрует
    /// неизвестные ключи — инвариант «каждая `ColumnId` ровно один раз»
    /// проверяется при разборе файла (§6.2), а не здесь.
    pub fn ordered_columns(&self) -> Vec<ColumnId> {
        self.order.clone()
    }

    /// Переставить колонку из `from` в `to` в сохранённом порядке (бывший
    /// `Settings::move_column`, `src/settings.rs:1021`, §2.4).
    pub fn move_column(&mut self, from: usize, to: usize) {
        if from >= self.order.len() {
            return;
        }
        let col = self.order.remove(from);
        let to = to.min(self.order.len());
        self.order.insert(to, col);
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

impl CoverSettings {
    /// Имена файлов обложки папки для поиска: пустые/пробельные записи
    /// отбрасываются, при пустом результате — запасной список (бывший
    /// `Settings::cover_folder_names_list`, `src/settings.rs:1054`, §2.4).
    /// В отличие от `priority` (дополняется и дедуплицируется при разборе,
    /// §6.2), для `folder_names` этот запас не гарантирован инвариантом
    /// парсинга, поэтому метод остаётся нужен.
    pub fn cover_folder_names_list(&self) -> Vec<String> {
        let names: Vec<String> = self
            .folder_names
            .iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        if names.is_empty() {
            crate::settings::default_cover_folder_names()
        } else {
            names
        }
    }

    /// Переставить источник обложки в сохранённом порядке приоритета (бывший
    /// `Settings::move_cover`, `src/settings.rs:1069`, §2.4).
    pub fn move_cover(&mut self, from: usize, to: usize) {
        if from >= self.priority.len() {
            return;
        }
        let item = self.priority.remove(from);
        let to = to.min(self.priority.len());
        self.priority.insert(to, item);
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

impl Settings {
    /// Подпись панели информации по ключу: переопределение пользователя,
    /// иначе — дефолт (English), пустая строка в переопределении тоже
    /// считается отсутствующей (бывший `Settings::info_label`,
    /// `src/settings.rs:882`, §2.4).
    pub fn info_label(&self, key: &InfoLabelKey) -> String {
        self.info_labels
            .get(key)
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| {
                default_info_labels()
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| key.as_str().to_string())
            })
    }

    /// Все подписи панели информации в порядке отображения
    /// (`INFO_LABEL_KEYS`, бывший `Settings::info_labels_ordered`,
    /// `src/settings.rs:896`, §2.4).
    pub fn info_labels_ordered(&self) -> Vec<String> {
        crate::settings::INFO_LABEL_KEYS
            .iter()
            .map(|&k| self.info_label(&InfoLabelKey(k)))
            .collect()
    }
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

impl LegacyPlayback {
    /// ТЗ §7.4/§8.4 (01_audio_modes): bit-perfect активен, а DSD выводится
    /// конверсией в PCM (`dsd.mode = pcm`) — конфликт сценариев, bit-perfect
    /// для DSD-потока не сохраняется (бывший
    /// `Settings::dsd_pcm_breaks_bit_perfect`, `src/settings.rs:868`, §2.4).
    pub fn dsd_pcm_breaks_bit_perfect(&self) -> bool {
        self.audio.bit_perfect && self.dsd.mode == DsdMode::Pcm
    }
}

/// Извлечь значение листа через serde; `Err` — текст допустимого множества
/// для заметки `Invalid` (§6.2). Не годится для `ColumnId`/`CoverSource` —
/// их `Deserialize` (PascalCase) не совпадает с форматом файла (`.key()`,
/// snake_case): для `column_order`/`cover_priority`/`columns.<key>.*` ключ —
/// сам путь, сопоставление по `from_key` делается отдельно.
fn de<R: serde::de::DeserializeOwned>(v: &toml::Value, allowed: &'static str) -> Result<R, &'static str> {
    R::deserialize(v.clone()).map_err(|_| allowed)
}

/// Листовой ключ без доп. проверки диапазона (§2.4, §6.2): отсутствует →
/// `default_val`; неверный тип → `default_val` + `Invalid`.
fn leaf<R>(
    path: &str,
    get: impl Fn(&mut Settings) -> &mut R + Clone + 'static,
    default_val: R,
    allowed: &'static str,
) -> KeySpec<Settings>
where
    R: serde::de::DeserializeOwned + Clone + 'static,
{
    leaf_checked(path, get, default_val, allowed, |_| true)
}

/// Листовой ключ с проверкой допустимого множества `ok` (диапазон/набор
/// значений, §2.4, §6.2): вне множества — как неверный тип.
fn leaf_checked<R>(
    path: &str,
    get: impl Fn(&mut Settings) -> &mut R + Clone + 'static,
    default_val: R,
    allowed: &'static str,
    ok: impl Fn(&R) -> bool + 'static,
) -> KeySpec<Settings>
where
    R: serde::de::DeserializeOwned + Clone + 'static,
{
    KeySpec {
        path: path.to_string(),
        optional: false,
        read: Box::new({
            let get = get.clone();
            move |v, t| {
                let val: R = de(v, allowed)?;
                if !ok(&val) {
                    return Err(allowed);
                }
                *get(t) = val;
                Ok(())
            }
        }),
        default: Box::new(move |t| *get(t) = default_val.clone()),
    }
}

/// Текущее значение-заготовка колонки `id`: всегда есть запись в
/// `default_column_defs()` для каждой `ColumnId::ALL` (проверено
/// `settings_default_matches_documented_defaults`); запасное значение — на
/// случай расхождения, чтобы не паниковать (ТЗ §"запрет unwrap").
fn column_def_fallback(id: ColumnId) -> ColumnDef {
    default_column_defs().get(&id).cloned().unwrap_or(ColumnDef {
        title: id.key().to_string(),
        priority: 0.0,
        min_width: 0.0,
        max_width: None,
        max_width_percent: None,
        visible: true,
        kind: ColumnKind::Data,
    })
}

/// Описание колонки `id` в разбираемых настройках, создаёт запись при
/// отсутствии (карта изначально полна через `Settings::default()`, §2.4).
fn column_def_mut(t: &mut Settings, id: ColumnId) -> &mut ColumnDef {
    t.columns.defs.entry(id).or_insert_with(|| column_def_fallback(id))
}

/// `column_order` — массив ключей `ColumnId::key()`; неизвестные строки
/// отбрасываются при чтении, дедупликация и достройка — пост-проверкой
/// `adjust_column_order` (§2.4, §6.2).
fn column_order_spec() -> KeySpec<Settings> {
    KeySpec {
        path: "column_order".to_string(),
        optional: false,
        read: Box::new(|v, t| {
            let arr = v.as_array().ok_or("массив строк — ключи ColumnId::key()")?;
            t.columns.order = arr.iter().filter_map(|item| item.as_str().and_then(ColumnId::from_key)).collect();
            Ok(())
        }),
        default: Box::new(|t| t.columns.order = ColumnId::ALL.to_vec()),
    }
}

/// `columns.<key>.*` — семь листовых ключей на каждую `ColumnId` (§2.4, §6.2:
/// «таблица `KeySpec` — развёртка по каждому листовому ключу»).
fn column_specs() -> Vec<KeySpec<Settings>> {
    let mut specs = Vec::new();
    for id in ColumnId::ALL {
        let key = id.key();
        let def = column_def_fallback(id);

        specs.push(leaf(
            &format!("columns.{key}.title"),
            move |t: &mut Settings| &mut column_def_mut(t, id).title,
            def.title.clone(),
            "строка",
        ));
        specs.push(leaf_checked(
            &format!("columns.{key}.priority"),
            move |t: &mut Settings| &mut column_def_mut(t, id).priority,
            def.priority,
            "число ≥ 0",
            |v: &f32| *v >= 0.0,
        ));
        specs.push(leaf_checked(
            &format!("columns.{key}.min_width"),
            move |t: &mut Settings| &mut column_def_mut(t, id).min_width,
            def.min_width,
            "число ≥ 0",
            |v: &f32| *v >= 0.0,
        ));
        specs.push(KeySpec {
            path: format!("columns.{key}.max_width"),
            optional: true,
            read: Box::new(move |v, t| {
                let val: Option<f32> = de(v, "число ≥ 0")?;
                if val.is_some_and(|x| x < 0.0) {
                    return Err("число ≥ 0");
                }
                column_def_mut(t, id).max_width = val;
                Ok(())
            }),
            default: {
                let fallback = def.max_width;
                Box::new(move |t: &mut Settings| column_def_mut(t, id).max_width = fallback)
            },
        });
        specs.push(KeySpec {
            path: format!("columns.{key}.max_width_percent"),
            optional: true,
            read: Box::new(move |v, t| {
                let val: Option<f32> = de(v, "число в (0, 1]")?;
                if val.is_some_and(|x| !(x > 0.0 && x <= 1.0)) {
                    return Err("число в (0, 1]");
                }
                column_def_mut(t, id).max_width_percent = val;
                Ok(())
            }),
            default: {
                let fallback = def.max_width_percent;
                Box::new(move |t: &mut Settings| column_def_mut(t, id).max_width_percent = fallback)
            },
        });
        specs.push(leaf(
            &format!("columns.{key}.visible"),
            move |t: &mut Settings| &mut column_def_mut(t, id).visible,
            def.visible,
            "bool",
        ));
        specs.push(KeySpec {
            path: format!("columns.{key}.column_type"),
            optional: true,
            read: Box::new(move |v, t| {
                let s = v.as_str().ok_or("\"data\" или \"now-playing\"")?;
                let kind = match s {
                    "data" => ColumnKind::Data,
                    "now-playing" => ColumnKind::NowPlaying,
                    _ => return Err("\"data\" или \"now-playing\""),
                };
                column_def_mut(t, id).kind = kind;
                Ok(())
            }),
            default: {
                let fallback = def.kind;
                Box::new(move |t: &mut Settings| column_def_mut(t, id).kind = fallback)
            },
        });
    }
    specs
}

/// `cover_priority` — массив ключей `CoverSource::key()`; неизвестные строки
/// отбрасываются при чтении, дедупликация и достройка — пост-проверкой
/// `adjust_cover_priority` (§2.4, §6.2).
fn cover_priority_spec() -> KeySpec<Settings> {
    KeySpec {
        path: "cover_priority".to_string(),
        optional: false,
        read: Box::new(|v, t| {
            let arr = v.as_array().ok_or("массив строк — ключи CoverSource::key()")?;
            t.covers.priority = arr.iter().filter_map(|item| item.as_str().and_then(CoverSource::from_key)).collect();
            Ok(())
        }),
        default: Box::new(|t| t.covers.priority = CoverSource::ALL.to_vec()),
    }
}

/// `info_labels.<key>` — по одной строке на каждый ключ `INFO_LABEL_KEYS`
/// (§2.4, §6.2).
fn info_label_specs() -> Vec<KeySpec<Settings>> {
    crate::settings::INFO_LABEL_KEYS
        .iter()
        .map(|&key| {
            let default_val = default_info_labels().get(&InfoLabelKey(key)).cloned().unwrap_or_default();
            leaf(
                &format!("info_labels.{key}"),
                move |t: &mut Settings| t.info_labels.entry(InfoLabelKey(key)).or_default(),
                default_val,
                "строка",
            )
        })
        .collect()
}

/// `visualization.*` — три верхних поля `VizSettings` плюс развёртка
/// `oscilloscope.*`/`spectrogram.*`/`spectrum.*` по листам (§2.4, §6.2).
fn visualization_specs() -> Vec<KeySpec<Settings>> {
    let mut specs = vec![
        leaf(
            "visualization.skip_fulltrack_for_dsd",
            |t: &mut Settings| &mut t.visualization.skip_fulltrack_for_dsd,
            true,
            "bool",
        ),
        leaf(
            "visualization.viz_max_ram_mb",
            |t: &mut Settings| &mut t.visualization.viz_max_ram_mb,
            defaults::viz_max_ram_mb(),
            "целое ≥ 0",
        ),
        leaf(
            "visualization.disk_max_size_mb",
            |t: &mut Settings| &mut t.visualization.disk_max_size_mb,
            defaults::disk_max_size_mb(),
            "целое ≥ 0",
        ),
    ];
    specs.extend(oscilloscope_specs());
    specs.extend(spectrogram_specs());
    specs.extend(spectrum_specs());
    specs
}

fn oscilloscope_specs() -> Vec<KeySpec<Settings>> {
    let d = OscilloscopeCfg::default();
    vec![
        leaf(
            "visualization.oscilloscope.channels",
            |t: &mut Settings| &mut t.visualization.oscilloscope.channels,
            d.channels,
            "mono/stereo",
        ),
        leaf(
            "visualization.oscilloscope.bg_color",
            |t: &mut Settings| &mut t.visualization.oscilloscope.bg_color,
            d.bg_color.clone(),
            "строка (цвет)",
        ),
        leaf(
            "visualization.oscilloscope.fg_color",
            |t: &mut Settings| &mut t.visualization.oscilloscope.fg_color,
            d.fg_color.clone(),
            "строка (цвет)",
        ),
        leaf(
            "visualization.oscilloscope.sensitivity",
            |t: &mut Settings| &mut t.visualization.oscilloscope.sensitivity,
            d.sensitivity,
            "число",
        ),
        leaf_checked(
            "visualization.oscilloscope.line_width",
            |t: &mut Settings| &mut t.visualization.oscilloscope.line_width,
            d.line_width,
            "число 0.5..=4.0",
            |v: &f32| (0.5..=4.0).contains(v),
        ),
        leaf(
            "visualization.oscilloscope.draw_center_line",
            |t: &mut Settings| &mut t.visualization.oscilloscope.draw_center_line,
            d.draw_center_line,
            "bool",
        ),
        leaf_checked(
            "visualization.oscilloscope.max_columns",
            |t: &mut Settings| &mut t.visualization.oscilloscope.max_columns,
            d.max_columns,
            "целое 512..=8192",
            |v: &u32| (512..=8192).contains(v),
        ),
        leaf(
            "visualization.oscilloscope.cache_in_memory",
            |t: &mut Settings| &mut t.visualization.oscilloscope.cache_in_memory,
            d.cache_in_memory,
            "bool",
        ),
        leaf(
            "visualization.oscilloscope.cache_on_disk",
            |t: &mut Settings| &mut t.visualization.oscilloscope.cache_on_disk,
            d.cache_on_disk,
            "bool",
        ),
    ]
}

fn spectrogram_specs() -> Vec<KeySpec<Settings>> {
    let d = SpectrogramCfg::default();
    vec![
        leaf(
            "visualization.spectrogram.channels",
            |t: &mut Settings| &mut t.visualization.spectrogram.channels,
            d.channels,
            "mono/stereo",
        ),
        leaf(
            "visualization.spectrogram.bg_color",
            |t: &mut Settings| &mut t.visualization.spectrogram.bg_color,
            d.bg_color.clone(),
            "строка (цвет)",
        ),
        leaf(
            "visualization.spectrogram.fg_color",
            |t: &mut Settings| &mut t.visualization.spectrogram.fg_color,
            d.fg_color.clone(),
            "строка (цвет)",
        ),
        leaf(
            "visualization.spectrogram.sensitivity",
            |t: &mut Settings| &mut t.visualization.spectrogram.sensitivity,
            d.sensitivity,
            "число",
        ),
        leaf_checked(
            "visualization.spectrogram.fft_size",
            |t: &mut Settings| &mut t.visualization.spectrogram.fft_size,
            d.fft_size,
            "512, 1024, 2048, 4096 или 8192",
            |v: &u32| matches!(v, 512 | 1024 | 2048 | 4096 | 8192),
        ),
        leaf(
            "visualization.spectrogram.window_type",
            |t: &mut Settings| &mut t.visualization.spectrogram.window_type,
            d.window_type,
            "hann/hamming/blackman",
        ),
        leaf(
            "visualization.spectrogram.freq_scale",
            |t: &mut Settings| &mut t.visualization.spectrogram.freq_scale,
            d.freq_scale,
            "linear/log/mel",
        ),
        leaf(
            "visualization.spectrogram.freq_min",
            |t: &mut Settings| &mut t.visualization.spectrogram.freq_min,
            d.freq_min,
            "целое ≥ 0",
        ),
        leaf(
            "visualization.spectrogram.freq_max",
            |t: &mut Settings| &mut t.visualization.spectrogram.freq_max,
            d.freq_max,
            "целое ≥ 0",
        ),
        leaf_checked(
            "visualization.spectrogram.gain_db",
            |t: &mut Settings| &mut t.visualization.spectrogram.gain_db,
            d.gain_db,
            "число -40.0..=100.0",
            |v: &f32| (-40.0..=100.0).contains(v),
        ),
        leaf_checked(
            "visualization.spectrogram.range_db",
            |t: &mut Settings| &mut t.visualization.spectrogram.range_db,
            d.range_db,
            "число 1.0..=200.0",
            |v: &f32| (1.0..=200.0).contains(v),
        ),
        leaf_checked(
            "visualization.spectrogram.high_boost_db",
            |t: &mut Settings| &mut t.visualization.spectrogram.high_boost_db,
            d.high_boost_db,
            "число 0.0..=60.0",
            |v: &f32| (0.0..=60.0).contains(v),
        ),
        leaf(
            "visualization.spectrogram.palette",
            |t: &mut Settings| &mut t.visualization.spectrogram.palette,
            d.palette,
            "magma/viridis/plasma/inferno/gray/thermal/rainbow/solid",
        ),
        leaf_checked(
            "visualization.spectrogram.max_frames",
            |t: &mut Settings| &mut t.visualization.spectrogram.max_frames,
            d.max_frames,
            "целое 512..=8192",
            |v: &u32| (512..=8192).contains(v),
        ),
        leaf(
            "visualization.spectrogram.dsd_cic_compensation",
            |t: &mut Settings| &mut t.visualization.spectrogram.dsd_cic_compensation,
            d.dsd_cic_compensation,
            "bool",
        ),
        leaf(
            "visualization.spectrogram.cache_in_memory",
            |t: &mut Settings| &mut t.visualization.spectrogram.cache_in_memory,
            d.cache_in_memory,
            "bool",
        ),
        leaf(
            "visualization.spectrogram.cache_on_disk",
            |t: &mut Settings| &mut t.visualization.spectrogram.cache_on_disk,
            d.cache_on_disk,
            "bool",
        ),
    ]
}

fn spectrum_specs() -> Vec<KeySpec<Settings>> {
    let d = SpectrumCfg::default();
    vec![
        leaf(
            "visualization.spectrum.channels",
            |t: &mut Settings| &mut t.visualization.spectrum.channels,
            d.channels,
            "mono/stereo",
        ),
        leaf(
            "visualization.spectrum.bg_color",
            |t: &mut Settings| &mut t.visualization.spectrum.bg_color,
            d.bg_color.clone(),
            "строка (цвет)",
        ),
        leaf(
            "visualization.spectrum.fg_color",
            |t: &mut Settings| &mut t.visualization.spectrum.fg_color,
            d.fg_color.clone(),
            "строка (цвет)",
        ),
        leaf(
            "visualization.spectrum.sensitivity",
            |t: &mut Settings| &mut t.visualization.spectrum.sensitivity,
            d.sensitivity,
            "число",
        ),
        leaf_checked(
            "visualization.spectrum.bands",
            |t: &mut Settings| &mut t.visualization.spectrum.bands,
            d.bands,
            "целое 4..=128",
            |v: &u32| (4..=128).contains(v),
        ),
        leaf(
            "visualization.spectrum.freq_scale",
            |t: &mut Settings| &mut t.visualization.spectrum.freq_scale,
            d.freq_scale,
            "linear/log/mel",
        ),
        leaf(
            "visualization.spectrum.level_scale",
            |t: &mut Settings| &mut t.visualization.spectrum.level_scale,
            d.level_scale,
            "linear/log",
        ),
        leaf_checked(
            "visualization.spectrum.smoothing",
            |t: &mut Settings| &mut t.visualization.spectrum.smoothing,
            d.smoothing,
            "число 0.0..=0.99",
            |v: &f32| (0.0..=0.99).contains(v),
        ),
        leaf(
            "visualization.spectrum.peak_hold",
            |t: &mut Settings| &mut t.visualization.spectrum.peak_hold,
            d.peak_hold,
            "bool",
        ),
        leaf(
            "visualization.spectrum.peak_decay_ms",
            |t: &mut Settings| &mut t.visualization.spectrum.peak_decay_ms,
            d.peak_decay_ms,
            "целое ≥ 0",
        ),
        leaf_checked(
            "visualization.spectrum.bar_gap",
            |t: &mut Settings| &mut t.visualization.spectrum.bar_gap,
            d.bar_gap,
            "целое 0..=10",
            |v: &u32| (0..=10).contains(v),
        ),
        leaf_checked(
            "visualization.spectrum.bar_radius",
            |t: &mut Settings| &mut t.visualization.spectrum.bar_radius,
            d.bar_radius,
            "целое 0..=12",
            |v: &u32| (0..=12).contains(v),
        ),
        leaf(
            "visualization.spectrum.gradient",
            |t: &mut Settings| &mut t.visualization.spectrum.gradient,
            d.gradient,
            "bool",
        ),
        leaf(
            "visualization.spectrum.dsd_cic_compensation",
            |t: &mut Settings| &mut t.visualization.spectrum.dsd_cic_compensation,
            d.dsd_cic_compensation,
            "bool",
        ),
    ]
}

/// Раздел `[playback]` до С4 (01_audio_modes, §8.1 С3): старые `[audio]`,
/// `[dsd]`, `audio_device`, по листовым ключам `AudioCfg`/`DsdCfg`
/// (`src/settings.rs`, §2.4).
fn legacy_playback_specs() -> Vec<KeySpec<Settings>> {
    let audio = AudioCfg::default();
    let dsd = DsdCfg::default();
    vec![
        leaf(
            "audio.bit_perfect",
            |t: &mut Settings| &mut t.playback.audio.bit_perfect,
            audio.bit_perfect,
            "bool",
        ),
        leaf_checked(
            "audio.ring_buffer_ms",
            |t: &mut Settings| &mut t.playback.audio.ring_buffer_ms,
            audio.ring_buffer_ms,
            "целое 100..=10000",
            |v: &u32| (crate::settings::RING_BUFFER_MS_MIN..=crate::settings::RING_BUFFER_MS_MAX).contains(v),
        ),
        leaf(
            "audio.resampler.algorithm",
            |t: &mut Settings| &mut t.playback.audio.resampler.algorithm,
            audio.resampler.algorithm,
            "linear/cubic/sinc_fast/sinc_medium/sinc_slow",
        ),
        leaf(
            "audio.resampler.dither",
            |t: &mut Settings| &mut t.playback.audio.resampler.dither,
            audio.resampler.dither,
            "tpdf/triangular/off",
        ),
        leaf(
            "audio.resampler.mode",
            |t: &mut Settings| &mut t.playback.audio.resampler.mode,
            audio.resampler.mode,
            "auto/native/fixed",
        ),
        leaf(
            "audio.resampler.fixed_rate",
            |t: &mut Settings| &mut t.playback.audio.resampler.fixed_rate,
            audio.resampler.fixed_rate,
            "целое ≥ 0",
        ),
        leaf(
            "audio.resampler.prefer_family",
            |t: &mut Settings| &mut t.playback.audio.resampler.prefer_family,
            audio.resampler.prefer_family,
            "auto/family44k/family48k",
        ),
        leaf(
            "audio.resampler.fallback_rate",
            |t: &mut Settings| &mut t.playback.audio.resampler.fallback_rate,
            audio.resampler.fallback_rate,
            "nearest/same_family/never_downsample",
        ),
        leaf(
            "audio.exclusive",
            |t: &mut Settings| &mut t.playback.audio.exclusive,
            audio.exclusive,
            "off/auto/strict",
        ),
        leaf(
            "audio.fallback",
            |t: &mut Settings| &mut t.playback.audio.fallback,
            audio.fallback,
            "nearest/device_default/fail",
        ),
        leaf(
            "audio.filter_hardware_only",
            |t: &mut Settings| &mut t.playback.audio.filter_hardware_only,
            audio.filter_hardware_only,
            "bool",
        ),
        leaf(
            "audio.filter_stereo_only",
            |t: &mut Settings| &mut t.playback.audio.filter_stereo_only,
            audio.filter_stereo_only,
            "bool",
        ),
        leaf(
            "dsd.mode",
            |t: &mut Settings| &mut t.playback.dsd.mode,
            dsd.mode,
            "pcm/native/dop",
        ),
        leaf(
            "dsd.target_bit_depth",
            |t: &mut Settings| &mut t.playback.dsd.target_bit_depth,
            dsd.target_bit_depth,
            "16/24/32/32float",
        ),
        leaf(
            "dsd.target_sample_rate",
            |t: &mut Settings| &mut t.playback.dsd.target_sample_rate,
            dsd.target_sample_rate,
            "auto/44100/48000/88200/96000/176400/192000",
        ),
        leaf(
            "audio_device",
            |t: &mut Settings| &mut t.playback.audio_device,
            String::new(),
            "строка",
        ),
    ]
}

/// `theme` — непустая строка без `/` и `\` (§2.4, `src/settings.rs:77-80`).
fn theme_spec() -> KeySpec<Settings> {
    KeySpec {
        path: "theme".to_string(),
        optional: false,
        read: Box::new(|v, t| {
            let s = v.as_str().ok_or("непустая строка без / и \\")?;
            if s.is_empty() || s.contains('/') || s.contains('\\') {
                return Err("непустая строка без / и \\");
            }
            t.theme = ThemeName(s.into());
            Ok(())
        }),
        default: Box::new(|t| t.theme = ThemeName::default()),
    }
}

/// `save_interval` — одно из {10, 30, 60, 120} секунд (ТЗ-8, ТЗ-33, НФ-1, §2.4).
fn save_interval_spec() -> KeySpec<Settings> {
    KeySpec {
        path: "save_interval".to_string(),
        optional: false,
        read: Box::new(|v, t| {
            let n = v.as_integer().ok_or("10, 30, 60 или 120")?;
            t.save_interval = SaveInterval::from_secs(n).ok_or("10, 30, 60 или 120")?;
            Ok(())
        }),
        default: Box::new(|t| t.save_interval = SaveInterval::default()),
    }
}

/// Полная таблица `KeySpec` для `settings.toml` (§2.4, §6.2): развёртка по
/// каждому листовому ключу, без группировки поддеревьев. `pub(crate)` —
/// используется напрямую тестом §7.2 `settings_and_state_keys_disjoint_and_cover_lists`
/// в `state_file.rs` (ТЗ-2).
pub(crate) fn settings_spec() -> Vec<KeySpec<Settings>> {
    let mut specs = vec![
        theme_spec(),
        save_interval_spec(),
        leaf(
            "minimize_to_tray",
            |t: &mut Settings| &mut t.minimize_to_tray,
            false,
            "bool",
        ),
        leaf(
            "scroll_to_playing",
            |t: &mut Settings| &mut t.scroll_to_playing,
            true,
            "bool",
        ),
        leaf_checked(
            "cover_size",
            |t: &mut Settings| &mut t.top_panel.cover_size,
            COVER_SIZE_DEFAULT,
            "число 100.0..=400.0",
            |v: &f32| (COVER_SIZE_MIN..=COVER_SIZE_MAX).contains(v),
        ),
        leaf_checked(
            "col_info_w",
            |t: &mut Settings| &mut t.top_panel.col_info_w,
            COL_INFO_W_DEFAULT,
            "число 200.0..=400.0",
            |v: &f32| (COL_INFO_W_MIN..=COL_INFO_W_MAX).contains(v),
        ),
        leaf_checked(
            "col_gap",
            |t: &mut Settings| &mut t.top_panel.col_gap,
            COL_GAP_DEFAULT,
            "число 0.0..=20.0",
            |v: &f32| (COL_GAP_MIN..=COL_GAP_MAX).contains(v),
        ),
        column_order_spec(),
        cover_priority_spec(),
        leaf(
            "cover_folder_names",
            |t: &mut Settings| &mut t.covers.folder_names,
            crate::settings::default_cover_folder_names(),
            "массив строк",
        ),
        leaf(
            "cover_online",
            |t: &mut Settings| &mut t.covers.online,
            true,
            "bool",
        ),
    ];
    specs.extend(column_specs());
    specs.extend(info_label_specs());
    specs.extend(visualization_specs());
    specs.extend(legacy_playback_specs());
    specs
}

/// Пост-проверка `column_order` (§6.2): дубликаты удаляются, недостающие
/// `ColumnId` дописываются в порядке `ColumnId::ALL` — заметка `Adjusted`,
/// если список изменился.
fn adjust_column_order(value: &mut Settings, notes: &mut Vec<LoadNote>) {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    let mut changed = false;
    for id in std::mem::take(&mut value.columns.order) {
        if seen.insert(id) {
            deduped.push(id);
        } else {
            changed = true;
        }
    }
    for id in ColumnId::ALL {
        if seen.insert(id) {
            deduped.push(id);
            changed = true;
        }
    }
    value.columns.order = deduped;
    if changed {
        notes.push(LoadNote {
            key: KeyPath::new("column_order"),
            kind: LoadNoteKind::Adjusted {
                reason: "дубликаты удалены, недостающие ColumnId дописаны в порядке ColumnId::ALL".into(),
            },
        });
    }
}

/// Пост-проверка `cover_priority` (§6.2): дубликаты удаляются, недостающие
/// `CoverSource` дописываются в порядке по умолчанию — заметка `Adjusted`,
/// если список изменился.
fn adjust_cover_priority(value: &mut Settings, notes: &mut Vec<LoadNote>) {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    let mut changed = false;
    for src in std::mem::take(&mut value.covers.priority) {
        if seen.insert(src) {
            deduped.push(src);
        } else {
            changed = true;
        }
    }
    for src in CoverSource::ALL {
        if seen.insert(src) {
            deduped.push(src);
            changed = true;
        }
    }
    value.covers.priority = deduped;
    if changed {
        notes.push(LoadNote {
            key: KeyPath::new("cover_priority"),
            kind: LoadNoteKind::Adjusted {
                reason: "дубликаты удалены, недостающие источники дописаны в порядке по умолчанию".into(),
            },
        });
    }
}

/// Разбор `settings.toml` (ТЗ-4, ТЗ-5, §6.1): чистая функция без
/// ввода-вывода. `Absent`/`Failed` — значения по умолчанию без заметок;
/// байты — utf8 и toml, ошибка любого из этапов — `Unparsable` с исходными
/// байтами; иначе обход по `settings_spec()` и пост-проверки `column_order`/
/// `cover_priority` (§6.2).
pub fn parse_settings(read: FileRead) -> Parsed<Settings> {
    let bytes = match read {
        FileRead::Absent => return Parsed::Absent { value: Settings::default() },
        FileRead::Failed(err) => return Parsed::ReadFailed { value: Settings::default(), err },
        FileRead::Bytes(b) => b,
    };

    let text = match std::str::from_utf8(&bytes) {
        Ok(t) => t,
        Err(e) => {
            return Parsed::Unparsable {
                value: Settings::default(),
                original: bytes.clone(),
                error: e.to_string().into(),
            };
        }
    };

    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            return Parsed::Unparsable {
                value: Settings::default(),
                original: bytes.clone(),
                error: e.to_string().into(),
            };
        }
    };

    let (mut value, mut notes) = walk(&table, &settings_spec());
    adjust_column_order(&mut value, &mut notes);
    adjust_cover_priority(&mut value, &mut notes);
    notes.sort_by(|a, b| a.key.cmp(&b.key));

    Parsed::Parsed { value, notes, reference: ReferenceText::of(bytes) }
}

/// DTO `columns.<key>.*` для сериализации (§2.4, §2.6): поля в том же
/// порядке, что в таблице ключей; `max_width`/`max_width_percent` — те же
/// optional-ключи, что при разборе (§6.2), пропускаются при `None`.
#[derive(serde::Serialize)]
struct ColumnDefDto<'a> {
    title: &'a str,
    priority: f32,
    min_width: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_width_percent: Option<f32>,
    visible: bool,
    column_type: &'static str,
}

impl<'a> ColumnDefDto<'a> {
    fn from(def: &'a ColumnDef) -> Self {
        ColumnDefDto {
            title: &def.title,
            priority: def.priority,
            min_width: def.min_width,
            max_width: def.max_width,
            max_width_percent: def.max_width_percent,
            visible: def.visible,
            column_type: match def.kind {
                ColumnKind::Data => "data",
                ColumnKind::NowPlaying => "now-playing",
            },
        }
    }
}

/// DTO `visualization.*` для сериализации (§2.4, §2.6): `VizSettings` не
/// реализует `Serialize` (его поля читаются по отдельности при разборе,
/// §6.2), поэтому верхние три листа перечислены явно; вложенные `*Cfg` — по
/// ссылке (уже `Serialize`, имена полей совпадают с таблицей ключей §2.4).
#[derive(serde::Serialize)]
struct VisualizationDto<'a> {
    skip_fulltrack_for_dsd: bool,
    viz_max_ram_mb: u32,
    disk_max_size_mb: u32,
    oscilloscope: &'a OscilloscopeCfg,
    spectrogram: &'a SpectrogramCfg,
    spectrum: &'a SpectrumCfg,
}

/// DTO записи `settings.toml` (§2.6). Порядок полей — не буквальный порядок
/// таблицы §2.4: TOML требует все скалярные/массивные ключи до первой
/// вложенной таблицы, иначе они достанутся последней открытой `[table]`.
/// Поэтому сперва идут скаляры и массивы §2.4 в их исходном относительном
/// порядке, затем таблицы (`columns`, `info_labels`, `visualization`,
/// `audio`, `dsd`) — тоже в порядке §2.4. Отображения — `BTreeMap` (ОВ-2,
/// ADR-2): одинаковые значения дают одинаковые байты в любом процессе.
#[derive(serde::Serialize)]
struct SettingsFile<'a> {
    theme: &'a str,
    save_interval: u64,
    minimize_to_tray: bool,
    scroll_to_playing: bool,
    cover_size: f32,
    col_info_w: f32,
    col_gap: f32,
    column_order: Vec<&'static str>,
    cover_priority: Vec<&'static str>,
    cover_folder_names: &'a [String],
    cover_online: bool,
    audio_device: &'a str,
    columns: BTreeMap<&'static str, ColumnDefDto<'a>>,
    info_labels: BTreeMap<&'a str, &'a str>,
    visualization: VisualizationDto<'a>,
    audio: &'a AudioCfg,
    dsd: &'a DsdCfg,
}

/// Детерминированный текст файла (ОВ-2): одинаковые значения `Settings` →
/// одинаковые байты в любом процессе — гарантируется фиксированным порядком
/// полей DTO и `BTreeMap` для отображений (§2.6).
pub fn serialize_settings(s: &Settings) -> Result<Arc<[u8]>, SerializeError> {
    let columns: BTreeMap<&'static str, ColumnDefDto> =
        s.columns.defs.iter().map(|(id, def)| (id.key(), ColumnDefDto::from(def))).collect();
    let info_labels: BTreeMap<&str, &str> =
        s.info_labels.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let dto = SettingsFile {
        theme: s.theme.as_str(),
        save_interval: s.save_interval.secs(),
        minimize_to_tray: s.minimize_to_tray,
        scroll_to_playing: s.scroll_to_playing,
        cover_size: s.top_panel.cover_size,
        col_info_w: s.top_panel.col_info_w,
        col_gap: s.top_panel.col_gap,
        column_order: s.columns.order.iter().map(|id| id.key()).collect(),
        cover_priority: s.covers.priority.iter().map(|src| src.key()).collect(),
        cover_folder_names: &s.covers.folder_names,
        cover_online: s.covers.online,
        audio_device: &s.playback.audio_device,
        columns,
        info_labels,
        visualization: VisualizationDto {
            skip_fulltrack_for_dsd: s.visualization.skip_fulltrack_for_dsd,
            viz_max_ram_mb: s.visualization.viz_max_ram_mb,
            disk_max_size_mb: s.visualization.disk_max_size_mb,
            oscilloscope: &s.visualization.oscilloscope,
            spectrogram: &s.visualization.spectrogram,
            spectrum: &s.visualization.spectrum,
        },
        audio: &s.playback.audio,
        dsd: &s.playback.dsd,
    };

    let text = toml::to_string(&dto).map_err(|e| SerializeError(e.to_string().into()))?;
    Ok(Arc::from(text.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

    #[test]
    fn move_column_reorders_stored_keys() {
        let mut cfg = ColumnsConfig {
            order: vec![ColumnId::Title, ColumnId::Genre, ColumnId::Artist, ColumnId::Year],
            ..ColumnsConfig::default()
        };
        cfg.move_column(0, 2); // title -> position 2
        assert_eq!(
            cfg.order,
            vec![ColumnId::Genre, ColumnId::Artist, ColumnId::Title, ColumnId::Year]
        );
    }

    #[test]
    fn column_cfg_title_applies() {
        let cfg = ColumnsConfig::default();
        assert_eq!(cfg.column_title(ColumnId::NowPlaying), "\u{25B6}");
        assert_eq!(cfg.column_title(ColumnId::Title), "Название");
        assert_eq!(cfg.column_title(ColumnId::Artist), "Исполнитель");
    }

    #[test]
    fn cover_move_reorders_stored_keys() {
        let mut covers = CoverSettings::default();
        covers.move_cover(0, 2); // folder -> last
        assert_eq!(
            covers.priority,
            vec![CoverSource::Embedded, CoverSource::Internet, CoverSource::Folder]
        );
    }

    #[test]
    fn cover_folder_names_fallback_and_trim() {
        let covers = CoverSettings::default();
        assert_eq!(covers.cover_folder_names_list(), crate::settings::default_cover_folder_names());
        assert_eq!(crate::settings::default_cover_folder_names().len(), 17);

        let covers = CoverSettings {
            folder_names: vec!["front.jpg".into(), "   ".into(), "art.png".into()],
            ..CoverSettings::default()
        };
        assert_eq!(covers.cover_folder_names_list(), vec!["front.jpg", "art.png"]);

        // All-blank stored names fall back to the default list.
        let covers = CoverSettings { folder_names: vec![" ".into()], ..CoverSettings::default() };
        assert_eq!(covers.cover_folder_names_list(), crate::settings::default_cover_folder_names());
    }

    #[test]
    fn info_labels_defaults_in_display_order() {
        let s = Settings::default();
        let ordered = s.info_labels_ordered();
        assert_eq!(ordered.len(), crate::settings::INFO_LABEL_KEYS.len());
        assert_eq!(ordered[0], "Artist");
        assert_eq!(ordered[2], "Title");
        assert_eq!(ordered[10], "Bit depth");
        assert_eq!(ordered[12], "Channels");
    }

    #[test]
    fn info_label_overrides_and_falls_back() {
        let mut s = Settings::default();
        let artist = InfoLabelKey("artist");
        assert_eq!(s.info_label(&artist), "Artist");

        s.info_labels.insert(artist.clone(), "Исполнитель".into());
        let channels = InfoLabelKey("channels");
        s.info_labels.insert(channels.clone(), String::new());
        assert_eq!(s.info_label(&artist), "Исполнитель");
        // Empty string falls back to the default (not blank/raw key).
        assert_eq!(s.info_label(&channels), "Channels");
        // Unknown key: raw key as last resort.
        let bogus = InfoLabelKey("bogus");
        assert_eq!(s.info_label(&bogus), "bogus");
    }

    fn bytes(text: &str) -> FileRead {
        FileRead::Bytes(Arc::from(text.as_bytes()))
    }

    fn parsed(read: FileRead) -> (Settings, Vec<LoadNote>) {
        match parse_settings(read) {
            Parsed::Parsed { value, notes, .. } => (value, notes),
            Parsed::Absent { .. } | Parsed::Unparsable { .. } | Parsed::ReadFailed { .. } => {
                panic!("expected Parsed")
            }
        }
    }

    #[test]
    fn parse_absent_gives_defaults_without_notes() {
        match parse_settings(FileRead::Absent) {
            Parsed::Absent { value } => assert_eq!(value, Settings::default()),
            Parsed::Parsed { .. } | Parsed::Unparsable { .. } | Parsed::ReadFailed { .. } => {
                panic!("expected Absent")
            }
        }
    }

    #[test]
    fn parse_read_failed_gives_defaults() {
        let err = crate::platform::fs::ReadError {
            class: crate::platform::fs::ReadErrorClass::Io,
            os_code: None,
            os_text: "disk error".into(),
            path: std::path::PathBuf::from("settings.toml"),
        };
        match parse_settings(FileRead::Failed(err)) {
            Parsed::ReadFailed { value, .. } => assert_eq!(value, Settings::default()),
            Parsed::Absent { .. } | Parsed::Parsed { .. } | Parsed::Unparsable { .. } => {
                panic!("expected ReadFailed")
            }
        }
    }

    #[test]
    fn parse_unparsable_keeps_original_bytes() {
        let original = b"[[".to_vec();
        match parse_settings(FileRead::Bytes(Arc::from(original.as_slice()))) {
            Parsed::Unparsable { value, original: kept, .. } => {
                assert_eq!(value, Settings::default());
                assert_eq!(kept.as_ref(), original.as_slice());
            }
            Parsed::Absent { .. } | Parsed::Parsed { .. } | Parsed::ReadFailed { .. } => {
                panic!("expected Unparsable")
            }
        }
    }

    #[test]
    fn empty_settings_gives_documented_defaults() {
        let (value, _notes) = parsed(bytes(""));
        assert_eq!(value, Settings::default());
    }

    #[test]
    fn save_interval_invalid_value() {
        let (value, notes) = parsed(bytes("save_interval = 45"));
        assert_eq!(value.save_interval, SaveInterval::S30);
        let note = notes.iter().find(|n| n.key == KeyPath::new("save_interval")).expect("note present");
        match &note.kind {
            LoadNoteKind::Invalid { allowed, .. } => assert_eq!(*allowed, "10, 30, 60 или 120"),
            other => panic!("unexpected note kind: {other:?}"),
        }
    }

    #[test]
    fn invalid_cover_size_falls_back_to_default() {
        let (value, notes) = parsed(bytes("cover_size = 9999"));
        assert_eq!(value.top_panel.cover_size, COVER_SIZE_DEFAULT);
        let note = notes.iter().find(|n| n.key == KeyPath::new("cover_size")).expect("note present");
        match &note.kind {
            LoadNoteKind::Invalid { .. } => {}
            other => panic!("unexpected note kind: {other:?}"),
        }
    }

    #[test]
    fn parse_column_order_adjusts() {
        let (value, notes) = parsed(bytes("column_order = [\"title\", \"title\", \"artist\"]"));
        // Дубликат убран, недостающие ColumnId дописаны в порядке ColumnId::ALL.
        assert_eq!(value.columns.order[0], ColumnId::Title);
        assert_eq!(value.columns.order[1], ColumnId::Artist);
        assert_eq!(value.columns.order.len(), ColumnId::ALL.len());
        let dup_count = value.columns.order.iter().filter(|&&id| id == ColumnId::Title).count();
        assert_eq!(dup_count, 1);
        let note = notes.iter().find(|n| n.key == KeyPath::new("column_order")).expect("note present");
        assert!(matches!(note.kind, LoadNoteKind::Adjusted { .. }));
    }

    #[test]
    fn parse_cover_priority_adjusts() {
        let (value, notes) = parsed(bytes("cover_priority = [\"embedded\", \"embedded\"]"));
        assert_eq!(value.covers.priority[0], CoverSource::Embedded);
        assert_eq!(value.covers.priority.len(), CoverSource::ALL.len());
        let dup_count = value.covers.priority.iter().filter(|&&s| s == CoverSource::Embedded).count();
        assert_eq!(dup_count, 1);
        let note = notes.iter().find(|n| n.key == KeyPath::new("cover_priority")).expect("note present");
        assert!(matches!(note.kind, LoadNoteKind::Adjusted { .. }));
    }

    #[test]
    fn legacy_audio_keys_parse_by_keys() {
        let text = "\
audio.bit_perfect = true
audio.ring_buffer_ms = 500
audio.resampler.algorithm = \"cubic\"
audio.resampler.dither = \"off\"
audio.resampler.mode = \"fixed\"
audio.resampler.fixed_rate = 48000
audio.resampler.prefer_family = \"family48k\"
audio.resampler.fallback_rate = \"never_downsample\"
audio.exclusive = \"strict\"
audio.fallback = \"fail\"
audio.filter_hardware_only = true
audio.filter_stereo_only = true
dsd.mode = \"native\"
dsd.target_bit_depth = \"24\"
dsd.target_sample_rate = \"96000\"
audio_device = \"hw:0,0\"
";
        let (value, notes) = parsed(bytes(text));
        assert!(value.playback.audio.bit_perfect);
        assert_eq!(value.playback.audio.ring_buffer_ms, 500);
        assert!(value.playback.audio.filter_hardware_only);
        assert!(value.playback.audio.filter_stereo_only);
        assert_eq!(value.playback.audio_device, "hw:0,0");
        assert_eq!(value.playback.dsd.mode, DsdMode::Native);
        assert!(notes.iter().all(|n| !matches!(n.kind, LoadNoteKind::Unknown)));
    }

    #[test]
    fn old_state_keys_are_reported_unknown() {
        let text = "\
volume = 0.5
muted = false
last_dir = \"/music\"
repeat = \"all\"
shuffle = true
sorted_col = \"title\"
sort_desc = false
win_x = 10
win_y = 20
win_w = 800
win_h = 600
column_widths = []
column_visibility = []

[columns.title]
width = 120

[visualization]
mode = \"spectrum\"
";
        let (_value, notes) = parsed(bytes(text));
        for key in [
            "volume",
            "muted",
            "last_dir",
            "repeat",
            "shuffle",
            "sorted_col",
            "sort_desc",
            "win_x",
            "win_y",
            "win_w",
            "win_h",
            "column_widths",
            "column_visibility",
            "columns.title.width",
            "visualization.mode",
        ] {
            let note = notes.iter().find(|n| n.key == KeyPath::new(key));
            assert!(matches!(note.map(|n| &n.kind), Some(LoadNoteKind::Unknown)), "key {key} should be Unknown, notes: {notes:?}");
        }
    }

    /// `Settings` отличная от значений по умолчанию во всех разделах §2.4:
    /// темы, колонок, обложек, подписей, визуализации и `[playback]`.
    #[allow(clippy::field_reassign_with_default)]
    fn non_default_settings() -> Settings {
        let mut s = Settings::default();
        s.theme = ThemeName("dark".into());
        s.save_interval = SaveInterval::S60;
        s.minimize_to_tray = true;
        s.scroll_to_playing = false;
        s.top_panel.cover_size = 300.0;
        s.top_panel.col_info_w = 260.0;
        s.top_panel.col_gap = 4.0;
        s.columns.move_column(0, 2);
        column_def_mut(&mut s, ColumnId::Title).title = "Track title".to_string();
        s.covers.move_cover(0, 1);
        s.covers.folder_names = vec!["front.jpg".to_string()];
        s.covers.online = false;
        s.info_labels.insert(InfoLabelKey("artist"), "Performer".to_string());
        s.visualization.skip_fulltrack_for_dsd = false;
        s.visualization.oscilloscope.line_width = 2.5;
        s.playback.audio.bit_perfect = true;
        s.playback.audio.ring_buffer_ms = 2000;
        s.playback.dsd.mode = DsdMode::Native;
        s.playback.audio_device = "hw:1,0".to_string();
        s
    }

    /// Серилизация `s` детерминирована (ОВ-2) и round-trip'ится без заметок
    /// (§7.2): `serialize_settings` дважды даёт одинаковые байты, разбор
    /// результата даёт равное значение без заметок, а повторная сериализация
    /// разобранного значения воспроизводит исходные байты.
    fn assert_roundtrip_deterministic(s: &Settings) {
        let bytes1 = serialize_settings(s).expect("serialize ok");
        let bytes2 = serialize_settings(s).expect("serialize ok");
        assert_eq!(bytes1, bytes2, "same Settings must serialize to identical bytes");

        let (value, notes) = parsed(FileRead::Bytes(bytes1.clone()));
        assert_eq!(value, *s);
        assert!(notes.is_empty(), "expected zero notes, got: {notes:?}");

        let bytes3 = serialize_settings(&value).expect("serialize ok");
        assert_eq!(bytes1, bytes3, "re-serializing parsed value must reproduce original bytes");
    }

    #[test]
    fn settings_roundtrip_deterministic() {
        assert_roundtrip_deterministic(&Settings::default());
        assert_roundtrip_deterministic(&non_default_settings());
    }

    #[test]
    fn defaults_serialize_parses_with_zero_notes() {
        let bytes = serialize_settings(&Settings::default()).expect("serialize ok");
        let (value, notes) = parsed(FileRead::Bytes(bytes));
        assert_eq!(value, Settings::default());
        assert!(notes.is_empty(), "expected zero notes, got: {notes:?}");
    }
}
