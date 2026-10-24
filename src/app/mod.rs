use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem, VecModel};
use slint::language::{SortOrder, TableColumn};

use music_player_rs::audio::analyzer::TAP_CAPACITY;
use music_player_rs::audio::backend::{BackendError, HwDeviceId, SharedDeviceId};
use music_player_rs::audio::decoder::TrackInfo;
use music_player_rs::audio::error::{CaptureFailure, EngineFault, FileError, OpenError, Reaction, StreamFault};
use music_player_rs::audio::output::DeviceInfo;
// `default_device_name` — только для не-Linux ветки Совместимого режима
// (`apply_cpal_compatible_device_listing`, `ui_manager.rs`): на Linux список
// строится из `DeviceCatalog.shared` (AM1.0-B37, шаг 9), импорт без cfg был
// бы неиспользуемым.
#[cfg(not(target_os = "linux"))]
use music_player_rs::audio::output::default_device_name;
use music_player_rs::audio::path::report::PathReport;
use music_player_rs::audio::visualizer::{
    FreqScale, LevelScale, VisualizationMode, VisualizerConfig,
};
use music_player_rs::core::dialog::{ApplyStep, DialogDraft, ThemeLoad};
use music_player_rs::core::SaveButton;
use music_player_rs::core::gate::{BlockReason, LoadKind, MainCmd};
use music_player_rs::core::messages::{
    lock_failure_actions, CloseEffect, Message, MessageAction, MessageCenter, MessageLevel, MsgEffect,
};
use music_player_rs::core::exit::{ExitOutcome, ExitReason};
use music_player_rs::core::io::{CacheKind, CacheSizes, IoDone, IoJob};
use music_player_rs::core::load::LoadApplied;
use music_player_rs::core::volume::{VolumeCmd, VolumeSource};
use music_player_rs::core::AppCore;
use music_player_rs::cover::{self, CoverDone, CoverJob};
use music_player_rs::engine::lock::{LockInput, LockState};
use music_player_rs::engine::messages::{EngineCmd, EngineEvent, Notice, SkipReason};
use music_player_rs::engine::run::{BackendCaps, EngineHandle};
use music_player_rs::persist::keys::LoadNoteKind;
use music_player_rs::persist::settings_file::Settings as PersistSettings;
use music_player_rs::persist::settings_file::{ColumnsConfig, ThemeName};
use music_player_rs::persist::state_file::{
    LegacyPlaybackState, widths_on_disable, widths_on_enable, Origin, StateChange, WidthPct,
};
use music_player_rs::persist::tracker::ReplyEffect;
use music_player_rs::persist::{ConfigPaths, WorkFile};
use music_player_rs::platform::input::{AttachOutcome, SlintScroll};
use music_player_rs::platform::lifecycle::PlatformCaps;
use music_player_rs::platform::notify::Notifier;
use music_player_rs::platform::pick::{FilePicker, PickRequest, PickResult};
use music_player_rs::playlist::{self, ScanMsg, Track};
use music_player_rs::playlist::model::TrackId;
use music_player_rs::settings::params::ModeSettingsUpdate;
use music_player_rs::settings::playback::{ModeSettings, SharedDeviceChoice, Volume};
use music_player_rs::settings::{ColumnId, ModeKind, RepeatMode};
use music_player_rs::theme::{
    default_light_toml, parse_hex, ColorsData, ThemeData, ThemeEntry, ThemeError,
};
use music_player_rs::platform::lifecycle::TrayEvent;
use music_player_rs::platform::tray;

mod badge_view;
mod device_info_view;
mod lock_view;
mod path_view;
pub(crate) mod engine_sink;
pub mod events;
pub mod fulltrack_manager;
mod mode_dialog;
pub mod playback_manager;
pub mod playlist_manager;
mod ui_audio_state;
pub mod ui_manager;
pub mod visualizer_manager;
pub mod viz_settings_manager;

use badge_view::{BadgeView, BadgeViewState, TestView};
use engine_sink::EngineEventQueue;
use events::AppEvent;
use fulltrack_manager::{FullCmd, FullEvt};
use lock_view::{lock_failure_transition, lock_view, LockFailureEffect};
use path_view::PathWindowCache;
use ui_audio_state::{Applied, UiAudioState};

/// Last values pushed to the UI by the delta playback sync. Kept so the
/// 100 ms tick only re-writes properties that actually changed (e.g. no
/// seekbar churn while paused/stopped, no volume re-write unless it moved).
#[derive(Clone, PartialEq)]
struct UiState {
    playing: bool,
    muted: bool,
    volume: f32,
    bit_perfect: bool,
    pos: String,
    dur: String,
    seek_fraction: f32,
    status: String,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            playing: false,
            muted: false,
            // Sentinel: forces the first tick to push the real volume.
            volume: -1.0,
            bit_perfect: false,
            pos: String::new(),
            dur: String::new(),
            seek_fraction: -1.0,
            status: String::new(),
        }
    }
}

/// Пустой набор ширин колонок: его показывает список колонок диалога после
/// «Сбросить ширины» — до «Сохранить» действующие ширины не меняются
/// (Т-2, §6.9).
static EMPTY_WIDTHS: BTreeMap<ColumnId, WidthPct> = BTreeMap::new();

/// Ширины колонок, уходящие в состояние сессии по «Сохранить» (Т-2, §2.5,
/// §6.9); `None` — ширины не меняются. Действующие ширины `current`
/// нормализуются для каждой колонки, чья видимость в `new` отличается от
/// `old`: скрытая теряет сохранённую ширину, показанная получает среднюю по
/// прежним видимым. Черновик ширин не хранит, поэтому нормализация
/// выполняется в момент сохранения, по итоговому набору видимых колонок.
/// «Сбросить ширины» исполняет ядро (`AppCore::dialog_save`) — при сбросе эта
/// функция не вызывается.
fn widths_on_save(
    old: &ColumnsConfig,
    new: &ColumnsConfig,
    current: &BTreeMap<ColumnId, WidthPct>,
) -> Option<BTreeMap<ColumnId, WidthPct>> {
    let mut widths: Option<BTreeMap<ColumnId, WidthPct>> = None;
    for col in ColumnId::ALL {
        let visible = new.column_visible(col);
        if visible == old.column_visible(col) {
            continue;
        }
        let w = widths.get_or_insert_with(|| current.clone());
        if visible {
            widths_on_enable(new, w, col);
        } else {
            widths_on_disable(new, w, col);
        }
    }
    widths
}

/// Метаданные темы, показываемые в диалоге настроек (T1.0 §8.2):
/// name/description. Пустое имя — файл есть, но не читается/невалиден.
/// Доступность «Сохранить» от них не зависит — её считает
/// `save_button_view` по черновику (ТЗ-27, §6.9).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ThemeMeta {
    name: String,
    description: Option<String>,
}

/// Throttle for persisting user-dragged column widths: ticks (100 ms each)
/// with a stable column signature before a write and re-snap. Kept short
/// (~600 ms) so the min/max correction feels like it fires on mouse release,
/// while still long enough to absorb one tick's worth of drag jitter.
const COL_SAVE_DEBOUNCE_TICKS: u32 = 6;

/// Consecutive stable reflow ticks (16 ms each) before the columns are snapped
/// to the (settled) window width ≈ the moment the user releases the mouse after
/// a window resize. ~64 ms.
const REFLOW_SETTLE_TICKS: u32 = 4;

/// Срок ответа движка `ShutdownComplete` на пути выхода — 2 с от запроса
/// (§6.28 п. 3); по истечении event loop завершается без ответа.
const ENGINE_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// Полная сигнатура визуализации для детекта изменений DSP-настроек. Кортежи
/// std реализуют `PartialEq` только до 12 элементов, поэтому — отдельный тип.
#[derive(Debug, Clone, PartialEq)]
struct VizSig {
    mode: i32,
    bands: usize,
    channels: usize,
    bar_gap: u32,
    bar_radius: u32,
    gradient: bool,
    freq_scale: FreqScale,
    smoothing: f32,
    peak_hold: bool,
    sensitivity: f32,
    level_scale: LevelScale,
    peak_decay_ms: u32,
    dsd_cic_compensation: bool,
}

/// Сгенерированный код Slint: `unwrap`/`expect` в нём не наши, поэтому линтер
/// прод-кода (AM1.0 §7.7, ТЗ-101) для этого модуля отключён.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod slint_generated {
    slint::include_modules!();
}
pub use slint_generated::*;

pub fn create_ui() -> Result<AppWindow, slint::PlatformError> {
    let ui = AppWindow::new()?;
    // AM1.0-B14 шаг 8: на macOS строка меню — системный NSMenu (не занимает
    // место в окне), поэтому группа «режим ▾ · устройство · замок»
    // (MenuBarTrailing, ui/top_panel.slint) не накладывается оверлеем на
    // MenuBar (ui/app.slint), а встаёт у правого края верхней панели.
    ui.set_menu_bar_in_window(!cfg!(target_os = "macos"));
    Ok(ui)
}

fn fmt_num(num: u32, total: u32) -> SharedString {
    if num > 0 {
        if total > 0 {
            format!("{num} / {total}").into()
        } else {
            format!("{num}").into()
        }
    } else {
        "—".into()
    }
}

fn opt_str(s: &Option<String>) -> SharedString {
    s.as_deref().unwrap_or("—").into()
}

fn empty_dash(s: &str) -> SharedString {
    if s.is_empty() { "—".into() } else { s.into() }
}

/// Parse a hex color `#RRGGBB` or `#AARRGGBB` into a Slint color.
///
/// Returns `None` on any invalid input (wrong length, non-hex characters, or
/// a non-UTF-8 string) instead of panicking, so callers are safe against
/// malformed configuration or other input. Thin wrapper over
/// `theme::parse_hex` (§2.3).
fn hex_color(hex: &str) -> Option<slint::Color> {
    let (a, r, g, b) = music_player_rs::theme::parse_hex(hex)?;
    if a == 255 {
        Some(slint::Color::from_rgb_u8(r, g, b))
    } else {
        Some(slint::Color::from_argb_u8(a, r, g, b))
    }
}

/// Валидирует HEX всех 22 полей `ColorsData` через `theme::parse_hex` (§2.3).
/// Любой отказ → `ThemeError::InvalidHex` с именем поля для диагностики.
fn validate_colors(colors: &ColorsData) -> Result<(), ThemeError> {
    type ColorField = fn(&ColorsData) -> &str;
    const FIELDS: [(&str, ColorField); 22] = [
        ("bg_window", |c| &c.bg_window),
        ("bg_surface", |c| &c.bg_surface),
        ("bg_toolbar", |c| &c.bg_toolbar),
        ("bg_elevated", |c| &c.bg_elevated),
        ("bg_overlay", |c| &c.bg_overlay),
        ("border_subtle", |c| &c.border_subtle),
        ("border_default", |c| &c.border_default),
        ("text_primary", |c| &c.text_primary),
        ("text_secondary", |c| &c.text_secondary),
        ("text_tertiary", |c| &c.text_tertiary),
        ("text_dim", |c| &c.text_dim),
        ("text_on_accent", |c| &c.text_on_accent),
        ("text_error", |c| &c.text_error),
        ("accent", |c| &c.accent),
        ("accent_container", |c| &c.accent_container),
        ("accent_on", |c| &c.accent_on),
        ("surface_hover", |c| &c.surface_hover),
        ("surface_active", |c| &c.surface_active),
        ("surface_selected", |c| &c.surface_selected),
        ("viz_1", |c| &c.viz_1),
        ("viz_2", |c| &c.viz_2),
        ("viz_3", |c| &c.viz_3),
    ];
    for (field, get) in FIELDS {
        if parse_hex(get(colors)).is_none() {
            return Err(ThemeError::InvalidHex {
                field: field.to_owned(),
            });
        }
    }
    Ok(())
}

/// Вид кнопки «Сохранить» диалога настроек — `(активна, подсказка)` (ТЗ-27,
/// §6.9, ADR-11). Основа — ответ ядра `AppCore::save_button`: отличия есть и
/// недопустимых значений нет; подсказка — первое недопустимое значение
/// «<param>: <reason>», без него — пустая строка. Ядро проверяет только
/// структуру прочитанной темы, поэтому HEX-цвета темы черновика
/// (`ThemeLoad::Ready`) проверяются здесь: при недопустимом цвете кнопка
/// неактивна, а если у ядра подсказки нет — подсказка называет параметр
/// «Тема» и причину. Тема в остальных состояниях на результат не влияет.
fn save_button_view(button: SaveButton, theme: Option<&ThemeLoad>) -> (bool, String) {
    let colors_error = match theme {
        Some(ThemeLoad::Ready(data)) => validate_colors(&data.colors).err(),
        _ => None,
    };
    let Some(error) = colors_error else {
        return (button.enabled, button.tooltip.unwrap_or_default());
    };
    let tooltip = button.tooltip.unwrap_or_else(|| {
        let reason = music_player_rs::i18n::errors::theme_error_reason(&error);
        music_player_rs::i18n::errors::theme_tooltip(&reason)
    });
    (false, tooltip)
}

/// Разрешает стартовую тему без GUI (§8.2/§6.1).
///
/// Цепочка fallback: `<name>.toml` (структура + HEX) → `light.toml` →
/// `default_light_toml()`. Возвращает `(данные_темы, was_fallback)`; `None` —
/// не разобралась даже встроенная тема (тогда остаётся палитра по умолчанию
/// из `theme.slint`, без паники, ТЗ-101).
/// `settings.toml` не пишет — намерение пользователя сохраняется.
fn resolve_startup_theme(name: &str, themes_dir: &std::path::Path) -> Option<(ThemeData, bool)> {
    let try_file = |p: &std::path::Path| -> Option<ThemeData> {
        let data = ThemeData::load_from_file(p).ok()?;
        if validate_colors(&data.colors).is_ok() {
            Some(data)
        } else {
            None
        }
    };

    if let Some(data) = try_file(&themes_dir.join(format!("{name}.toml"))) {
        return Some((data, false));
    }

    match try_file(&themes_dir.join("light.toml")) {
        Some(data) => Some((data, true)),
        // Константа покрыта test_load_default_light.
        None => toml::from_str(&default_light_toml()).ok().map(|data| (data, true)),
    }
}

/// Отличается ли строка диалога настроек режима `new` от уже показанной
/// `old` (AM1.0-B33, SP1.0 ADR-11): используется, чтобы `set_row_data`
/// вызывался только для изменившихся строк персистентной модели
/// `MusicApp::mode_rows`, а не для всех — иначе Slider ползунка каждую
/// правку перерисовывается полностью, хоть и без пересоздания. Не
/// `ModeParamRow: PartialEq` напрямую: `ModelRc::eq` сравнивает указатель
/// на модель, а `to_param_row` строит новый `ModelRc` для `options` на
/// каждый вызов, поэтому список пунктов сверяется по содержимому.
fn mode_param_row_changed(old: &ModeParamRow, new: &ModeParamRow) -> bool {
    if old.param != new.param
        || old.label != new.label
        || old.kind != new.kind
        || old.enabled != new.enabled
        || old.note != new.note
        || old.selected != new.selected
        || old.checked != new.checked
        || old.value != new.value
        || old.min != new.min
        || old.max != new.max
    {
        return true;
    }
    let old_opts: Vec<SharedString> = old.options.iter().collect();
    let new_opts: Vec<SharedString> = new.options.iter().collect();
    old_opts != new_opts
}

/// Решение: слать ли `EngineCmd::ProbeDeviceCaps` для устройства, выбранного
/// в показанной панели режима диалога настроек (AM1.0-B38, §2.3, шаг 9).
/// Чистая функция без доступа к `MusicApp`/движку — по образцу
/// `play_route`/`mode_param_row_changed`, для прямого юнит-теста; реальные
/// аргументы собирает `MusicApp::maybe_probe_selected_device`.
///
/// `device` — устройство, выбранное в черновике показанного режима
/// (`None` — Совместимый режим, устройство не выбрано или не в текущем
/// каталоге, `ui_manager.rs::selected_hw_device_for_mode`). `held` —
/// устройство, которое держит замок (`LockState::Held`, иначе `None`) — не
/// трогается, то же исключение, что и у собственной защиты движка
/// (`engine::run::probe_device_caps`, §2.3). `caps_known` — возможности
/// `device` уже кэшированы (`UiAudioState::hw_caps`) — повторный опрос
/// ничего не даст. `last_probed` — устройство последней отправленной команды
/// (дебаунс повторного выбора того же устройства подряд, не распространяется
/// на выбор другого устройства). Возвращает устройство для отправки, если
/// ни одно исключение не сработало.
fn should_probe_device(
    mode: ModeKind,
    device: Option<&HwDeviceId>,
    held: Option<&HwDeviceId>,
    caps_known: bool,
    last_probed: Option<&HwDeviceId>,
) -> Option<HwDeviceId> {
    if mode == ModeKind::Compatible {
        return None;
    }
    let device = device?;
    if held == Some(device) {
        return None;
    }
    if caps_known {
        return None;
    }
    if last_probed == Some(device) {
        return None;
    }
    Some(device.clone())
}

/// Строки рекомендаций одной группы окна «Путь сигнала» → `PathRecItem`
/// (ТЗ-81, ТЗ-84). Действие с кнопкой (`QuickFix`/`OpenSettings`) дописывается
/// в `actions`, а `action-id` элемента — его индекс там; без действия —
/// `action-id: -1` (кнопка не показывается, см. `ui/app.slint`).
fn path_rec_items(
    recs: Vec<path_view::RecView>,
    actions: &mut Vec<path_view::RecViewAction>,
) -> Vec<PathRecItem> {
    recs.into_iter()
        .map(|r| {
            let (action_id, action_label) = match r.action {
                path_view::RecViewAction::None => (-1, String::new()),
                path_view::RecViewAction::QuickFix(_) => {
                    actions.push(r.action);
                    (
                        i32::try_from(actions.len() - 1).unwrap_or(-1),
                        music_player_rs::i18n::audio::PATH_REC_FIX_LABEL.to_string(),
                    )
                }
                path_view::RecViewAction::OpenSettings(_) => {
                    actions.push(r.action);
                    (
                        i32::try_from(actions.len() - 1).unwrap_or(-1),
                        music_player_rs::i18n::audio::PATH_REC_OPEN_SETTINGS_LABEL.to_string(),
                    )
                }
            };
            PathRecItem { text: r.text.into(), action_label: action_label.into(), action_id }
        })
        .collect()
}

/// Текущее поколение задания на каждый вид ответа потока `apap-io` (ADR-20,
/// §4): растёт при каждой новой постановке задания своего вида, сверяется
/// с `gen` пришедшего `IoDone` в `drain_io`, чтобы отбросить устаревший
/// ответ (обгон предыдущего запроса тем же видом задания).
#[derive(Default)]
struct IoGens {
    theme: u64,
    themes: u64,
    sizes: u64,
    clear: u64,
}

