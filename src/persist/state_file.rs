//! Новые типы и разбор `state.toml` (ТЗ-5, §2.5, §6.2).
//!
//! `SessionState` хранит состояние сессии: поля приватны, читаются через
//! геттеры, меняются только через `apply` (`AppCore::change_state` будет
//! вызывать его с конкретным `Origin` — С4, писатель). До С4 (01_audio_modes)
//! `ModeKind`/`PlaybackState` нет: вместо целевого `playback.<режим>.volume` /
//! `playback.<режим>.muted` — прежняя единая громкость/mute в
//! `LegacyPlaybackState` (§8.1 С3: «в state.toml — прежняя одна громкость
//! `volume` (0…100, по умолчанию 100) и `muted`»).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::audio::visualizer::VisualizationMode;
use crate::persist::keys::{walk, FileRead, KeyPath, KeySpec, LoadNote, LoadNoteKind, Parsed};
use crate::persist::{ReferenceText, SerializeError};
use crate::settings::{ColumnId, RepeatMode};

/// Состояние сессии (§2.5): поля приватны, снаружи — только геттеры и
/// `apply`. `window_x`/`window_y`/`window_width`/`window_height` — черновик
/// разбора `window.x`/`window.y`/`window.width`/`window.height` (§6.2):
/// `walk` пишет туда по отдельным ключам, `adjust_window_geometry` собирает
/// из них `window.position`/`window.size` и возвращает их в `None` — наружу
/// (через `window()`) они не видны и не входят в `StateChange`.
#[derive(Clone, PartialEq, Debug)]
pub struct SessionState {
    playback: LegacyPlaybackState,
    column_widths: BTreeMap<ColumnId, WidthPct>,
    sort: Option<SortKey>,
    viz_mode: VisualizationMode,
    repeat: RepeatMode,
    shuffle: bool,
    window: WindowGeometry,
    last_dir: Option<PathBuf>,
    sort_column_raw: Option<ColumnId>,
    sort_direction_raw: Option<SortDirection>,
    window_x: Option<i32>,
    window_y: Option<i32>,
    window_width: Option<u32>,
    window_height: Option<u32>,
}

impl Default for SessionState {
    fn default() -> SessionState {
        SessionState {
            playback: LegacyPlaybackState::default(),
            column_widths: BTreeMap::new(),
            sort: None,
            viz_mode: VisualizationMode::Off,
            repeat: RepeatMode::Off,
            shuffle: false,
            window: WindowGeometry::default(),
            last_dir: None,
            sort_column_raw: None,
            sort_direction_raw: None,
            window_x: None,
            window_y: None,
            window_width: None,
            window_height: None,
        }
    }
}

impl SessionState {
    pub fn playback(&self) -> LegacyPlaybackState {
        self.playback
    }

    pub fn column_widths(&self) -> &BTreeMap<ColumnId, WidthPct> {
        &self.column_widths
    }

    pub fn sort(&self) -> Option<SortKey> {
        self.sort
    }

    pub fn viz_mode(&self) -> VisualizationMode {
        self.viz_mode
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn window(&self) -> WindowGeometry {
        self.window
    }

    pub fn last_dir(&self) -> Option<&Path> {
        self.last_dir.as_deref()
    }

    /// Единственная точка мутации (И-Т7): `AppCore::change_state` вызывает
    /// её с конкретным `Origin` (С4, писатель); здесь `Origin` не нужен —
    /// он управляет таймингом записи, не самим значением.
    #[allow(dead_code)]
    pub(crate) fn apply(&mut self, ch: StateChange) {
        match ch {
            StateChange::Volume(v) => self.playback.volume = v,
            StateChange::Muted(m) => self.playback.muted = m,
            StateChange::ColumnWidths(w) => self.column_widths = w,
            StateChange::Sort(s) => self.sort = s,
            StateChange::VizMode(m) => self.viz_mode = m,
            StateChange::Repeat(r) => self.repeat = r,
            StateChange::Shuffle(s) => self.shuffle = s,
            StateChange::Window(w) => self.window = w,
            StateChange::LastDir(d) => self.last_dir = Some(d),
        }
    }
}

/// Громкость/mute (§8.1 С3) до С4 (01_audio_modes): прежняя единая пара
/// вместо целевых `playback.<режим>.volume` / `playback.<режим>.muted`.
/// Заменяется на модель режимов, когда она появится.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LegacyPlaybackState {
    pub volume: u8,
    pub muted: bool,
}

impl Default for LegacyPlaybackState {
    fn default() -> LegacyPlaybackState {
        LegacyPlaybackState { volume: 100, muted: false }
    }
}

/// Ширина колонки в процентах (§2.5, Т-2): `(0, 100]`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct WidthPct(f32);