pub struct MusicApp {
    /// Слабая ссылка на окно (R-20, ADR-01, ТЗ-45): сильный `AppWindow` держит
    /// только `main` на весь цикл событий. Сильное поле здесь замыкало бы цикл
    /// window → callback (захватывает `Rc<RefCell<MusicApp>>`) → `MusicApp` → window.
    ui: slint::Weak<AppWindow>,
    /// Пути файлов настроек, состояния, плейлиста и журнала (ADR-19).
    paths: ConfigPaths,
    /// Владелец действующих настроек и состояния сессии (ADR-19, И-Т7, §8.1 С3).
    core: AppCore,
    /// `EngineHandle` для потока движка (ADR-01, ADR-02): `Drop` шлёт
    /// `Shutdown` и join-ит поток. `None` — движок не запустился
    /// (`engine_fault`); команды через `engine_send` становятся no-op.
    engine: Option<EngineHandle>,
    /// Зеркало состояния движка в UI-потоке — единственный источник правды
    /// для транспорта/позиции/трека/потока/устройства/резервирования/
    /// завершения сессии и плана пропуска (ADR-02, И-Р13, И-Р14).
    audio_state: UiAudioState,
    /// Текущее поколение задания `Open`, отфильтровывающее устаревшие
    /// ответы движка (И-Р14, ТЗ-56).
    req_gen: u64,
    /// Вид бейджа тракта и кнопки «Тест» (ADR-06, ТЗ-52, ТЗ-60…ТЗ-62) —
    /// принимает бейдж из `UiAudioState::take_badge_dirty` и смену
    /// активного режима, снимается через `take_badge_view`.
    badge_view: BadgeViewState,
    /// Кэш отчёта окна «Путь сигнала» (ТЗ-79, ТЗ-83, §7.2) — пересчитывается
    /// только через `take_report_dirty`/`path_report`, никогда по тику UI.
    path_cache: PathWindowCache,
    /// Persistent row model for the playlist table. Mutated incrementally
    /// (push/set_row_data) instead of rebuilding the whole list on every
    /// append, so folder scans stay cheap.
    playlist_rows: Rc<VecModel<ModelRc<StandardListViewItem>>>,
    /// Persistent column model for the playlist table. Stored across window
    /// resizes so that proportional reflow only touches each column's `width`
    /// (set_row_data) instead of re-creating a fresh ModelRc on every tick —
    /// keeps resizing smooth and never lets the column widths constrain the
    /// window size.
    playlist_cols: Rc<VecModel<TableColumn>>,
    /// Persistent row model for the mode-settings dialog panel (SP1.0
    /// ADR-11, AM1.0-B33): `sync_mode_dialog_to_ui` touches only the rows
    /// whose value changed via `set_row_data` instead of replacing the whole
    /// `ModelRc`, so a `Slider` mid-drag is never torn down and recreated by
    /// the `ui/mode_params.slint` repeater.
    mode_rows: Rc<VecModel<ModeParamRow>>,
    current: Option<usize>,
    /// Трек, открытие которого отправлено движку; применяется по событию
    /// Opened (ADR-01, ТЗ-103).
    pending_open: Option<(usize, Option<usize>)>,
    scan_rx: Option<Receiver<ScanMsg>>,
    /// Tracks buffered by `drain_scan` while a background scan runs; committed
    /// to `tracks` atomically when `ScanMsg::Done` arrives.
    scan_pending: Vec<Track>,
    known_paths: HashSet<PathBuf>,
    status: SharedString,
    repeat: RepeatMode,
    shuffle: bool,
    /// `true`, once the engine has confirmed a usable output device —
    /// either by enumerating it in a `Devices` catalog update or by opening
    /// a stream on it. No synchronous probe blocks `new()` any more; the
    /// window shows before availability is known (ТЗ-104, ADR-16).
    audio_ready: bool,
    /// Device-unavailable message from `Notice::DevicesUnavailable`, if the
    /// engine's last catalog refresh failed. `None` while availability is
    /// still unknown (not yet an error) and once a device is confirmed
    /// ready (ТЗ-104).
    audio_error: Option<String>,
    /// Effective output device name: the saved/preferred device until the
    /// engine's device catalog confirms one, then the catalog's name for it
    /// (ТЗ-104, ADR-16).
    active_device: String,
    tray_rx: Option<std::sync::mpsc::Receiver<TrayEvent>>,
    /// Итог регистрации трея → `caps` (ADR-6, В-1); `None` — итог получен.
    tray_ready: Option<std::sync::mpsc::Receiver<bool>>,
    tray_up_tx: Option<tokio::sync::mpsc::UnboundedSender<tray::TrayState>>,
    last_tray_update: Instant,
    last_view_width: f32,
    view_w_stable_ticks: u32,
    col_model_sig: u64,
    col_sig_stable_ticks: u32,
    cover_tx: Option<std::sync::mpsc::Sender<CoverJob>>,
    cover_rx: Option<Receiver<CoverDone>>,
    cover_gen: u64,
    /// In-flight async enumeration of output devices for the Settings dialog.
    /// The worker sends full [`DeviceInfo`] structs; the dialog consumes both
    /// the `(id, label)` pairs (ComboBox) and the raw info list
    /// (capabilities + validation, ТЗ A3.0 §8.1).
    audio_devices_rx: Option<Receiver<Vec<DeviceInfo>>>,
    /// Last device listing as `(id, label)` pairs; the label is what the
    /// ComboBox shows, the id is the stable backend key (ALSA pcm id) that gets
    /// persisted and matched by the audio backend, so selection must translate
    /// label -> id.
    audio_devices_pairs: Vec<(String, String)>,
    /// Full device info list from the *last* enumeration (kept across filter
    /// toggles so the dialog never re-queries the backend for a filter preview,
    /// ТЗ A3.0 §8.1). Consumed by capabilities/validation sync.
    audio_device_infos: Vec<DeviceInfo>,
    /// Последний `DeviceInfoView`, записанный в Slint (AM1.0-B30, шаг 3,
    /// пункт A): `sync_device_info_to_ui` вызывается каждый тик, пока диалог
    /// настроек открыт, но пишет 5 свойств UI только когда вид реально
    /// изменился — `DeviceInfoView` дёшево сравнить (`derive(PartialEq)`).
    last_device_info_view: Option<device_info_view::DeviceInfoView>,
    /// Последний текст бейджа устройства в MenuBarTrailing (AM1.0-B30, шаг
    /// 3) — `None` до первой записи, `Some("")` — бейдж скрыт (ничего не
    /// открыто). Пишет в Slint только при изменении (`sync_device_badge`).
    last_device_badge: Option<String>,
    /// Устройство, для которого последним была отправлена
    /// `EngineCmd::ProbeDeviceCaps` (AM1.0-B38, шаг 9) — дебаунс повторного
    /// выбора того же устройства подряд в `on_settings_device`
    /// (`maybe_probe_selected_device`): движок и так не плодит параллельные
    /// опросы одного устройства (`probe_device_caps`, §2.3), но лишнюю
    /// команду в канал всё равно не шлём. Выбор другого устройства —
    /// обычное новое значение, не снятие дебаунса отдельным событием.
    last_probed_device: Option<HwDeviceId>,
    /// Имя трека, по которому последний раз выставлялся заголовок окна
    /// (T1.0-B2, решение пользователя 2026-10-23): `None` — заголовок
    /// «APAP» (ничего не играет). Пишет в Slint только при изменении
    /// (`sync_window_title`).
    last_window_title: Option<String>,
    events_tx: std::sync::mpsc::Sender<AppEvent>,
    events_rx: std::sync::mpsc::Receiver<AppEvent>,
    /// Delta-synced playback values last pushed to the UI.
    last_ui: UiState,
    /// Double-click detection: row of the previous single-click.
    last_click_row: Option<i32>,
    /// Double-click detection: timestamp of the previous single-click.
    last_click_time: Instant,
    /// Live spectrum worker + tap (только для режима spectrum, ТЗ §16.2).
    viz: Option<music_player_rs::audio::analyzer::LiveWorker>,
    /// Сигнатура конфига, применённого к воркеру/UI (delta-применение).
    viz_sig: Option<VizSig>,
    /// Последний запушенный кадр полос (для распада на паузе и дельты).
    viz_bars: Vec<f32>,
    /// Текущее состояние tap-тумблера в плеере.
    viz_tap_active: bool,
    /// Полнотрековая осциллограмма (§16.4): каналы воркера.
    fulltrack_tx: Option<std::sync::mpsc::Sender<FullCmd>>,
    fulltrack_rx: Option<std::sync::mpsc::Receiver<FullEvt>>,
    /// Последний выданный id билда.
    fulltrack_id: u64,
    /// Целевой (path, cache_key) текущего билда.
    fulltrack_target: Option<(std::path::PathBuf, String)>,
    /// Метаданные текущего файла для ключа кэша полнотрековой картинки:
    /// (путь, mtime, размер). Читаются один раз при смене пути, а не на
    /// каждом UI-тике (V5.1 §13, V5.1-B8).
    fulltrack_file_meta: Option<(std::path::PathBuf, Option<std::time::SystemTime>, u64)>,
    /// Ключ билда, уже показанный на UI (для дропа устаревших Ready).
    fulltrack_key: Option<String>,
    /// RAM-кэш построенных изображений по cache-ключу (режим+параметры).
    /// LRU: лимит записей из `viz_max_ram_mb` (см. `fulltrack_cache_max_entries`).
    /// Значение хранит фактический размер RGBA-буфера в байтах (для §10.5).
    fulltrack_cache: clru::CLruCache<String, (slint::Image, usize)>,
    /// Режим, под который построен текущий целевой билд (детект смены типа).
    fulltrack_mode: Option<music_player_rs::audio::visualizer::VisualizationMode>,
    /// Центр сообщений: очередь до первого показа, по одному, сводное окно
    /// ошибок записи с «Повторить»/«OK» (ADR-13, §6.15, §6.8, ТЗ-52, ТЗ-20).
    messages: MessageCenter,
    /// Возможности платформы, которыми руководствуется `MessageCenter` при
    /// решении «окно или уведомление» (ADR-9, ТЗ-52 п. 3). До итога
    /// регистрации трея (`tray_ready`) — без трея и уведомлений (В-1).
    caps: PlatformCaps,
    /// Системные уведомления при окне в трее (ADR-9, ТЗ-52 п. 2).
    notifier: Box<dyn Notifier>,
    /// Причина отказа запуска движка, если `self.engine` — `None` (ТЗ-88,
    /// §2.10); здесь хранится только причина отказа — для стартового
    /// сообщения пользователю.
    engine_fault: Option<EngineFault>,
    /// Приёмная сторона канала событий движка (ADR-02); разбирается в
    /// `drain_engine_events` каждый тик.
    engine_events: EngineEventQueue,
    /// Буфер разобранных событий движка, переиспользуется между тиками
    /// (ADR-02, И-Р13).
    engine_applied: Vec<Applied>,
    /// Буфер сырых событий движка, снятых с `engine_events` за один
    /// `drain_engine_events`, переиспользуется между тиками без аллокации
    /// (ADR-02, И-Р13) — нужен, чтобы разбор `EngineEvent::Devices` и
    /// обработка через `on_engine_event` не делили `self` одновременно.
    engine_raw_events: Vec<EngineEvent>,
    /// Счётчик треков, пропущенных подряд из-за ошибки файла; мост до
    /// SkipSeries С8 (ТЗ-85, ТЗ-86) — защита от бесконечного пропуска, если
    /// повреждена вся библиотека при Repeat All.
    skip_streak: usize,
    /// Текущее поколение задания на каждый вид ответа `apap-io` (ADR-20, §4).
    /// Сам поток принадлежит `AppCore` (`AppDeps.io`, §2.12): задания и
    /// ответы идут через `self.core.io()`.
    io_gens: IoGens,
    /// Последние дисковые размеры кэшей, полученные от `apap-io`; показаны
    /// сразу при открытии диалога, пока свежий подсчёт не пришёл (ТЗ-22,
    /// ADR-20).
    cache_disk: CacheSizes,
    /// Асинхронный выбор файлов/каталогов (ADR-10, ТЗ-53): системные
    /// диалоги `rfd` в проде, `FakePicker` в тестах.
    picker: Box<dyn FilePicker>,
    /// Платформенная доступность Оптимального/Строгого режима (ТЗ-109,
    /// ТЗ-110, §2.8): статична на время жизни процесса, не меняется
    /// заново запущенным движком — используется переключателем режимов в
    /// `on_select_mode` и `sync_mode_to_ui` (§6.18).
    backend_caps: BackendCaps,
    /// Позиция и состояние воспроизведения, сохранённые перед переоткрытием
    /// трека на смене активного режима (§6.18, ТЗ-20): `open_track` всегда
    /// открывает с нуля без автовоспроизведения, поэтому восстановить их
    /// нужно явно после прихода `Opened` в `on_track_opened`.
    pending_mode_reopen: Option<(f64, bool)>,
    /// Уведомление «режим недоступен на этой платформе» уже поставлено и не
    /// закрыто (ТЗ-111, §6.2 п. 5, ADR-13): не копится повторно при старте и
    /// при каждом неудачном открытии трека по той же причине.
    mode_notice_pending: bool,
    /// Действия кнопок последнего показанного окна «Путь сигнала» (ТЗ-81,
    /// ТЗ-84): индекс в этом векторе — `action-id`, переданный Slint в
    /// `path-rec-action`; `push_path_view_to_ui` заполняет его заново на
    /// каждый показ отчёта.
    path_actions: Vec<path_view::RecViewAction>,
    /// «Захватить и играть» нажато в окне «Замок открыт» (ТЗ-125, §6.19):
    /// `EngineCmd::Lock(CaptureAndPlay)` ушёл движку, но воспроизведение сам
    /// захват не запускает — ждём `Held` от следующего `EngineEvent::Lock` в
    /// `drain_engine_events`, чтобы запустить его тогда. `Acquiring` оставляет
    /// флаг как есть; любой другой исход (`Failed`/`Open`/`NotApplicable`)
    /// снимает его без запуска воспроизведения.
    capture_and_play_pending: bool,
    /// Окно ошибки замка, которое сейчас отслеживает эта сессия — показано
    /// или ещё стоит в очереди `MessageCenter` (ТЗ-46, ТЗ-144, AM1.0-B14
    /// шаг 5): отличает «моё» сообщение от любого другого (ошибки записи,
    /// уведомление о недоступном режиме), занявшего показ впереди.
    /// `apply_lock_failure_message` ставит и снимает его по решению
    /// `lock_failure_transition`; `on_msg_action` снимает при нажатии любой
    /// кнопки этого окна.
    lock_failure_msg: Option<Message>,
}

/// Команды, которыми при старте применяется сохранённая политика режимов
/// (ОВС-14, §6.27, ADR-22): `SetModeSettings` с `diff: None` — движок просто
/// копирует `settings` без пересчёта `ApplyKind`, затем `SetActiveMode`
/// сохранённого активного режима. Чистая функция — пригодна для проверки
/// равенства с восстановленной после перезапуска политикой (ТЗ-19, И-Р14).
fn startup_commands(modes: &ModeSettings, active: ModeKind) -> [EngineCmd; 2] {
    [
        EngineCmd::SetModeSettings(ModeSettingsUpdate { settings: modes.clone(), diff: None }),
        EngineCmd::SetActiveMode(active),
    ]
}

/// Строит копию настроек с сохранённым автовыбором устройства режима
/// (ТЗ-143, ТЗ-134, ADR-24, шаг 38): движок уже применил `device` к своей
/// копии `ModeSettings` режима `mode`, здесь он переносится в настройки
/// приложения тем же `set_device`, не трогая другой режим. `None` —
/// Совместимый режим (автовыбора не знает, §2.3): вызывающая сторона не
/// пишет файл.
fn settings_with_auto_selected_device(
    settings: &PersistSettings,
    mode: ModeKind,
    device: &HwDeviceId,
) -> Option<PersistSettings> {
    if mode == ModeKind::Compatible {
        return None;
    }
    let mut settings = settings.clone();
    settings.playback.set_device(mode, device.as_str());
    Some(settings)
}

/// Разобрать событие движка, обновить состояние/бейдж/кэш отчёта «Путь
/// сигнала» (ADR-02, ТЗ-79, ТЗ-83, ТЗ-68, §3.5, §7.2).
fn apply_engine_event(
    state: &mut UiAudioState,
    badge_view: &mut BadgeViewState,
    path_cache: &mut PathWindowCache,
    ev: EngineEvent,
) -> Applied {
    let applied = state.apply(ev);
    if let Some(b) = state.take_badge_dirty() {
        badge_view.set_badge(b);
    }
    if state.take_path_dirty().is_some() {
        path_cache.invalidate();
    }
    // Динамическая часть пути (Underrun/Xrun/MD5/…) входит в отчёт
    // (`compute_report` → `audio.dynamics()`), поэтому её изменение тоже
    // инвалидирует кэш окна «Путь сигнала» (ТЗ-68, §3.5, §7.2).
    if state.take_dynamics_dirty().is_some() {
        path_cache.invalidate();
    }
    if matches!(&applied, Applied::Skipped { .. } | Applied::OpenFailed { .. }) {
        path_cache.invalidate();
    }
    applied
}