impl WidthPct {
    pub fn new(pct: f32) -> Option<WidthPct> {
        if pct > 0.0 && pct <= 100.0 {
            Some(WidthPct(pct))
        } else {
            None
        }
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// Ключ сортировки (§2.5, ТЗ-43): `column` не бывает `ColumnId::NowPlaying`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SortKey {
    pub column: ColumnId,
    pub direction: SortDirection,
}

/// Позиция окна, физические px (§2.5, `src/settings.rs:776-781`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PhysPos {
    pub x: i32,
    pub y: i32,
}

/// Размер окна, физические px (§2.5, `src/settings.rs:782-786`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PhysSize {
    pub width: u32,
    pub height: u32,
}

/// Геометрия окна (§2.5, `src/settings.rs:776-793`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WindowGeometry {
    /// `None` — не известно; на Wayland положение не читается (ADR-22).
    pub position: Option<PhysPos>,
    pub size: Option<PhysSize>,
    pub maximized: bool,
    pub fullscreen: bool,
}

/// Источник изменения состояния (ADR-22, ОВ-3). Потребитель — будущий
/// `AppCore::change_state` (С4, писатель); `SessionState::apply` его не
/// принимает.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    User,
    Program,
}

/// Единственная форма изменения `SessionState` (И-Т7). `ActiveMode`
/// (целевая модель режимов) опущен до С4 (01_audio_modes); `Volume`/`Muted`
/// работают с прежней единой парой `LegacyPlaybackState` (§8.1 С3).
#[derive(Clone, PartialEq, Debug)]
pub enum StateChange {
    Volume(u8),
    Muted(bool),
    ColumnWidths(BTreeMap<ColumnId, WidthPct>),
    /// `None` — снять ключ сортировки (ТЗ-42).
    Sort(Option<SortKey>),
    VizMode(VisualizationMode),
    Repeat(RepeatMode),
    Shuffle(bool),
    Window(WindowGeometry),
    LastDir(PathBuf),
}

/// Извлечь значение листа через serde; `Err` — текст допустимого множества
/// для заметки `Invalid` (§6.2). Не годится для `ColumnId` — его
/// `Deserialize` (PascalCase) не совпадает с форматом файла (`.key()`).
fn de<R: serde::de::DeserializeOwned>(v: &toml::Value, allowed: &'static str) -> Result<R, &'static str> {
    R::deserialize(v.clone()).map_err(|_| allowed)
}

/// Листовой обязательный ключ без доп. проверки диапазона (§6.2).
fn leaf<R>(
    path: &str,
    get: impl Fn(&mut SessionState) -> &mut R + Clone + 'static,
    default_val: R,
    allowed: &'static str,
) -> KeySpec<SessionState>
where
    R: serde::de::DeserializeOwned + Clone + 'static,
{
    leaf_checked(path, get, default_val, allowed, |_| true)
}

/// Листовой обязательный ключ с проверкой допустимого множества `ok` (§6.2).
fn leaf_checked<R>(
    path: &str,
    get: impl Fn(&mut SessionState) -> &mut R + Clone + 'static,
    default_val: R,
    allowed: &'static str,
    ok: impl Fn(&R) -> bool + 'static,
) -> KeySpec<SessionState>
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

/// `volume`/`muted` (§8.1 С3): прежняя единая пара вместо
/// `playback.<режим>.volume`/`playback.<режим>.muted` (целевая модель, до
/// С4 01_audio_modes).
fn legacy_playback_specs() -> Vec<KeySpec<SessionState>> {
    vec![
        leaf_checked(
            "volume",
            |t: &mut SessionState| &mut t.playback.volume,
            100,
            "целое 0..=100",
            |v: &u8| *v <= 100,
        ),
        leaf("muted", |t: &mut SessionState| &mut t.playback.muted, false, "bool"),
    ]
}

/// `columns.widths.<ключ колонки>` (§2.5, необяз., Т-2): отсутствие ключа —
/// «ширина не задана», не ошибка.
fn column_width_specs() -> Vec<KeySpec<SessionState>> {
    ColumnId::ALL
        .iter()
        .map(|&id| {
            let key = id.key();
            KeySpec {
                path: format!("columns.widths.{key}"),
                optional: true,
                read: Box::new(move |v, t: &mut SessionState| {
                    let pct: f32 = de(v, "число в (0, 100]")?;
                    let width = WidthPct::new(pct).ok_or("число в (0, 100]")?;
                    t.column_widths.insert(id, width);
                    Ok(())
                }),
                default: Box::new(move |t: &mut SessionState| {
                    t.column_widths.remove(&id);
                }),
            }
        })
        .collect()
}

/// `sort.column`/`sort.direction` (§2.5, необяз., оба или ни одного, ТЗ-43):
/// пишут в черновик `sort_column_raw`/`sort_direction_raw`, пара собирается
/// в `adjust_sort_pair` после `walk`.
fn sort_specs() -> Vec<KeySpec<SessionState>> {
    vec![
        KeySpec {
            path: "sort.column".to_string(),
            optional: true,
            read: Box::new(|v, t: &mut SessionState| {
                let s = v.as_str().ok_or("ключ колонки, кроме \"now_playing\"")?;
                let id = ColumnId::from_key(s).ok_or("ключ колонки, кроме \"now_playing\"")?;
                if id == ColumnId::NowPlaying {
                    return Err("ключ колонки, кроме \"now_playing\"");
                }
                t.sort_column_raw = Some(id);
                Ok(())
            }),
            default: Box::new(|t: &mut SessionState| t.sort_column_raw = None),
        },
        KeySpec {
            path: "sort.direction".to_string(),
            optional: true,
            read: Box::new(|v, t: &mut SessionState| {
                let s = v.as_str().ok_or("\"asc\" или \"desc\"")?;
                let dir = match s {
                    "asc" => SortDirection::Asc,
                    "desc" => SortDirection::Desc,
                    _ => return Err("\"asc\" или \"desc\""),
                };
                t.sort_direction_raw = Some(dir);
                Ok(())
            }),
            default: Box::new(|t: &mut SessionState| t.sort_direction_raw = None),
        },
    ]
}

/// `window.x`/`window.y`/`window.width`/`window.height` (§2.5, необяз.) и
/// `window.maximized`/`window.fullscreen` (обяз., default `false`).
/// `x`/`y`/`width`/`height` пишут в черновик, пара собирается в
/// `adjust_window_geometry` после `walk`.
fn window_specs() -> Vec<KeySpec<SessionState>> {
    vec![
        KeySpec {
            path: "window.x".to_string(),
            optional: true,
            read: Box::new(|v, t: &mut SessionState| {
                let n = v.as_integer().ok_or("целое i32")?;
                t.window_x = Some(i32::try_from(n).map_err(|_| "целое i32")?);
                Ok(())
            }),
            default: Box::new(|t: &mut SessionState| t.window_x = None),
        },
        KeySpec {
            path: "window.y".to_string(),
            optional: true,
            read: Box::new(|v, t: &mut SessionState| {
                let n = v.as_integer().ok_or("целое i32")?;
                t.window_y = Some(i32::try_from(n).map_err(|_| "целое i32")?);
                Ok(())
            }),
            default: Box::new(|t: &mut SessionState| t.window_y = None),
        },
        KeySpec {
            path: "window.width".to_string(),
            optional: true,
            read: Box::new(|v, t: &mut SessionState| {
                let n = v.as_integer().ok_or("целое > 0")?;
                let width = u32::try_from(n).map_err(|_| "целое > 0")?;
                if width == 0 {
                    return Err("целое > 0");
                }
                t.window_width = Some(width);
                Ok(())
            }),
            default: Box::new(|t: &mut SessionState| t.window_width = None),
        },
        KeySpec {
            path: "window.height".to_string(),
            optional: true,
            read: Box::new(|v, t: &mut SessionState| {
                let n = v.as_integer().ok_or("целое > 0")?;
                let height = u32::try_from(n).map_err(|_| "целое > 0")?;
                if height == 0 {
                    return Err("целое > 0");
                }
                t.window_height = Some(height);
                Ok(())
            }),
            default: Box::new(|t: &mut SessionState| t.window_height = None),
        },
        leaf(
            "window.maximized",
            |t: &mut SessionState| &mut t.window.maximized,
            false,
            "bool",
        ),
        leaf(
            "window.fullscreen",
            |t: &mut SessionState| &mut t.window.fullscreen,
            false,
            "bool",
        ),
    ]
}

/// `repeat` (§2.5): строки нижнего регистра, `RepeatMode`'s `Deserialize` —
/// PascalCase и не совпадает с форматом файла, поэтому разбор вручную.
fn repeat_spec() -> KeySpec<SessionState> {
    KeySpec {
        path: "repeat".to_string(),
        optional: false,
        read: Box::new(|v, t: &mut SessionState| {
            let s = v.as_str().ok_or("\"off\"/\"all\"/\"one\"")?;
            t.repeat = match s {
                "off" => RepeatMode::Off,
                "all" => RepeatMode::All,
                "one" => RepeatMode::One,
                _ => return Err("\"off\"/\"all\"/\"one\""),
            };
            Ok(())
        }),
        default: Box::new(|t: &mut SessionState| t.repeat = RepeatMode::Off),
    }
}

/// `last_dir` (§2.5, необяз.): строка — абсолютный путь, иначе `Invalid`.
fn last_dir_spec() -> KeySpec<SessionState> {
    KeySpec {
        path: "last_dir".to_string(),
        optional: true,
        read: Box::new(|v, t: &mut SessionState| {
            let s = v.as_str().ok_or("абсолютный путь")?;
            let path = PathBuf::from(s);
            if !path.is_absolute() {
                return Err("абсолютный путь");
            }
            t.last_dir = Some(path);
            Ok(())
        }),
        default: Box::new(|t: &mut SessionState| t.last_dir = None),
    }
}

/// Полная таблица ключей `state.toml` (§2.5). `pub(crate)` — используется
/// напрямую тестом §7.2 `settings_and_state_keys_disjoint_and_cover_lists`
/// (ТЗ-2).
pub(crate) fn state_spec() -> Vec<KeySpec<SessionState>> {
    let mut specs = legacy_playback_specs();
    specs.extend(column_width_specs());
    specs.extend(sort_specs());
    specs.push(leaf(
        "visualization.mode",
        |t: &mut SessionState| &mut t.viz_mode,
        VisualizationMode::Off,
        "\"off\"/\"oscilloscope\"/\"spectrogram\"/\"spectrum\"",
    ));
    specs.push(repeat_spec());
    specs.push(leaf("shuffle", |t: &mut SessionState| &mut t.shuffle, false, "bool"));
    specs.extend(window_specs());
    specs.push(last_dir_spec());
    specs
}

/// Собрать `sort` из черновика (§2.5: обе или ни одной, иначе `Invalid` у
/// присутствующего ключа, ключа сортировки нет).
fn adjust_sort_pair(value: &mut SessionState, notes: &mut Vec<LoadNote>) {
    match (value.sort_column_raw, value.sort_direction_raw) {
        (Some(column), Some(direction)) => value.sort = Some(SortKey { column, direction }),
        (Some(column), None) => {
            value.sort = None;
            notes.push(LoadNote {
                key: KeyPath::new("sort.column"),
                kind: LoadNoteKind::Invalid {
                    found: format!("\"{}\"", column.key()).into(),
                    allowed: "sort.column и sort.direction задаются только вместе",
                },
            });
        }
        (None, Some(direction)) => {
            value.sort = None;
            let found = match direction {
                SortDirection::Asc => "\"asc\"",
                SortDirection::Desc => "\"desc\"",
            };
            notes.push(LoadNote {
                key: KeyPath::new("sort.direction"),
                kind: LoadNoteKind::Invalid {
                    found: found.into(),
                    allowed: "sort.column и sort.direction задаются только вместе",
                },
            });
        }
        (None, None) => value.sort = None,
    }
    value.sort_column_raw = None;
    value.sort_direction_raw = None;
}

/// Собрать `window.position`/`window.size` из черновика. Таблица §2.5 не
/// оговаривает «обе или ни одной» для `window.x`/`window.y` (в отличие от
/// пары сортировки) — одиночный присутствующий ключ просто не формирует
/// позицию/размер, без заметки `Invalid`.
fn adjust_window_geometry(value: &mut SessionState) {
    value.window.position = match (value.window_x, value.window_y) {
        (Some(x), Some(y)) => Some(PhysPos { x, y }),
        _ => None,
    };
    value.window.size = match (value.window_width, value.window_height) {
        (Some(width), Some(height)) => Some(PhysSize { width, height }),
        _ => None,
    };
    value.window_x = None;
    value.window_y = None;
    value.window_width = None;
    value.window_height = None;
}

/// Разбор `state.toml` по ключам (ТЗ-5, §2.3, §2.5, §6.2). Чистая функция,
/// без ввода-вывода.
pub fn parse_state(read: FileRead) -> Parsed<SessionState> {
    let bytes = match read {
        FileRead::Absent => return Parsed::Absent { value: SessionState::default() },
        FileRead::Failed(err) => return Parsed::ReadFailed { value: SessionState::default(), err },
        FileRead::Bytes(b) => b,
    };

    let text = match std::str::from_utf8(&bytes) {
        Ok(t) => t,
        Err(e) => {
            return Parsed::Unparsable {
                value: SessionState::default(),
                original: bytes.clone(),
                error: e.to_string().into(),
            };
        }
    };

    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            return Parsed::Unparsable {
                value: SessionState::default(),
                original: bytes.clone(),
                error: e.to_string().into(),
            };
        }
    };

    let (mut value, mut notes) = walk(&table, &state_spec());
    adjust_sort_pair(&mut value, &mut notes);
    adjust_window_geometry(&mut value);
    notes.sort_by(|a, b| a.key.cmp(&b.key));

    Parsed::Parsed { value, notes, reference: ReferenceText::of(bytes) }
}