impl MusicApp {
    /// `paths` и `AppCore` (с писателем и журналом) строит `main` (ADR-19): приложение не ищет
    /// каталог настроек пользователя само, поэтому тесты его не трогают (ТЗ-49).
    /// `tray` — UI-концы каналов трея; сам трей запускает `Lifecycle::install`
    /// (ADR-23 шаг 8). `notifier` — системные уведомления (ADR-9).
    /// `engine`/`engine_events` — поток `apap-engine` и канал его событий,
    /// запущенные в `main` до `AppCore` (ADR-01, ADR-19, ТЗ-88); движок ещё
    /// не подключён к логике приложения на этом шаге. `backend_caps` —
    /// платформенная доступность Оптимального/Строгого режима (ТЗ-109,
    /// ТЗ-110, §2.8), известная даже если запуск движка провалился.
    /// Поток `apap-io` (ADR-20, §2.12) отдельным параметром не передаётся:
    /// им владеет `core` (`AppDeps.io`), доступ — через `AppCore::io()`.
    /// `picker` — асинхронный выбор файлов/каталогов (ADR-10, ТЗ-53);
    /// внедряется через параметр, а не создаётся здесь, чтобы тесты могли
    /// подставить свою реализацию.
    // Конструктор собирает инжектируемые зависимости (ADR-19, ADR-20) —
    // восьмой параметр (`picker`) не повод вводить промежуточную структуру.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        window: &AppWindow,
        core: AppCore,
        paths: ConfigPaths,
        tray: tray::TrayChannels,
        notifier: Box<dyn Notifier>,
        engine: Result<EngineHandle, EngineFault>,
        backend_caps: BackendCaps,
        engine_events: EngineEventQueue,
        picker: Box<dyn FilePicker>,
    ) -> Self {
        let (engine, engine_fault) = match engine {
            Ok(handle) => (Some(handle), None),
            Err(fault) => (None, Some(fault)),
        };
        let pb_state = core.state().playback();
        // Стартовые звуковые настройки движка уходят отдельными командами
        // (ADR-01, ADR-04, ТЗ-134), не поштучными сеттерами `Player`:
        // `startup_commands` шлёт `SetModeSettings`/`SetActiveMode`, из
        // которых движок пересобирает `LegacyAudio` (`legacy_audio()` в
        // `engine/legacy_path.rs`) раньше первого `Open`; `bit_perfect()`
        // выводит те же флаги тем же отображением, без отдельного кэша
        // (§6.18, ADR-04, ТЗ-52).
        let modes = core.settings().playback.clone();
        let active_mode = core.state().active_mode();
        let send_startup = |cmd: EngineCmd| -> bool { engine.as_ref().is_some_and(|e| e.send(cmd)) };
        // Стартовая политика режимов — до первого `Open` (шаг 20, ОВС-14,
        // §6.27, ADR-22): движок должен знать сохранённые `ModeSettings` и
        // активный режим раньше, чем откроется первый трек плейлиста.
        for cmd in startup_commands(&modes, active_mode) {
            send_startup(cmd);
        }
        // Громкость/mute/предпочитаемое устройство — не часть `LegacyAudio`
        // (это UI-состояние и выбор устройства, не DSP-политика движка),
        // поэтому отправляются отдельными командами после стартовой политики.
        send_startup(EngineCmd::SetVolume(pb_state.gain()));
        send_startup(EngineCmd::SetMuted(pb_state.muted));
        // Устройство читается из `ModeSettings` активного режима, а не из
        // прежнего легаси-поля устройства (ТЗ-94, §2.4). Прямая отправка
        // здесь остаётся нужна: `SetActiveMode` из `startup_commands` не
        // применяет устройство, если активный режим движка на старте уже
        // совпадает с сохранённым (см. `Engine::store_active_mode`).
        if let Some(device) = core.settings().playback.device(active_mode) {
            send_startup(EngineCmd::SetDevice { id: device.to_string() });
        }

        let known_paths: HashSet<PathBuf> = HashSet::new();
        let (tray_rx, tray_up_tx, tray_ready) =
            (Some(tray.events), Some(tray.updates), Some(tray.ready));

        let (cover_tx, cover_job_rx) = channel::<CoverJob>();
        let (cover_done_tx, cover_done_rx) = channel::<CoverDone>();
        cover::start_worker(cover_job_rx, cover_done_tx);

        let (events_tx, events_rx) = channel::<AppEvent>();

        // Wire the persistent playlist row model to the table once; later
        // mutations flow through it without re-creating ModelRc objects.
        // Сильная ссылка нужна только здесь; хранится `Weak` (R-20, ТЗ-45, ТЗ-101).
        let ui_handle = window;
        let ui = window.as_weak();
        let playlist_rows: Rc<VecModel<ModelRc<StandardListViewItem>>> =
            Rc::new(slint::VecModel::default());
        ui_handle.set_playlist_rows(ModelRc::from(playlist_rows.clone()));

        let playlist_cols: Rc<VecModel<TableColumn>> = Rc::new(slint::VecModel::default());
        ui_handle.set_playlist_cols(ModelRc::from(playlist_cols.clone()));

        // Same pattern for the mode-settings dialog rows (AM1.0-B33): wired
        // once here, mutated in place by `sync_mode_dialog_to_ui`.
        let mode_rows: Rc<VecModel<ModeParamRow>> = Rc::new(slint::VecModel::default());
        ui_handle.set_settings_mode_rows(ModelRc::from(mode_rows.clone()));

        let repeat = core.state().repeat();
        let shuffle = core.state().shuffle();

        // Output device availability is no longer probed synchronously here:
        // the engine thread enumerates devices in the background and reports
        // them via `EngineEvent::Devices`/`Notice::DevicesUnavailable`, applied
        // in `drain_engine_events`/`dispatch_applied` (ТЗ-104, ADR-16). The
        // window must show before that first report arrives, so the initial
        // state is "unknown", not an error.
        let saved_device = core.settings().playback.device(active_mode).unwrap_or("").to_string();
        let audio_ready = false;
        let audio_error = None;
        let active_device = if saved_device.is_empty() {
            String::from("default")
        } else {
            saved_device.clone()
        };
        let startup_status = String::from(music_player_rs::i18n::STATUS_AUDIO_CHECKING);

        // Visualization tap (ТЗ §16.2): кольцо PCM для анализатора, consumer
        // уходит в LiveWorker, producer — в audio-callback плеера.
        let (viz_prod, viz_cons) = rtrb::RingBuffer::new(TAP_CAPACITY);
        let viz_cfg = Arc::new(VisualizerConfig::from_persist_settings(core.settings(), core.state().viz_mode()));
        let cache_max_entries =
            music_player_rs::audio::fulltrack::fulltrack_cache_max_entries(
                core.settings().visualization.viz_max_ram_mb,
            );

        let mut app = Self {
            ui,
            paths,
            core,
            engine,
            audio_state: UiAudioState::new(),
            req_gen: 0,
            badge_view: BadgeViewState::new(),
            path_cache: PathWindowCache::new(),
            playlist_rows,
            playlist_cols,
            mode_rows,
            current: None,
            pending_open: None,
            scan_rx: None,
            scan_pending: Vec::new(),
            known_paths,
            status: startup_status.into(),
            repeat,
            shuffle,
            audio_ready,
            audio_error,
            active_device,
            tray_rx,
            tray_ready,
            tray_up_tx,
            last_tray_update: Instant::now(),
            last_view_width: 0.0,
            view_w_stable_ticks: 0,
            col_model_sig: 0,
            col_sig_stable_ticks: 0,
            cover_tx: Some(cover_tx),
            cover_rx: Some(cover_done_rx),
            cover_gen: 0,
            audio_devices_rx: None,
            audio_devices_pairs: Vec::new(),
            audio_device_infos: Vec::new(),
            last_device_info_view: None,
            last_device_badge: None,
            last_probed_device: None,
            last_window_title: None,
            events_tx,
            events_rx,
            last_ui: UiState::default(),
            last_click_row: None,
            last_click_time: Instant::now(),
            viz: None,
            viz_sig: None,
            viz_bars: Vec::new(),
            viz_tap_active: false,
            fulltrack_tx: None,
            fulltrack_rx: None,
            fulltrack_id: 0,
            fulltrack_target: None,
            fulltrack_file_meta: None,
            fulltrack_key: None,
            fulltrack_cache: clru::CLruCache::new(cache_max_entries),
            fulltrack_mode: None,
            messages: MessageCenter::default(),
            // Трей и уведомления — после регистрации значка (poll_tray, ADR-6).
            caps: PlatformCaps::default(),
            notifier,
            engine_fault,
            engine_events,
            engine_applied: Vec::new(),
            engine_raw_events: Vec::new(),
            skip_streak: 0,
            io_gens: IoGens::default(),
            cache_disk: CacheSizes::default(),
            picker,
            backend_caps,
            pending_mode_reopen: None,
            mode_notice_pending: false,
            path_actions: Vec::new(),
            capture_and_play_pending: false,
            lock_failure_msg: None,
        };
        // Стартовая загрузка плейлиста в фоне (ADR-16, ADR-23 шаг 9, §6.12,
        // ТЗ-21, ТЗ-47): поток `apap-playlist` через `AppCore`; `tick()`
        // (`poll_load_playlist`) разбирает итог. Список недоступен до её
        // окончания (ТЗ-48, §2.11).
        app.core.start_startup_load();
        app.core.gate_mut().set_loading(Some(LoadKind::Startup));
        if let Some(ui) = app.try_ui() {
            ui.set_busy(true);
        }
        app.setup_fulltrack();
        app.setup_visualizer(viz_cfg, viz_prod, viz_cons);
        // Ключ сортировки сессии применяется в `poll_load_playlist`, когда
        // стартовая загрузка завершится (здесь `tracks` всегда пуст)
        // (§6.13, ТЗ-42).
        app.apply_window_geometry();
        // Восстановленная геометрия — программная установка: её эхо на тике
        // не взводит срок записи (ADR-22, §6.17).
        let restored = app.core.state().window();
        app.core.program_set_geometry(restored);
        // Движок не запустился — показать пользователю окно ошибки сразу
        // при старте, не дожидаясь первого обращения к звуку (ТЗ-88).
        if let Some(fault) = app.engine_fault.clone() {
            app.push_message(Message {
                level: MessageLevel::Error,
                title: music_player_rs::i18n::messages::ENGINE_NOT_STARTED_TITLE.into(),
                body: format!("{fault:?}").into(),
                actions: vec![MessageAction::Ok],
            });
        }
        // Стартовая синхронизация переключателя режимов (§6.18, ТЗ-109, ТЗ-110).
        app.sync_mode_to_ui();
        // Сохранённый активный режим недоступен на этой платформе: звук не
        // запускается и режим не меняется сам — только уведомление с
        // кнопкой переключения в Совместимый (ТЗ-111, §6.2 п. 5).
        if !app.backend_caps.available(app.core.state().active_mode()) {
            app.push_mode_unavailable_notice();
        }
        // Сохранённое устройство Совместимого было выбрано мимо аудиосервера
        // (старый hw:/plughw: id до фильтра `is_server_routed_pcm`) и
        // мигрировало на «Системное по умолчанию» при загрузке
        // (`migrate_legacy_compatible_device`, AM1.0-B32, фаза 2) —
        // одноразовое уведомление по образцу `push_mode_unavailable_notice`
        // (ТЗ-111); исправленное значение переживёт как обычно следующую
        // запись настроек (И-Р22: та же схема, что у согласования буфера
        // устройства).
        if app.core.startup_notes().settings.iter().any(|n| {
            n.key.as_str() == "playback.compatible.device" && matches!(n.kind, LoadNoteKind::Adjusted { .. })
        }) {
            app.push_message(Message {
                level: MessageLevel::Info,
                title: music_player_rs::i18n::audio::COMPATIBLE_DEVICE_MIGRATED_TITLE.into(),
                body: music_player_rs::i18n::audio::COMPATIBLE_DEVICE_MIGRATED_BODY.into(),
                actions: vec![MessageAction::Ok],
            });
        }
        // Запуск ничего не пишет (ТЗ-4, §8.1 С3).
        app
    }

    /// Окно приложения без паники (R-20, ТЗ-45, ТЗ-101). `None` — окно уже
    /// уничтожено (выход); вызывающий молча пропускает обновление UI.
    pub(super) fn try_ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    /// Трек по индексу видимого порядка плейлиста (ADR-15, §4.2): чтение
    /// через модель `AppCore` вместо временного зеркала `tracks` —
    /// `visible()` даёт `TrackId` по индексу, `get` — сам трек.
    pub(super) fn track_at(&self, i: usize) -> Option<&Track> {
        let id = *self.core.playlist().visible().get(i)?;
        self.core.playlist().get(id)
    }

    /// Число треков в видимом порядке плейлиста (ADR-15, §4.2).
    pub(super) fn track_count(&self) -> usize {
        self.core.playlist().visible().len()
    }

    /// Применить эффекты ответов писателя из `AppCore::tick`/`AppCore::retry`
    /// (ADR-22, §6.4, §6.5): `Succeeded`/`Failed` — через те же методы
    /// `MessageCenter` (ТЗ-18, ТЗ-20); `None` отфильтрован самим
    /// `AppCore` (уходит в `TickOutput.other`), здесь не встречается.
    pub(super) fn apply_reply_effects(&mut self, effects: Vec<ReplyEffect>) {
        for effect in effects {
            match effect {
                ReplyEffect::None => {}
                ReplyEffect::Succeeded(file) => {
                    let effect = self.messages.write_succeeded(file);
                    self.apply_msg_effect(effect);
                }
                ReplyEffect::Failed(file, class) => {
                    let effect = self.messages.write_failed(file, class, self.caps);
                    self.apply_msg_effect(effect);
                }
            }
        }
    }

    /// Применить эффект `MessageCenter` к Slint-свойствам окна сообщения и
    /// к шлюзу главного окна (ADR-13, §6.15). `notify` — уведомление при окне
    /// в трее (ТЗ-52 п. 3); доставляется через инжектируемый трейт
    /// `Notifier` (ADR-9).
    pub(super) fn apply_msg_effect(&mut self, effect: MsgEffect) {
        let MsgEffect { show, hide, notify } = effect;
        if let Some(n) = notify {
            self.notifier.notify(n);
        }
        let ui = self.try_ui();
        if hide {
            if let Some(ui) = &ui {
                ui.set_msg_shown(false);
            }
            self.core.gate_mut().unblock(BlockReason::Message);
        }
        if let Some(message) = show {
            let level = match message.level {
                MessageLevel::Info => 0,
                MessageLevel::Warning => 1,
                MessageLevel::Error => 2,
            };
            // Переменный набор кнопок (AM1.0-B14): `message.actions` — тот
            // же `MessageAction`-список, что рендерит `MessageWindow` и
            // обрабатывает `on_msg_action` ниже; индекс в этом списке —
            // `action-id`.
            let items: Vec<MessageActionItem> = message
                .actions
                .into_iter()
                .enumerate()
                .map(|(i, action)| MessageActionItem {
                    label: action.label().into(),
                    action_id: i32::try_from(i).unwrap_or(0),
                })
                .collect();
            if let Some(ui) = &ui {
                ui.set_msg_level(level);
                ui.set_msg_title(message.title.as_ref().into());
                ui.set_msg_body(message.body.as_ref().into());
                ui.set_msg_actions(ModelRc::from(items.as_slice()));
                ui.set_msg_shown(true);
            }
            self.core.gate_mut().block(BlockReason::Message);
        }
        self.sync_gate_ui();
    }

    /// Синхронизировать `window-blocked` со шлюзом после любой его мутации
    /// (ADR-12, ТЗ-23).
    pub(super) fn sync_gate_ui(&mut self) {
        if let Some(ui) = self.try_ui() {
            ui.set_window_blocked(self.core.gate().window_blocked());
        }
    }

    /// Синхронизировать переключатель режимов в UI: активный режим (ADR-04)
    /// и доступность Оптимального/Строгого по возможностям бэкенда, а не
    /// условной компиляцией (§6.18, ТЗ-18, ТЗ-109, ТЗ-110). Вызывается при
    /// старте и после каждой смены активного режима.
    pub(super) fn sync_mode_to_ui(&mut self) {
        let Some(ui) = self.try_ui() else { return };
        let active = self.core.state().active_mode();
        ui.set_active_mode(match active {
            ModeKind::Compatible => 0,
            ModeKind::Optimal => 1,
            ModeKind::Strict => 2,
        });
        // Текст пояснения — дословно по ТЗ-110.
        let modes = [ModeKind::Compatible, ModeKind::Optimal, ModeKind::Strict];
        let available: Vec<bool> = modes.iter().map(|m| self.backend_caps.available(*m)).collect();
        let reasons: Vec<SharedString> = modes
            .iter()
            .map(|m| {
                if self.backend_caps.available(*m) {
                    SharedString::default()
                } else {
                    SharedString::from(music_player_rs::i18n::messages::MODE_SWITCH_UNAVAILABLE_REASON)
                }
            })
            .collect();
        ui.set_mode_available(ModelRc::from(available.as_slice()));
        ui.set_mode_unavailable_reason(ModelRc::from(reasons.as_slice()));
        // Ползунок/колесо громкости в top_panel блокируются визуально, когда
        // активный режим фиксирует громкость (ТЗ-41, ADR-23, §6.18).
        ui.set_volume_enabled(!self.volume_locked());
        // Текст подсказки недоступной громкости даёт ядро (ТЗ-39, ADR-14,
        // §2.13); пустая строка — громкость доступна, подсказки нет.
        ui.set_volume_block_text(
            self.core
                .volume_block()
                .map(|b| SharedString::from(b.tooltip()))
                .unwrap_or_default(),
        );
    }

    /// Асинхронный выбор файла/каталога, не блокирующий цикл событий
    /// (ADR-10, ADR-12, ТЗ-53): шлюз держит причину `FilePicker` до
    /// завершения диалога (успешного или отменённого) — повторный выбор,
    /// как и остальные пункты меню, за это время отклоняется шлюзом.
    /// `make_req` строит запрос по стартовому каталогу последнего выбора
    /// (`SessionState::last_dir`, §2.5); успешный результат запоминает
    /// каталог первого пути через `StateChange::LastDir` и передаёт все
    /// пути в `on_paths`. Пустой результат (`Cancelled` или пустой список)
    /// не вызывает `on_paths`.
    pub(super) fn pick_async(
        app: &Rc<RefCell<Self>>,
        make_req: impl FnOnce(Option<PathBuf>) -> PickRequest,
        on_paths: impl FnOnce(&Rc<RefCell<Self>>, Vec<PathBuf>) + 'static,
    ) {
        let fut = {
            let mut a = app.borrow_mut();
            let start = a.core.state().last_dir().map(|p| p.to_path_buf());
            let req = make_req(start);
            a.core.gate_mut().block(BlockReason::FilePicker);
            a.sync_gate_ui();
            let parent = a.try_ui();
            a.picker.pick(req, parent.as_ref().map(|ui| ui.window()))
        };
        let app = app.clone();
        let app_for_err = app.clone();
        if let Err(e) = slint::spawn_local(async move {
            let result = fut.await;
            {
                let mut a = app.borrow_mut();
                a.core.gate_mut().unblock(BlockReason::FilePicker);
                a.sync_gate_ui();
            }
            let PickResult::Paths(paths) = result else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            let first = &paths[0];
            let dir = if first.is_dir() {
                Some(first.clone())
            } else {
                first.parent().map(|p| p.to_path_buf())
            };
            if let Some(dir) = dir {
                app.borrow_mut().core.change_state(Origin::User, StateChange::LastDir(dir));
            }
            on_paths(&app, paths);
        }) {
            let mut a = app_for_err.borrow_mut();
            a.core.gate_mut().unblock(BlockReason::FilePicker);
            a.sync_gate_ui();
            eprintln!("[picker] spawn_local не запустился: {e}");
        }
    }

    /// Повторить запись перечисленных рабочих файлов (кнопка «Повторить»
    /// сводного окна ошибок записи, §6.8, ТЗ-20, ТЗ-52): снимки уходят
    /// писателю `apap-persist` через `AppCore::retry` (ADR-22); ответы
    /// приходят в `tick` и обновляют окно через `write_failed`/
    /// `write_succeeded` (ТЗ-13).
    pub(super) fn retry_writes(&mut self, files: Vec<WorkFile>) {
        if files.is_empty() {
            return;
        }
        let effects = self.core.retry(&files);
        self.apply_reply_effects(effects);
    }

    /// Путь выхода (ТЗ-14, ТЗ-32, ADR-7, §6.10): черновик диалога
    /// отбрасывается ядром (`dialog_cancel`), окно сообщений и очередь закрываются без вопроса,
    /// геометрия окна фиксируется в состоянии; `AppCore::exit` освобождает
    /// движок, отправляет итоговые снимки писателю и ждёт ответов не дольше
    /// бюджета выхода. Повторный запрос во время/после выхода игнорируется
    /// (`ExitOutcome::Ignored`). Для `WindowsSessionEnd`/`MacosTerminate`
    /// цикл событий Slint не останавливается — процесс завершит ОС (ADR-7,
    /// ТЗ-15, §6.10).
    pub(super) fn exit(&mut self, reason: ExitReason) -> ExitOutcome {
        // Черновик и причина блокировки `Dialog` снимаются до `sync_gate_ui`
        // и независимо от исхода `AppCore::exit` (при повторном запросе ядро
        // возвращает `Ignored`, не доходя до своего сброса черновика, ТЗ-32).
        self.core.dialog_cancel();
        if let Some(ui) = self.try_ui() {
            ui.set_settings_open(false);
        }
        self.messages.dismiss_for_exit();
        if let Some(ui) = self.try_ui() {
            ui.set_msg_shown(false);
        }
        self.core.gate_mut().unblock(BlockReason::Message);
        self.sync_gate_ui();
        // Последнее показание геометрии — в состояние до итоговых снимков.
        if let Some(g) = self.window_geometry() {
            if g != self.core.state().window() {
                self.core.change_state(Origin::User, StateChange::Window(g));
            }
        }
        // `Shutdown` шлёт `AppCore::exit`; здесь — синхронное ожидание
        // `ShutdownComplete` на UI-потоке до 2 с от запроса (§6.28 п. 3,
        // ТЗ-45). Запись файлов от него не зависит (ТЗ-136). Попутные события
        // движка доходят до `UiAudioState`, их итог на выходе не нужен.
        // `EngineHandle` join-ится при drop фасада — поток уже завершён.
        let requested = Instant::now();
        let events = &self.engine_events;
        let mut pending_events: Vec<EngineEvent> = Vec::new();
        let mut await_engine = || {
            events.wait_for(
                ENGINE_SHUTDOWN_WAIT.saturating_sub(requested.elapsed()),
                |ev| matches!(ev, EngineEvent::ShutdownComplete),
                |ev| pending_events.push(ev),
            )
        };
        let outcome = self.core.exit(reason, &mut await_engine);
        for ev in pending_events {
            let _ = self.on_engine_event(ev);
        }
        if outcome == ExitOutcome::Ignored {
            eprintln!("[app] exit {reason:?}: уже выполняется, повтор проигнорирован");
            return outcome;
        }
        if reason.quits_event_loop() {
            let _ = slint::quit_event_loop();
        }
        outcome
    }

    /// Поставить сообщение в `MessageCenter` и сразу применить эффект
    /// (используется раскладкой ТЗ-52/ОВ-7 на следующих шагах).
    pub(super) fn push_message(&mut self, message: Message) {
        let effect = self.messages.push(message, self.caps);
        self.apply_msg_effect(effect);
    }

    /// Уведомление «на этой платформе доступен только Совместимый режим,
    /// для воспроизведения переключите режим» с кнопкой переключения
    /// (ТЗ-111, §6.2 п. 5, ADR-13): ставится при старте, если сохранённый
    /// активный режим недоступен, и при отказе открытия трека по той же
    /// причине (`OpenError::ModeUnavailable`). Звук не запускается и режим
    /// не меняется сам — выбор остаётся за пользователем. `mode_notice_pending`
    /// не даёт поставить дубликат, пока предыдущее такое окно не закрыто.
    pub(super) fn push_mode_unavailable_notice(&mut self) {
        if self.mode_notice_pending {
            return;
        }
        self.mode_notice_pending = true;
        self.push_message(Message {
            level: MessageLevel::Warning,
            title: music_player_rs::i18n::messages::MODE_UNAVAILABLE_NOTICE_TITLE.into(),
            body: music_player_rs::i18n::messages::MODE_UNAVAILABLE_NOTICE_BODY.into(),
            actions: vec![MessageAction::SwitchToCompatible, MessageAction::Ok],
        });
    }

    /// Выбрать активный режим: из переключателя top_panel (`on_select_mode`)
    /// или из кнопки «Переключить в Совместимый режим» окна уведомления о
    /// недоступном режиме (§6.18, ТЗ-20, ТЗ-111). Недоступный на платформе
    /// или уже активный режим — без действий. Иначе: `SetActiveMode`,
    /// переоткрытие текущего трека с сохранённой позиции, применение
    /// громкости нового режима и синхронизация переключателя в UI.
    pub(super) fn select_active_mode(&mut self, mode: ModeKind) {
        if !self.backend_caps.available(mode) {
            return;
        }
        if self.core.state().active_mode() == mode {
            return;
        }
        self.mode_notice_pending = false;
        // UI отправляет SetActiveMode, затем переоткрывает тот же
        // трек с текущей позиции (§6.18 «Смена режима», ТЗ-20).
        self.core.change_state(Origin::User, StateChange::ActiveMode(mode));
        self.engine_set_active_mode(mode);
        self.reopen_current_for_mode_switch();
        self.apply_mode_gain();
        self.sync_mode_to_ui();
        // Признак bit-perfect выводится из активного режима (ADR-04, §6.18) —
        // подсказка трея обновляется сразу, не дожидаясь событий транспорта.
        self.push_tray_now();
    }

    // --- Мост к потоку движка (ADR-01, ADR-02) --------------------------------
    //
    // `engine`/`audio_state`/`req_gen`/`badge_view`/`path_cache` — прямые
    // поля `MusicApp` (AM1.0-8.5, шаг 53): здесь только точки входа, которыми
    // пользуется вызывающий код `playback_manager.rs`/`visualizer_manager.rs`/
    // `playlist_manager.rs`.

    /// Отправить команду движку (ADR-01); `self.engine == None` — no-op.
    pub(super) fn engine_send(&self, cmd: EngineCmd) -> bool {
        self.engine.as_ref().is_some_and(|engine| engine.send(cmd))
    }

    /// Текущее зеркало состояния движка в UI-потоке (ADR-02, И-Р13, И-Р14).
    pub(super) fn audio(&self) -> &UiAudioState {
        &self.audio_state
    }

    /// Фоновый опрос caps устройства, выбранного в редактируемой панели
    /// показанного режима диалога настроек (`shown_mode`, иначе активный
    /// режим — диалог закрыт), если это даст новую информацию (AM1.0-B38,
    /// §2.3, шаг 9). Вызывается из `on_settings_device` (смена выбора в
    /// списке) и из `open_settings_dialog` (разовый опрос при открытии, если
    /// caps выбранного устройства ещё не известны).
    ///
    /// Пропускает: Совместимый режим (нет аппаратных caps устройства —
    /// только формат микшера сервера, `OsStage`); устройство не выбрано или
    /// не найдено в текущем каталоге движка (`selected_hw_device_for_mode`,
    /// `ui_manager.rs`); устройство держит замок (`LockState::Held`) — то же
    /// исключение, что и у собственной защиты движка
    /// (`engine::run::probe_device_caps`), здесь — чтобы не слать команду,
    /// которую он всё равно отбросит; устройство, чьи caps уже кэшированы
    /// (`UiAudioState::hw_caps`) — повторный опрос ничего не даст. Дебаунс
    /// повторного выбора того же устройства подряд — `last_probed_device`
    /// (не плодит повторные команды в канал движка, даже если первый опрос
    /// ещё не завершился).
    pub(super) fn maybe_probe_selected_device(&mut self) {
        let mode = self
            .core
            .draft()
            .map(|d| d.shown_mode)
            .unwrap_or_else(|| self.core.state().active_mode());
        let device = self.selected_hw_device_for_mode(mode);
        let held = match self.audio().lock() {
            LockState::Held { info } => Some(&info.device),
            _ => None,
        };
        let caps_known = device.is_some_and(|d| self.audio().hw_caps(d).is_some());
        let Some(device) =
            should_probe_device(mode, device, held, caps_known, self.last_probed_device.as_ref())
        else {
            return;
        };
        self.last_probed_device = Some(device.clone());
        self.engine_send(EngineCmd::ProbeDeviceCaps { device });
    }

    /// Открыть трек асинхронно: команда уходит в движок немедленно, сам
    /// `TrackInfo` приходит позже событием `Opened`/`OpenFailed`/`Skipped`
    /// через `on_engine_event` (ADR-01, ADR-02, И-Р14, ТЗ-56).
    pub(super) fn open_track(&mut self, path: &Path) -> Result<(), String> {
        self.req_gen += 1;
        let req_gen = self.req_gen;
        self.audio_state.begin_open(req_gen);
        let cmd = EngineCmd::Open { req_gen, path: Arc::from(path), start_secs: 0.0, autoplay: false };
        if self.engine_send(cmd) {
            Ok(())
        } else {
            Err(music_player_rs::i18n::messages::ENGINE_NOT_RUNNING_SHORT.to_string())
        }
    }

    /// Разобрать событие движка, обновить состояние/бейдж/кэш отчёта окна
    /// «Путь сигнала» и вернуть то, что не свелось к полю состояния —
    /// включая реакции на типизированные ошибки открытия (ADR-02, ADR-14).
    pub(super) fn on_engine_event(&mut self, ev: EngineEvent) -> Applied {
        apply_engine_event(&mut self.audio_state, &mut self.badge_view, &mut self.path_cache, ev)
    }

    /// Применить вид замка устройства к кнопке-значку top_panel:
    /// `lock-kind`/`lock-text`/`lock-hint` из `lock_view` (ADR-06, §6.19,
    /// ТЗ-44, ТЗ-46, ТЗ-118…ТЗ-123, ТЗ-144, AM1.0-B14 шаг 6). Вызывается из
    /// `drain_engine_events` на каждый `EngineEvent::Lock`. Клик по кнопке
    /// выбирает `lock-close`/`lock-open` сам по `lock-kind` (Slint,
    /// `ui/top_panel.slint::LockButton`); набор кнопок окна ошибки по
    /// причине — отдельная функция `lock_failure_actions`
    /// (`show_lock_failure_message` ниже), включая «Настройки аудио» для
    /// «устройство не выбрано» (ADR-24, ТЗ-144).
    pub(super) fn apply_lock_view(&self, state: &LockState) {
        let Some(ui) = self.try_ui() else { return };
        let view = lock_view(state);
        ui.set_lock_kind(view.kind);
        ui.set_lock_text(view.text.into());
        ui.set_lock_hint(view.hint.into());
    }

    /// Окно ошибки замка по переходу `prev -> next` (ТЗ-46, ТЗ-119, ТЗ-122,
    /// ТЗ-126, ТЗ-144, AM1.0-B14 шаг 5): `lock_failure_transition` решает
    /// Show/Replace/Dismiss/None, этот метод выполняет решение через
    /// `MessageCenter`. Вызывается из `drain_engine_events` на каждый
    /// принятый `EngineEvent::Lock`, рядом с `apply_lock_view`; `prev` —
    /// состояние замка до этого события (`self.audio_state.lock()` в точке
    /// вызова, пока `on_engine_event` его не перезаписал).
    pub(super) fn apply_lock_failure_message(&mut self, prev: &LockState, next: &LockState) {
        match lock_failure_transition(prev, next) {
            LockFailureEffect::Show(reason) => self.show_lock_failure_message(&reason),
            LockFailureEffect::Replace(reason) => {
                self.dismiss_lock_failure_message();
                self.show_lock_failure_message(&reason);
            }
            LockFailureEffect::Dismiss => self.dismiss_lock_failure_message(),
            LockFailureEffect::None => {}
        }
    }

    /// Поставить окно ошибки замка для причины `reason` и запомнить его в
    /// `lock_failure_msg` (AM1.0-B14 шаг 5): заголовок/текст — те же строки,
    /// что подсказка самого замка (`music_player_rs::i18n::errors::
    /// capture_failure_label`/`capture_failure_hint`, §6.19, ТЗ-122 — с
    /// именем владельца, если он известен), набор кнопок —
    /// `lock_failure_actions` (ТЗ-46, ТЗ-144).
    fn show_lock_failure_message(&mut self, reason: &CaptureFailure) {
        let message = Message {
            level: MessageLevel::Error,
            title: music_player_rs::i18n::errors::capture_failure_label(reason).into(),
            body: music_player_rs::i18n::errors::capture_failure_hint(reason).into(),
            actions: lock_failure_actions(reason),
        };
        self.lock_failure_msg = Some(message.clone());
        self.push_message(message);
    }

    /// Снять окно ошибки замка, если оно ещё отслеживается — показано или
    /// стоит в очереди (AM1.0-B14 шаг 5, см. `MessageCenter::retract`).
    fn dismiss_lock_failure_message(&mut self) {
        if let Some(message) = self.lock_failure_msg.take() {
            let effect = self.messages.retract(&message);
            self.apply_msg_effect(effect);
        }
    }

    /// Истина только для Строгого режима (ADR-04, ТЗ-52, §6.18): Строгий
    /// всегда bit-perfect, Совместимый и Оптимальный без фиксации громкости —
    /// нет; считается напрямую из активного режима `AppCore` (адаптер
    /// `legacy_path` после С6 обслуживает только Совместимый режим).
    pub(super) fn bit_perfect(&self) -> bool {
        self.core.state().active_mode() == ModeKind::Strict
    }

    /// Переключить активный режим движка: вид бейджа/«Теста» и инвалидация
    /// отчёта «Путь сигнала» обновляются и команда `EngineCmd::SetActiveMode`
    /// уходит движку (ТЗ-20, §6.18, ADR-02). Вызывается только из
    /// `select_active_mode`, которая уже отфильтровала режим, равный
    /// текущему (`self.core.state().active_mode()`), поэтому здесь действие
    /// безусловно; `BadgeViewState::set_mode` дополнительно гасит свой
    /// собственный no-op.
    pub(super) fn engine_set_active_mode(&mut self, mode: ModeKind) {
        self.badge_view.set_mode(mode);
        self.path_cache.invalidate();
        self.engine_send(EngineCmd::SetActiveMode(mode));
    }

    /// Переключить воспроизведение/паузу (ADR-01, ADR-02): играет, если
    /// транспорт был на паузе или остановлен, иначе ставит на паузу.
    pub(super) fn toggle_playback(&mut self) {
        if self.audio().is_playing() {
            self.engine_send(EngineCmd::Pause);
        } else {
            // Возобновление из паузы — тоже запуск воспроизведения, которому
            // замок открытого устройства (Оптимальный/Строгий режим)
            // запрещает начаться (ТЗ-125, §6.19).
            if self.lock_blocks_play() {
                return;
            }
            self.engine_send(EngineCmd::Play);
        }
    }

    /// Снять флаг завершения трека (ADR-02, И-Р13).
    pub(super) fn clear_end(&mut self) {
        self.audio_state.clear_end();
    }

    /// Снять принятый `TrackInfo` открытого трека, если движок его доставил
    /// с прошлого чтения (ADR-02, И-Р13).
    pub(super) fn take_opened(&mut self) -> Option<Arc<TrackInfo>> {
        self.audio_state.take_opened()
    }

    /// Снять вид бейджа тракта и кнопки «Тест», если он устарел с прошлого
    /// чтения (ADR-02, И-Р13, ТЗ-52, ТЗ-61, ТЗ-62).
    fn take_badge_view(&mut self) -> Option<(BadgeView, TestView)> {
        self.badge_view.take_view()
    }

    /// Текущий отчёт окна «Путь сигнала», построенный при необходимости
    /// (ADR-02, ТЗ-79, §7.2).
    pub(super) fn path_report(&mut self) -> &PathReport {
        self.path_cache.get(&self.audio_state, &self.core.settings().playback)
    }

    /// Снять отчёт окна «Путь сигнала», если он устарел с прошлого чтения
    /// (ADR-02, ТЗ-79, ТЗ-83, §7.2).
    pub(super) fn take_report_dirty(&mut self) -> Option<&PathReport> {
        self.path_cache.take_dirty(&self.audio_state, &self.core.settings().playback)
    }

    /// Первый показ окна: разблокировать очередь `MessageCenter` (И-Р9,
    /// ТЗ-52 п. 1 «после первого показа») — вызывается из `main` сразу после
    /// `ui.show()`.
    pub(crate) fn window_shown(&mut self) {
        // Следующее показание геометрии — эхо повторного применения после
        // `show()`, источник — программа (ОВС-5 а, ТЗ-11, §6.17).
        self.core.window_shown();
        let effect = self.messages.window_shown();
        self.apply_msg_effect(effect);
    }

    /// Подключает модуль ввода колёсика к показанному окну — единственный
    /// фильтр событий окна (ADR-8, §6.11, ТЗ-36). Вызывается из `main` один
    /// раз после `ui.show()`. Исход `DefaultRuleOnly` в журнал записывает
    /// вызывающий код.
    pub fn attach_wheel_input(&mut self, window: &slint::Window) -> AttachOutcome {
        self.core.input_mut().attach(window)
    }

    pub fn init(this: &Rc<RefCell<Self>>) {
        {
            let mut app = this.borrow_mut();
            let themes_dir = app.paths.dir.join("themes");
            let theme_name = app.core.settings().theme.as_str().to_string();
            let (theme, was_fallback) = match resolve_startup_theme(&theme_name, &themes_dir) {
                Some((theme, was_fallback)) => (Some(theme), was_fallback),
                None => (None, true),
            };
            if let Some(theme) = &theme {
                app.apply_theme(theme);
            }
            // T1.0 §6.1: падение загрузки темы → Light визуально для текущего
            // запуска + окно Warning (данные не под угрозой, но явное
            // намерение пользователя — выбранная тема — не выполнено;
            // ТЗ-52, ОВ-7). Значение `theme` в settings.toml НЕ
            // перезаписывается (resolve_startup_theme чистая, намерение
            // пользователя сохраняется до исправления TOML).
            if was_fallback {
                eprintln!(
                    "[theme] startup: тема \"{theme_name}\" не загружена, применена светлая"
                );
                app.push_message(Message {
                    level: MessageLevel::Warning,
                    title: music_player_rs::i18n::messages::THEME_NOT_LOADED_TITLE.into(),
                    body: music_player_rs::i18n::messages::theme_not_loaded_body(&theme_name).into(),
                    actions: vec![MessageAction::Ok],
                });
            }
            app.sync_settings_to_ui();
            app.sync_playlist_to_ui();
            // Pre-warm the output-device enumeration so the settings dialog's
            // ComboBox is already populated (with the active device selected)
            // by the time it is first opened.
            app.sync_audio_devices();
        }
        Self::bind_callbacks(this);
    }

    /// Черновик диалога настроек, если он открыт, иначе применённые
    /// настройки ядра (§6.9, SP1.0 ADR-11). Только для построения и
    /// синхронизации самого диалога настроек: коду вне диалога, которому
    /// нужны действующие значения, брать `self.core.settings()` (SP1.0-B20).
    fn dialog_cfg(&self) -> &PersistSettings {
        self.core.draft().map_or(self.core.settings(), |d| &d.settings)
    }

    /// Тип визуализации с учётом черновика диалога (Т-1, §6.9, SP1.0 ADR-11)
    /// — только для диалога настроек; действующий тип снаружи —
    /// `self.core.state().viz_mode()` (SP1.0-B20).
    fn dialog_viz_mode(&self) -> VisualizationMode {
        self.core.draft().map_or(self.core.state().viz_mode(), |d| d.viz_mode)
    }

    /// Ширины колонок для списка колонок диалога (Т-2, §6.9): черновик ширин
    /// не хранит, только признак `reset_widths`. Пока он взведён — пустой
    /// набор (ширины по приоритетам из описаний колонок), иначе действующие
    /// ширины состояния сессии.
    fn cfg_column_widths(&self) -> &BTreeMap<ColumnId, WidthPct> {
        match self.core.draft() {
            Some(d) if d.reset_widths => &EMPTY_WIDTHS,
            _ => self.core.state().column_widths(),
        }
    }

    /// Правка черновика диалога в ядре; при закрытом диалоге — ничего:
    /// настройки меняются только через «Сохранить» (§6.9, ADR-11, ТЗ-25).
    /// После каждой правки пересчитывается кнопка «Сохранить» (ТЗ-27).
    fn edit_draft(&mut self, f: impl FnOnce(&mut DialogDraft)) {
        self.core.dialog_edit(f);
        self.sync_save_button();
    }

    /// Синхронизирует кнопку «Сохранить» диалога с черновиком (ТЗ-27, §6.9,
    /// ADR-11): доступность и подсказку считает `save_button_view` по ответу
    /// ядра `save_button` и теме черновика — это единственный источник
    /// доступности кнопки. Вызывается после открытия диалога, каждой правки
    /// черновика и принятого черновиком ответа `apap-io` по теме.
    fn sync_save_button(&self) {
        let (enabled, tooltip) =
            save_button_view(self.core.save_button(), self.core.draft().map(|d| &d.theme));
        let Some(ui) = self.try_ui() else { return };
        ui.set_settings_save_enabled(enabled);
        ui.set_settings_save_tooltip(tooltip.into());
    }

    /// Правка настроек черновика; при закрытом диалоге — ничего (§6.9, ADR-11).
    fn edit_cfg(&mut self, f: impl FnOnce(&mut PersistSettings)) {
        self.edit_draft(|d| f(&mut d.settings));
    }

    /// Открыт ли диалог настроек, то есть существует ли черновик (§6.9, ADR-11).
    fn dialog_is_open(&self) -> bool {
        self.core.draft().is_some()
    }

    /// Правка типа визуализации в черновике; при закрытом диалоге — ничего
    /// (§6.9, ADR-11).
    fn edit_cfg_viz_mode(&mut self, mode: VisualizationMode) {
        self.edit_draft(|d| d.viz_mode = mode);
    }

    /// Открыть диалог настроек в ядре (§6.9, ADR-11, ADR-12, ТЗ-22, ТЗ-23):
    /// ядро проверяет шлюз, создаёт черновик, взводит блокировку `Dialog` и
    /// ставит один подсчёт размера кэшей; его поколение запоминается для
    /// сверки ответа `IoDone::CacheSizes`. `false` — диалог не открыт
    /// (шлюз не пропустил команду или черновик уже есть). У открытого диалога
    /// кнопка «Сохранить» приводится к свежему черновику (ТЗ-27).
    fn open_dialog_draft(&mut self) -> bool {
        let Some(gen) = self.core.dialog_open() else { return false };
        self.io_gens.sizes = gen;
        self.sync_save_button();
        true
    }

    /// Построить строки панели настроек режима из черновика диалога и
    /// отправить их в UI вместе с выбранным режимом (§6.18, §6.27, ТЗ-41,
    /// ТЗ-94, ОВС-14); очищает сообщение валидации. Нет открытого диалога —
    /// ничего не делает.
    ///
    /// Строки обновляются на месте в `self.mode_rows` (`set_row_data` только
    /// для изменившихся индексов), а не пересборкой `ModelRc` целиком: число
    /// и порядок строк не зависят от режима (`build_rows` зеркалит `PARAMS`
    /// без фильтрации, §2.2), поэтому индекс всегда соответствует одному
    /// `ParamId`. Это сохраняет виджет `Slider` ползунка буфера живым во
    /// время перетаскивания — пересборка модели посреди `changed` пересоздаёт
    /// его в репитере `ui/mode_params.slint` и рвёт drag (AM1.0-B33, SP1.0
    /// ADR-11).
    fn sync_mode_dialog_to_ui(&mut self) {
        let Some(draft) = self.core.draft() else { return };
        let edit_mode = draft.shown_mode;
        // `self.core.settings().playback` — сохранённые настройки (не
        // черновик): `build_rows`/`row_buffer` сравнивают их с черновиком,
        // чтобы показать «Поднят автоматически до N мс» только когда
        // границу буфера плеера подняла сама правка «Буфера устройства»
        // (ТЗ-131, §6.27, AM1.0-B35, step 7).
        let rows: Vec<ModeParamRow> =
            mode_dialog::build_rows(edit_mode, &draft.settings.playback, &self.core.settings().playback)
                .iter()
                .map(mode_dialog::to_param_row)
                .collect();
        if self.mode_rows.row_count() == rows.len() {
            for (i, row) in rows.into_iter().enumerate() {
                let changed = self.mode_rows.row_data(i).is_none_or(|old| mode_param_row_changed(&old, &row));
                if changed {
                    self.mode_rows.set_row_data(i, row);
                }
            }
        } else {
            // Первое заполнение (модель пуста) либо число строк разошлось с
            // `PARAMS` — полная пересборка как страховка.
            self.mode_rows.set_vec(rows);
        }
        let Some(ui) = self.try_ui() else { return };
        ui.set_settings_mode_edit_mode(match edit_mode {
            ModeKind::Compatible => 0,
            ModeKind::Optimal => 1,
            ModeKind::Strict => 2,
        });
        ui.set_settings_mode_validation(SharedString::new());
    }

    /// Применить правку одной строки панели настроек режима к черновику
    /// (§6.18, §6.27, ТЗ-41, ТЗ-94). `param` — индекс `ParamId` по таблице
    /// `PARAMS` (см. `mode_dialog::param_at`); неизвестный индекс — игнор.
    fn apply_mode_dialog_edit(&mut self, param: i32, value: mode_dialog::EditValue) {
        let Some(id) = mode_dialog::param_at(param) else { return };
        // Результат правки выносится из замыкания `dialog_edit`; `None` —
        // черновика нет, диалог закрыт.
        let mut result = None;
        self.edit_draft(|d| {
            let shown_mode = d.shown_mode;
            result = Some(mode_dialog::apply_edit(&mut d.settings.playback, shown_mode, id, value));
        });
        let Some(result) = result else { return };
        match result {
            Ok(()) => self.sync_mode_dialog_to_ui(),
            Err(mode_dialog::EditRejected::BuffersNotAllowed(blocker)) => {
                self.sync_mode_dialog_to_ui();
                if let Some(ui) = self.try_ui() {
                    ui.set_settings_mode_validation(SharedString::from(blocker.reason()));
                }
            }
            Err(_) => {}
        }
    }

    /// Переносит вид окна «Путь сигнала» в Slint-свойства (§3.6, ТЗ-71,
    /// ТЗ-81, ТЗ-84): шесть этапов, рекомендации по трём группам, подвал и
    /// текст копирования. Действия кнопок рекомендаций записываются в
    /// `self.path_actions` по порядку показа — `action-id` элемента в
    /// `PathRecItem` — это индекс в нём (ТЗ-81), `on_path_rec_action` ищет
    /// по нему исходное действие.
    fn push_path_view_to_ui(&mut self, view: path_view::PathView) {
        let Some(ui) = self.try_ui() else { return };
        ui.set_path_title(view.title.into());

        let stages: Vec<PathStageRow> = view
            .stages
            .iter()
            .map(|s| PathStageRow {
                name: s.name.clone().into(),
                status: s.status.clone().into(),
                changes: s.changes.clone().into(),
                causes: s.causes.clone().into(),
                confidence: s.confidence.clone().into(),
                notes: s.notes.clone().into(),
            })
            .collect();
        ui.set_path_stages(ModelRc::from(stages.as_slice()));

        self.path_actions.clear();
        let player_items = path_rec_items(view.player, &mut self.path_actions);
        ui.set_path_recs_player(ModelRc::from(player_items.as_slice()));
        let os_items = path_rec_items(view.os, &mut self.path_actions);
        ui.set_path_recs_os(ModelRc::from(os_items.as_slice()));
        let hardware_items = path_rec_items(view.hardware, &mut self.path_actions);
        ui.set_path_recs_hardware(ModelRc::from(hardware_items.as_slice()));

        ui.set_path_footer(view.footer.into());
        ui.set_path_copy_text(view.copy_text.into());
    }

    /// Открывает диалог настроек (§6.9, ADR-11, ADR-12, ТЗ-22, ТЗ-23): общий
    /// путь кнопки «Настройки» в top_panel и кнопки «Выбрать устройство»
    /// замка (ADR-24, ТЗ-144). `false` — диалог не открыт (шлюз не пропустил
    /// команду или черновик уже есть).
    fn open_settings_dialog(&mut self) -> bool {
        if !self.open_dialog_draft() {
            return false;
        }
        // T1.0 §6.2: свежий диалог (черновик стартует с
        // `ThemeLoad::Unchanged`) → пересборка списка/метаданных из
        // themes/*.toml.
        self.populate_theme_ui();
        // Re-push every dialog field from the real settings: a reopened
        // dialog must show current values, never a stale un-applied draft.
        self.sync_settings_to_ui();
        self.sync_viz_settings_to_ui();
        self.sync_audio_devices();
        self.sync_cover_settings_to_ui();
        self.sync_mode_dialog_to_ui();
        // До ответа на подсчёт размеров кэшей показываем последние
        // известные значения (ТЗ-22, ADR-20).
        let cache_disk = self.cache_disk;
        self.apply_cache_sizes(&cache_disk);
        if let Some(ui) = self.try_ui() {
            ui.set_settings_open(true);
        }
        self.sync_gate_ui();
        // AM1.0-B38, шаг 9: разовый опрос caps устройства, уже выбранного в
        // показанной панели режима, если они ещё не известны (иначе блок
        // «Аудио» показывал бы NOT_CACHED до первого изменения выбора).
        self.maybe_probe_selected_device();
        true
    }

    /// Переход из окна «Путь сигнала» в диалог настроек (ТЗ-81, ТЗ-84): тот же
    /// путь, что у кнопки «Настройки» (`open_settings_dialog`: шлюз, черновик,
    /// поля диалога, размеры кэшей — ТЗ-22, ТЗ-23, ADR-11), затем окно тракта
    /// закрывается (SP1.0-B9).
    fn open_settings_from_path_window(&mut self) {
        if !self.open_settings_dialog() {
            return;
        }
        if let Some(ui) = self.try_ui() {
            ui.set_bp_report_open(false);
        }
    }

    // ---------------- Тема: состояние диалога настроек (T1.0) ----------------

    /// Применяет метаданные темы к UI диалога (§5.2/§5.3). `None` — тема не
    /// найдена в `themes/`; `name` пуст — файл есть, но не читается/невалиден.
    /// Только показ под ComboBox: на кнопку «Сохранить» не влияет (ТЗ-27).
    fn set_theme_meta(&mut self, meta: Option<ThemeMeta>) {
        let (name, desc) = match &meta {
            Some(m) if !m.name.is_empty() => (
                m.name.clone(),
                m.description.clone().unwrap_or_default(),
            ),
            Some(_) => (
                music_player_rs::i18n::messages::THEME_META_UNREADABLE.to_string(),
                String::new(),
            ),
            None => (music_player_rs::i18n::messages::THEME_NOT_FOUND.to_string(), String::new()),
        };
        if let Some(ui) = self.try_ui() {
            ui.set_theme_name(name.into());
            ui.set_theme_description(desc.into());
        }
    }

    /// Запрашивает список тем при открытии диалога через поток `apap-io`
    /// (ТЗ-22, ADR-20, §6.2): без обхода каталога `themes/` в UI-потоке.
    /// Список и метаданные текущей темы применяются
    /// `apply_theme_list`/`apply_theme_read` в `drain_io`, когда ответ
    /// приходит; «Сохранить» этот запрос не блокирует — черновик он не
    /// меняет (ТЗ-27).
    fn populate_theme_ui(&mut self) {
        self.io_gens.themes = self.core.next_io_gen();
        self.core.io().submit(IoJob::ListThemes { gen: self.io_gens.themes });
    }

    /// Применяет ответ `IoJob::ListThemes` (ТЗ-22, ADR-20, §6.2): заполняет
    /// список тем и текущую тему в UI. Текущая тема может быть удалена (не
    /// найдена в списке → §5.1) — метаданные не запрашиваются, иначе читается
    /// её файл через `IoJob::ReadTheme` (ответ разбирает `apply_theme_read`).
    fn apply_theme_list(&mut self, entries: Vec<ThemeEntry>) {
        let names: Vec<SharedString> = entries.iter().map(|e| e.file_stem.clone().into()).collect();
        let current = self.core.settings().theme.as_str().to_string();
        // Индекс current в entries, для подсветки попапа ComboBox (§5.1,
        // T1.0-B4); -1, если тема удалена — ComboBox ничего не подсвечивает.
        let current_index = entries
            .iter()
            .position(|e| e.file_stem == current)
            .map(|i| i as i32)
            .unwrap_or(-1);
        if let Some(ui) = self.try_ui() {
            // Индекс ставится раньше модели: `changed model` в ComboBox
            // (std-widgets) делает reset-current(), который клэмпит уже
            // верный current-index, а не обнуляет его (как в
            // sync_audio_devices, ui_manager.rs).
            ui.set_theme_current_index(current_index);
            ui.set_theme_list_model(ModelRc::from(names.as_slice()));
            ui.set_theme_current(current.clone().into());
        }
        let found = current_index >= 0;
        match found.then(|| ThemeName::new(current.clone())).flatten() {
            Some(name) => self.request_current_theme_meta(name),
            None => self.set_theme_meta(None),
        }
    }

    /// Запрашивает у потока `apap-io` файл действующей темы только ради
    /// показа её метаданных под ComboBox (ТЗ-22, ADR-20, §6.2): поколение
    /// берётся из общего счётчика ядра и запоминается в `io_gens.theme`,
    /// черновик не меняется, поэтому «Сохранить» это чтение не блокирует
    /// (ТЗ-27): действующая тема сохранением не применяется.
    fn request_current_theme_meta(&mut self, name: ThemeName) {
        let gen = self.core.next_io_gen();
        self.io_gens.theme = gen;
        self.core.io().submit(IoJob::ReadTheme { gen, name });
    }

    /// Метаданные прочитанной темы (§5.2/§5.3): имя и описание для показа
    /// под ComboBox. HEX-цвета темы черновика проверяет `save_button_view`
    /// (ТЗ-27).
    fn theme_meta_read(data: &ThemeData) -> ThemeMeta {
        ThemeMeta {
            name: data.name.clone(),
            description: data.description.clone(),
        }
    }

    /// Метаданные темы, файл которой есть, но не читается/невалиден (§5.2):
    /// пустое имя — под ComboBox показывается «не удалось загрузить».
    fn theme_meta_broken() -> ThemeMeta {
        ThemeMeta {
            name: String::new(),
            description: None,
        }
    }

    /// Показывает под ComboBox метаданные темы, выбранной в черновике
    /// (§2.10, §5.2, ТЗ-22, ТЗ-27): `ThemeLoad::Ready` — метаданные
    /// прочитанной темы, `ThemeLoad::Invalid` — «не удалось загрузить».
    /// В остальных состояниях черновика (и без него) ничего не меняет.
    fn show_draft_theme_meta(&mut self) {
        let meta = match self.core.draft().map(|d| &d.theme) {
            Some(ThemeLoad::Ready(data)) => Self::theme_meta_read(data),
            Some(ThemeLoad::Invalid { .. }) => Self::theme_meta_broken(),
            _ => return,
        };
        self.set_theme_meta(Some(meta));
    }

    /// Применяет ответ `IoJob::ReadTheme` (ТЗ-22, ТЗ-27, ТЗ-29, ADR-20,
    /// §5.2/§6.2). Если черновик ждёт тему именно этого поколения
    /// (`ThemeLoad::Loading { gen }`) — это ответ на выбор в ComboBox: данные
    /// уходят в черновик ядра (`dialog_theme_loaded`), и «Сохранить» применит
    /// их без повторного чтения файла; под ComboBox показываются метаданные
    /// по состоянию черновика, кнопка «Сохранить» пересчитывается
    /// (`sync_save_button`). Иначе, если поколение совпадает с
    /// `io_gens.theme`, — это чтение действующей темы для показа: `Ok` —
    /// её метаданные, `Err` — файл есть, но не читается/невалиден; черновик
    /// не меняется, и на «Сохранить» это не влияет.
    /// Любой другой ответ устарел и отбрасывается.
    fn apply_theme_read(&mut self, gen: u64, result: Result<Arc<ThemeData>, Box<str>>) {
        let awaited = self.core.draft().is_some_and(|d| d.theme == ThemeLoad::Loading { gen });
        if awaited {
            if self.core.dialog_theme_loaded(gen, result) {
                self.show_draft_theme_meta();
                self.sync_save_button();
            }
            return;
        }
        if gen != self.io_gens.theme {
            return;
        }
        let meta = match &result {
            Ok(data) => Self::theme_meta_read(data),
            Err(_) => Self::theme_meta_broken(),
        };
        self.set_theme_meta(Some(meta));
    }

    /// Обработчик выбора темы в ComboBox (§5.4/§6.3, ТЗ-22, ADR-11, ADR-20):
    /// без доступа к диску — имя темы пишется в черновик ядра, которое само
    /// переводит его в `ThemeLoad::Loading` и отправляет `IoJob::ReadTheme`
    /// потоку `apap-io`; «Сохранить» неактивна до ответа (черновик в
    /// `Loading` недопустим для ядра, кнопку обновляет `edit_draft`, ТЗ-27),
    /// а прежнее чтение метаданных действующей темы объявляется устаревшим. Возврат к
    /// действующей теме (`ThemeLoad::Unchanged`) ядро не читает — её
    /// метаданные запрашиваются для показа. Невалидное имя черновик не
    /// трогает.
    fn on_settings_theme_selected(&mut self, name: &str) {
        let Some(theme_name) = ThemeName::new(name) else {
            self.set_theme_meta(Some(Self::theme_meta_broken()));
            return;
        };
        self.edit_cfg(|s| s.theme = theme_name.clone());
        let (loading, unchanged) = match self.core.draft().map(|d| &d.theme) {
            Some(theme) => (
                matches!(theme, ThemeLoad::Loading { .. }),
                matches!(theme, ThemeLoad::Unchanged),
            ),
            None => return,
        };
        if loading {
            // Запоздавший ответ на чтение метаданных действующей темы не
            // должен перезаписать UI выбранной темы (ADR-20).
            self.io_gens.theme = self.core.next_io_gen();
        } else if unchanged {
            self.request_current_theme_meta(theme_name);
        } else {
            // Повторный выбор уже прочитанной темы: ядро ничего не читает,
            // метаданные берутся из черновика.
            self.show_draft_theme_meta();
        }
    }

    /// Table header click (ТЗ-42, §6.13): UI reports only which column was
    /// clicked, not a direction — the Asc -> Desc -> unsorted cycle (and the
    /// NowPlaying no-op) lives entirely in `core.playlist_header_click` via
    /// `sort_tracks`. `sort-ascending`/`sort-descending` from the Slint fork
    /// therefore resolve to the same handling here.
    fn handle_header_click(app: &Rc<RefCell<Self>>, col_idx: i32) {
        eprintln!("[gui] header_click col={col_idx}");
        let col = {
            let a = app.borrow();
            visible_col_at_index(&a.core.settings().columns, col_idx)
        };
        if let Some(col) = col {
            let mut a = app.borrow_mut();
            if !a.core.gate().allows(MainCmd::SortBy(col)) {
                return;
            }
            a.sort_tracks(col);
        }
    }

    fn bind_callbacks(this: &Rc<RefCell<Self>>) {
        let Some(ui) = this.borrow().try_ui() else { return };

        // 1. play-pause
        {
            let app = this.clone();
            ui.on_play_pause(move || {
                eprintln!("[gui] play_pause");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::PlayPause) {
                    return;
                }
                if a.play_via_open_if_stopped() {
                    return;
                }
                a.toggle_playback();
                if a.audio().is_playing() {
                    a.emit(AppEvent::PlaybackStarted);
                } else {
                    a.emit(AppEvent::PlaybackPaused);
                }
            });
        }

        // 2. stop
        {
            let app = this.clone();
            ui.on_stop(move || {
                eprintln!("[gui] stop");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::Stop) {
                    return;
                }
                a.engine_send(EngineCmd::Stop);
                a.emit(AppEvent::PlaybackStopped);
            });
        }

        // 3. prev-track
        {
            let app = this.clone();
            ui.on_prev_track(move || {
                eprintln!("[gui] prev_track");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::Prev) {
                    return;
                }
                a.play_prev();
            });
        }

        // 4. next-track
        {
            let app = this.clone();
            ui.on_next_track(move || {
                eprintln!("[gui] next_track");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::Next) {
                    return;
                }
                a.play_next(1);
            });
        }

        // 5. toggle-repeat
        {
            let app = this.clone();
            ui.on_toggle_repeat(move || {
                eprintln!("[gui] toggle_repeat");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::Repeat) {
                    return;
                }
                a.cycle_repeat();
            });
        }

        // 6. toggle-shuffle
        {
            let app = this.clone();
            ui.on_toggle_shuffle(move || {
                eprintln!("[gui] toggle_shuffle");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::Shuffle) {
                    return;
                }
                a.shuffle = !a.shuffle;
                let shuffle = a.shuffle;
                a.core.change_state(Origin::User, StateChange::Shuffle(shuffle));
                if let Some(ui) = a.try_ui() {
                    ui.set_shuffle(a.shuffle);
                }
            });
        }

        // 7. seek
        {
            let app = this.clone();
            ui.on_seek(move |fraction| {
                eprintln!("[gui] seek fraction={fraction:.3}");
                if !app.borrow().core.gate().allows(MainCmd::Seek) {
                    return;
                }
                let duration = app.borrow().audio().duration().unwrap_or(0.0);
                app.borrow_mut().engine_send(EngineCmd::Seek { secs: fraction as f64 * duration });
            });
        }

        // 7b. seek-commit — сикбар отпущен (конец драга): один seek в точку.
        {
            let app = this.clone();
            ui.on_seek_commit(move |fraction| {
                eprintln!("[gui] seek_commit fraction={fraction:.3}");
                if !app.borrow().core.gate().allows(MainCmd::Seek) {
                    return;
                }
                let duration = app.borrow().audio().duration().unwrap_or(0.0);
                app.borrow_mut().engine_send(EngineCmd::Seek { secs: fraction as f64 * duration });
            });
        }

        // 8. volume-changed
        {
            let app = this.clone();
            ui.on_volume_changed(move |volume| {
                eprintln!("[gui] volume_changed volume={volume:.3}");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::Volume) {
                    return;
                }
                // Ползунок — один из источников единой команды громкости
                // (ADR-14, ТЗ-37): доступность, запись состояния и команду
                // движку выполняет `apply_volume_cmd`.
                let pct = LegacyPlaybackState::pct_from_gain(volume);
                let Some(v) = Volume::new(pct) else {
                    return;
                };
                a.apply_volume_cmd(VolumeSource::Slider, VolumeCmd::Set(v));
            });
        }

        // 8a. volume-scroll
        {
            let app = this.clone();
            ui.on_volume_scroll(move |dx, dy| {
                eprintln!("[gui] volume_scroll dx={dx:.1} dy={dy:.1}");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::Volume) {
                    return;
                }
                // Колёсико над ползунком — источник единой команды громкости
                // (ADR-14, ТЗ-37): UI передаёт сырые смещения `scroll-event`,
                // щелчки считает модуль ввода (ADR-8, §6.11, ТЗ-36), шаг
                // 2 % за щелчок — ядро (НФ-2).
                let t = a.core.now();
                let clicks = a.core.input_mut().slider_scroll(SlintScroll { t, dx, dy });
                // Ноль щелчков — накопитель тачпада ещё не набрал шаг:
                // команда громкости не подаётся.
                if clicks.0 != 0 {
                    a.apply_volume_cmd(VolumeSource::SliderWheel, VolumeCmd::Step(clicks));
                }
            });
        }

        // 9. toggle-mute
        {
            let app = this.clone();
            ui.on_toggle_mute(move || {
                eprintln!("[gui] toggle_mute");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::ToggleMute) {
                    return;
                }
                // Mute активного режима меняет ядро с `Origin::User` (ADR-14,
                // ТЗ-38): значение пишется в состояние сессии и взводит срок
                // сохранения. Команду движку до этапа С3 шлёт слой app.
                // Работает и при недоступной громкости (Строгий режим;
                // Оптимальный с фиксацией).
                let muted = a.core.toggle_mute();
                a.engine_send(EngineCmd::SetMuted(muted));
            });
        }

        // 9b. select-mode: переключатель режимов в top_panel (§6.18, ТЗ-18,
        // ТЗ-20, ТЗ-109, ТЗ-110, ADR-23).
        {
            let app = this.clone();
            ui.on_select_mode(move |i| {
                eprintln!("[gui] select_mode i={i}");
                let mut a = app.borrow_mut();
                // Команда главного окна — с разрешения шлюза (ТЗ-23, ADR-12, И-Р8).
                if !a.core.gate().allows(MainCmd::SwitchMode) {
                    return;
                }
                let mode = match i {
                    0 => ModeKind::Compatible,
                    1 => ModeKind::Optimal,
                    2 => ModeKind::Strict,
                    _ => return,
                };
                a.select_active_mode(mode);
            });
        }

        // 9c. lock-close/lock-open: клик по кнопке-значку замка в top_panel
        // (ADR-06, §6.19, ТЗ-44, ТЗ-46, ТЗ-118…ТЗ-123, ТЗ-144, AM1.0-B14 шаг
        // 6) — Slint выбирает, какой из двух коллбэков вызвать, по
        // `lock-kind` (`LockButton` в `ui/top_panel.slint`). Гейт — тот же
        // `MainCmd::SwitchMode`, что и у переключателя режимов: действия
        // замка переводят монопольный тракт между открытым/захваченным
        // состояниями и Совместимым режимом, отдельного варианта шлюза под
        // них нет. «Повторить захват»/«Настройки аудио»/«В Совместимый» для
        // отказа захвата — кнопки окна ошибки (`on_msg_action` ниже,
        // `lock_failure_actions`), не этой кнопки.
        {
            let app = this.clone();
            ui.on_lock_close(move || {
                eprintln!("[gui] lock_close");
                let a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::SwitchMode) {
                    return;
                }
                a.engine_send(EngineCmd::Lock(LockInput::UserClose));
            });
        }
        {
            let app = this.clone();
            ui.on_lock_open(move || {
                eprintln!("[gui] lock_open");
                let a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::SwitchMode) {
                    return;
                }
                a.engine_send(EngineCmd::Lock(LockInput::UserOpen));
            });
        }

        // 10. play-track (double-click detection: single click selects, double-click plays)
        {
            let app = this.clone();
            ui.on_play_track(move |index| {
                eprintln!("[gui] play_track index={index}");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::PlayRow) {
                    return;
                }
                let now = Instant::now();
                let is_double = a.last_click_row == Some(index)
                    && now.duration_since(a.last_click_time).as_millis() < 400;
                a.last_click_row = Some(index);
                a.last_click_time = now;
                if is_double {
                    a.play_track(index as usize);
                }
            });
        }

        // 11-12. sort-ascending / sort-descending: both are the same header
        // click, the UI only tells us which direction it currently shows
        // (Self::handle_header_click owns the actual cycle).
        {
            let app = this.clone();
            ui.on_sort_ascending(move |col_idx| {
                Self::handle_header_click(&app, col_idx);
            });
        }
        {
            let app = this.clone();
            ui.on_sort_descending(move |col_idx| {
                Self::handle_header_click(&app, col_idx);
            });
        }

        // 13. open-settings
        {
            let app = this.clone();
            ui.on_open_settings(move || {
                eprintln!("[gui] open_settings");
                // Проверка шлюза, черновик, блокировка `Dialog` и один подсчёт
                // размера дисковых кэшей потоком `apap-io` — в ядре
                // (ТЗ-22, ТЗ-23, ADR-11, ADR-20).
                app.borrow_mut().open_settings_dialog();
            });
        }

        // 14. add-files
        {
            let app = this.clone();
            ui.on_add_files(move || {
                eprintln!("[gui] add_files");
                if !app.borrow().core.gate().allows(MainCmd::AddFiles) {
                    return;
                }
                MusicApp::pick_async(
                    &app,
                    |start| PickRequest::AddFiles { start },
                    |app, paths| app.borrow_mut().start_scan(paths),
                );
            });
        }

        // 15. add-folder
        {
            let app = this.clone();
            ui.on_add_folder(move || {
                eprintln!("[gui] add_folder");
                if !app.borrow().core.gate().allows(MainCmd::AddFolder) {
                    return;
                }
                MusicApp::pick_async(
                    &app,
                    |start| PickRequest::AddFolder { start },
                    |app, paths| app.borrow_mut().start_scan(paths),
                );
            });
        }

        // 16. save-playlist (ADR-17, ADR-10, §6.18, ТЗ-13 а): диалог
        // «Сохранить как» с расширением по умолчанию `.m3u8` (ОВ-17);
        // выбранный путь уходит в `AppCore::export` одной попыткой —
        // отдельно от рабочего `playlist.m3u` и без влияния на флаг
        // «изменён». Отмена диалога — без эффекта.
        {
            let app = this.clone();
            ui.on_save_playlist(move || {
                eprintln!("[gui] save_playlist");
                if !app.borrow().core.gate().allows(MainCmd::SavePlaylist) {
                    return;
                }
                MusicApp::pick_async(
                    &app,
                    |start| PickRequest::SavePlaylist { start },
                    |app, paths| {
                        let path = paths[0].clone();
                        app.borrow_mut().core.export(path);
                    },
                );
            });
        }

        // 17. load-playlist (ТЗ-13 б, ТЗ-48, ADR-16, §6.12): файл читается и
        // разбирается в фоне потоком `apap-playlist`; до окончания загрузки
        // прежний трек продолжает играть. Итог (замена плейлиста, Play Now
        // или ошибка чтения) разбирает `poll_load_playlist` на тике.
        {
            let app = this.clone();
            ui.on_load_playlist(move || {
                eprintln!("[gui] load_playlist");
                if !app.borrow().core.gate().allows(MainCmd::LoadPlaylist) {
                    return;
                }
                MusicApp::pick_async(
                    &app,
                    |start| PickRequest::OpenPlaylist { start },
                    |app, paths| {
                        let path = paths[0].clone();
                        let mut a = app.borrow_mut();
                        a.core.gate_mut().set_loading(Some(LoadKind::Command));
                        if let Some(ui) = a.try_ui() {
                            ui.set_busy(true);
                        }
                        a.core.start_command_load(path);
                    },
                );
            });
        }

        // 18. settings-close (cancel: discard draft, restore original)
        {
            let app = this.clone();
            ui.on_settings_close(move || {
                eprintln!("[gui] settings_close (Cancel)");
                let mut a = app.borrow_mut();
                // Discard the draft; the dialog itself is recreated from the
                // real settings on the next open, so no field resync is needed.
                // The core drops it and lifts the `Dialog` block (ТЗ-26).
                a.core.dialog_cancel();
                if let Some(ui) = a.try_ui() {
                    ui.set_settings_open(false);
                }
                a.sync_gate_ui();
            });
        }

        // 19. settings-theme-selected (T1.0 §5.4/§6.3): выбор темы в ComboBox
        {
            let app = this.clone();
            ui.on_settings_theme_selected(move |name| {
                eprintln!("[gui] settings_theme_selected: {name}");
                let mut a = app.borrow_mut();
                a.on_settings_theme_selected(&name);
            });
        }

        // 20. settings-cover-size
        {
            let app = this.clone();
            ui.on_settings_cover_size(move |size| {
                eprintln!("[gui] settings_cover_size size={size:.1}");
                let mut a = app.borrow_mut();
                a.edit_cfg(|s| s.top_panel.cover_size = size);
            });
        }

        // 21. settings-col-info-w
        {
            let app = this.clone();
            ui.on_settings_col_info_w(move |width| {
                eprintln!("[gui] settings_col_info_w width={width:.1}");
                let mut a = app.borrow_mut();
                a.edit_cfg(|s| s.top_panel.col_info_w = width);
            });
        }

        // 22. settings-col-gap
        {
            let app = this.clone();
            ui.on_settings_col_gap(move |gap| {
                eprintln!("[gui] settings_col_gap gap={gap:.1}");
                let mut a = app.borrow_mut();
                a.edit_cfg(|s| s.top_panel.col_gap = gap);
            });
        }

        // 23. settings-toggle-minimize
        {
            let app = this.clone();
            ui.on_settings_toggle_minimize(move |enabled| {
                eprintln!("[gui] settings_toggle_minimize enabled={enabled}");
                let mut a = app.borrow_mut();
                a.edit_cfg(|s| s.minimize_to_tray = enabled);
            });
        }

        // 23a. settings-save-interval — параметр N (ТЗ-33), сохраняется по «Сохранить»
        {
            let app = this.clone();
            ui.on_settings_save_interval(move |idx| {
                let n = usize::try_from(idx).ok().and_then(|i| ui_manager::SAVE_INTERVALS.get(i).copied());
                if let Some(n) = n {
                    app.borrow_mut().edit_cfg(|s| s.save_interval = n);
                }
            });
        }

        // 24. settings-device
        {
            let app = this.clone();
            ui.on_settings_device(move |name| {
                eprintln!("[gui] settings_device name={name:?}");
                // Edit-Commit: store the choice in the dialog draft; the real
                // device switch (stream restart, probe, save) happens only when
                // the draft is applied on "Save".
                //
                // The ComboBox hands back the *label* (deduplicated, grouped,
                // possibly suffixed for server nodes); the settings persist the
                // stable device id it maps to.
                let mut a = app.borrow_mut();
                let raw = a.resolve_device_label(&name);
                // Записывается в `ModeSettings` показанной в диалоге панели
                // режима (`shown_mode`), а не в прежнее легаси-поле устройства
                // (ТЗ-94, §2.4) — `edit_cfg` не подходит, так как даёт доступ
                // только к `PersistSettings` черновика, без `shown_mode`.
                a.edit_draft(|d| {
                    let mode = d.shown_mode;
                    d.settings.playback.set_device(mode, &raw);
                });
                // AM1.0-B38, шаг 9: выбор устройства в Оптимальном/Строгом
                // запускает фоновый опрос caps, если они ещё не известны и
                // устройство не держит замок (`maybe_probe_selected_device`).
                a.maybe_probe_selected_device();
            });
        }

        // 25. settings-refresh-devices
        {
            let app = this.clone();
            ui.on_settings_refresh_devices(move || {
                eprintln!("[gui] settings_refresh_devices");
                app.borrow_mut().sync_audio_devices();
            });
        }

        // 25f-b. bit-perfect badge -> report dialog
        {
            let app = this.clone();
            ui.on_bp_clicked(move || {
                let mut a = app.borrow_mut();
                if let Some(ui) = a.try_ui() {
                    ui.set_bp_report_open(true);
                }
                let view = path_view::path_view(a.path_report());
                a.push_path_view_to_ui(view);
            });
        }

        {
            let app = this.clone();
            ui.on_bp_report_close(move || {
                if let Some(ui) = app.borrow().try_ui() {
                    ui.set_bp_report_open(false);
                }
            });
        }

        {
            let app = this.clone();
            ui.on_bp_report_open_settings(move || {
                app.borrow_mut().open_settings_from_path_window();
            });
        }

        // 25f-c. окно «Путь сигнала»: действие кнопки рекомендации (ТЗ-81,
        // ТЗ-84). Быстрые исправления громкости/mute применяются напрямую;
        // переход к настройкам повторяет `on_bp_report_open_settings` —
        // отдельного выбора вкладки в диалоге пока нет (ТЗ-81).
        {
            let app = this.clone();
            ui.on_path_rec_action(move |id| {
                let mut a = app.borrow_mut();
                let Ok(idx) = usize::try_from(id) else { return };
                let Some(action) = a.path_actions.get(idx).copied() else { return };
                match action {
                    path_view::RecViewAction::QuickFix(path_view::QuickFix::Volume) => {
                        a.engine_send(EngineCmd::SetVolume(1.0));
                        a.core.change_state(Origin::User, StateChange::Volume(100));
                    }
                    path_view::RecViewAction::QuickFix(path_view::QuickFix::Unmute) => {
                        a.engine_send(EngineCmd::SetMuted(false));
                        a.core.change_state(Origin::User, StateChange::Muted(false));
                    }
                    path_view::RecViewAction::OpenSettings(_tab) => {
                        a.open_settings_from_path_window();
                    }
                    path_view::RecViewAction::None => {}
                }
            });
        }

        // 26. settings-toggle-col
        {
            let app = this.clone();
            ui.on_settings_toggle_col(move |idx| {
                eprintln!("[gui] settings_toggle_col idx={idx}");
                let idx = idx as usize;
                let (col, visible) = {
                    let a = app.borrow();
                    let ordered = a.dialog_cfg().columns.ordered_columns();
                    let Some(&col) = ordered.get(idx) else { return };
                    let visible = a.dialog_cfg().columns.column_visible(col);
                    (col, visible)
                };
                let mut a = app.borrow_mut();
                // В черновике меняется только видимость (настройка); ширины —
                // состояние сессии — пересчитываются по «Сохранить»
                // (`widths_on_save`, §2.5, §6.9).
                a.edit_draft(|d| {
                    if visible {
                        d.settings.columns.disable_column(col);
                    } else {
                        d.settings.columns.enable_column(col);
                    }
                });
                // Draft-only: refresh just the dialog list; live columns change
                // only when the draft is applied on Save.
                a.sync_dialog_cols();
            });
        }

        // 27. settings-reset-cols
        {
            let app = this.clone();
            ui.on_settings_reset_cols(move || {
                eprintln!("[gui] settings_reset_cols");
                let mut a = app.borrow_mut();
                // «Сбросить ширины» (Т-2, §6.9): черновик запоминает только
                // признак; по «Сохранить» в состояние уходит пустой набор
                // ширин — колонки получают ширины по приоритетам. Видимость и
                // порядок колонок не меняются.
                a.edit_draft(|d| d.reset_widths = true);
                a.sync_dialog_cols();
            });
        }

        // 28. settings-move-col-up
        {
            let app = this.clone();
            ui.on_settings_move_col_up(move |idx| {
                eprintln!("[gui] settings_move_col_up idx={idx}");
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                let ordered = a.dialog_cfg().columns.ordered_columns();
                if idx == 0 || idx >= ordered.len() {
                    return;
                }
                let col_id = ordered[idx];
                a.edit_cfg(|s| s.columns.move_column(idx, idx - 1));
                a.sync_dialog_cols();
                let new_ordered = a.dialog_cfg().columns.ordered_columns();
                let new_idx = new_ordered.iter().position(|&c| c == col_id).unwrap_or(idx - 1);
                if let Some(ui) = a.try_ui() {
                    ui.set_settings_selected_col(new_idx as i32);
                }
            });
        }

        // 29. settings-move-col-down
        {
            let app = this.clone();
            ui.on_settings_move_col_down(move |idx| {
                eprintln!("[gui] settings_move_col_down idx={idx}");
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                let ordered = a.dialog_cfg().columns.ordered_columns();
                if idx + 1 >= ordered.len() {
                    return;
                }
                let col_id = ordered[idx];
                a.edit_cfg(|s| s.columns.move_column(idx, idx + 1));
                a.sync_dialog_cols();
                let new_ordered = a.dialog_cfg().columns.ordered_columns();
                let new_idx = new_ordered.iter().position(|&c| c == col_id).unwrap_or(idx + 1);
                if let Some(ui) = a.try_ui() {
                    ui.set_settings_selected_col(new_idx as i32);
                }
            });
        }

        // 29b. settings-save (apply draft)
        {
            let app = this.clone();
            ui.on_settings_save(move || {
                eprintln!("[gui] settings_save (Save) draft_present={}", app.borrow().core.draft().is_some());
                let mut a = app.borrow_mut();
                // T1.0 §6.4 (ТЗ-22, ТЗ-27, ТЗ-29, ADR-20): тему, которая ещё
                // читается или не прочиталась, отсекает само ядро
                // (`dialog::validate`). Прочитанную тему ядро считает
                // допустимой без проверки HEX-цветов, поэтому она проверяется
                // здесь: с недопустимым цветом сохранения нет, диалог остаётся
                // открытым (§5.3).
                if let Some(ThemeLoad::Ready(data)) = a.core.draft().map(|d| &d.theme) {
                    if validate_colors(&data.colors).is_err() {
                        eprintln!("[gui] settings_save: theme colors invalid, not saved");
                        return;
                    }
                }
                // «Сбросить ширины» отменяет нормализацию ширин по видимости:
                // в состоянии остаётся пустой набор. Признак читается до
                // сохранения — шага `ResetWidths` в плане нет, если набор и
                // так пуст (Т-2, §6.9).
                let reset_widths = a.core.draft().is_some_and(|d| d.reset_widths);
                // Прежние настройки — база сравнения для шагов отображения
                // (видимость колонок, устройство активного режима, лимит кэша).
                let old = a.core.settings().clone();

                // «Сохранить» в ядре (ТЗ-28, ТЗ-29, ADR-11, §6.9): настройки
                // становятся действующими, `settings.toml` пишется одним
                // снимком, исполняются шаги ядра (интервал записи, тип
                // визуализации, сброс ширин, команда движку) и снимается
                // блокировка `Dialog`. `None` — отличий нет или значение
                // недопустимо: диалог остаётся открытым, ничего не применяется.
                let Some(saved) = a.core.dialog_save() else {
                    eprintln!("[gui] settings_save: nothing to save or invalid value, dialog stays open");
                    return;
                };
                if let Some(effect) = saved.write {
                    a.apply_reply_effects(vec![effect]);
                }

                // Шаги отображения — только для изменившегося (ТЗ-29, §5.2 ТЗ).
                // Таблица плейлиста пересобирается один раз после плана.
                let mut rebuild_table = false;
                for step in saved.plan {
                    match step {
                        // ТЗ-22, ТЗ-29, ADR-20: применяются уже прочитанные
                        // `apap-io` данные темы, без повторного чтения файла.
                        ApplyStep::Theme(data) => a.apply_theme(&data),
                        ApplyStep::Columns { visibility, order: _, defs: _ } => {
                            // Черновик ширин не хранит: при смене видимости
                            // действующие ширины нормализуются по итоговому
                            // набору видимых колонок (Т-2, §2.5, §6.9, ТЗ-31).
                            if visibility && !reset_widths {
                                let widths = widths_on_save(
                                    &old.columns,
                                    &a.core.settings().columns,
                                    a.core.state().column_widths(),
                                );
                                if let Some(widths) = widths {
                                    a.core.change_state(Origin::User, StateChange::ColumnWidths(widths));
                                }
                            }
                            rebuild_table = true;
                        }
                        ApplyStep::TopPanel => {
                            let top = a.core.settings().top_panel;
                            if let Some(ui) = a.try_ui() {
                                ui.set_cover_size(top.cover_size);
                                ui.set_col_info_w(top.col_info_w);
                                ui.set_col_gap(top.col_gap);
                            }
                        }
                        // Воркер обложек получает `CoverConfig` из действующих
                        // настроек с каждым заданием: поля UI и перезапрос
                        // обложки текущего трека с новыми настройками (SP1.0-B12).
                        ApplyStep::Covers => {
                            a.sync_cover_settings_to_ui();
                            if let Some(i) = a.current {
                                a.request_cover(i);
                            }
                        }
                        ApplyStep::InfoLabels => {
                            let labels: Vec<SharedString> = a
                                .core
                                .settings()
                                .info_labels_ordered()
                                .into_iter()
                                .map(SharedString::from)
                                .collect();
                            if let Some(ui) = a.try_ui() {
                                ui.set_info_labels(ModelRc::from(labels.as_slice()));
                            }
                        }
                        // Параметры и тип визуализации читаются из действующих
                        // настроек и состояния на каждом тике (`viz_push`,
                        // `drain_fulltrack`): воркеры и UI обновятся сами.
                        ApplyStep::Visualization | ApplyStep::VizMode(_) => {}
                        // ТЗ §10.4: горячий `resize` существующего LRU (лишние
                        // записи вытесняются внутри `CLruCache::resize`).
                        ApplyStep::VizCacheBudget => {
                            let new_viz_ram_mb = a.core.settings().visualization.viz_max_ram_mb;
                            if new_viz_ram_mb != old.visualization.viz_max_ram_mb {
                                let cap = music_player_rs::audio::fulltrack::fulltrack_cache_max_entries(
                                    new_viz_ram_mb,
                                );
                                a.fulltrack_cache.resize(cap);
                                eprintln!("[gui] settings_save: fulltrack cache resize -> {new_viz_ram_mb} MiB");
                            }
                        }
                        // Сворачивание в трей и прокрутка к играющему треку
                        // читаются из действующих настроек в момент события;
                        // интервал записи ядро уже переставило (У-1, ТЗ-33).
                        ApplyStep::Tray | ApplyStep::ScrollToPlaying | ApplyStep::SaveInterval(_) => {}
                        // Пустой набор ширин ядро уже записало в состояние
                        // (Т-2): остаётся пересчитать колонки таблицы.
                        ApplyStep::ResetWidths => rebuild_table = true,
                        ApplyStep::Engine(update) => {
                            // §6.18, §6.27, ТЗ-41, ТЗ-79, ТЗ-83, §7.2: команду
                            // движку ядро уже отправило — здесь остаётся
                            // пересчитать зависимые от режима поля UI
                            // (блокировка громкости) и, если настройки
                            // режимов реально изменились, сбросить кэш
                            // отчёта «Путь сигнала».
                            if update.settings != old.playback {
                                a.path_cache.invalidate();
                            }
                            a.sync_mode_to_ui();
                            // Устройство активного режима (ТЗ-94, §2.4):
                            // `set_output_device` обновляет только UI и трей,
                            // движок переключает устройство по той же команде.
                            let active_mode = a.core.state().active_mode();
                            let device = a.core.settings().playback.device(active_mode);
                            if device != old.playback.device(active_mode) {
                                let device = device.unwrap_or("").to_string();
                                a.set_output_device(device);
                            }
                        }
                        // Ядро уже сменило активный режим в состоянии
                        // (§6.9, ТЗ-30); здесь — синхронизация существующего
                        // индикатора активного режима (бейдж строки меню —
                        // после подключения в отдельном шаге). Переоткрытия
                        // нет (§6.9, таблица «Что/когда применяется»).
                        ApplyStep::ActiveMode(_) => a.sync_mode_to_ui(),
                    }
                }
                if rebuild_table {
                    a.sync_playlist_to_ui();
                }
                a.sync_audio_devices();
                if let Some(ui) = a.try_ui() {
                    ui.set_settings_open(false);
                }
                a.sync_gate_ui();
                eprintln!("[gui] settings_save: applied and closed");
            });
        }

        // 29c. settings-mode-edit-mode-selected: смена просматриваемого
        // режима в панели настроек режима (§6.18, §6.27, ТЗ-41).
        {
            let app = this.clone();
            ui.on_settings_mode_edit_mode_selected(move |idx| {
                let mut a = app.borrow_mut();
                let mode = match idx {
                    0 => ModeKind::Compatible,
                    1 => ModeKind::Optimal,
                    2 => ModeKind::Strict,
                    _ => return,
                };
                a.edit_draft(|d| d.shown_mode = mode);
                a.sync_mode_dialog_to_ui();
                // Список и выбор устройства — показанного режима (ТЗ-94,
                // ТЗ-126, ADR-24, AM1.0-B5).
                a.apply_audio_device_listing();
            });
        }

        // 29d. settings-mode-row-index-changed: выбор пункта списка строки
        // панели настроек режима (§6.18, §6.27, ТЗ-41, ТЗ-94).
        {
            let app = this.clone();
            ui.on_settings_mode_row_index_changed(move |param, idx| {
                let mut a = app.borrow_mut();
                let Ok(index) = usize::try_from(idx) else { return };
                a.apply_mode_dialog_edit(param, mode_dialog::EditValue::Index(index));
            });
        }

        // 29e. settings-mode-row-toggled: переключатель строки панели
        // настроек режима (§6.18, §6.27, ТЗ-41, ТЗ-94).
        {
            let app = this.clone();
            ui.on_settings_mode_row_toggled(move |param, on| {
                let mut a = app.borrow_mut();
                a.apply_mode_dialog_edit(param, mode_dialog::EditValue::Toggle(on));
            });
        }

        // 29f. settings-mode-row-slider-changed: ползунок строки панели
        // настроек режима (§6.18, §6.27, ТЗ-41, ТЗ-94).
        {
            let app = this.clone();
            ui.on_settings_mode_row_slider_changed(move |param, value| {
                let mut a = app.borrow_mut();
                a.apply_mode_dialog_edit(param, mode_dialog::EditValue::Slider(value.round() as u16));
            });
        }

        // 30. menu-clear-playlist: пункт меню «Файл» (ТЗ-34, ОВ-10); шлюз
        // отсекает его при блокировке окна и во время загрузки (ТЗ-48, ADR-12).
        {
            let app = this.clone();
            ui.on_menu_clear_playlist(move || {
                eprintln!("[gui] menu_clear_playlist");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::ClearPlaylist) {
                    return;
                }
                a.clear_playlist();
            });
        }

        // 31. menu-remove-selected: пункт меню «Файл» удаляет строку,
        // выделенную в таблице (ТЗ-34, ОВ-10, SP1.0-B6); шлюз — ТЗ-48, ADR-12.
        {
            let app = this.clone();
            ui.on_menu_remove_selected(move |row| {
                eprintln!("[gui] menu_remove_selected row={row}");
                let mut a = app.borrow_mut();
                if !a.core.gate().allows(MainCmd::RemoveCurrent) {
                    return;
                }
                if let Ok(idx) = usize::try_from(row) {
                    a.remove_track(idx);
                }
            });
        }

        // 31b. settings-cover-move-up
        {
            let app = this.clone();
            ui.on_settings_cover_move_up(move |idx| {
                eprintln!("[gui] settings_cover_move_up idx={idx}");
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                let ordered = a.dialog_cfg().covers.priority.clone();
                if idx == 0 || idx >= ordered.len() {
                    return;
                }
                a.edit_cfg(|s| s.covers.move_cover(idx, idx - 1));
                a.sync_cover_settings_to_ui();
            });
        }

        // 31c. settings-cover-move-down
        {
            let app = this.clone();
            ui.on_settings_cover_move_down(move |idx| {
                eprintln!("[gui] settings_cover_move_down idx={idx}");
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                let ordered = a.dialog_cfg().covers.priority.clone();
                if idx + 1 >= ordered.len() {
                    return;
                }
                a.edit_cfg(|s| s.covers.move_cover(idx, idx + 1));
                a.sync_cover_settings_to_ui();
            });
        }

        // 31d. settings-cover-names-edited
        {
            let app = this.clone();
            ui.on_settings_cover_names_edited(move |text| {
                eprintln!("[gui] settings_cover_names_edited text='{text}'");
                let names: Vec<String> = text
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                app.borrow_mut().edit_cfg(|s| s.covers.folder_names = names);
            });
        }

        // 31e. settings-toggle-cover-online
        {
            let app = this.clone();
            ui.on_settings_toggle_cover_online(move |on| {
                eprintln!("[gui] settings_toggle_cover_online on={on}");
                app.borrow_mut().edit_cfg(|s| s.covers.online = on);
            });
        }

        // 31f. settings-clear-viz-cache (ТЗ-34, ADR-20, §6.9): RAM-кэш — в UI,
        // диск — apap-io (ClearCache)
        {
            let app = this.clone();
            ui.on_settings_clear_viz_cache(move || {
                eprintln!("[gui] settings_clear_viz_cache");
                let mut a = app.borrow_mut();
                a.fulltrack_cache.clear();
                a.io_gens.clear = a.core.clear_cache(CacheKind::Visualization);
                let disk = a.cache_disk;
                a.apply_cache_sizes(&disk);
            });
        }

        // 31g. settings-clear-cover-cache (ТЗ-34, ADR-20, §6.9): RAM-кэш — в
        // UI, диск — apap-io (ClearCache)
        {
            let app = this.clone();
            ui.on_settings_clear_cover_cache(move || {
                eprintln!("[gui] settings_clear_cover_cache");
                let mut a = app.borrow_mut();
                a.io_gens.clear = a.core.clear_cache(CacheKind::Covers);
            });
        }

        // 31h. settings-clear-all-cache (ТЗ-34, ADR-20, §6.9): RAM-кэш — в
        // UI, диск — apap-io (ClearCache)
        {
            let app = this.clone();
            ui.on_settings_clear_all_cache(move || {
                eprintln!("[gui] settings_clear_all_cache");
                let mut a = app.borrow_mut();
                a.fulltrack_cache.clear();
                a.io_gens.clear = a.core.clear_cache(CacheKind::All);
                let disk = a.cache_disk;
                a.apply_cache_sizes(&disk);
            });
        }

        // 32. show-about (stub)
        {
            ui.on_show_about(move || {
                eprintln!("[gui] show_about");
            });
        }

        // 32b. message window: один callback на клик любой кнопки (§6.15,
        // AM1.0-B14): `id` — `action-id` из модели `apply_msg_effect`,
        // индекс в `message.actions` показанного сообщения.
        {
            let app = this.clone();
            ui.on_msg_action(move |id| {
                eprintln!("[gui] msg_action id={id}");
                let mut a = app.borrow_mut();
                let Ok(idx) = usize::try_from(id) else { return };
                let Some(shown) = a.messages.shown() else { return };
                let Some(action) = shown.actions.get(idx).copied() else { return };
                // Показанное — окно ошибки замка, которое отслеживает
                // `apply_lock_failure_message` (AM1.0-B14 шаг 5): любая его
                // кнопка закрывает его (см. `MessageCenter::press` ниже),
                // поэтому снимаем метку здесь же, одним местом для всех
                // кнопок, а не в каждой ветке `match action`.
                if a.lock_failure_msg.as_ref() == Some(&shown) {
                    a.lock_failure_msg = None;
                }
                let (close_effect, msg_effect) = a.messages.press(action);
                a.apply_msg_effect(msg_effect);
                // Окно недоступного режима закрыто одной из своих кнопок —
                // снять признак, иначе новое такое окно не поставится
                // (ТЗ-111, §6.2 п. 5).
                a.mode_notice_pending = false;
                match close_effect {
                    CloseEffect::Retry(files) => a.retry_writes(files),
                    CloseEffect::SwitchToCompatible => a.select_active_mode(ModeKind::Compatible),
                    // «Захватить и играть» окна «Замок открыт»: захват как при
                    // ручном закрытии замка (ТЗ-125, ТЗ-42, §6.19). Сам захват
                    // воспроизведение не запускает — флаг ждёт `Held` от
                    // следующего `EngineEvent::Lock` в `drain_engine_events`.
                    CloseEffect::CaptureAndPlay => {
                        a.capture_and_play_pending = true;
                        a.engine_send(EngineCmd::Lock(LockInput::CaptureAndPlay));
                    }
                    CloseEffect::Closed => {}
                }
                // Действия окна ошибки замка без своего `CloseEffect` (ТЗ-46,
                // ТЗ-144, AM1.0-B14 шаг 5/6): «Повторить захват» шлёт ту же
                // команду движку, что раньше слала отдельная кнопка
                // `lock-retry` в top_panel (убрана — кнопка-значок там
                // теперь одна, клик по ней — `lock-close`/`lock-open`).
                // `SwitchToCompatible` уже обработан выше через
                // `CloseEffect::SwitchToCompatible` (общий для любого окна с
                // этой кнопкой, не только окна замка).
                match action {
                    MessageAction::RetryCapture => {
                        a.engine_send(EngineCmd::Lock(LockInput::RetryCapture));
                    }
                    MessageAction::OpenAudioSettings => {
                        if a.open_settings_dialog() {
                            if let Some(ui) = a.try_ui() {
                                ui.set_settings_tab(1);
                            }
                        }
                    }
                    MessageAction::Cancel
                    | MessageAction::Ok
                    | MessageAction::Retry
                    | MessageAction::SwitchToCompatible
                    | MessageAction::CaptureAndPlay => {}
                }
            });
        }

        // 33. window close -> minimize to tray (if enabled and shown), otherwise quit
        {
            let app = this.clone();
            ui.window().on_close_requested(move || {
                // В трей — только если значок трея действительно показан
                // (§6.10, ТЗ-52 п. 2, В-1); иначе окно исчезло бы без выхода.
                let minimize = {
                    let a = app.borrow();
                    a.core.settings().minimize_to_tray && a.caps.tray
                };
                eprintln!("[gui] close_requested minimize={minimize}");
                if minimize {
                    let mut a = app.borrow_mut();
                    if let Some(ui) = a.try_ui() {
                        let _ = ui.hide();
                    }
                    let effect = a.messages.set_in_tray(true);
                    a.apply_msg_effect(effect);
                    slint::CloseRequestResponse::KeepWindowShown
                } else {
                    // Путь выхода (ТЗ-14, ТЗ-22, §6.10): освобождение движка,
                    // итоговые снимки писателю, ожидание в бюджете выхода.
                    app.borrow_mut().exit(ExitReason::WindowClose);
                    slint::CloseRequestResponse::HideWindow
                }
            });
        }

        // 33а. menu-quit: пункт «Выход» меню «Файл» — путь выхода (ТЗ-14,
        // §6.10) независимо от настройки сворачивания в трей (SP1.0-B17);
        // повтор во время выхода игнорируется (`ExitOutcome::Ignored`).
        {
            let app = this.clone();
            ui.on_menu_quit(move || {
                eprintln!("[gui] menu_quit");
                app.borrow_mut().exit(ExitReason::WindowClose);
            });
        }

        // 34. Вкладка «Visualization» и горячая клавиша V (ТЗ §9 / §3.2).
        viz_settings_manager::bind_viz_settings_callbacks(this);
    }

    pub fn tick(&mut self) {
        self.drain_engine_events();
        self.poll_tray();
        self.drain_events();
        self.poll_load_playlist();
        self.drain_scan();
        self.drain_cover();
        self.drain_fulltrack();
        self.drain_audio_devices();
        self.drain_io();
        self.handle_auto_advance();
        self.sync_playback_state_to_ui();
        // Показание геометрии окна: `AppCore` отличает эхо программной
        // установки от действия пользователя и взводит срок записи только
        // для последнего (ОВС-5 а, ADR-22, §6.17, V5.1-B7).
        let geometry = self.window_geometry();
        let output = self.core.tick(geometry);
        self.apply_reply_effects(output.effects);
        // `output.other` (экспорт/бэд-копии/карантин) — разбор добавится на
        // этапе писателя для этих путей; пока ответы отбрасываются.
        // Сообщения `tick()` (карантин/нечитаемый файл, ADR-13, §6.15,
        // ТЗ-52) — через тот же MessageCenter, что и отчёты поллинга
        // загрузки (ADR-16, ADR-23, §6.12).
        for message in output.messages {
            self.push_message(message);
        }
        // Окно «Путь сигнала» обновляется только по событию, взводящему
        // отчёт устаревшим (`take_report_dirty`), а не каждый тик — закрытое
        // окно не читает отчёт вовсе, чтобы не гасить его устаревший флаг
        // впустую (ТЗ-79, ТЗ-83, §7.2).
        if self.try_ui().is_some_and(|ui| ui.get_bp_report_open()) {
            if let Some(r) = self.take_report_dirty() {
                let view = path_view::path_view(r);
                self.push_path_view_to_ui(view);
            }
        }
        self.push_tray_status();

        // MainCmd::ColumnResize спрашивает шлюз (§2.11, ADR-12, SP1.0-B15
        // п.7): при блокировке главного окна (диалог/сообщение/выбор файла)
        // детектирование и сохранение живых ширин колонок приостанавливается
        // целиком, чтобы не обойти ТЗ-23 опросом тика.
        if self.core.gate().allows(MainCmd::ColumnResize) {
            let sig = self.compute_col_sig();
            if sig != 0 && sig != self.col_model_sig {
                // Column layout changed from the UI (user dragging a border): adopt
                // it as the new baseline and wait until the drag settles before
                // taking the widths into session state and re-flowing.
                self.col_model_sig = sig;
                self.col_sig_stable_ticks = 0;
            } else if self.col_sig_stable_ticks < COL_SAVE_DEBOUNCE_TICKS {
                self.col_sig_stable_ticks = self.col_sig_stable_ticks.saturating_add(1);
                if self.col_sig_stable_ticks == COL_SAVE_DEBOUNCE_TICKS {
                    // Перетаскивание границы колонки — действие пользователя.
                    self.save_column_widths_from_ui(Origin::User);
                    self.update_column_widths();
                }
            }
        }
        // Конец тика UI: событие winit без парного `scroll-event` ползунка
        // отбрасывается — указатель был вне ползунка (§6.11 «Сопоставление»,
        // В-2).
        self.core.input_mut().end_tick();
    }

    /// High-frequency column reflow, driven by its own fast timer (see
    /// `REF_INTERVAL_MS` in main.rs) that runs independently of the 100 ms
    /// `tick()`. With the table's `max-width` decoupled in playlist.slint, the
    /// window itself resizes freely — we only need to snap the columns to the
    /// new width once the resize *finishes*. While the width keeps changing we
    /// freeze the columns (no live rewriting, no fighting); once it has been
    /// stable for `REFLOW_SETTLE_TICKS` (~64 ms ≈ mouse release), we reflow.
    pub fn reflow(&mut self) {
        let Some(ui) = self.try_ui() else { return };
        let w = ui.get_playlist_view_width();
        if w <= 100.0 {
            return;
        }
        if (w - self.last_view_width).abs() > 1.0 {
            // Window is being resized: just track the new width, leave columns
            // alone. The column snap happens when the width settles.
            self.last_view_width = w;
            self.view_w_stable_ticks = 0;
        } else if self.view_w_stable_ticks < REFLOW_SETTLE_TICKS {
            self.view_w_stable_ticks += 1;
            if self.view_w_stable_ticks == REFLOW_SETTLE_TICKS {
                self.update_column_widths();
            }
        }
    }


    /// Разбирает итог загрузки плейлиста на каждом тике (ADR-16, ADR-23
    /// шаг 9, §6.12, ТЗ-13 б, ТЗ-21, ТЗ-47, ТЗ-48): `AppCore::poll_load` сам
    /// сверяет поколение и завершает `LoadState` -> `Idle`, вернув отчёт
    /// только для итога ТЕКУЩЕЙ загрузки. `Replaced` (стартовая загрузка) —
    /// плейлист уже заменён внутри `core` (`playlist_replace` вызван в
    /// `on_load_outcome`), здесь только перестраивается модель таблицы.
    /// `PlayNow` (командная загрузка, «Загрузить плейлист», ТЗ-13 б) — тем
    /// же путём обновляется модель таблицы, а запуск воспроизведения
    /// первого трека (или стоп при пустом плейлисте) выполняет
    /// `apply_play_now` (`playback_manager.rs`): `core` уже заменил
    /// плейлист и взвёл флаг «изменён», сам стоп/`Open` прежнего/нового
    /// трека остаётся за `MusicApp`. Сообщения (повреждённый/нечитаемый
    /// `playlist.m3u`, ошибка чтения выбранного файла) идут в
    /// `MessageCenter` (ADR-13, §6.15, ТЗ-52).
    fn poll_load_playlist(&mut self) {
        let Some(report) = self.core.poll_load() else {
            return;
        };
        match report.applied {
            Some(LoadApplied::Replaced) => {
                self.rebuild_known_paths();
                self.sync_playlist_to_ui();
            }
            Some(LoadApplied::PlayNow { first }) => {
                self.rebuild_known_paths();
                self.sync_playlist_to_ui();
                self.apply_play_now(first);
            }
            None => {}
        }
        // Загрузка завершена — снять признак ТЗ-48 и индикатор занятости,
        // список снова доступен.
        self.core.gate_mut().set_loading(None);
        if let Some(ui) = self.try_ui() {
            ui.set_busy(false);
        }
        if let Some(message) = report.message {
            self.push_message(message);
        }
        // Успешная загрузка стартового плейлиста без сообщения — результат
        // виден в таблице и track-count, сообщение/строка состояния не
        // нужны (ТЗ-52, ОВ-7).
    }

    /// Разбирает ответы потока `apap-io` на тике: ответы сверяются с
    /// поколением своего вида; устаревшие отбрасываются (ADR-20). Свежий
    /// `CacheSizes`/`Cleared` применяется в UI (ТЗ-22, ТЗ-34); `Themes`/`Theme`
    /// — список тем и метаданные темы в диалоге (ТЗ-22, §6.2). Ответ `Theme`
    /// сверяет сам `apply_theme_read`: его может ждать черновик ядра
    /// (`ThemeLoad::Loading`) либо показ метаданных действующей темы.
    fn drain_io(&mut self) {
        while let Some(done) = self.core.io().try_recv() {
            let stale = match &done {
                IoDone::Theme { .. } => false,
                IoDone::Themes { gen, .. } => *gen != self.io_gens.themes,
                IoDone::CacheSizes { gen, .. } => *gen != self.io_gens.sizes,
                IoDone::Cleared { gen, .. } => *gen != self.io_gens.clear,
            };
            if stale {
                continue;
            }
            match done {
                IoDone::Themes { entries, .. } => self.apply_theme_list(entries),
                IoDone::Theme { gen, result } => self.apply_theme_read(gen, result),
                IoDone::CacheSizes { sizes, .. } | IoDone::Cleared { sizes, .. } => {
                    self.cache_disk = sizes;
                    self.apply_cache_sizes(&sizes);
                }
            }
        }
    }

    fn poll_tray(&mut self) {
        // Хост StatusNotifier найден → трей и уведомления есть (ADR-6, В-1,
        // ТЗ-52 п.2): уведомления идут через рантайм потока трея (ADR-9).
        if let Some(ready) = &self.tray_ready {
            match ready.try_recv() {
                Ok(up) => {
                    self.caps = PlatformCaps { tray: up, notifications: up };
                    self.tray_ready = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.tray_ready = None,
            }
        }
        let Some(rx) = self.tray_rx.take() else {
            return;
        };
        while let Ok(cmd) = rx.try_recv() {
            eprintln!("[tray] cmd={cmd:?}");
            match cmd {
                TrayEvent::TogglePlay => {
                    if self.play_via_open_if_stopped() {
                        continue;
                    }
                    self.toggle_playback();
                    if self.audio().is_playing() {
                        self.emit(AppEvent::PlaybackStarted);
                    } else {
                        self.emit(AppEvent::PlaybackPaused);
                    }
                }
                TrayEvent::Stop => {
                    self.engine_send(EngineCmd::Stop);
                    self.emit(AppEvent::PlaybackStopped);
                }
                TrayEvent::Prev => self.play_prev(),
                TrayEvent::Next => self.play_next(1),
                TrayEvent::ShowHide => {
                    let Some(ui) = self.try_ui() else { continue };
                    let visible = ui.window().is_visible();
                    let res = if visible {
                        // Замок удерживает устройство (ТЗ-43, §8 С6): уход в
                        // трей больше не освобождает захват — освобождение
                        // эксклюзива при скрытии удалено (V5.1-B6).
                        ui.hide()
                    } else {
                        ui.show()
                    };
                    if let Err(e) = res {
                        eprintln!("tray show/hide failed: {e}");
                    } else {
                        // В трее (ADR-13, ТЗ-52 п. 3): скрытие окна уходит
                        // сообщение в уведомление по PlatformCaps, возврат —
                        // обратно в окно.
                        let effect = self.messages.set_in_tray(visible);
                        self.apply_msg_effect(effect);
                    }
                }
                TrayEvent::Quit => {
                    // Путь выхода (ТЗ-14, ТЗ-22, §6.10); повтор во время
                    // выхода игнорируется (`ExitOutcome::Ignored`).
                    self.exit(ExitReason::TrayQuit);
                }
                TrayEvent::Scroll(ev) => {
                    // Знак и величину события по хосту трея нормализует модуль
                    // ввода (ADR-8, §6.11 «Трей», В-5, ТЗ-36). Трей — источник
                    // единой команды громкости (ADR-14, ТЗ-37, НФ-2); работает
                    // и при открытом диалоге (ТЗ-24).
                    let clicks = self.core.input_mut().tray_scroll(ev);
                    if clicks.0 != 0 {
                        self.apply_volume_cmd(VolumeSource::TrayWheel, VolumeCmd::Step(clicks));
                    }
                }
                TrayEvent::Host { host, via } => {
                    // Хост трея выбирает строку таблицы модуля ввода для
                    // прокрутки над значком (знак и деление, В-5) и фиксируется
                    // в журнале (ADR-8, §6.11 «Хост трея», §2.9).
                    self.core.set_tray_host(host, via);
                }
            }
        }
        self.tray_rx = Some(rx);
    }

    fn push_tray_now(&mut self) {
        if let Some(tx) = &self.tray_up_tx {
            let state = self.tray_state();
            let _ = tx.send(state);
        }
    }

    /// Emit a state-change event for the direction-feed (surfaces react in
    /// `drain_events`, which runs every tick before the UI sync).
    fn emit(&mut self, event: AppEvent) {
        let _ = self.events_tx.send(event);
    }

    /// Выполняет операцию `op` над моделью плейлиста `AppCore` и переносит
    /// `current` и `pending_open` через `TrackId`, чтобы они остались
    /// согласованными после изменения видимого порядка — затем
    /// перестраивает `known_paths` (§8 С6, §4.2, ТЗ-12). Shuffle-состояние
    /// зеркалится самим `AppCore` в `apply_playlist_effect` (§3.4, ТЗ-45,
    /// ТЗ-46).
    pub(super) fn playlist_op(&mut self, op: impl FnOnce(&mut AppCore)) {
        let visible_before = self.core.playlist().visible().to_vec();
        let current_id = self.current.and_then(|i| visible_before.get(i).copied());
        let pending_open_id = self.pending_open.map(|(idx, prev)| {
            (
                visible_before.get(idx).copied(),
                prev.and_then(|p| visible_before.get(p).copied()),
            )
        });

        op(&mut self.core);

        self.rebuild_known_paths();

        self.current = current_id.and_then(|id| self.core.playlist().index_of(id));
        self.pending_open = pending_open_id.and_then(|(idx_id, prev_id)| {
            let idx = idx_id.and_then(|id| self.core.playlist().index_of(id))?;
            Some((idx, prev_id.and_then(|id| self.core.playlist().index_of(id))))
        });
    }

    /// Перестраивает `known_paths` (пути видимого порядка плейлиста) из
    /// модели `AppCore` после операции над плейлистом (§8 С6, §4.2, ТЗ-12).
    pub(super) fn rebuild_known_paths(&mut self) {
        let p = self.core.playlist();
        self.known_paths = p.visible().iter().filter_map(|&id| p.get(id)).map(|t| t.path.clone()).collect();
    }

    /// Разобрать все накопленные события движка, применить принятый
    /// `Opened` к плейлисту и раздать остальные варианты `Applied` в
    /// `dispatch_applied` (ADR-02, И-Р13). Буфер `engine_applied`
    /// переиспользуется между тиками — без аллокации.
    pub fn drain_engine_events(&mut self) {
        let mut applied = std::mem::take(&mut self.engine_applied);
        applied.clear();

        // Список устройства вкладки Audio для Оптимального/Строгого режима
        // строится из каталога движка, а не из cpal-перечисления Совместимого
        // (ADR-24, ТЗ-126); взводится ниже при смене каталога устройств или
        // переписывании автовыбранного устройства в настройках — чтобы
        // открытый диалог не держал устаревший список (AM1.0-B16).
        let mut hw_listing_dirty = false;

        // Device readiness is decided here, from the raw `EngineEvent`
        // stream, before it's handed to `on_engine_event` — the catalog
        // update itself reduces to a field inside `UiAudioState` and never
        // reaches `Applied` (ТЗ-104, ADR-16).
        // Устройство — из `ModeSettings` активного режима, не из прежнего
        // легаси-поля устройства (ТЗ-94, §2.4).
        let active_mode = self.core.state().active_mode();
        let saved_device = self.core.settings().playback.device(active_mode).unwrap_or("").to_string();
        let mut ready_device: Option<String> = None;

        // Сырые события снимаются в переиспользуемый буфер отдельным
        // проходом: `self.engine_events.drain` берёт `&self.engine_events`
        // на время вызова, а разбор через `self.on_engine_event` нужен весь
        // `&mut self` — вместе эти заимствования не делятся (ADR-02).
        let mut raw_events = std::mem::take(&mut self.engine_raw_events);
        raw_events.clear();
        self.engine_events.drain(|ev| {
            if let EngineEvent::Devices(ref catalog) = ev {
                // Оптимальный/Строгий — активное устройство из `catalog.hw`,
                // не из `catalog.shared.default_device()` (ТЗ-76, ADR-24,
                // ТЗ-143, AM1.0-B31): до этого бага обе ветки режима брали
                // имя Shared-устройства системного выхода по умолчанию,
                // поэтому диалог «Настройки» и «Путь сигнала» показывали
                // «default audio device» даже когда реально был захвачен
                // аппаратный ЦАП (MOTU M2 и т. п.) — подпись не совпадала с
                // открытым hw-устройством и с его же подписью в списке
                // устройств вкладки Audio (`hw_device_pairs_and_selected`).
                ready_device = if active_mode == ModeKind::Compatible {
                    let found = if saved_device.is_empty() {
                        catalog.default_device()
                    } else {
                        catalog
                            .find(&SharedDeviceId::new(saved_device.as_str()))
                            .or_else(|| catalog.default_device())
                    };
                    found.map(|dev| dev.name.clone())
                } else {
                    let saved_ref = if saved_device.is_empty() { None } else { Some(saved_device.as_str()) };
                    let (pairs, selected) = ui_manager::hw_device_pairs_and_selected(catalog, saved_ref);
                    selected.and_then(|i| pairs.get(i)).map(|(_, label)| label.clone())
                };
                // Каталог изменился — список hw-устройств вкладки Audio
                // устарел (ADR-24, ТЗ-126, AM1.0-B16).
                hw_listing_dirty = true;
            }
            raw_events.push(ev);
        });

        for ev in raw_events.drain(..) {
            // Вид замка строится из сырого `EngineEvent::Lock` до разбора
            // `on_engine_event` (который его съедает, сведя к `Applied::None`
            // в `apply_engine_event`) — иначе состояние замка не добралось
            // бы до top_panel (ADR-06, §6.19, ТЗ-118…ТЗ-123, ТЗ-144).
            let mut resume_after_capture = false;
            if let EngineEvent::Lock(ref state) = ev {
                // Состояние до этого события — ещё не перезаписано
                // `on_engine_event` ниже (`UiAudioState::apply` снимает его
                // в `self.audio_state.lock()` только там); нужно как `prev`
                // для `lock_failure_transition` (ТЗ-46, ТЗ-126, AM1.0-B14
                // шаг 5) — иначе окно ошибки замка не узнало бы, что замок
                // только что вошёл или вышел из `Failed`.
                let prev_lock = self.audio_state.lock().clone();
                self.apply_lock_view(state);
                self.apply_lock_failure_message(&prev_lock, state);
                // «Захватить и играть» ждёт `Held`: воспроизведение
                // запускается только после того, как `on_engine_event` ниже
                // применит это же состояние к `self.audio_state.lock()`,
                // иначе `lock_blocks_play` внутри ещё увидит старый замок
                // (ТЗ-125, §6.19).
                if self.capture_and_play_pending {
                    match state {
                        LockState::Held { .. } => {
                            self.capture_and_play_pending = false;
                            resume_after_capture = true;
                        }
                        LockState::Acquiring { .. } => {}
                        LockState::NotApplicable | LockState::Open | LockState::Failed(_) => {
                            self.capture_and_play_pending = false;
                        }
                    }
                }
            }
            let a = self.on_engine_event(ev);
            if a != Applied::None {
                applied.push(a);
            }
            if resume_after_capture {
                self.resume_after_capture_and_play();
            }
        }
        self.engine_raw_events = raw_events;

        if let Some(name) = ready_device {
            if !self.audio_ready {
                // Первое подтверждение устройства заменяет стартовый STATUS_AUDIO_CHECKING (ТЗ-104).
                self.status = music_player_rs::i18n::status_audio_ready(&name).into();
            }
            self.audio_ready = true;
            self.audio_error = None;
            self.active_device = name;
        }

        if let Some(info) = self.take_opened() {
            if let Some((index, prev_current)) = self.pending_open.take() {
                if index < self.track_count() {
                    self.on_track_opened(index, prev_current, &info);
                    // Успешное открытие прерывает серию пропусков (ТЗ-85, ТЗ-86).
                    self.skip_streak = 0;
                }
            }
        }

        for a in applied.drain(..) {
            // Автовыбор переписывает устройство режима в настройках —
            // список вкладки Audio для этого режима тоже устарел
            // (ADR-24, ТЗ-126, AM1.0-B16).
            if matches!(a, Applied::DeviceAutoSelected { .. }) {
                hw_listing_dirty = true;
            }
            self.dispatch_applied(a);
        }
        self.engine_applied = applied;

        // Диалог настроек открыт не на Совместимом режиме — перестраиваем
        // hw-список вкладки Audio, иначе он держал бы устаревший каталог до
        // следующего переключения режима/фильтра (ADR-24, ТЗ-126, AM1.0-B16).
        let shown_mode = self.core.draft().map(|d| d.shown_mode);
        if needs_hw_listing_refresh(shown_mode, hw_listing_dirty) {
            self.apply_audio_device_listing();
        }

        // Блок «Аудио» диалога настроек (DeviceInfoView, AM1.0-B30, §8 С11)
        // держится в синхроне с движком на каждом тике, пока диалог открыт:
        // поводов обновиться несколько (выбор режима/устройства в диалоге,
        // `DeviceCapsCached`, `Devices`, путь/замок) и разводить их по
        // отдельным «грязным» флагам дороже, чем раз в тик пересобрать
        // несколько строк текста.
        if self.core.draft().is_some() {
            self.sync_device_info_to_ui();
        }

        // Бейдж устройства в MenuBarTrailing (AM1.0-B30, шаг 3) — всегда, не
        // только при открытом диалоге настроек (виден в главном окне);
        // дёшево проверить каждый тик, пишет в Slint только при изменении
        // (`sync_device_badge`).
        self.sync_device_badge();

        // Заголовок окна «APAP — <трек>» / «APAP» (T1.0-B2) — та же схема:
        // дёшево пересчитать каждый тик, пишет в Slint только при смене
        // текущего трека.
        self.sync_window_title();
    }

    /// Заголовок окна «APAP — <трек>» / «APAP» (T1.0-B2, решение
    /// пользователя 2026-10-23): имя трека — заголовок/исполнитель тем же
    /// способом, что `tray_state().now_playing`; текст строит
    /// `music_player_rs::i18n::window_title`. Пишет в Slint только при изменении
    /// (ui/app.slint::window-title).
    fn sync_window_title(&mut self) {
        let name = self.current.and_then(|i| self.track_at(i)).map(|t| match &t.artist {
            Some(a) if !a.is_empty() => format!("{} \u{2014} {}", t.title, a),
            _ => t.title.clone(),
        });
        if self.last_window_title == name {
            return;
        }
        let Some(ui) = self.try_ui() else { return };
        ui.set_window_title(music_player_rs::i18n::window_title(name.as_deref()).into());
        self.last_window_title = name;
    }

    /// Раздать принятое событие движка (кроме `Opened`, обработанного в
    /// `drain_engine_events`) соответствующей реакции UI (ADR-02, С3).
    fn dispatch_applied(&mut self, a: Applied) {
        match a {
            // Поглощено полями состояния `UiAudioState`, либо устаревшее
            // событие отброшено по `req_gen` (И-Р14) — действий не требуется.
            Applied::None => {}
            Applied::Skipped { path, reason } => {
                // Порча/ошибка чтения файла посреди трека — пропуск, а не
                // естественное завершение (ТЗ-86, ТЗ-87). Открытие, на
                // которое ссылался пропущенный трек, уже не актуально.
                self.pending_open = None;
                self.skip_streak += 1;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                // Фоновое событие (не результат явного действия пользователя
                // над этим треком) — строка состояния, не окно (ТЗ-52, ОВ-7).
                self.status =
                    music_player_rs::i18n::messages::track_skipped_status(&name, &describe_skip_reason(&reason))
                        .into();

                if self.track_count() == 0 || self.skip_streak >= self.track_count() {
                    // Защита от бесконечного пропуска при Repeat All, если
                    // вся библиотека повреждена (мост до SkipSeries С8).
                    self.engine_send(EngineCmd::Stop);
                    self.emit(AppEvent::PlaybackStopped);
                    self.status = music_player_rs::i18n::messages::ALL_TRACKS_SKIPPED.into();
                    return;
                }
                // `RepeatMode::One` на пропуске ведёт себя как `All`/`Off` —
                // иначе плеер зациклился бы на одном и том же повреждённом
                // треке.
                let repeat = if self.repeat == RepeatMode::One {
                    RepeatMode::All
                } else {
                    self.repeat
                };
                match self.next_track_index(repeat) {
                    Some(idx) => self.play_track(idx),
                    None => {
                        self.engine_send(EngineCmd::Stop);
                        self.emit(AppEvent::PlaybackStopped);
                    }
                }
            }
            Applied::OpenFailed { err } => {
                // Захват устройства (`Capture`) не удался — отдельное
                // сообщение шлёт сам движок (`EngineEvent::Lock`), здесь
                // только снять ожидание открытия.
                if matches!(err, OpenError::Capture(_)) {
                    self.pending_open = None;
                    return;
                }
                // Сохранённый активный режим недоступен на этой платформе
                // (ТЗ-111, §6.2 п. 5): звук не играет, режим не меняется
                // сам — только уведомление с кнопкой переключения. Не
                // `on_open_failed`, чтобы не проскочить дальше по плейлисту.
                if matches!(err, OpenError::ModeUnavailable(_)) {
                    self.pending_open = None;
                    self.engine_send(EngineCmd::Stop);
                    self.emit(AppEvent::PlaybackStopped);
                    self.push_mode_unavailable_notice();
                    return;
                }
                let index = self.pending_open.take().map(|(index, _)| index).or(self.current);
                let text = describe_open_error(&err);
                match index {
                    Some(index) => self.on_open_failed(index, &text),
                    None => {
                        self.engine_send(EngineCmd::Stop);
                        self.push_message(Message {
                            level: MessageLevel::Error,
                            title: music_player_rs::i18n::messages::PLAYBACK_FAILED_TITLE.into(),
                            body: text.into(),
                            actions: vec![MessageAction::Ok],
                        });
                    }
                }
            }
            // `UiAudioState::apply` уже взвела флаг `ended`; авто-переход на
            // следующий трек делает `handle_auto_advance` на тике — повторная
            // обработка здесь продублировала бы переход.
            Applied::Ended => {}
            // AM1.0-B34 (шаг 4, §6.7 п. 2, §6.20): поток, который открылся,
            // но hw-указатель так и не двинулся (сторож запуска, `alsa_out.
            // rs::run_output_loop`), — честная, но другая по смыслу ошибка,
            // чем реальная потеря уже игравшего устройства: тот же текст
            // «Устройство вывода потеряно» был бы неверным (устройство не
            // терялось — оно не стартовало). `fault` всегда идёт с
            // `Reaction::StopWithError` на этом пути (`run.rs::poll_session`).
            Applied::DeviceLost { reaction, fault: Some(StreamFault::Stalled) } => {
                debug_assert_eq!(reaction, Reaction::StopWithError);
                self.engine_send(EngineCmd::Stop);
                self.emit(AppEvent::PlaybackStopped);
                let body = self
                    .audio()
                    .path()
                    .and_then(|p| p.output.device_buffer.map(|buf| (buf, p.output.requested)))
                    .map(|(buf, fmt)| {
                        let ms = u64::from(buf.buffer_frames) * 1000 / u64::from(fmt.rate.hz());
                        music_player_rs::i18n::messages::device_buffer_stalled_body(
                            &ms.to_string(),
                            &buf.period_frames.to_string(),
                        )
                    })
                    .unwrap_or_else(|| music_player_rs::i18n::messages::DRIVER_NOT_STARTED.to_owned());
                self.push_message(Message {
                    level: MessageLevel::Error,
                    title: music_player_rs::i18n::messages::DEVICE_NOT_STARTED_TITLE.into(),
                    body: body.into(),
                    actions: vec![MessageAction::OpenAudioSettings, MessageAction::Cancel],
                });
            }
            Applied::DeviceLost { reaction, fault: _ } => match reaction {
                Reaction::StopWithError | Reaction::LockFailed => {
                    self.engine_send(EngineCmd::Stop);
                    self.emit(AppEvent::PlaybackStopped);
                    self.push_message(Message {
                        level: MessageLevel::Error,
                        title: music_player_rs::i18n::messages::DEVICE_LOST_TITLE.into(),
                        body: music_player_rs::i18n::messages::DEVICE_LOST_BODY.into(),
                        actions: vec![MessageAction::Ok],
                    });
                }
                Reaction::FollowSystemDefault => {
                    // Фоновое переключение на устройство по умолчанию — не
                    // явное действие пользователя (ТЗ-52, ОВ-7) — строка
                    // состояния.
                    self.status = music_player_rs::i18n::messages::OUTPUT_SWITCHED_TO_DEFAULT_STATUS.into();
                }
                Reaction::Skip => {
                    // Трактуется как пропуск трека без отдельного сообщения —
                    // избегаем спама сообщениями при частых потерях устройства.
                    if let Some(idx) = self.next_track_index(self.repeat) {
                        self.play_track(idx);
                    } else {
                        self.engine_send(EngineCmd::Stop);
                        self.emit(AppEvent::PlaybackStopped);
                    }
                }
            },
            Applied::Notice(Notice::DevicesUnavailable(BackendError::Unavailable(msg))) => {
                // Фоновый сбой перечисления устройств (ТЗ-104, ТЗ-105, ADR-16) —
                // строка состояния, не окно. То же сообщение, что раньше
                // выдавал синхронный startup-probe, теперь приходит событием.
                self.audio_ready = false;
                self.audio_error = Some(msg.clone());
                self.status = music_player_rs::i18n::messages::devices_unavailable_status(&msg).into();
            }
            // Пропавший per-sink узел PipeWire Совместимого режима во время
            // работы движка (ADR-07 «Уточнение», AM1.0-B37, шаг 13): движок
            // уже переключил свою копию на «Системное по умолчанию» —
            // приложению остаётся показать то же окно, что при аналогичной
            // миграции на старте, и перезаписать `settings.toml`, чтобы
            // следующее «Сохранить» диалога настроек не вернуло узел обратно
            // из открытого черновика.
            Applied::Notice(Notice::CompatibleDeviceMigrated) => {
                self.push_message(Message {
                    level: MessageLevel::Info,
                    title: music_player_rs::i18n::audio::COMPATIBLE_DEVICE_MIGRATED_TITLE.into(),
                    body: music_player_rs::i18n::audio::COMPATIBLE_DEVICE_MIGRATED_BODY.into(),
                    actions: vec![MessageAction::Ok],
                });
                self.save_compatible_device_migrated();
            }
            // Автовыбор устройства Оптимального/Строгого режима (шаг 38,
            // ТЗ-143, ТЗ-134, ADR-24) — сохраняется в настройках.
            Applied::DeviceAutoSelected { mode, device } => self.save_auto_selected_device(mode, device),
            // Путь завершения приложения обрабатывается отдельным шагом моста.
            Applied::ShutdownComplete => {}
        }
    }

    /// Сохраняет автовыбор устройства Оптимального/Строгого режима в
    /// `settings.toml` одним снимком (ТЗ-143, ТЗ-134, ADR-24, шаг 38):
    /// движок уже скопировал его в свою копию `ModeSettings`, поэтому
    /// `EngineCmd::SetModeSettings` сюда не отправляется — повторная
    /// отправка той же настройки движку привела бы к петле переоткрытия
    /// устройства. Устройство другого режима не трогается. Совместимый
    /// режим автовыбора не знает (§2.3) — событие для него игнорируется.
    fn save_auto_selected_device(&mut self, mode: ModeKind, device: HwDeviceId) {
        let Some(settings) = settings_with_auto_selected_device(self.core.settings(), mode, &device) else {
            return;
        };
        if let Some(effect) = self.core.save_settings_now(settings) {
            self.apply_reply_effects(vec![effect]);
        }
        // Внешнее изменение настроек перебазирует открытый черновик — кнопка
        // «Сохранить» пересчитывается следом (SP1.0 ТЗ-27).
        self.sync_save_button();
    }

    /// Перезаписывает устройство Совместимого режима в `settings.toml` на
    /// «Системное по умолчанию» (ADR-07 «Уточнение», AM1.0-B37, шаг 13) тем
    /// же снимочным путём, что автовыбор устройства Оптимального/Строгого
    /// (`save_auto_selected_device`, шаг 38): движок уже переключил свою
    /// копию `ModeSettings` (`Engine::migrate_vanished_compatible_device`),
    /// повторная отправка `SetModeSettings` обратно не нужна — предпочтение
    /// «переписать через обычный путь сохранения» (ADR-07 «Уточнение»):
    /// запись файла остаётся честной, иначе следующее «Сохранить» диалога
    /// настроек вернуло бы пропавший узел обратно из открытого черновика.
    fn save_compatible_device_migrated(&mut self) {
        let mut settings = self.core.settings().clone();
        settings.playback.compatible.device = SharedDeviceChoice::SystemDefault;
        if let Some(effect) = self.core.save_settings_now(settings) {
            self.apply_reply_effects(vec![effect]);
        }
        self.sync_save_button();
    }

    /// Drain the event feed. Discrete transitions that matter to the tray
    /// (track switch, play/pause/stop, device change) push a fresh tray state
    /// immediately instead of waiting for the throttled status interval.
    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                AppEvent::TrackChanged(_)
                | AppEvent::PlaybackStarted
                | AppEvent::PlaybackPaused
                | AppEvent::PlaybackStopped
                | AppEvent::DeviceChanged => self.push_tray_now(),
                AppEvent::QueueChanged
                | AppEvent::VolumeChanged(_)
                | AppEvent::CoverChanged => {}
            }
        }
    }

    fn push_tray_status(&mut self) {
        let interval = tray::TRAY_UPDATE_INTERVAL_MS;
        if self.tray_up_tx.is_some() && self.last_tray_update.elapsed().as_millis() >= interval {
            self.last_tray_update = Instant::now();
            self.push_tray_now();
        }
    }

    fn tray_state(&self) -> tray::TrayState {
        let now_playing = match self.current.and_then(|i| self.track_at(i)) {
            Some(t) => match &t.artist {
                Some(a) if !a.is_empty() => format!("{} \u{2014} {}", t.title, a),
                _ => t.title.clone(),
            },
            None => String::new(),
        };
        tray::TrayState {
            now_playing,
            playing: self.audio().is_playing(),
            bit_perfect: self.bit_perfect(),
            error: self.audio_error.clone(),
        }
    }
}

fn visible_col_at_index(columns: &ColumnsConfig, visible_index: i32) -> Option<ColumnId> {
    columns.visible_columns().get(visible_index as usize).copied()
}

/// Нужно ли перестроить hw-список устройства вкладки Audio: диалог
/// настроек открыт (`shown_mode` — показанный в нём режим) не на
/// Совместимом режиме, и каталог устройств движка/автовыбранное устройство
/// в настройках изменились (ADR-24, ТЗ-126, AM1.0-B16).
fn needs_hw_listing_refresh(shown_mode: Option<ModeKind>, hw_listing_dirty: bool) -> bool {
    hw_listing_dirty && shown_mode.is_some_and(|mode| mode != ModeKind::Compatible)
}

/// Короткий русский текст причины отказа открытия трека для сообщений
/// пользователю (ADR-14, ТЗ-86). `Capture` не описывается здесь — он не
/// доходит до этой функции, т.к. `dispatch_applied` гасит его отдельно
/// (дублирует уведомление о резервировании).
fn describe_open_error(err: &OpenError) -> String {
    match err {
        // §2.4, ТЗ-57, ТЗ-80: план остановлен на этапе `blocked.at` —
        // устройство несовместимо с треком в Строгом режиме.
        OpenError::Incompatible(blocked) => music_player_rs::i18n::errors::incompatible_track_text(
            &music_player_rs::audio::path::report::blocked_text(blocked),
        ),
        OpenError::Capture(_) => String::new(),
        OpenError::DeviceLost => music_player_rs::i18n::errors::DEVICE_LOST_SHORT.to_string(),
        OpenError::File(file_err) => describe_file_error(file_err),
        OpenError::Internal(fault) => {
            let (_, text) = music_player_rs::i18n::errors::engine_fault_text(fault);
            music_player_rs::i18n::errors::internal_fault_text(&text)
        }
        // ТЗ-111, §6.18 п. 2: сохранённый режим недоступен на платформе.
        OpenError::ModeUnavailable(mode) => music_player_rs::i18n::errors::mode_unavailable_text(&format!("{mode:?}")),
    }
}