/// DTO `columns.widths.*` для сериализации (§2.5, §2.6): `widths` —
/// `BTreeMap` (ОВ-2, ADR-2), запись только для колонок с заданной шириной.
#[derive(serde::Serialize)]
struct ColumnsDto {
    widths: BTreeMap<&'static str, f32>,
}

/// DTO `sort.*` для сериализации (§2.5, §2.6): обе части пары приходят
/// вместе из `SessionState::sort` (`adjust_sort_pair` не допускает другого
/// состояния), поэтому `None`/`None` и `Some`/`Some` — единственные случаи.
#[derive(serde::Serialize)]
struct SortDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<&'static str>,
}

/// DTO `visualization.mode` для сериализации (§2.5, §2.6): `VisualizationMode`
/// уже `Serialize` в нижнем регистре (`#[serde(rename_all = "lowercase")]`).
#[derive(serde::Serialize)]
struct VisualizationDto {
    mode: VisualizationMode,
}

/// DTO `window.*` для сериализации (§2.5, §2.6): `x`/`y`/`width`/`height` —
/// те же optional-ключи, что при разборе (§6.2), пропускаются при `None`.
#[derive(serde::Serialize)]
struct WindowDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    y: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    maximized: bool,
    fullscreen: bool,
}

/// DTO записи `state.toml` (§2.6). Порядок полей — не буквальный порядок
/// таблицы §2.5: TOML требует все скалярные ключи до первой вложенной
/// таблицы, иначе они достанутся последней открытой `[table]`. Поэтому
/// сперва идут скаляры §2.5 (`volume`, `muted`, `repeat`, `shuffle`,
/// `last_dir`) в их исходном относительном порядке, затем таблицы
/// (`columns`, `sort`, `visualization`, `window`) — тоже в порядке §2.5.
#[derive(serde::Serialize)]
struct StateFile<'a> {
    volume: u8,
    muted: bool,
    repeat: &'static str,
    shuffle: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_dir: Option<&'a str>,
    columns: ColumnsDto,
    sort: SortDto,
    visualization: VisualizationDto,
    window: WindowDto,
}