/// Короткий русский текст ошибки файла (ТЗ-86, ТЗ-87).
fn describe_file_error(err: &FileError) -> String {
    match err {
        FileError::Corrupt(kind) => {
            let (_, text) = music_player_rs::i18n::errors::corrupt_kind_text(kind);
            music_player_rs::i18n::errors::corrupt_file_text(text)
        }
        FileError::Unsupported { codec } => music_player_rs::i18n::errors::unsupported_format_text(codec),
        FileError::Io(kind) => {
            let text = music_player_rs::i18n::errors::describe_io_error(*kind);
            music_player_rs::i18n::errors::read_error_text(&text)
        }
        FileError::ReadDuringPlayback { at_frame, kind } => {
            let text = music_player_rs::i18n::errors::describe_io_error(*kind);
            music_player_rs::i18n::errors::read_error_at_frame_text(&at_frame.to_string(), &text)
        }
    }
}

/// Короткий русский текст причины пропуска трека (ТЗ-86, ТЗ-87).
fn describe_skip_reason(reason: &SkipReason) -> String {
    match reason {
        // §2.4, ТЗ-57, ТЗ-80: план остановлен на этапе `blocked.at` —
        // устройство несовместимо с треком в Строгом режиме.
        SkipReason::Incompatible(blocked) => music_player_rs::i18n::errors::incompatible_track_text(
            &music_player_rs::audio::path::report::blocked_text(blocked),
        ),
        SkipReason::File(err) => describe_file_error(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artist: &str, year: &str, bitrate: u32, dur: Option<f64>) -> Track {
        Track {
            path: PathBuf::from("/tmp/x.flac"),
            title: title.to_string(),
            duration: dur,
            artist: if artist.is_empty() { None } else { Some(artist.to_string()) },
            album: None,
            genre: None,
            track_number: 0,
            track_total: 0,
            disc: 0,
            disc_total: 0,
            channels: 2,
            year: year.to_string(),
            format: "FLAC".to_string(),
            bitrate,
            bit_depth: "24 bit".to_string(),
            sample_rate: 96000,
        }
    }

    fn mode_param_row(value: f32, options: &[&str]) -> ModeParamRow {
        let opts: Vec<SharedString> = options.iter().map(|s| SharedString::from(*s)).collect();
        ModeParamRow {
            param: 5,
            label: SharedString::from("Буфер декодирование→вывод"),
            kind: 2,
            enabled: true,
            note: SharedString::new(),
            options: ModelRc::from(opts.as_slice()),
            selected: 0,
            checked: false,
            value,
            min: 100.0,
            max: 4000.0,
        }
    }

    /// AM1.0-B33: `mode_param_row_changed` сравнивает скалярные поля и
    /// содержимое `options` — не идентичность `ModelRc` (`to_param_row`
    /// строит новый `ModelRc` на каждый вызов, поэтому `old.options !=
    /// new.options` указателем даже без изменений, см. комментарий у
    /// функции).
    #[test]
    fn mode_param_row_changed_ignores_fresh_modelrc_identity() {
        let old = mode_param_row(200.0, &["a", "b"]);
        let same_content = mode_param_row(200.0, &["a", "b"]);
        assert!(!mode_param_row_changed(&old, &same_content));
    }

    #[test]
    fn mode_param_row_changed_detects_value_change() {
        let old = mode_param_row(200.0, &["a", "b"]);
        let moved = mode_param_row(250.0, &["a", "b"]);
        assert!(mode_param_row_changed(&old, &moved));
    }

    #[test]
    fn mode_param_row_changed_detects_option_list_change() {
        let old = mode_param_row(200.0, &["a", "b"]);
        let relabeled = mode_param_row(200.0, &["a", "b (недоступно)"]);
        assert!(mode_param_row_changed(&old, &relabeled));
    }

    fn dev(s: &str) -> HwDeviceId {
        s.parse().expect("валидный HwDeviceId в тесте")
    }

    /// AM1.0-B38, шаг 9: Совместимый режим — опрос не шлётся, даже если
    /// устройство выбрано и ничем не занято.
    #[test]
    fn should_probe_device_skips_compatible_mode() {
        let d = dev("hw:CARD=USB,DEV=0");
        assert_eq!(should_probe_device(ModeKind::Compatible, Some(&d), None, false, None), None);
    }

    /// Устройство не выбрано (ещё не пришло из каталога/не сохранено) —
    /// ничего слать некому.
    #[test]
    fn should_probe_device_skips_no_selection() {
        assert_eq!(should_probe_device(ModeKind::Optimal, None, None, false, None), None);
    }

    /// Выбранное устройство держит замок (активное/захваченное,
    /// `LockState::Held`) — не трогаем его опросом, та же защита, что у
    /// движка.
    #[test]
    fn should_probe_device_skips_held_device() {
        let d = dev("hw:CARD=USB,DEV=0");
        assert_eq!(should_probe_device(ModeKind::Strict, Some(&d), Some(&d), false, None), None);
    }

    /// Caps выбранного устройства уже кэшированы — повторный опрос не нужен.
    #[test]
    fn should_probe_device_skips_known_caps() {
        let d = dev("hw:CARD=USB,DEV=0");
        assert_eq!(should_probe_device(ModeKind::Optimal, Some(&d), None, true, None), None);
    }

    /// Дебаунс: подряд то же устройство, что уже было отправлено на опрос, —
    /// повторная команда не шлётся.
    #[test]
    fn should_probe_device_debounces_repeat_selection() {
        let d = dev("hw:CARD=USB,DEV=0");
        assert_eq!(should_probe_device(ModeKind::Optimal, Some(&d), None, false, Some(&d)), None);
    }

    /// Нет исключений — устройство возвращается для отправки
    /// `EngineCmd::ProbeDeviceCaps`.
    #[test]
    fn should_probe_device_returns_device_when_unknown_and_free() {
        let d = dev("hw:CARD=USB,DEV=0");
        assert_eq!(should_probe_device(ModeKind::Optimal, Some(&d), None, false, None), Some(d));
    }

    /// Выбор другого устройства после отправки опроса первого — дебаунс не
    /// распространяется на новое устройство.
    #[test]
    fn should_probe_device_allows_different_device_after_debounce() {
        let prev = dev("hw:CARD=USB,DEV=0");
        let next = dev("hw:CARD=USB,DEV=1");
        assert_eq!(
            should_probe_device(ModeKind::Optimal, Some(&next), None, false, Some(&prev)),
            Some(next)
        );
    }

    /// AM1.0-B21, §6.18, ТЗ-20: из «Стоп» (в т. ч. после смены режима,
    /// снявшей поток) и без трека Play идёт через `Open`; пауза/игра —
    /// переключение транспорта.
    #[test]
    fn play_route_opens_when_stopped_or_no_track() {
        use playback_manager::{play_route, PlayRoute};
        assert_eq!(play_route(true, true), PlayRoute::Open);
        assert_eq!(play_route(false, true), PlayRoute::Open);
        assert_eq!(play_route(false, false), PlayRoute::Open);
        assert_eq!(play_route(true, false), PlayRoute::Toggle);
    }

    #[test]
    fn sort_rows_text_formats_columns() {
        let mut t = track("Title", "Artist", "2001", 1411, Some(186.0));
        t.genre = Some("Rock".to_string());
        t.track_number = 3;
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Title), "Title");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Artist), "Artist");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Genre), "Rock");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::TrackNumber), "3");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Year), "2001");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Format), "FLAC");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Bitrate), "1411 кбит/с");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::BitDepth), "24 bit");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::SampleRate), "96 кГц");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Duration), "3:06");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::FileName), "x.flac");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::FilePath), "/tmp/x.flac");
    }

    #[test]
    fn sort_rows_empty_fields_render_blank() {
        let mut t = track("Title", "", "2001", 0, None);
        t.sample_rate = 0;
        t.bit_depth = String::new();
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Artist), "");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Bitrate), "");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::SampleRate), "");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Duration), "--:--");
    }

    #[test]
    fn needs_hw_listing_refresh_only_for_non_compatible_dialog() {
        // Диалог закрыт — список не перестраиваем, даже если каталог менялся.
        assert!(!needs_hw_listing_refresh(None, true));
        // Совместимый режим показан в диалоге — hw-список ему не нужен.
        assert!(!needs_hw_listing_refresh(Some(ModeKind::Compatible), true));
        // Оптимальный/Строгий режим показан, но каталог/автовыбор не менялись.
        assert!(!needs_hw_listing_refresh(Some(ModeKind::Optimal), false));
    }

    #[test]
    fn needs_hw_listing_refresh_true_for_optimal_or_strict_dialog() {
        assert!(needs_hw_listing_refresh(Some(ModeKind::Optimal), true));
        assert!(needs_hw_listing_refresh(Some(ModeKind::Strict), true));
    }

    #[test]
    fn num_slash_total_formats_numbers() {
        assert_eq!(fmt_num(0, 0).as_str(), "\u{2014}");
        assert_eq!(fmt_num(3, 0).as_str(), "3");
        assert_eq!(fmt_num(3, 12).as_str(), "3 / 12");
        assert_eq!(fmt_num(0, 12).as_str(), "\u{2014}");
    }

    #[test]
    fn hex_color_parses_valid_inputs() {
        assert!(hex_color("#121018").is_some());
        assert!(hex_color("112233").is_some());
        assert!(hex_color("#00000088").is_some());
        assert!(hex_color("aaff0000").is_some());
    }

    #[test]
    fn hex_color_rejects_invalid_inputs() {
        assert!(hex_color("").is_none());
        assert!(hex_color("#12345").is_none()); // wrong length
        assert!(hex_color("#1234567").is_none()); // wrong length
        assert!(hex_color("#gggggg").is_none()); // non-hex
        assert!(hex_color("#12345g").is_none()); // non-hex tail
        assert!(hex_color("ФфФФФФ").is_none()); // non-utf8/ascii hex
    }

    #[test]
    fn test_validate_colors_ok() {
        let data: ThemeData = toml::from_str(&music_player_rs::theme::default_dark_toml()).unwrap();
        assert!(validate_colors(&data.colors).is_ok());
    }

    #[test]
    fn test_validate_colors_bad_hex() {
        let mut data: ThemeData = toml::from_str(&music_player_rs::theme::default_dark_toml()).unwrap();
        data.colors.viz_3 = "zzzzzz".to_string();
        let err = validate_colors(&data.colors).unwrap_err();
        assert!(err.to_string().contains("viz_3"), "ошибка: {err}");
    }

    /// ТЗ-27, §6.9: без темы в черновике и с действующей темой
    /// (`Unchanged`) вид кнопки — ровно ответ ядра, подсказка пуста.
    #[test]
    fn save_button_view_follows_core_without_draft_theme() {
        let enabled = SaveButton { enabled: true, tooltip: None };
        assert_eq!(save_button_view(enabled.clone(), None), (true, String::new()));
        assert_eq!(save_button_view(enabled, Some(&ThemeLoad::Unchanged)), (true, String::new()));
    }

    /// ТЗ-27, §6.9: подсказка ядра о недопустимом значении сохраняется.
    #[test]
    fn save_button_view_keeps_core_tooltip() {
        let button = SaveButton { enabled: false, tooltip: Some("Буфер: недопустимая пара".to_string()) };
        assert_eq!(
            save_button_view(button, Some(&ThemeLoad::Unchanged)),
            (false, "Буфер: недопустимая пара".to_string())
        );
    }

    /// ТЗ-27, §6.9: прочитанная тема с недопустимым HEX-цветом делает кнопку
    /// неактивной; подсказка называет параметр «Тема» и поле; подсказка ядра,
    /// если она есть, имеет приоритет.
    #[test]
    fn save_button_view_blocks_ready_theme_with_bad_hex() {
        let mut data: ThemeData = toml::from_str(&music_player_rs::theme::default_dark_toml()).unwrap();
        data.colors.viz_3 = "zzzzzz".to_string();
        let theme = ThemeLoad::Ready(Arc::new(data));

        let (enabled, tooltip) = save_button_view(SaveButton { enabled: true, tooltip: None }, Some(&theme));
        assert!(!enabled);
        assert!(tooltip.starts_with("Тема: "), "подсказка: {tooltip}");
        assert!(tooltip.contains("viz_3"), "подсказка: {tooltip}");

        let core = SaveButton { enabled: false, tooltip: Some("Буфер: недопустимая пара".to_string()) };
        assert_eq!(save_button_view(core, Some(&theme)), (false, "Буфер: недопустимая пара".to_string()));
    }

    /// ТЗ-27, §6.9: прочитанная тема с допустимыми цветами ответ ядра не меняет.
    #[test]
    fn save_button_view_ready_theme_with_valid_colors() {
        let data: ThemeData = toml::from_str(&music_player_rs::theme::default_dark_toml()).unwrap();
        let theme = ThemeLoad::Ready(Arc::new(data));
        assert_eq!(
            save_button_view(SaveButton { enabled: true, tooltip: None }, Some(&theme)),
            (true, String::new())
        );
        assert_eq!(
            save_button_view(SaveButton { enabled: false, tooltip: None }, Some(&theme)),
            (false, String::new())
        );
    }

    #[test]
    fn test_startup_fallback_missing_file() {
        let dir = std::env::temp_dir().join(format!("mp_resolve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (data, was_fallback) = resolve_startup_theme("ghost", &dir).expect("built-in light theme");
        assert!(was_fallback);
        assert_eq!(data.standard_palette, music_player_rs::theme::StandardPalette::Light);
        assert_eq!(data.colors.accent, "#6750a4");
        assert!(!dir.join("settings.toml").exists(), "resolve не должен писать на диск");
    }

    // ── A3.5: Audio-tab tests (ТЗ A3.0 §4.1, §7.3, §8.1) ──────────────

    use crate::app::ui_manager::{audio_filter_matches, find_device_index_in};
    use cpal::{SampleFormat, SupportedBufferSize};
    use music_player_rs::audio::output::{DeviceCategory, DeviceInfo, RateRange};

    /// Minimal mock for [`DeviceInfo`]. Only the fields actually consumed by
    /// the filter projection are varied per test; the rest get safe defaults.
    fn mock_device(
        id: &str,
        name: &str,
        channels: u16,
        category: DeviceCategory,
        rates: Vec<u32>,
        formats: Vec<SampleFormat>,
    ) -> DeviceInfo {
        let ranges = rates
            .iter()
            .map(|&r| RateRange {
                channels,
                min: r,
                max: r,
                buffer_size: SupportedBufferSize::Range { min: 64, max: 4096 },
            })
            .collect();
        let mut seen_fmt = Vec::new();
        let mut dedup_fmt = Vec::new();
        for f in formats {
            if !seen_fmt.contains(&f) {
                seen_fmt.push(f);
                dedup_fmt.push(f);
            }
        }
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            channels,
            sample_rate: rates.first().copied().unwrap_or(44100),
            sample_format: dedup_fmt.first().copied().unwrap_or(SampleFormat::I32),
            buffer_size: SupportedBufferSize::Range { min: 64, max: 4096 },
            supported: ranges,
            category,
            supported_rates: rates,
            supported_formats: dedup_fmt,
        }
    }

    #[test]
    fn audio_filter_hardware_only_keeps_hw_drops_software() {
        let hw = mock_device("hw:PCH,0", "Intel HDA", 2, DeviceCategory::Hardware, vec![], vec![]);
        let sp = mock_device("pipewire", "PipeWire", 2, DeviceCategory::ServerProxy, vec![], vec![]);
        let virt = mock_device("dmix", "dmix", 2, DeviceCategory::Virtual, vec![], vec![]);
        assert!(audio_filter_matches(&hw, true, false));
        assert!(!audio_filter_matches(&sp, true, false));
        assert!(!audio_filter_matches(&virt, true, false));
        // Фильтр выключен — проходят все категории.
        assert!(audio_filter_matches(&hw, false, false));
        assert!(audio_filter_matches(&sp, false, false));
    }

    #[test]
    fn audio_filter_stereo_only_keeps_2ch_drops_multichannel() {
        let stereo = mock_device("hw:PCH,0", "Intel HDA", 2, DeviceCategory::Hardware, vec![], vec![]);
        let surround = mock_device("hw:DAC,0", "HiFi DAC", 6, DeviceCategory::Hardware, vec![], vec![]);
        assert!(audio_filter_matches(&stereo, false, true));
        assert!(!audio_filter_matches(&surround, false, true));
        // Оба фильтра сразу: hw + стерео.
        assert!(audio_filter_matches(&stereo, true, true));
        assert!(!audio_filter_matches(&surround, true, true));
        let virtual_mono = mock_device("dmix", "dmix", 1, DeviceCategory::Virtual, vec![], vec![]);
        assert!(!audio_filter_matches(&virtual_mono, false, true));
        assert!(!audio_filter_matches(&virtual_mono, true, true));
    }

    #[test]
    fn audio_filter_no_presets_accepts_anything() {
        let hw = mock_device("hw:PCH,0", "Intel HDA", 2, DeviceCategory::Hardware, vec![], vec![]);
        let sp = mock_device("pipewire", "PipeWire", 99, DeviceCategory::ServerProxy, vec![], vec![]);
        assert!(audio_filter_matches(&hw, false, false));
        assert!(audio_filter_matches(&sp, false, false));
    }

    #[test]
    fn find_device_index_in_matches_raw_id() {
        let pairs = vec![
            ("hw:PCH,0".into(), "Intel HDA".into()),
            ("pipewire".into(), "PipeWire (Software)".into()),
        ];
        assert_eq!(find_device_index_in(&pairs, "hw:PCH,0"), Some(0));
        assert_eq!(find_device_index_in(&pairs, "pipewire"), Some(1));
    }

    #[test]
    fn find_device_index_in_matches_label() {
        let pairs = vec![(
            "hw:DAC,0".into(),
            "HiFi DAC".into(),
        )];
        // Label «HiFi DAC» matches the label itself.
        assert_eq!(find_device_index_in(&pairs, "HiFi DAC"), Some(0));
    }

    #[test]
    fn find_device_index_in_strips_server_suffix() {
        let suffix = music_player_rs::i18n::audio::SERVER_NODE_SUFFIX;
        let pairs = vec![(
            "default".into(),
            format!("default{suffix}"),
        )];
        // Raw id «default» should match stripped label.
        assert_eq!(find_device_index_in(&pairs, "default"), Some(0));
        // Full label (with suffix) also matches.
        assert_eq!(
            find_device_index_in(&pairs, &format!("default{suffix}")),
            Some(0),
        );
    }

    #[test]
    fn find_device_index_in_returns_none_when_absent() {
        let pairs = vec![("hw:A,0".into(), "Device A".into())];
        assert_eq!(find_device_index_in(&pairs, "hw:B,0"), None);
        assert_eq!(find_device_index_in(&pairs, "ghost"), None);
    }

    use music_player_rs::audio::decoder::Tags;
    use music_player_rs::audio::verdict::BadgeState;
    use music_player_rs::persist::keys::{FileRead, Parsed};
    use music_player_rs::persist::state_file::parse_state;
    use music_player_rs::settings::playback_dto::{load_mode_settings, playback_dto, PlaybackDto};

    fn track_info() -> TrackInfo {
        TrackInfo {
            sample_rate: 44_100,
            channels: 2,
            num_frames: Some(441_000),
            format_name: "flac".to_string(),
            bitrate: 0,
            bits: Some(16),
            tags: Tags::default(),
        }
    }

    fn opened(req_gen: u64) -> EngineEvent {
        EngineEvent::Opened {
            req_gen,
            info: track_info(),
            stream_format: None,
            path: None,
            badge: BadgeState::NoStream,
        }
    }

    /// ТЗ-19, И-Р14: после перезапуска стартовые команды (`startup_commands`),
    /// построенные из `ModeSettings`/активного режима, прочитанных обратно из
    /// реальных форматов персистентности (без участия движка), совпадают с
    /// тем, что было бы отправлено из значений до перезапуска (ОВС-14,
    /// §6.27, ADR-22).
    #[test]
    fn startup_policy_equals_saved_policy() {
        let mut original = ModeSettings::default();
        original.compatible.dsd_gain_comp = !original.compatible.dsd_gain_comp;
        original.optimal.volume_lock = !original.optimal.volume_lock;

        // Политика режимов — через тот же DTO/TOML, что и `settings.toml`.
        let dto = playback_dto(&original);
        let toml_text = toml::to_string(&dto).expect("сериализация PlaybackDto в toml");
        let reloaded_dto: PlaybackDto = toml::from_str(&toml_text).expect("разбор toml в PlaybackDto");
        let (reloaded_modes, _notes) = load_mode_settings(Some(reloaded_dto));
        assert_eq!(reloaded_modes, original);

        // Активный режим — через реальный парсер `state.toml`.
        let state_bytes: Arc<[u8]> = Arc::from(b"playback.active = \"strict\"\n".as_slice());
        let reloaded_active = match parse_state(FileRead::Bytes(state_bytes)) {
            Parsed::Parsed { value, .. } => value.active_mode(),
            _ => panic!("ожидался Parsed::Parsed"),
        };
        assert_eq!(reloaded_active, ModeKind::Strict);

        let original_cmds = startup_commands(&original, ModeKind::Strict);
        let reloaded_cmds = startup_commands(&reloaded_modes, reloaded_active);

        for cmds in [&original_cmds, &reloaded_cmds] {
            match (&cmds[0], &cmds[1]) {
                (EngineCmd::SetModeSettings(update), EngineCmd::SetActiveMode(mode)) => {
                    assert!(update.diff.is_none());
                    assert_eq!(update.settings, original);
                    assert_eq!(*mode, ModeKind::Strict);
                }
                other => panic!("ожидались SetModeSettings/SetActiveMode, получено: {other:?}"),
            }
        }
    }

    /// Шаг 38 (ТЗ-143, ТЗ-134, ADR-24): автовыбор устройства сохраняется
    /// только в своём режиме — другой режим и его устройство не меняются;
    /// Совместимый режим автовыбора не знает (§2.3), поэтому для него
    /// вызывающая сторона не пишет файл.
    #[test]
    fn device_auto_selected_updates_only_target_mode() {
        let settings = PersistSettings::default();
        let device: HwDeviceId = "hw:CARD=USB,DEV=0".parse().expect("валидный HwDeviceId");

        let updated = settings_with_auto_selected_device(&settings, ModeKind::Optimal, &device)
            .expect("Оптимальный режим сохраняет автовыбор");
        assert_eq!(updated.playback.optimal.device, Some(device.clone()));
        assert_eq!(updated.playback.strict.device, settings.playback.strict.device);
        assert_eq!(updated.playback.compatible.device, settings.playback.compatible.device);

        assert!(settings_with_auto_selected_device(&settings, ModeKind::Compatible, &device).is_none());
    }

    /// ADR-02, ТЗ-79, ТЗ-83, §7.2: `Opened` обновляет зеркало состояния в
    /// UI-потоке и заново взводит и бейдж тракта, и кэш отчёта «Путь
    /// сигнала» — проверяется напрямую на чистой функции `apply_engine_event`,
    /// без конструирования `MusicApp`.
    #[test]
    fn apply_engine_event_opened_dirties_badge_and_report() {
        let mut state = UiAudioState::new();
        let mut badge_view = BadgeViewState::new();
        let mut path_cache = PathWindowCache::new();
        // Снять начальную «грязь» конструкторов, чтобы проверить именно
        // эффект события, а не стартовое значение.
        badge_view.take_view();
        path_cache.take_dirty(&state, &ModeSettings::default());

        state.begin_open(1);
        apply_engine_event(&mut state, &mut badge_view, &mut path_cache, opened(1));

        assert!(state.take_opened().is_some());
        assert!(badge_view.take_view().is_some());
        assert!(path_cache.take_dirty(&state, &ModeSettings::default()).is_some());
    }

    /// ADR-02, ТЗ-79, ТЗ-83, §7.2: `OpenFailed` (как и `Skipped`) не меняет
    /// снимок пути сигнала относительно уже пустого (`None` → `None`,
    /// `UiAudioState::take_path_dirty` не взводится), поэтому инвалидация
    /// кэша отчёта на `Applied::OpenFailed { .. }` в `apply_engine_event`
    /// — отдельная, явная ветка, а не следствие `take_path_dirty`.
    #[test]
    fn apply_engine_event_open_failed_invalidates_report_without_path_dirty() {
        let mut state = UiAudioState::new();
        let mut badge_view = BadgeViewState::new();
        let mut path_cache = PathWindowCache::new();
        // Снять начальную «грязь» конструкторов.
        badge_view.take_view();
        path_cache.take_dirty(&state, &ModeSettings::default());
        // Путь уже `None` до события — `take_path_dirty` ниже должен
        // остаться пустым, подтверждая, что инвалидация идёт не от него.
        assert!(state.take_path_dirty().is_none());

        state.begin_open(2);
        let applied = apply_engine_event(
            &mut state,
            &mut badge_view,
            &mut path_cache,
            EngineEvent::OpenFailed { req_gen: 2, err: OpenError::DeviceLost },
        );

        assert_eq!(applied, Applied::OpenFailed { err: OpenError::DeviceLost });
        assert!(state.take_path_dirty().is_none());
        assert!(path_cache.take_dirty(&state, &ModeSettings::default()).is_some());
    }

    /// ТЗ-68, §3.5, §7.2: `DynamicsChanged` для активной сессии взводит кэш
    /// отчёта «Путь сигнала», как и смена самого пути.
    #[test]
    fn apply_engine_event_dynamics_changed_invalidates_report() {
        use music_player_rs::audio::path::signal::DynamicPathState;

        let mut state = UiAudioState::new();
        let mut badge_view = BadgeViewState::new();
        let mut path_cache = PathWindowCache::new();

        state.begin_open(1);
        apply_engine_event(&mut state, &mut badge_view, &mut path_cache, opened(1));
        // Снять начальную «грязь» открытия сессии, чтобы проверить именно
        // эффект `DynamicsChanged`.
        badge_view.take_view();
        path_cache.take_dirty(&state, &ModeSettings::default());

        let dynamics = DynamicPathState { underruns: 1, ..DynamicPathState::default() };
        let applied = apply_engine_event(
            &mut state,
            &mut badge_view,
            &mut path_cache,
            EngineEvent::DynamicsChanged { session: 1, dynamics },
        );

        assert_eq!(applied, Applied::None);
        assert!(path_cache.take_dirty(&state, &ModeSettings::default()).is_some());
    }

    /// ТЗ-68, §3.5, §7.2: повторный `DynamicsChanged` с тем же значением не
    /// взводит кэш отчёта заново — пересчёта вхолостую нет (тот же принцип,
    /// что и `window_no_recompute_on_idle_ticks` в `path_view.rs`).
    #[test]
    fn apply_engine_event_same_dynamics_no_report_rebuild() {
        use music_player_rs::audio::path::signal::DynamicPathState;

        let mut state = UiAudioState::new();
        let mut badge_view = BadgeViewState::new();
        let mut path_cache = PathWindowCache::new();

        state.begin_open(1);
        apply_engine_event(&mut state, &mut badge_view, &mut path_cache, opened(1));
        badge_view.take_view();
        path_cache.take_dirty(&state, &ModeSettings::default());

        let dynamics = DynamicPathState { underruns: 1, ..DynamicPathState::default() };
        apply_engine_event(
            &mut state,
            &mut badge_view,
            &mut path_cache,
            EngineEvent::DynamicsChanged { session: 1, dynamics: dynamics.clone() },
        );
        path_cache.take_dirty(&state, &ModeSettings::default());

        apply_engine_event(
            &mut state,
            &mut badge_view,
            &mut path_cache,
            EngineEvent::DynamicsChanged { session: 1, dynamics },
        );
        assert!(path_cache.take_dirty(&state, &ModeSettings::default()).is_none());
    }

    /// ТЗ-68, §3.5, И-Р14: `DynamicsChanged` с чужой (не активной) сессией
    /// не инвалидирует кэш отчёта — тот же фильтр по `session`, что и у
    /// `Ended`.
    #[test]
    fn apply_engine_event_stale_dynamics_does_not_invalidate_report() {
        use music_player_rs::audio::path::signal::DynamicPathState;

        let mut state = UiAudioState::new();
        let mut badge_view = BadgeViewState::new();
        let mut path_cache = PathWindowCache::new();

        state.begin_open(1);
        apply_engine_event(&mut state, &mut badge_view, &mut path_cache, opened(1));
        badge_view.take_view();
        path_cache.take_dirty(&state, &ModeSettings::default());

        let dynamics = DynamicPathState { underruns: 7, ..DynamicPathState::default() };
        let applied = apply_engine_event(
            &mut state,
            &mut badge_view,
            &mut path_cache,
            EngineEvent::DynamicsChanged { session: 0, dynamics },
        );

        assert_eq!(applied, Applied::None);
        assert!(path_cache.take_dirty(&state, &ModeSettings::default()).is_none());
    }
}