/// Детерминированный текст файла (ОВ-2): одинаковые значения `SessionState`
/// → одинаковые байты в любом процессе — гарантируется фиксированным
/// порядком полей DTO и `BTreeMap` для `columns.widths` (§2.6).
pub fn serialize_state(s: &SessionState) -> Result<Arc<[u8]>, SerializeError> {
    let widths: BTreeMap<&'static str, f32> =
        s.column_widths.iter().map(|(id, w)| (id.key(), w.get())).collect();

    let (sort_column, sort_direction) = match s.sort {
        Some(key) => (
            Some(key.column.key()),
            Some(match key.direction {
                SortDirection::Asc => "asc",
                SortDirection::Desc => "desc",
            }),
        ),
        None => (None, None),
    };

    let dto = StateFile {
        volume: s.playback.volume,
        muted: s.playback.muted,
        repeat: match s.repeat {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        },
        shuffle: s.shuffle,
        last_dir: s.last_dir.as_deref().and_then(Path::to_str),
        columns: ColumnsDto { widths },
        sort: SortDto { column: sort_column, direction: sort_direction },
        visualization: VisualizationDto { mode: s.viz_mode },
        window: WindowDto {
            x: s.window.position.map(|p| p.x),
            y: s.window.position.map(|p| p.y),
            width: s.window.size.map(|sz| sz.width),
            height: s.window.size.map(|sz| sz.height),
            maximized: s.window.maximized,
            fullscreen: s.window.fullscreen,
        },
    };

    let text = toml::to_string(&dto).map_err(|e| SerializeError(e.to_string().into()))?;
    Ok(Arc::from(text.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn bytes(text: &str) -> FileRead {
        FileRead::Bytes(Arc::from(text.as_bytes()))
    }

    fn parsed(read: FileRead) -> (SessionState, Vec<LoadNote>) {
        match parse_state(read) {
            Parsed::Parsed { value, notes, .. } => (value, notes),
            Parsed::Absent { .. } | Parsed::Unparsable { .. } | Parsed::ReadFailed { .. } => {
                panic!("expected Parsed")
            }
        }
    }

    #[test]
    fn parse_absent_gives_defaults_without_notes() {
        match parse_state(FileRead::Absent) {
            Parsed::Absent { value } => assert_eq!(value, SessionState::default()),
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
            path: PathBuf::from("state.toml"),
        };
        match parse_state(FileRead::Failed(err)) {
            Parsed::ReadFailed { value, .. } => assert_eq!(value, SessionState::default()),
            Parsed::Absent { .. } | Parsed::Parsed { .. } | Parsed::Unparsable { .. } => {
                panic!("expected ReadFailed")
            }
        }
    }

    #[test]
    fn parse_unparsable_keeps_original_bytes() {
        let original = b"[[".to_vec();
        match parse_state(FileRead::Bytes(Arc::from(original.as_slice()))) {
            Parsed::Unparsable { value, original: kept, .. } => {
                assert_eq!(value, SessionState::default());
                assert_eq!(kept.as_ref(), original.as_slice());
            }
            Parsed::Absent { .. } | Parsed::Parsed { .. } | Parsed::ReadFailed { .. } => {
                panic!("expected Unparsable")
            }
        }
    }

    #[test]
    fn empty_state_gives_documented_defaults() {
        let (value, _notes) = parsed(bytes(""));
        assert_eq!(value, SessionState::default());
    }

    #[test]
    fn volume_out_of_range_falls_back_to_default() {
        let (value, notes) = parsed(bytes("volume = 150\nmuted = false\nrepeat = \"off\"\nshuffle = false\n"));
        assert_eq!(value.playback.volume, 100);
        let note = notes.iter().find(|n| n.key == KeyPath::new("volume")).expect("note present");
        match &note.kind {
            LoadNoteKind::Invalid { allowed, .. } => assert_eq!(*allowed, "целое 0..=100"),
            other => panic!("unexpected note kind: {other:?}"),
        }
    }

    #[test]
    fn last_dir_relative_path_is_invalid() {
        let (value, notes) = parsed(bytes("last_dir = \"music\"\n"));
        assert_eq!(value.last_dir(), None);
        let note = notes.iter().find(|n| n.key == KeyPath::new("last_dir")).expect("note present");
        match &note.kind {
            LoadNoteKind::Invalid { allowed, .. } => assert_eq!(*allowed, "абсолютный путь"),
            other => panic!("unexpected note kind: {other:?}"),
        }
    }

    #[test]
    fn sort_column_alone_is_invalid_and_drops_sort() {
        let (value, notes) = parsed(bytes("sort.column = \"title\"\n"));
        assert_eq!(value.sort(), None);
        let note = notes.iter().find(|n| n.key == KeyPath::new("sort.column")).expect("note present");
        match &note.kind {
            LoadNoteKind::Invalid { allowed, .. } => {
                assert_eq!(*allowed, "sort.column и sort.direction задаются только вместе")
            }
            other => panic!("unexpected note kind: {other:?}"),
        }
    }

    #[test]
    fn sort_direction_alone_is_invalid_and_drops_sort() {
        let (value, notes) = parsed(bytes("sort.direction = \"asc\"\n"));
        assert_eq!(value.sort(), None);
        let note = notes.iter().find(|n| n.key == KeyPath::new("sort.direction")).expect("note present");
        assert!(matches!(note.kind, LoadNoteKind::Invalid { .. }));
    }

    #[test]
    fn sort_pair_present_combines() {
        let (value, notes) = parsed(bytes("sort.column = \"title\"\nsort.direction = \"desc\"\n"));
        assert_eq!(
            value.sort(),
            Some(SortKey { column: ColumnId::Title, direction: SortDirection::Desc })
        );
        assert!(notes.iter().all(|n| n.key != KeyPath::new("sort.column") && n.key != KeyPath::new("sort.direction")));
    }

    #[test]
    fn sort_column_now_playing_is_invalid() {
        let (value, notes) = parsed(bytes("sort.column = \"now_playing\"\nsort.direction = \"asc\"\n"));
        assert_eq!(value.sort(), None);
        let note = notes.iter().find(|n| n.key == KeyPath::new("sort.column")).expect("note present");
        assert!(matches!(note.kind, LoadNoteKind::Invalid { .. }));
    }

    #[test]
    fn window_position_needs_both_x_and_y() {
        let (value, notes) = parsed(bytes("window.x = 10\n"));
        assert_eq!(value.window().position, None);
        assert!(notes.iter().all(|n| n.key.as_str() != "window.x" && n.key.as_str() != "window.y"));
    }

    #[test]
    fn window_position_combines_when_both_present() {
        let (value, _notes) = parsed(bytes("window.x = 10\nwindow.y = 20\n"));
        assert_eq!(value.window().position, Some(PhysPos { x: 10, y: 20 }));
    }

    #[test]
    fn window_size_combines_when_both_present() {
        let (value, _notes) = parsed(bytes("window.width = 800\nwindow.height = 600\n"));
        assert_eq!(value.window().size, Some(PhysSize { width: 800, height: 600 }));
    }

    #[test]
    fn column_width_parses_and_rejects_out_of_range() {
        let (value, notes) = parsed(bytes("columns.widths.title = 42.5\ncolumns.widths.artist = 0\n"));
        assert_eq!(value.column_widths().get(&ColumnId::Title).map(|w| w.get()), Some(42.5));
        assert!(!value.column_widths().contains_key(&ColumnId::Artist));
        let note = notes.iter().find(|n| n.key == KeyPath::new("columns.widths.artist")).expect("note present");
        assert!(matches!(note.kind, LoadNoteKind::Invalid { .. }));
    }

    #[test]
    fn viz_mode_parses_lowercase() {
        let (value, _notes) = parsed(bytes("visualization.mode = \"spectrum\"\n"));
        assert_eq!(value.viz_mode(), VisualizationMode::Spectrum);
    }

    #[test]
    fn repeat_parses_lowercase_strings() {
        let (value, _notes) = parsed(bytes("repeat = \"all\"\n"));
        assert_eq!(value.repeat(), RepeatMode::All);
    }

    #[test]
    fn repeat_invalid_value_falls_back_to_off() {
        let (value, notes) = parsed(bytes("repeat = \"loop\"\n"));
        assert_eq!(value.repeat(), RepeatMode::Off);
        assert!(notes.iter().any(|n| n.key == KeyPath::new("repeat") && matches!(n.kind, LoadNoteKind::Invalid { .. })));
    }

    #[test]
    fn apply_mutates_matching_field() {
        let mut state = SessionState::default();

        state.apply(StateChange::Volume(42));
        assert_eq!(state.playback().volume, 42);

        state.apply(StateChange::Muted(true));
        assert!(state.playback().muted);

        let mut widths = BTreeMap::new();
        widths.insert(ColumnId::Title, WidthPct::new(50.0).expect("valid"));
        state.apply(StateChange::ColumnWidths(widths.clone()));
        assert_eq!(state.column_widths(), &widths);

        let sort = Some(SortKey { column: ColumnId::Artist, direction: SortDirection::Asc });
        state.apply(StateChange::Sort(sort));
        assert_eq!(state.sort(), sort);

        state.apply(StateChange::VizMode(VisualizationMode::Spectrum));
        assert_eq!(state.viz_mode(), VisualizationMode::Spectrum);

        state.apply(StateChange::Repeat(RepeatMode::All));
        assert_eq!(state.repeat(), RepeatMode::All);

        state.apply(StateChange::Shuffle(true));
        assert!(state.shuffle());

        let window = WindowGeometry {
            position: Some(PhysPos { x: 1, y: 2 }),
            size: Some(PhysSize { width: 3, height: 4 }),
            maximized: true,
            fullscreen: false,
        };
        state.apply(StateChange::Window(window));
        assert_eq!(state.window(), window);

        state.apply(StateChange::LastDir(PathBuf::from("/music")));
        assert_eq!(state.last_dir(), Some(Path::new("/music")));
    }

    /// §7.2: сценарий «один ключ недопустим, один отсутствует, один
    /// неизвестен» — спецификация не приводит конкретный TOML, поэтому
    /// сценарий собран здесь: `shuffle` — неверный тип (`Invalid`), `repeat`
    /// отсутствует (`Missing`), `unknown_leaf` — неизвестный ключ (`Unknown`).
    #[test]
    fn parse_by_keys_three_notes() {
        let (value, notes) = parsed(bytes("shuffle = \"yes\"\nunknown_leaf = 1\n"));
        assert!(!value.shuffle());
        assert_eq!(value.repeat(), RepeatMode::Off);

        let shuffle_note = notes.iter().find(|n| n.key == KeyPath::new("shuffle")).expect("note present");
        assert!(matches!(shuffle_note.kind, LoadNoteKind::Invalid { .. }));

        let repeat_note = notes.iter().find(|n| n.key == KeyPath::new("repeat")).expect("note present");
        assert!(matches!(repeat_note.kind, LoadNoteKind::Missing));

        let unknown_note = notes.iter().find(|n| n.key == KeyPath::new("unknown_leaf")).expect("note present");
        assert!(matches!(unknown_note.kind, LoadNoteKind::Unknown));
    }

    /// `SessionState` отличное от значений по умолчанию во всех разделах
    /// §2.5: громкость/mute, ширины колонок, сортировка, визуализация,
    /// повтор/перемешивание, геометрия окна, последний каталог.
    fn non_default_state() -> SessionState {
        let mut s = SessionState::default();
        s.apply(StateChange::Volume(42));
        s.apply(StateChange::Muted(true));

        let mut widths = BTreeMap::new();
        widths.insert(ColumnId::Title, WidthPct::new(50.0).expect("valid"));
        widths.insert(ColumnId::Artist, WidthPct::new(25.0).expect("valid"));
        s.apply(StateChange::ColumnWidths(widths));

        s.apply(StateChange::Sort(Some(SortKey { column: ColumnId::Artist, direction: SortDirection::Desc })));
        s.apply(StateChange::VizMode(VisualizationMode::Spectrum));
        s.apply(StateChange::Repeat(RepeatMode::All));
        s.apply(StateChange::Shuffle(true));
        s.apply(StateChange::Window(WindowGeometry {
            position: Some(PhysPos { x: 10, y: 20 }),
            size: Some(PhysSize { width: 1024, height: 768 }),
            maximized: true,
            fullscreen: true,
        }));
        s.apply(StateChange::LastDir(PathBuf::from("/home/user/music")));
        s
    }

    /// Серилизация `s` детерминирована (ОВ-2) и round-trip'ится без заметок
    /// (§7.2): `serialize_state` дважды даёт одинаковые байты, разбор
    /// результата даёт равное значение без заметок, а повторная сериализация
    /// разобранного значения воспроизводит исходные байты. Заменяет старый
    /// `window_state_flags_roundtrip` (`src/settings.rs`) — геометрия окна
    /// теперь в `state.toml`.
    fn assert_roundtrip_deterministic(s: &SessionState) {
        let bytes1 = serialize_state(s).expect("serialize ok");
        let bytes2 = serialize_state(s).expect("serialize ok");
        assert_eq!(bytes1, bytes2, "same SessionState must serialize to identical bytes");

        let (value, notes) = parsed(FileRead::Bytes(bytes1.clone()));
        assert_eq!(value, *s);
        assert!(notes.is_empty(), "expected zero notes, got: {notes:?}");

        let bytes3 = serialize_state(&value).expect("serialize ok");
        assert_eq!(bytes1, bytes3, "re-serializing parsed value must reproduce original bytes");
    }

    #[test]
    fn state_roundtrip_deterministic() {
        assert_roundtrip_deterministic(&SessionState::default());
        assert_roundtrip_deterministic(&non_default_state());
    }

    #[test]
    fn defaults_serialize_parses_with_zero_notes() {
        let bytes = serialize_state(&SessionState::default()).expect("serialize ok");
        let (value, notes) = parsed(FileRead::Bytes(bytes));
        assert_eq!(value, SessionState::default());
        assert!(notes.is_empty(), "expected zero notes, got: {notes:?}");
    }

    /// §7.2 `settings_and_state_keys_disjoint_and_cover_lists` (ТЗ-2): тест
    /// сравнивает множества путей таблиц `KeySpec` (`settings_spec()` из
    /// `settings_file.rs` и `state_spec()` выше, обе `pub(crate)`) —
    /// пересечение пусто, а объединение покрывает семейства ключей §2.4/§2.5
    /// (без `[playback]`/`playback.<режим>.*` — целевая модель 01_audio_modes,
    /// до С4 их замещают легаси `audio.*`/`dsd.*`/`audio_device` и
    /// `volume`/`muted`).
    #[test]
    fn settings_and_state_keys_disjoint_and_cover_lists() {
        use crate::persist::settings_file::settings_spec;

        let settings_keys: std::collections::BTreeSet<String> = settings_spec().into_iter().map(|s| s.path).collect();
        let state_keys: std::collections::BTreeSet<String> = state_spec().into_iter().map(|s| s.path).collect();

        let overlap: Vec<&String> = settings_keys.intersection(&state_keys).collect();
        assert!(overlap.is_empty(), "settings.toml and state.toml key paths must be disjoint: {overlap:?}");

        for key in [
            "theme",
            "save_interval",
            "minimize_to_tray",
            "scroll_to_playing",
            "cover_size",
            "col_info_w",
            "col_gap",
            "column_order",
            "cover_priority",
            "cover_folder_names",
            "cover_online",
            "audio_device",
        ] {
            assert!(settings_keys.contains(key), "missing settings key {key}");
        }
        for prefix in ["columns.", "info_labels.", "visualization.", "audio.", "dsd."] {
            assert!(settings_keys.iter().any(|k| k.starts_with(prefix)), "missing settings key family {prefix}");
        }
        for suffix in [".title", ".priority", ".min_width", ".max_width", ".max_width_percent", ".visible", ".column_type"] {
            assert!(
                settings_keys.iter().any(|k| k.starts_with("columns.") && k.ends_with(suffix)),
                "missing columns.* key {suffix}"
            );
        }

        for key in ["volume", "muted", "repeat", "shuffle", "visualization.mode", "last_dir"] {
            assert!(state_keys.contains(key), "missing state key {key}");
        }
        for prefix in ["columns.widths.", "sort.", "window."] {
            assert!(state_keys.iter().any(|k| k.starts_with(prefix)), "missing state key family {prefix}");
        }
    }
}
