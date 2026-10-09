use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rfd::FileDialog;
use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem, VecModel};
use slint::language::{SortOrder, TableColumn};

use music_player_rs::audio::analyzer::TAP_CAPACITY;
use music_player_rs::audio::backend::{BackendError, SharedDeviceId};
use music_player_rs::audio::error::{EngineFault, FileError, OpenError, Reaction};
use music_player_rs::audio::output::{default_device_name, DeviceInfo};
use music_player_rs::audio::player::ReservationEvent;
use music_player_rs::audio::visualizer::{
    FreqScale, LevelScale, VisualizationMode, VisualizerConfig,
};
use music_player_rs::core::gate::{BlockReason, LoadKind, MainCmd, UiGate};
use music_player_rs::core::messages::{
    CloseEffect, Message, MessageButton, MessageButtons, MessageCenter, MessageLevel, MsgEffect,
};
use music_player_rs::core::exit::{ExitOutcome, ExitReason};
use music_player_rs::core::io::{CacheKind, CacheSizes, IoDone, IoJob, IoWorker};
use music_player_rs::core::AppCore;
use music_player_rs::cover::{self, CoverDone, CoverJob};
use music_player_rs::engine::messages::{EngineEvent, LegacyAudio, Notice, SkipReason};
use music_player_rs::engine::run::EngineHandle;
use music_player_rs::persist::settings_file::Settings as PersistSettings;
use music_player_rs::persist::settings_file::{ColumnsConfig, ThemeName};
use music_player_rs::persist::state_file::{
    effective_width_pct, LegacyPlaybackState, normalize_visible, widths_on_disable, widths_on_enable, Origin, StateChange, WidthPct,
};
use music_player_rs::persist::tracker::ReplyEffect;
use music_player_rs::persist::{ConfigPaths, WorkFile};
use music_player_rs::platform::lifecycle::PlatformCaps;
use music_player_rs::platform::notify::Notifier;
use music_player_rs::playlist::{self, ScanMsg, Track};
use music_player_rs::playlist::compare::{compare_keys, CompareKeys};
use music_player_rs::playlist::model::SortKey;
use music_player_rs::settings::{
    clamp_ring_buffer_ms, ClockFamily, ColumnId, DsdMode, ExclusiveMode, FallbackPolicy,
    FallbackRatePolicy, RepeatMode, ResamplerMode,
};
use music_player_rs::theme::{
    ColorsData, ThemeData, ThemeEntry, ThemeError, DEFAULT_LIGHT_TOML, parse_hex,
};
use music_player_rs::platform::lifecycle::TrayEvent;
use music_player_rs::platform::tray;

/// Фиксированные частоты выхода для `ResamplerMode::Fixed` (ТЗ A3.0 §7.3):
/// индекс ComboBox («Авто», «44.1k» … «192k») → Гц. «Авто» (0) → 0 = не задано.
const FIXED_RATES: [u32; 7] = [0, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

mod audio_facade;
pub mod bp_report;
pub(crate) mod engine_sink;
pub mod events;
pub mod fulltrack_manager;
pub mod playback_manager;
pub mod playlist_manager;
mod ui_audio_state;
pub mod ui_manager;
pub mod visualizer_manager;
pub mod viz_settings_manager;

use engine_sink::EngineEventQueue;
use events::AppEvent;
use fulltrack_manager::{FullCmd, FullEvt};
use ui_audio_state::Applied;

/// Last values pushed to the UI by the delta playback sync. Kept so the
/// 100 ms tick only re-writes properties that actually changed (e.g. no
/// seekbar churn while paused/stopped, no volume re-write unless it moved).
#[derive(Clone, PartialEq)]
struct UiState {
    playing: bool,
    muted: bool,
    volume: f32,
    bit_perfect: bool,
    bp_resample: bool,
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
            bp_resample: false,
            pos: String::new(),
            dur: String::new(),
            seek_fraction: -1.0,
            status: String::new(),
        }
    }
}

/// Черновик диалога настроек (§8.1 С3, И-Т7): настройки и правимые в
/// диалоге поля состояния (тип визуализации, ширины колонок §2.5). Применяется
/// целиком по «Сохранить», отбрасывается по «Отмена».
struct DialogDraft {
    settings: PersistSettings,
    viz_mode: VisualizationMode,
    column_widths: BTreeMap<ColumnId, WidthPct>,
}

/// Метаданные темы, показываемые в диалоге настроек (T1.0 §8.2):
/// name/description + полная валидность (структура + HEX, §6).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ThemeMeta {
    name: String,
    description: Option<String>,
    valid: bool,
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
    AppWindow::new()
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

/// Разрешает стартовую тему без GUI (§8.2/§6.1).
///
/// Цепочка fallback: `<name>.toml` (структура + HEX) → `light.toml` →
/// `DEFAULT_LIGHT_TOML`. Возвращает `(данные_темы, was_fallback)`; `None` —
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
        None => toml::from_str(DEFAULT_LIGHT_TOML).ok().map(|data| (data, true)),
    }
}

fn num_str(v: u32, suffix: &str) -> SharedString {
    if v > 0 { format!("{v}{suffix}").into() } else { "—".into() }
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
    player: audio_facade::AudioFacade,
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
    current: Option<usize>,
    /// Трек, открытие которого отправлено движку; применяется по событию
    /// Opened (ADR-01, ТЗ-103).
    pending_open: Option<(usize, Option<usize>)>,
    stream_desc: Option<music_player_rs::audio::player::StreamDesc>,
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
    /// Черновик диалога настроек (§8.1 С3, И-Т7): настройки меняются только
    /// через него и применяются на «Сохранить».
    dialog: Option<DialogDraft>,
    /// Транзитное состояние выбора темы в диалоге (T1.0 §5.4/§8.2): имя,
    /// выбранное в ComboBox, не пишется в draft/settings до «Сохранить».
    theme_selection: Option<String>,
    /// Метаданные темы, показанные под ComboBox (name/description/valid).
    theme_meta: Option<ThemeMeta>,
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
    /// Async startup playlist load: yields the persisted track list once it
    /// has been read off disk (avoids blocking UI init on large playlists).
    startup_tracks_rx: Option<Receiver<Vec<Track>>>,
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
    /// Ключ билда, уже показанный на UI (для дропа устаревших Ready).
    fulltrack_key: Option<String>,
    /// RAM-кэш построенных изображений по cache-ключу (режим+параметры).
    /// LRU: лимит записей из `viz_max_ram_mb` (см. `fulltrack_cache_max_entries`).
    /// Значение хранит фактический размер RGBA-буфера в байтах (для §10.5).
    fulltrack_cache: clru::CLruCache<String, (slint::Image, usize)>,
    /// Режим, под который построен текущий целевой билд (детект смены типа —
    /// смена режима применяется сразу, а не с debounce).
    fulltrack_mode: Option<music_player_rs::audio::visualizer::VisualizationMode>,
    /// Debounce перестроения полнотрековых при изменении параметров из диалога
    /// (ТЗ §9.2: 500 мс): (момент последнего изменения, целевой (path, key)).
    viz_debounce: Option<(Instant, Option<(PathBuf, String)>)>,
    /// Шлюз главного окна: причины блокировки (диалог/сообщение/выбор файла)
    /// и вид идущей загрузки плейлиста (ADR-12, ТЗ-23, ТЗ-48, §2.11).
    gate: UiGate,
    /// Центр сообщений: очередь до первого показа, по одному, сводное окно
    /// ошибок записи с «Повторить»/«OK» (ADR-13, §6.15, §6.8, ТЗ-52, ТЗ-20).
    messages: MessageCenter,
    /// Возможности платформы, которыми руководствуется `MessageCenter` при
    /// решении «окно или уведомление» (ADR-9, ТЗ-52 п. 3). До итога
    /// регистрации трея (`tray_ready`) — без трея и уведомлений (В-1).
    caps: PlatformCaps,
    /// Системные уведомления при окне в трее (ADR-9, ТЗ-52 п. 2).
    notifier: Box<dyn Notifier>,
    /// Причина отказа запуска движка, если `engine` — `None` (ТЗ-88, §2.10).
    /// Сам `EngineHandle` передан в `AudioFacade` (`self.player`) при
    /// конструировании (ADR-01, шаг 30); здесь хранится только причина
    /// отказа — для стартового сообщения пользователю.
    engine_fault: Option<EngineFault>,
    /// Приёмная сторона канала событий движка (ADR-02); разбирается в
    /// `drain_engine_events` каждый тик.
    engine_events: EngineEventQueue,
    /// Буфер разобранных событий движка, переиспользуется между тиками
    /// (ADR-02, И-Р13).
    engine_applied: Vec<Applied>,
    /// Счётчик треков, пропущенных подряд из-за ошибки файла; мост до
    /// SkipSeries С8 (ТЗ-85, ТЗ-86) — защита от бесконечного пропуска, если
    /// повреждена вся библиотека при Repeat All.
    skip_streak: usize,
    /// Поток `apap-io` (ADR-20, §2.12): чтение тем, размеры и очистка
    /// дисковых кэшей вне UI-потока. Спецификация помещает `io` в `AppDeps`
    /// `AppCore` (§2.12); ОТКЛОНЕНИЕ: до этапа С9 диалог настроек и шлюз UI
    /// остаются в `MusicApp`, поэтому воркер временно хранится здесь же
    /// (переезд в `AppCore` — на С9).
    io: Box<dyn IoWorker>,
    /// Текущее поколение задания на каждый вид ответа `apap-io` (ADR-20, §4).
    io_gens: IoGens,
    /// Последние дисковые размеры кэшей, полученные от `apap-io`; показаны
    /// сразу при открытии диалога, пока свежий подсчёт не пришёл (ТЗ-22,
    /// ADR-20).
    cache_disk: CacheSizes,
}

impl MusicApp {
    /// `paths` и `AppCore` (с писателем и журналом) строит `main` (ADR-19): приложение не ищет
    /// каталог настроек пользователя само, поэтому тесты его не трогают (ТЗ-49).
    /// `tray` — UI-концы каналов трея; сам трей запускает `Lifecycle::install`
    /// (ADR-23 шаг 8). `notifier` — системные уведомления (ADR-9).
    /// `engine`/`engine_events` — поток `apap-engine` и канал его событий,
    /// запущенные в `main` до `AppCore` (ADR-01, ADR-19, ТЗ-88); движок ещё
    /// не подключён к логике приложения на этом шаге.
    /// `io` — поток `apap-io` (ADR-20, §2.12), запущенный в `main`;
    /// внедряется через параметр, а не создаётся здесь, чтобы тесты могли
    /// подставить свою реализацию `IoWorker`.
    // Конструктор собирает инжектируемые зависимости (ADR-19, ADR-20) —
    // восьмой параметр (`io`) не повод вводить промежуточную структуру.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        window: &AppWindow,
        core: AppCore,
        paths: ConfigPaths,
        tray: tray::TrayChannels,
        notifier: Box<dyn Notifier>,
        engine: Result<EngineHandle, EngineFault>,
        engine_events: EngineEventQueue,
        io: Box<dyn IoWorker>,
    ) -> Self {
        let (engine, engine_fault) = match engine {
            Ok(handle) => (Some(handle), None),
            Err(fault) => (None, Some(fault)),
        };
        let pb_state = core.state().playback();
        let pb = &core.settings().playback;
        // Мост С3: стартовые звуковые настройки движка уходят в `LegacyAudio`
        // через `AudioFacade::new` (ADR-01, ADR-04, ТЗ-134), а не поштучными
        // сеттерами `Player` — так конструктор фасада сразу знает полную
        // легаси-конфигурацию (`SetLegacyAudio` на старте не нужен).
        let legacy = LegacyAudio {
            exclusive_mode: pb.audio.exclusive,
            fallback_policy: pb.audio.fallback,
            dsd_mode: pb.dsd.mode,
            resampler_mode: pb.audio.resampler.mode,
            resampler_algorithm: pb.audio.resampler.algorithm,
            fixed_rate: pb.audio.resampler.fixed_rate,
            prefer_family: pb.audio.resampler.prefer_family,
            fallback_rate: pb.audio.resampler.fallback_rate,
            ring_buffer_ms: clamp_ring_buffer_ms(pb.audio.ring_buffer_ms),
            bit_perfect: pb.audio.bit_perfect,
            dither: pb.audio.resampler.dither,
        };
        let mut player = audio_facade::AudioFacade::new(engine, legacy);
        // Громкость/mute/предпочитаемое устройство — не часть `LegacyAudio`
        // (это UI-состояние и выбор устройства, не DSP-политика движка),
        // поэтому применяются сеттерами фасада после конструирования.
        player.set_volume(pb_state.gain());
        player.set_muted(pb_state.muted);
        if !pb.audio_device.is_empty() {
            player.set_preferred_device(pb.audio_device.clone());
        }

        // Load the persisted playlist in the background so a large library
        // doesn't block window construction; `tick()` applies it on arrival.
        let (startup_tx, startup_tracks_rx) = channel::<Vec<Track>>();
        let startup_path = paths.playlist.clone();
        thread::spawn(move || {
            let tracks = playlist::load_track_list(&startup_path);
            let _ = startup_tx.send(tracks);
        });
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

        let repeat = core.state().repeat();
        let shuffle = core.state().shuffle();

        // Output device availability is no longer probed synchronously here:
        // the engine thread enumerates devices in the background and reports
        // them via `EngineEvent::Devices`/`Notice::DevicesUnavailable`, applied
        // in `drain_engine_events`/`dispatch_applied` (ТЗ-104, ADR-16). The
        // window must show before that first report arrives, so the initial
        // state is "unknown", not an error.
        let saved_device = core.settings().playback.audio_device.clone();
        let audio_ready = false;
        let audio_error = None;
        let active_device = if saved_device.is_empty() {
            String::from("default")
        } else {
            saved_device.clone()
        };
        let startup_status = String::from("Audio: checking device\u{2026}");

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
            player,
            playlist_rows,
            playlist_cols,
            current: None,
            pending_open: None,
            stream_desc: None,
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
            dialog: None,
            theme_selection: None,
            theme_meta: None,
            cover_tx: Some(cover_tx),
            cover_rx: Some(cover_done_rx),
            cover_gen: 0,
            audio_devices_rx: None,
            audio_devices_pairs: Vec::new(),
            audio_device_infos: Vec::new(),
            startup_tracks_rx: Some(startup_tracks_rx),
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
            fulltrack_key: None,
            fulltrack_cache: clru::CLruCache::new(cache_max_entries),
            fulltrack_mode: None,
            viz_debounce: None,
            gate: UiGate::default(),
            messages: MessageCenter::default(),
            // Трей и уведомления — после регистрации значка (poll_tray, ADR-6).
            caps: PlatformCaps::default(),
            notifier,
            engine_fault,
            engine_events,
            engine_applied: Vec::new(),
            skip_streak: 0,
            io,
            io_gens: IoGens::default(),
            cache_disk: CacheSizes::default(),
        };
        // Загрузка плейлиста при старте ещё не завершена (фон, выше) —
        // список недоступен до её окончания (ТЗ-48, §2.11).
        app.gate.set_loading(Some(LoadKind::Startup));
        app.setup_fulltrack();
        app.setup_visualizer(viz_cfg, viz_prod, viz_cons);
        // Ключ сортировки сессии применяется в `drain_startup_tracks`, когда
        // реальные треки загружены (здесь `tracks` всегда пуст) (§6.13, ТЗ-42).
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
                title: "Аудио-движок не запущен".into(),
                body: format!("{fault:?}").into(),
                buttons: MessageButtons::Ok,
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
            self.gate.unblock(BlockReason::Message);
        }
        if let Some(message) = show {
            let level = match message.level {
                MessageLevel::Info => 0,
                MessageLevel::Warning => 1,
                MessageLevel::Error => 2,
            };
            let retry = message.buttons == MessageButtons::RetryOk;
            if let Some(ui) = &ui {
                ui.set_msg_level(level);
                ui.set_msg_title(message.title.as_ref().into());
                ui.set_msg_body(message.body.as_ref().into());
                ui.set_msg_retry(retry);
                ui.set_msg_shown(true);
            }
            self.gate.block(BlockReason::Message);
        }
        self.sync_gate_ui();
    }

    /// Синхронизировать `window-blocked` со шлюзом после любой его мутации
    /// (ADR-12, ТЗ-23).
    pub(super) fn sync_gate_ui(&mut self) {
        if let Some(ui) = self.try_ui() {
            ui.set_window_blocked(self.gate.window_blocked());
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
    /// отбрасывается, окно сообщений и очередь закрываются без вопроса,
    /// геометрия окна фиксируется в состоянии; `AppCore::exit` освобождает
    /// движок, отправляет итоговые снимки писателю и ждёт ответов не дольше
    /// бюджета выхода. Повторный запрос во время/после выхода игнорируется
    /// (`ExitOutcome::Ignored`). Для `WindowsSessionEnd`/`MacosTerminate`
    /// цикл событий Slint не останавливается — процесс завершит ОС (ADR-7,
    /// ТЗ-15, §6.10).
    pub(super) fn exit(&mut self, reason: ExitReason) -> ExitOutcome {
        self.dialog = None;
        if let Some(ui) = self.try_ui() {
            ui.set_settings_open(false);
        }
        self.gate.unblock(BlockReason::Dialog);
        self.messages.dismiss_for_exit();
        if let Some(ui) = self.try_ui() {
            ui.set_msg_shown(false);
        }
        self.gate.unblock(BlockReason::Message);
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
        let player = &mut self.player;
        let mut await_engine = || {
            events.wait_for(
                ENGINE_SHUTDOWN_WAIT.saturating_sub(requested.elapsed()),
                |ev| matches!(ev, EngineEvent::ShutdownComplete),
                |ev| {
                    let _ = player.on_event(ev);
                },
            )
        };
        let outcome = self.core.exit(reason, &mut await_engine);
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
                    title: "Тема не загружена".into(),
                    body: format!(
                        "Тема \"{theme_name}\" не загружена, применена светлая тема"
                    )
                    .into(),
                    buttons: MessageButtons::Ok,
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

    /// Действующие настройки для чтения: черновик диалога, если он открыт,
    /// иначе применённые настройки ядра (§8.1 С3, И-Т7).
    fn cfg(&self) -> &PersistSettings {
        self.dialog.as_ref().map_or(self.core.settings(), |d| &d.settings)
    }

    /// Тип визуализации с учётом черновика диалога (§2.5, §8.1 С3).
    fn cfg_viz_mode(&self) -> VisualizationMode {
        self.dialog.as_ref().map_or(self.core.state().viz_mode(), |d| d.viz_mode)
    }

    /// Ширины колонок с учётом черновика диалога (§2.5, §8.1 С3).
    fn cfg_column_widths(&self) -> &BTreeMap<ColumnId, WidthPct> {
        self.dialog.as_ref().map_or(self.core.state().column_widths(), |d| &d.column_widths)
    }

    /// Черновик диалога для правки; `None` — диалог закрыт: настройки
    /// меняются только через «Сохранить» (§8.1 С3, И-Т7).
    fn dialog_mut(&mut self) -> Option<&mut DialogDraft> {
        self.dialog.as_mut()
    }

    /// Правка настроек черновика; при закрытом диалоге — ничего (§8.1 С3, И-Т7).
    fn edit_cfg(&mut self, f: impl FnOnce(&mut PersistSettings)) {
        if let Some(d) = self.dialog_mut() {
            f(&mut d.settings);
        }
    }

    /// Открыть черновик диалога из текущего состояния ядра (§8.1 С3).
    fn open_dialog_draft(&mut self) {
        self.dialog = Some(DialogDraft {
            settings: self.core.settings().clone(),
            viz_mode: self.core.state().viz_mode(),
            column_widths: self.core.state().column_widths().clone(),
        });
    }

    fn push_bp_report_to_ui(&self, report: bp_report::BpReport) {
        let Some(ui) = self.try_ui() else { return };
        ui.set_bp_stream_desc(report.stream_desc.into());
        ui.set_bp_source_desc(report.source_desc.into());

        let reasons: Vec<BpReason> = report
            .reasons
            .into_iter()
            .map(|r| BpReason {
                title: r.title.into(),
                detail: r.detail.into(),
                action_id: r.action_id,
                action_label: r.action_label.into(),
                severity: match r.severity {
                    bp_report::Severity::Warning => 0,
                    bp_report::Severity::Info => 1,
                },
            })
            .collect();
        ui.set_bp_reasons(ModelRc::from(reasons.as_slice()));

        let positives: Vec<SharedString> = report.positives.into_iter().map(SharedString::from).collect();
        ui.set_bp_positives(ModelRc::from(positives.as_slice()));
    }

    // ---------------- Тема: состояние диалога настроек (T1.0) ----------------

    /// Загружает тему по имени файла и валидирует структуру + HEX (§2.3/§6).
    /// `None` — файл отсутствует или структурно повреждён.
    fn load_theme_meta(name: &str, themes_dir: &Path) -> Option<ThemeMeta> {
        let data = ThemeData::load_from_file(&themes_dir.join(format!("{name}.toml"))).ok()?;
        Some(ThemeMeta {
            name: data.name,
            description: data.description,
            valid: validate_colors(&data.colors).is_ok(),
        })
    }

    /// Применяет метаданные темы к UI диалога (§5.2/§5.3). `None` — тема не
    /// найдена в `themes/`; `name` пуст — файл есть, но не читается/невалиден.
    fn set_theme_meta(&mut self, meta: Option<ThemeMeta>) {
        self.theme_meta = meta;
        let (name, desc, valid) = match &self.theme_meta {
            Some(m) if !m.name.is_empty() => (
                m.name.clone(),
                m.description.clone().unwrap_or_default(),
                m.valid,
            ),
            Some(_) => (
                "(не удалось загрузить метаданные)".to_string(),
                String::new(),
                false,
            ),
            None => ("(тема не найдена)".to_string(), String::new(), false),
        };
        if let Some(ui) = self.try_ui() {
            ui.set_theme_name(name.into());
            ui.set_theme_description(desc.into());
            ui.set_theme_save_enabled(valid);
        }
    }

    /// Запрашивает список тем при открытии диалога через поток `apap-io`
    /// (ТЗ-22, ADR-20, §6.2): без обхода каталога `themes/` в UI-потоке. Save
    /// блокируется до ответа; список и метаданные текущей темы применяются
    /// `apply_theme_list`/`apply_theme_read` в `drain_io`, когда ответ
    /// приходит.
    fn populate_theme_ui(&mut self) {
        self.io_gens.themes = self.io_gens.themes.wrapping_add(1);
        self.io.submit(IoJob::ListThemes { gen: self.io_gens.themes });
        if let Some(ui) = self.try_ui() {
            ui.set_theme_save_enabled(false);
        }
    }

    /// Применяет ответ `IoJob::ListThemes` (ТЗ-22, ADR-20, §6.2): заполняет
    /// список тем и текущую тему в UI. Текущая тема может быть удалена (не
    /// найдена в списке → §5.1) — метаданные не запрашиваются, иначе читается
    /// её файл через `IoJob::ReadTheme` (ответ разбирает `apply_theme_read`).
    fn apply_theme_list(&mut self, entries: Vec<ThemeEntry>) {
        let names: Vec<SharedString> = entries.iter().map(|e| e.file_stem.clone().into()).collect();
        let current = self.core.settings().theme.as_str().to_string();
        if let Some(ui) = self.try_ui() {
            ui.set_theme_list_model(ModelRc::from(names.as_slice()));
            ui.set_theme_current(current.clone().into());
        }
        let found = entries.iter().any(|e| e.file_stem == current);
        match found.then(|| ThemeName::new(current.clone())).flatten() {
            Some(name) => {
                self.io_gens.theme = self.io_gens.theme.wrapping_add(1);
                self.io.submit(IoJob::ReadTheme { gen: self.io_gens.theme, name });
            }
            None => self.set_theme_meta(None),
        }
    }

    /// Применяет ответ `IoJob::ReadTheme` (ТЗ-22, ADR-20, §5.2/§6.2): `Ok` —
    /// метаданные с полной валидацией (структура + HEX); `Err` — файл есть,
    /// но не читается/невалиден (Save заблокирован, как при сломанном файле
    /// до переноса в `apap-io`).
    fn apply_theme_read(&mut self, result: Result<Arc<ThemeData>, Box<str>>) {
        let meta = match result {
            Ok(data) => Some(ThemeMeta {
                name: data.name.clone(),
                description: data.description.clone(),
                valid: validate_colors(&data.colors).is_ok(),
            }),
            Err(_) => Some(ThemeMeta {
                name: String::new(),
                description: None,
                valid: false,
            }),
        };
        self.set_theme_meta(meta);
    }

    /// Обработчик выбора темы в ComboBox (§5.4/§6.3): в draft/settings не
    /// пишет — показывает метаданные и запоминает имя в `theme_selection`.
    fn on_settings_theme_selected(&mut self, name: &str) {
        let themes_dir = self.paths.dir.join("themes");
        let meta = Self::load_theme_meta(name, &themes_dir).or(Some(ThemeMeta {
            name: String::new(),
            description: None,
            valid: false,
        }));
        if meta.as_ref().is_some_and(|m| m.valid) {
            self.theme_selection = Some(name.to_string());
        }
        self.set_theme_meta(meta);
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
            if !a.gate.allows(MainCmd::SortBy(col)) {
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
                if !a.gate.allows(MainCmd::PlayPause) {
                    return;
                }
                if !a.player.has_decoder() {
                    let idx = a.current.unwrap_or(0);
                    if idx < a.track_count() {
                        a.play_track(idx);
                    }
                    return;
                }
                a.player.toggle();
                if a.player.is_playing() {
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
                if !a.gate.allows(MainCmd::Stop) {
                    return;
                }
                a.player.stop();
                a.emit(AppEvent::PlaybackStopped);
            });
        }

        // 3. prev-track
        {
            let app = this.clone();
            ui.on_prev_track(move || {
                eprintln!("[gui] prev_track");
                let mut a = app.borrow_mut();
                if !a.gate.allows(MainCmd::Prev) {
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
                if !a.gate.allows(MainCmd::Next) {
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
                if !a.gate.allows(MainCmd::Repeat) {
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
                if !a.gate.allows(MainCmd::Shuffle) {
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
                if !app.borrow().gate.allows(MainCmd::Seek) {
                    return;
                }
                let duration = app.borrow().player.snapshot().2.unwrap_or(0.0);
                app.borrow_mut().player.seek(fraction as f64 * duration);
            });
        }

        // 7b. seek-commit — сикбар отпущен (конец драга): один seek в точку.
        {
            let app = this.clone();
            ui.on_seek_commit(move |fraction| {
                eprintln!("[gui] seek_commit fraction={fraction:.3}");
                if !app.borrow().gate.allows(MainCmd::Seek) {
                    return;
                }
                let duration = app.borrow().player.snapshot().2.unwrap_or(0.0);
                app.borrow_mut().player.seek(fraction as f64 * duration);
            });
        }

        // 8. volume-changed
        {
            let app = this.clone();
            ui.on_volume_changed(move |volume| {
                eprintln!("[gui] volume_changed volume={volume:.3}");
                let mut a = app.borrow_mut();
                if !a.gate.allows(MainCmd::Volume) {
                    return;
                }
                // Bit-perfect (Direct Output) moves the volume stage to the
                // external DAC/amp: a stray slider/wheel event must never
                // insert a software gain into the untouched stream.
                if a.player.bit_perfect() {
                    return;
                }
                a.player.set_volume(volume);
                a.core.change_state(Origin::User, StateChange::Volume(LegacyPlaybackState::pct_from_gain(volume)));
                // Persisted at exit (save-at-exit); no per-slider-move disk I/O.
                a.emit(AppEvent::VolumeChanged(volume));
            });
        }

        // 9. toggle-mute
        {
            let app = this.clone();
            ui.on_toggle_mute(move || {
                eprintln!("[gui] toggle_mute");
                let mut a = app.borrow_mut();
                if !a.gate.allows(MainCmd::ToggleMute) {
                    return;
                }
                a.player.toggle_mute();
            });
        }

        // 10. play-track (double-click detection: single click selects, double-click plays)
        {
            let app = this.clone();
            ui.on_play_track(move |index| {
                eprintln!("[gui] play_track index={index}");
                let mut a = app.borrow_mut();
                if !a.gate.allows(MainCmd::PlayRow) {
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
                let mut a = app.borrow_mut();
                if !a.gate.allows(MainCmd::OpenSettings) {
                    return;
                }
                a.open_dialog_draft();
                // T1.0 §6.2: свежий диалог → сброс транзитного выбора темы и
                // пересборка списка/метаданных из themes/*.toml.
                a.theme_selection = None;
                a.populate_theme_ui();
                // Re-push every dialog field from the real settings: a reopened
                // dialog must show current values, never a stale un-applied draft.
                a.sync_settings_to_ui();
                a.sync_viz_settings_to_ui();
                a.sync_audio_devices();
                a.sync_audio_advanced();
                a.sync_dsd_chain_desc();
                a.sync_cover_settings_to_ui();
                // Размеры дисковых кэшей считаются один раз при открытии
                // диалога потоком `apap-io` (ТЗ-22, ADR-20); до ответа
                // показываем последние известные значения.
                a.io_gens.sizes = a.io_gens.sizes.wrapping_add(1);
                a.io.submit(IoJob::CacheSizes { gen: a.io_gens.sizes });
                let cache_disk = a.cache_disk;
                a.apply_cache_sizes(&cache_disk);
                if let Some(ui) = a.try_ui() {
                    ui.set_settings_open(true);
                }
                a.gate.block(BlockReason::Dialog);
                a.sync_gate_ui();
            });
        }

        // 14. add-files
        {
            let app = this.clone();
            ui.on_add_files(move || {
                eprintln!("[gui] add_files");
                if !app.borrow().gate.allows(MainCmd::AddFiles) {
                    return;
                }
                {
                    let mut a = app.borrow_mut();
                    a.gate.block(BlockReason::FilePicker);
                    a.sync_gate_ui();
                }
                let dialog = FileDialog::new()
                    .add_filter(
                        "Audio",
                        &["flac", "mp3", "ogg", "wav", "aac", "m4a", "dsf", "aiff"],
                    )
                    .pick_files();
                {
                    let mut a = app.borrow_mut();
                    a.gate.unblock(BlockReason::FilePicker);
                    a.sync_gate_ui();
                }
                if let Some(paths) = dialog {
                    app.borrow_mut().start_scan(paths);
                }
            });
        }

        // 15. add-folder
        {
            let app = this.clone();
            ui.on_add_folder(move || {
                eprintln!("[gui] add_folder");
                if !app.borrow().gate.allows(MainCmd::AddFolder) {
                    return;
                }
                {
                    let mut a = app.borrow_mut();
                    a.gate.block(BlockReason::FilePicker);
                    a.sync_gate_ui();
                }
                let folder = FileDialog::new().pick_folder();
                {
                    let mut a = app.borrow_mut();
                    a.gate.unblock(BlockReason::FilePicker);
                    a.sync_gate_ui();
                }
                if let Some(folder) = folder {
                    app.borrow_mut().start_scan(vec![folder]);
                }
            });
        }

        // 16. save-playlist
        {
            let app = this.clone();
            ui.on_save_playlist(move || {
                eprintln!("[gui] save_playlist");
                let mut a = app.borrow_mut();
                if !a.gate.allows(MainCmd::SavePlaylist) {
                    return;
                }
                a.save_playlist();
            });
        }

        // 17. load-playlist
        {
            let app = this.clone();
            ui.on_load_playlist(move || {
                eprintln!("[gui] load_playlist");
                if !app.borrow().gate.allows(MainCmd::LoadPlaylist) {
                    return;
                }
                {
                    let mut a = app.borrow_mut();
                    a.gate.block(BlockReason::FilePicker);
                    a.sync_gate_ui();
                }
                let picked = FileDialog::new()
                    .add_filter("Playlist", &["m3u", "m3u8"])
                    .pick_file();
                {
                    let mut a = app.borrow_mut();
                    a.gate.unblock(BlockReason::FilePicker);
                    a.sync_gate_ui();
                }
                if let Some(path) = picked {
                    let tracks = playlist::load_track_list(&path);
                    let mut a = app.borrow_mut();
                    // МОСТ (§6.13, ТЗ-42, ТЗ-45): строки для модели плейлиста
                    // и восстановление видимого порядка по ключу сессии.
                    let rows: Vec<(Track, CompareKeys)> = tracks
                        .into_iter()
                        .map(|t| {
                            let keys = CompareKeys::from_track(&t);
                            (t, keys)
                        })
                        .collect();
                    let key = a.session_sort_key();
                    let visible = MusicApp::load_visible_order(&rows, key);
                    a.core.playlist_replace(rows, visible, key);
                    a.rebuild_known_paths();
                    a.current = None;
                    a.sync_playlist_to_ui();
                    a.emit(AppEvent::QueueChanged);
                }
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
                a.dialog = None;
                if let Some(ui) = a.try_ui() {
                    ui.set_settings_open(false);
                }
                a.gate.unblock(BlockReason::Dialog);
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
                a.edit_cfg(|s| s.playback.audio_device = raw);
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

        // 25b. settings-set-dsd-mode (draft-only: применяется на Save)
        {
            let app = this.clone();
            ui.on_settings_set_dsd_mode(move |idx| {
                eprintln!("[gui] settings_set_dsd_mode idx={idx}");
                let mut a = app.borrow_mut();
                let Some(mode) = DsdMode::from_index(idx) else {
                    eprintln!("[gui] settings_set_dsd_mode: invalid index {idx}");
                    return;
                };
                a.edit_cfg(|s| s.playback.dsd.mode = mode);
                // Live-обновление предупреждения в диалоге: при выборе
                // Native/DoP конфликт исчезает немедленно.
                a.sync_dsd_settings_to_ui();
                // Цепочка DSD (Native → DoP → PCM и т.д.) меняется вместе с
                // режимом (ТЗ A3.0 §4.1/§8.3).
                a.sync_dsd_chain_desc();
            });
        }

        // 25c. settings-set-bit-perfect (draft-only: применяется на Save)
        {
            let app = this.clone();
            ui.on_settings_set_bit_perfect(move |enabled| {
                eprintln!("[gui] settings_set_bit_perfect enabled={enabled}");
                let mut a = app.borrow_mut();
                a.edit_cfg(|s| s.playback.audio.bit_perfect = enabled);
                // Live-обновление предупреждения DSD в диалоге.
                a.sync_dsd_settings_to_ui();
            });
        }

        // 25d-25m. Audio-tab draft-only controls (ТЗ A3.0 §7.1/§7.7): колбэки
        // пишут только в draft; реальные сеттеры Player применяются в Save.
        // 25d. settings-set-audio-filter-hardware (ТЗ A3.0 §8.1, draft-only)
        {
            let app = this.clone();
            ui.on_settings_set_audio_filter_hardware(move |enabled| {
                app.borrow_mut().edit_cfg(|s| s.playback.audio.filter_hardware_only = enabled);
                app.borrow_mut().apply_audio_filter();
            });
        }

        // 25e. settings-set-audio-filter-stereo (ТЗ A3.0 §8.1, draft-only)
        {
            let app = this.clone();
            ui.on_settings_set_audio_filter_stereo(move |enabled| {
                app.borrow_mut().edit_cfg(|s| s.playback.audio.filter_stereo_only = enabled);
                app.borrow_mut().apply_audio_filter();
            });
        }

        // 25f. settings-set-audio-toggle-advanced — чисто верстка коллапса.
        {
            let app = this.clone();
            ui.on_settings_set_audio_toggle_advanced(move |open| {
                if let Some(ui) = app.borrow().try_ui() {
                    ui.set_settings_audio_advanced_open(open);
                }
            });
        }

        // 25f-b. bit-perfect badge -> report dialog
        {
            let app = this.clone();
            ui.on_bp_clicked(move || {
                let a = app.borrow_mut();
                if let Some(ui) = a.try_ui() {
                    ui.set_bp_report_open(true);
                }
                let report = bp_report::build_bp_report(&bp_report::bp_inputs(&a));
                a.push_bp_report_to_ui(report);
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
            ui.on_bp_report_action(move |id| {
                let mut a = app.borrow_mut();
                bp_report::apply_action(&mut a, id);
                let report = bp_report::build_bp_report(&bp_report::bp_inputs(&a));
                a.push_bp_report_to_ui(report);
            });
        }

        {
            let app = this.clone();
            ui.on_bp_report_open_settings(move || {
                let mut a = app.borrow_mut();
                if let Some(ui) = a.try_ui() {
                    ui.set_bp_report_open(false);
                    ui.set_settings_open(true);
                }
                a.open_dialog_draft();
                a.sync_audio_devices();
                a.sync_audio_advanced();
            });
        }

        // 25g. settings-set-audio-exclusive (ТЗ A3.0 §7.3, draft-only)
        {
            let app = this.clone();
            ui.on_settings_set_audio_exclusive(move |i| {
                eprintln!("[gui] settings_set_audio_exclusive idx={i}");
                let mut a = app.borrow_mut();
                let excl = ExclusiveMode::from_index(i).unwrap_or(ExclusiveMode::Auto);
                a.edit_cfg(|s| s.playback.audio.exclusive = excl);
                a.sync_capabilities_and_validation();
            });
        }

        // 25h. settings-set-audio-fallback (ТЗ A3.0 §7.3, draft-only)
        {
            let app = this.clone();
            ui.on_settings_set_audio_fallback(move |i| {
                eprintln!("[gui] settings_set_audio_fallback idx={i}");
                let mut a = app.borrow_mut();
                let fb = FallbackPolicy::from_index(i).unwrap_or(FallbackPolicy::Nearest);
                a.edit_cfg(|s| s.playback.audio.fallback = fb);
                a.sync_capabilities_and_validation();
            });
        }

        // 25i. settings-set-audio-resampler-mode (ТЗ A3.0 §7.3/§7.7): при
        // выборе Fixed с активным Fallback = Fail принудительно сбрасываем
        // fallback на Nearest — исходное сочетание невозможно
        // (`resampler_fixed_fallback_guard`).
        {
            let app = this.clone();
            ui.on_settings_set_audio_resampler_mode(move |i| {
                eprintln!("[gui] settings_set_audio_resampler_mode idx={i}");
                let mut a = app.borrow_mut();
                let mode = ResamplerMode::from_index(i).unwrap_or(ResamplerMode::Auto);
                let cur_fb = a.cfg().playback.audio.fallback;
                let guarded = resampler_fixed_fallback_guard(mode, cur_fb);
                let forced = guarded != cur_fb;
                a.edit_cfg(|s| {
                    s.playback.audio.resampler.mode = mode;
                    if forced {
                        s.playback.audio.fallback = guarded;
                    }
                });
                if forced {
                    // Автоматическая правка черновика настроек, видимая сразу
                    // через sync_audio_advanced (комбобокс Fallback в диалоге)
                    // — сообщение не требуется (ТЗ-52, ОВ-7).
                    a.sync_audio_advanced();
                }
                a.sync_capabilities_and_validation();
            });
        }

        // 25j. settings-set-audio-fixed-rate (ТЗ A3.0 §7.3, draft-only)
        {
            let app = this.clone();
            ui.on_settings_set_audio_fixed_rate(move |i| {
                eprintln!("[gui] settings_set_audio_fixed_rate idx={i}");
                let mut a = app.borrow_mut();
                let rate = FIXED_RATES.get(i as usize).copied().unwrap_or_default();
                a.edit_cfg(|s| s.playback.audio.resampler.fixed_rate = rate);
                a.sync_capabilities_and_validation();
            });
        }

        // 25k. settings-set-audio-clock-family (ТЗ A3.0 §7.3, draft-only)
        {
            let app = this.clone();
            ui.on_settings_set_audio_clock_family(move |i| {
                eprintln!("[gui] settings_set_audio_clock_family idx={i}");
                let mut a = app.borrow_mut();
                let family = ClockFamily::from_index(i).unwrap_or(ClockFamily::Auto);
                a.edit_cfg(|s| s.playback.audio.resampler.prefer_family = family);
                a.sync_capabilities_and_validation();
            });
        }

        // 25l. settings-set-audio-fallback-rate (ТЗ A3.0 §7.3, draft-only)
        {
            let app = this.clone();
            ui.on_settings_set_audio_fallback_rate(move |i| {
                eprintln!("[gui] settings_set_audio_fallback_rate idx={i}");
                let mut a = app.borrow_mut();
                let policy = FallbackRatePolicy::from_index(i).unwrap_or(FallbackRatePolicy::Nearest);
                a.edit_cfg(|s| s.playback.audio.resampler.fallback_rate = policy);
                a.sync_capabilities_and_validation();
            });
        }

        // 25m. settings-set-audio-ring-buffer-ms (ТЗ A3.0 §7.3, draft-only)
        {
            let app = this.clone();
            ui.on_settings_set_audio_ring_buffer_ms(move |ms| {
                eprintln!("[gui] settings_set_audio_ring_buffer_ms ms={ms}");
                let mut a = app.borrow_mut();
                let ring_ms = music_player_rs::settings::clamp_ring_buffer_ms(ms.max(0) as u32);
                a.edit_cfg(|s| s.playback.audio.ring_buffer_ms = ring_ms);
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
                    let ordered = a.cfg().columns.ordered_columns();
                    let Some(&col) = ordered.get(idx) else { return };
                    let visible = a.cfg().columns.column_visible(col);
                    (col, visible)
                };
                let mut a = app.borrow_mut();
                // Видимость — настройка, ширина — состояние сессии; обе
                // правятся в черновике (§2.5, §8.1 С3).
                if let Some(d) = a.dialog_mut() {
                    if visible {
                        d.settings.columns.disable_column(col);
                        widths_on_disable(&d.settings.columns, &mut d.column_widths, col);
                    } else {
                        d.settings.columns.enable_column(col);
                        widths_on_enable(&d.settings.columns, &mut d.column_widths, col);
                    }
                }
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
                // Reset only the width proportions to their config priorities;
                // the current visibility and column order are kept (§2.5, §8.1 С3).
                if let Some(d) = a.dialog_mut() {
                    let none = BTreeMap::new();
                    let mut widths = BTreeMap::new();
                    for c in d.settings.columns.visible_columns() {
                        if let Some(w) = WidthPct::new(effective_width_pct(&d.settings.columns, &none, c)) {
                            widths.insert(c, w);
                        }
                    }
                    normalize_visible(&d.settings.columns, &mut widths);
                    d.column_widths = widths;
                }
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
                let ordered = a.cfg().columns.ordered_columns();
                if idx == 0 || idx >= ordered.len() {
                    return;
                }
                let col_id = ordered[idx];
                a.edit_cfg(|s| s.columns.move_column(idx, idx - 1));
                a.sync_dialog_cols();
                let new_ordered = a.cfg().columns.ordered_columns();
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
                let ordered = a.cfg().columns.ordered_columns();
                if idx + 1 >= ordered.len() {
                    return;
                }
                let col_id = ordered[idx];
                a.edit_cfg(|s| s.columns.move_column(idx, idx + 1));
                a.sync_dialog_cols();
                let new_ordered = a.cfg().columns.ordered_columns();
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
                eprintln!("[gui] settings_save (Save) draft_present={}", app.borrow().dialog.is_some());
                let mut a = app.borrow_mut();
                let Some(draft) = a.dialog.take() else { return };
                let old = a.core.settings().clone();
                let DialogDraft { settings: mut new, viz_mode, column_widths } = draft;
                // T1.0 §6.4: коммит выбранной в диалоге темы из транзитного поля
                // (кнопка Save активна только для валидной темы, §5.3).
                if let Some(name) = a.theme_selection.take().and_then(ThemeName::new) {
                    new.theme = name;
                }

                if new.playback.audio_device != old.playback.audio_device {
                    // Apply the device first: its early-return guard compares
                    // against the *live* setting, which still has the old value.
                    a.set_output_device(new.playback.audio_device.clone());
                }

                // «Сохранить» заменяет настройки целиком; тип визуализации и
                // ширины колонок — состояние сессии (§2.5, И-Т7, §8.1 С3).
                // Живые поля состояния (громкость, повтор, окно…) диалог не
                // трогает, поэтому они сохраняются без ручного переноса.
                // Состояние уходит по таймеру отложенной записи, `settings.toml`
                // — немедленно одним снимком через писатель, независимо от
                // запрета автозаписи; смена интервала переставляет срок
                // (ТЗ-28, §6.9).
                a.core.change_state(Origin::User, StateChange::VizMode(viz_mode));
                a.core.change_state(Origin::User, StateChange::ColumnWidths(column_widths));
                if let Some(effect) = a.core.save_settings_now(new) {
                    a.apply_reply_effects(vec![effect]);
                }
                let cur = a.core.settings().clone();
                // ТЗ §10.4: лимит RAM-кэша визуализации менялся в диалоге →
                // горячий `resize` существующего LRU (лишние записи вытесняются
                // по LRU внутри `CLruCache::resize`).
                let new_viz_ram_mb = cur.visualization.viz_max_ram_mb;
                if new_viz_ram_mb != old.visualization.viz_max_ram_mb {
                    let cap = music_player_rs::audio::fulltrack::fulltrack_cache_max_entries(
                        new_viz_ram_mb,
                    );
                    a.fulltrack_cache.resize(cap);
                    eprintln!("[gui] settings_save: fulltrack cache resize -> {new_viz_ram_mb} MiB");
                }
                // ТЗ §7.5/§8.6: bit-perfect менялся в диалоге → применить к
                // плееру. Включение = Direct Output (unity gain, снятие mute).
                let bp = cur.playback.audio.bit_perfect;
                if bp != old.playback.audio.bit_perfect {
                    a.player.set_bit_perfect(bp);
                    if bp {
                        a.player.set_volume(1.0);
                        a.player.set_muted(false);
                        a.core.change_state(Origin::User, StateChange::Volume(100));
                        a.core.change_state(Origin::User, StateChange::Muted(false));
                    }
                    a.emit(AppEvent::BitPerfectChanged);
                    eprintln!("[gui] settings_save: bit_perfect applied -> {bp}");
                }
                // ТЗ A3.0 §7.7: применять политики вывода/ресемплера из диалога.
                // Сеттеры идемпотентны; рестарт текущего трека (аналогично DSD)
                // переоткрывает поток с новым `OutputRequest`.
                let resample_of = |s: &PersistSettings| {
                    let au = &s.playback.audio;
                    (
                        au.resampler.algorithm,
                        au.resampler.dither,
                        au.resampler.mode,
                        au.resampler.fixed_rate,
                        au.resampler.prefer_family,
                        au.resampler.fallback_rate,
                        au.ring_buffer_ms,
                        au.exclusive,
                        au.fallback,
                    )
                };
                let old_resample = resample_of(&old);
                let new_resample = resample_of(&cur);
                let (algo, dither, mode, fixed_rate, prefer_family, fallback_rate, ring_ms, excl, fb) = new_resample;
                a.player.set_resampler_algorithm(algo);
                a.player.set_dither(dither);
                a.player.set_resampler_mode(mode);
                a.player.set_fixed_rate(fixed_rate);
                a.player.set_prefer_family(prefer_family);
                a.player.set_fallback_rate(fallback_rate);
                a.player.set_ring_buffer_ms(ring_ms);
                a.player.set_exclusive_mode(excl);
                a.player.set_fallback_policy(fb);
                if new_resample != old_resample {
                    if let Some(idx) = a.current {
                        eprintln!("[gui] settings_save: restarting current track {idx} with new audio policies");
                        a.play_track(idx);
                    }
                }
                // ТЗ §8.4: применить выбранный DSD-режим к плееру. Если в этот
                // момент играет DSD-трек — перезапустить его, чтобы новый
                // конвейер (PCM/Native/DoP) применился к незакрытому потоку.
                a.player.set_dsd_mode(cur.playback.dsd.mode);
                a.sync_dsd_settings_to_ui();
                a.sync_dsd_status_ui();
                if a.current_track_is_dsd() {
                    if let Some(idx) = a.current {
                        eprintln!("[gui] settings_save: restarting current DSD track {idx} with new dsd mode");
                        a.play_track(idx);
                    }
                }
                if let Some((theme, _)) =
                    resolve_startup_theme(cur.theme.as_str(), &a.paths.dir.join("themes"))
                {
                    a.apply_theme(&theme);
                }
                if let Some(ui) = a.try_ui() {
                    ui.set_cover_size(cur.top_panel.cover_size);
                    ui.set_col_info_w(cur.top_panel.col_info_w);
                    ui.set_col_gap(cur.top_panel.col_gap);
                }
                a.sync_cover_settings_to_ui();
                a.sync_playlist_to_ui();
                a.sync_audio_devices();
                if let Some(ui) = a.try_ui() {
                    ui.set_settings_open(false);
                }
                a.gate.unblock(BlockReason::Dialog);
                a.sync_gate_ui();
                eprintln!("[gui] settings_save: applied and closed");
            });
        }

        // 30. menu-clear-playlist: пункт меню «Файл» (ТЗ-34, ОВ-10); шлюз
        // отсекает его при блокировке окна и во время загрузки (ТЗ-48, ADR-12).
        {
            let app = this.clone();
            ui.on_menu_clear_playlist(move || {
                eprintln!("[gui] menu_clear_playlist");
                let mut a = app.borrow_mut();
                if !a.gate.allows(MainCmd::ClearPlaylist) {
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
                if !a.gate.allows(MainCmd::RemoveCurrent) {
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
                let ordered = a.cfg().covers.priority.clone();
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
                let ordered = a.cfg().covers.priority.clone();
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
                a.io_gens.clear = a.io_gens.clear.wrapping_add(1);
                a.io.submit(IoJob::ClearCache { gen: a.io_gens.clear, which: CacheKind::Visualization });
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
                a.io_gens.clear = a.io_gens.clear.wrapping_add(1);
                a.io.submit(IoJob::ClearCache { gen: a.io_gens.clear, which: CacheKind::Covers });
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
                a.io_gens.clear = a.io_gens.clear.wrapping_add(1);
                a.io.submit(IoJob::ClearCache { gen: a.io_gens.clear, which: CacheKind::All });
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

        // 32b. message window: primary ("OK"/"Повторить") and close buttons (§6.15).
        {
            let app = this.clone();
            ui.on_msg_primary(move || {
                eprintln!("[gui] msg_primary");
                let mut a = app.borrow_mut();
                let (close_effect, msg_effect) = a.messages.press(MessageButton::Primary);
                a.apply_msg_effect(msg_effect);
                if let CloseEffect::Retry(files) = close_effect {
                    a.retry_writes(files);
                }
            });
        }
        {
            let app = this.clone();
            ui.on_msg_close(move || {
                eprintln!("[gui] msg_close");
                let mut a = app.borrow_mut();
                let (close_effect, msg_effect) = a.messages.press(MessageButton::Close);
                a.apply_msg_effect(msg_effect);
                if let CloseEffect::Retry(files) = close_effect {
                    a.retry_writes(files);
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

        // 34. Вкладка «Visualization» и горячая клавиша V (ТЗ §9 / §3.2).
        viz_settings_manager::bind_viz_settings_callbacks(this);
    }

    pub fn tick(&mut self) {
        self.drain_engine_events();
        self.poll_tray();
        self.drain_events();
        self.drain_startup_tracks();
        self.drain_scan();
        self.drain_cover();
        self.drain_fulltrack();
        self.drain_audio_devices();
        self.drain_io();
        self.handle_reservation();
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
        self.sync_dsd_status_ui();
        if self.try_ui().is_some_and(|ui| ui.get_bp_report_open()) {
            let report = bp_report::build_bp_report(&bp_report::bp_inputs(self));
            self.push_bp_report_to_ui(report);
        }
        self.push_tray_status();

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


    /// Apply the async-loaded startup playlist once it arrives from the
    /// background thread. No-op while the load is still in flight.
    fn drain_startup_tracks(&mut self) {
        let Some(rx) = self.startup_tracks_rx.take() else {
            return;
        };
        let tracks = match rx.try_recv() {
            Ok(loaded) => loaded,
            Err(_) => {
                self.startup_tracks_rx = Some(rx);
                return;
            }
        };
        // Загрузка завершена — снять признак ТЗ-48, список снова доступен.
        self.gate.set_loading(None);
        // МОСТ (§6.13, ТЗ-42, ТЗ-45): строки для модели плейлиста и
        // восстановление видимого порядка по ключу сессии.
        let rows: Vec<(Track, CompareKeys)> = tracks
            .into_iter()
            .map(|t| {
                let keys = CompareKeys::from_track(&t);
                (t, keys)
            })
            .collect();
        let key = self.session_sort_key();
        let visible = MusicApp::load_visible_order(&rows, key);
        self.core.playlist_replace(rows, visible, key);
        self.rebuild_known_paths();
        self.sync_playlist_to_ui();
        // Успешная загрузка стартового плейлиста — результат виден в
        // таблице и track-count, сообщение/строка состояния не нужны
        // (ТЗ-52, ОВ-7).
    }

    /// Разбирает ответы потока `apap-io` на тике: ответы сверяются с
    /// поколением своего вида; устаревшие отбрасываются (ADR-20). Свежий
    /// `CacheSizes`/`Cleared` применяется в UI (ТЗ-22, ТЗ-34); `Themes`/`Theme`
    /// — список тем и метаданные текущей при открытии диалога (ТЗ-22, §6.2).
    fn drain_io(&mut self) {
        while let Some(done) = self.io.try_recv() {
            let stale = match &done {
                IoDone::Theme { gen, .. } => *gen != self.io_gens.theme,
                IoDone::Themes { gen, .. } => *gen != self.io_gens.themes,
                IoDone::CacheSizes { gen, .. } => *gen != self.io_gens.sizes,
                IoDone::Cleared { gen, .. } => *gen != self.io_gens.clear,
            };
            if stale {
                continue;
            }
            match done {
                IoDone::Themes { entries, .. } => self.apply_theme_list(entries),
                IoDone::Theme { result, .. } => self.apply_theme_read(result),
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
                    self.player.toggle();
                    if self.player.is_playing() {
                        self.emit(AppEvent::PlaybackStarted);
                    } else {
                        self.emit(AppEvent::PlaybackPaused);
                    }
                }
                TrayEvent::Stop => {
                    self.player.stop();
                    self.emit(AppEvent::PlaybackStopped);
                }
                TrayEvent::Prev => self.play_prev(),
                TrayEvent::Next => self.play_next(1),
                TrayEvent::ShowHide => {
                    let Some(ui) = self.try_ui() else { continue };
                    let visible = ui.window().is_visible();
                    let res = if visible {
                        // Window hides into the tray: free an exclusive raw-`hw:`
                        // node so the device is usable by other apps while the
                        // player waits in the background (V5.1-B6).
                        self.player.release_if_exclusive();
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
                    let delta = ev.delta;
                    // До С11 (модуль ввода) — прежний шаг громкости (ТЗ-24).
                    const WHEEL_VOLUME_STEP: f32 = 0.02;
                    // Hosts differ in the raw magnitude per event (Ubuntu: ±1,
                    // KDE/4k: ±120); only the sign matters — each scroll notch
                    // is a single step. Negative delta = louder.
                    // Bit-perfect (Direct Output): volume control is moved to
                    // the external DAC/amp — the tray wheel is silenced.
                    if self.player.bit_perfect() || delta == 0 {
                        // Skip volume change — either in bit-perfect mode or
                        // host sent a neutral (zero) event.
                    } else {
                        let dir = if delta < 0 { 1.0 } else { -1.0 };
                        let v = (self.player.volume() + dir * WHEEL_VOLUME_STEP).clamp(0.0, 1.0);
                        self.player.set_volume(v);
                        self.core.change_state(Origin::User, StateChange::Volume(LegacyPlaybackState::pct_from_gain(v)));
                        // Persisted at exit (save-at-exit).
                        self.emit(AppEvent::VolumeChanged(v));
                    }
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

    /// Ключ сортировки сессии (`state.toml`) (§6.13, ТЗ-42, ТЗ-43):
    /// используется при загрузке плейлиста, чтобы восстановить видимый
    /// порядок по сохранённому ключу. `None` — ключа нет.
    pub(super) fn session_sort_key(&self) -> Option<SortKey> {
        self.core.state().sort()
    }

    /// Видимый порядок при загрузке плейлиста (§3.1, §6.13, ТЗ-42, ТЗ-45):
    /// устойчивая сортировка исходных индексов строк по `compare_keys`; без
    /// ключа — тождественный порядок (порядок файла).
    pub(super) fn load_visible_order(rows: &[(Track, CompareKeys)], key: Option<SortKey>) -> Vec<u32> {
        let mut idx: Vec<usize> = (0..rows.len()).collect();
        if let Some(key) = key {
            idx.sort_by(|&a, &b| compare_keys(&rows[a].1, &rows[b].1, key));
        }
        idx.into_iter().filter_map(|i| u32::try_from(i).ok()).collect()
    }

    /// Разобрать все накопленные события движка, применить принятый
    /// `Opened` к плейлисту и раздать остальные варианты `Applied` в
    /// `dispatch_applied` (ADR-02, И-Р13). Буфер `engine_applied`
    /// переиспользуется между тиками — без аллокации.
    pub fn drain_engine_events(&mut self) {
        let mut applied = std::mem::take(&mut self.engine_applied);
        applied.clear();

        // Device readiness is decided here, from the raw `EngineEvent`
        // stream, before it's handed to `player.on_event` — the catalog
        // update itself reduces to a field inside `AudioFacade`/
        // `UiAudioState` and never reaches `Applied` (ТЗ-104, ADR-16).
        let saved_device = self.core.settings().playback.audio_device.clone();
        let mut ready_device: Option<String> = None;

        let player = &mut self.player;
        self.engine_events.drain(|ev| {
            if let EngineEvent::Devices(ref catalog) = ev {
                let found = if saved_device.is_empty() {
                    catalog.default_device()
                } else {
                    catalog
                        .find(&SharedDeviceId::new(saved_device.as_str()))
                        .or_else(|| catalog.default_device())
                };
                if let Some(dev) = found {
                    ready_device = Some(dev.name.clone());
                }
            }
            let a = player.on_event(ev);
            if a != Applied::None {
                applied.push(a);
            }
        });

        if let Some(name) = ready_device {
            if !self.audio_ready {
                // Первое подтверждение устройства заменяет стартовое «checking device…» (ТЗ-104).
                self.status = format!("Audio: {name} ready").into();
            }
            self.audio_ready = true;
            self.audio_error = None;
            self.active_device = name;
        }

        if let Some(info) = self.player.take_opened() {
            if let Some((index, prev_current)) = self.pending_open.take() {
                if index < self.track_count() {
                    self.on_track_opened(index, prev_current, &info);
                    // Успешное открытие прерывает серию пропусков (ТЗ-85, ТЗ-86).
                    self.skip_streak = 0;
                }
            }
        }

        for a in applied.drain(..) {
            self.dispatch_applied(a);
        }
        self.engine_applied = applied;
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
                self.status = format!("Пропущен трек «{name}»: {}", describe_skip_reason(&reason)).into();

                if self.track_count() == 0 || self.skip_streak >= self.track_count() {
                    // Защита от бесконечного пропуска при Repeat All, если
                    // вся библиотека повреждена (мост до SkipSeries С8).
                    self.player.stop();
                    self.emit(AppEvent::PlaybackStopped);
                    self.status = "Все треки пропущены".into();
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
                        self.player.stop();
                        self.emit(AppEvent::PlaybackStopped);
                    }
                }
            }
            Applied::OpenFailed { err } => {
                // `Capture` дублирует уведомление о резервировании
                // (`Notice::Reservation`, обрабатывается в `handle_reservation`) —
                // второе сообщение не нужно, только снять ожидание открытия.
                if matches!(err, OpenError::Capture(_)) {
                    self.pending_open = None;
                    return;
                }
                let index = self.pending_open.take().map(|(index, _)| index).or(self.current);
                let text = describe_open_error(&err);
                match index {
                    Some(index) => self.on_open_failed(index, &text),
                    None => {
                        self.player.stop();
                        self.push_message(Message {
                            level: MessageLevel::Error,
                            title: "Не удалось воспроизвести трек".into(),
                            body: text.into(),
                            buttons: MessageButtons::Ok,
                        });
                    }
                }
            }
            // `AudioFacade` уже взвела флаг `ended`; авто-переход на
            // следующий трек делает `handle_auto_advance` на тике — повторная
            // обработка здесь продублировала бы переход.
            Applied::Ended => {}
            Applied::DeviceLost(reaction) => match reaction {
                Reaction::StopWithError | Reaction::LockFailed => {
                    self.player.stop();
                    self.emit(AppEvent::PlaybackStopped);
                    self.push_message(Message {
                        level: MessageLevel::Error,
                        title: "Устройство вывода потеряно".into(),
                        body: "Воспроизведение остановлено (ТЗ-51, ADR-14).".into(),
                        buttons: MessageButtons::Ok,
                    });
                }
                Reaction::FollowSystemDefault => {
                    // Фоновое переключение на устройство по умолчанию — не
                    // явное действие пользователя (ТЗ-52, ОВ-7) — строка
                    // состояния.
                    self.status = "Вывод переключён на устройство по умолчанию".into();
                }
                Reaction::Skip => {
                    // Трактуется как пропуск трека без отдельного сообщения —
                    // избегаем спама сообщениями при частых потерях устройства.
                    if let Some(idx) = self.next_track_index(self.repeat) {
                        self.play_track(idx);
                    } else {
                        self.player.stop();
                        self.emit(AppEvent::PlaybackStopped);
                    }
                }
            },
            // Уведомление о резервировании уже поставлено в очередь
            // `AudioFacade` и опрашивается `handle_reservation` — повторная
            // обработка здесь не нужна.
            Applied::Notice(Notice::Reservation(_)) => {}
            Applied::Notice(Notice::DevicesUnavailable(BackendError::Unavailable(msg))) => {
                // Фоновый сбой перечисления устройств (ТЗ-104, ТЗ-105, ADR-16) —
                // строка состояния, не окно. То же сообщение, что раньше
                // выдавал синхронный startup-probe, теперь приходит событием.
                self.audio_ready = false;
                self.audio_error = Some(msg.clone());
                self.status = format!("Не удалось получить список устройств вывода: {msg}").into();
            }
            // Путь завершения приложения обрабатывается отдельным шагом моста.
            Applied::ShutdownComplete => {}
        }
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
                | AppEvent::DeviceChanged
                | AppEvent::BitPerfectChanged => self.push_tray_now(),
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
            playing: self.player.is_playing(),
            bit_perfect: self.player.bit_perfect(),
            error: self.audio_error.clone(),
        }
    }
}

fn visible_col_at_index(columns: &ColumnsConfig, visible_index: i32) -> Option<ColumnId> {
    columns.visible_columns().get(visible_index as usize).copied()
}

/// Guard для авто-коррекции Advanced-настроек (ТЗ A3.0 §7.7): сочетание
/// `ResamplerMode::Fixed` с `FallbackPolicy::Fail` невозможно — fixed-цепочка
/// всегда должна иметь запасной путь. Возвращает политику fallback, которую
/// следует применить после выбора режима: исходную во всех случаях, кроме
/// Fixed+Fail (там — Nearest). Чистая функция, покрыта bin-тестами.
fn resampler_fixed_fallback_guard(mode: ResamplerMode, fallback: FallbackPolicy) -> FallbackPolicy {
    if mode == ResamplerMode::Fixed && fallback == FallbackPolicy::Fail {
        FallbackPolicy::Nearest
    } else {
        fallback
    }
}

/// Короткий русский текст причины отказа открытия трека для сообщений
/// пользователю (ADR-14, ТЗ-86). `Capture` не описывается здесь — он не
/// доходит до этой функции, т.к. `dispatch_applied` гасит его отдельно
/// (дублирует уведомление о резервировании).
fn describe_open_error(err: &OpenError) -> String {
    match err {
        OpenError::Capture(_) => String::new(),
        OpenError::DeviceLost => "устройство вывода потеряно".to_string(),
        OpenError::File(file_err) => describe_file_error(file_err),
        OpenError::Internal(fault) => format!("внутренняя ошибка движка: {fault:?}"),
    }
}

/// Короткий русский текст ошибки файла (ТЗ-86, ТЗ-87).
fn describe_file_error(err: &FileError) -> String {
    match err {
        FileError::Corrupt(kind) => format!("файл повреждён ({kind:?})"),
        FileError::Unsupported { codec } => format!("неподдерживаемый формат: {codec}"),
        FileError::Io(kind) => format!("ошибка чтения: {kind:?}"),
        FileError::ReadDuringPlayback { at_frame, kind } => {
            format!("ошибка чтения на кадре {at_frame}: {kind:?}")
        }
    }
}

/// Короткий русский текст причины пропуска трека (ТЗ-86, ТЗ-87).
fn describe_skip_reason(reason: &SkipReason) -> String {
    match reason {
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
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Bitrate), "1411 kbps");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::BitDepth), "24 bit");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::SampleRate), "96000 Hz");
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
        let data: ThemeData = toml::from_str(music_player_rs::theme::DEFAULT_DARK_TOML).unwrap();
        assert!(validate_colors(&data.colors).is_ok());
    }

    #[test]
    fn test_validate_colors_bad_hex() {
        let mut data: ThemeData = toml::from_str(music_player_rs::theme::DEFAULT_DARK_TOML).unwrap();
        data.colors.viz_3 = "zzzzzz".to_string();
        let err = validate_colors(&data.colors).unwrap_err();
        assert!(err.to_string().contains("viz_3"), "ошибка: {err}");
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

    use crate::app::ui_manager::{audio_filter_matches, build_capabilities, find_device_index_in};
    use cpal::{SampleFormat, SupportedBufferSize};
    use music_player_rs::audio::output::{DeviceCategory, DeviceInfo, RateRange};

    /// Minimal mock for [`DeviceInfo`]. Only the fields actually consumed by
    /// `build_capabilities` and the filter projection are varied per test;
    /// the rest get safe defaults.
    fn mock_device(
        id: &str,
        name: &str,
        channels: u16,
        category: DeviceCategory,
        rates: Vec<u32>,
        formats: Vec<SampleFormat>,
        exclusive_capable: bool,
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
            exclusive_capable,
        }
    }

    #[test]
    fn fixed_rates_mapping_exact() {
        assert_eq!(
            FIXED_RATES,
            [0, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000]
        );
    }

    #[test]
    fn fixed_rates_has_same_count_as_settings_combo_options() {
        // «Авто» + 6 частот (44.1k, 48k, 88.2k, 96k, 176.4k, 192k) → 7
        assert_eq!(FIXED_RATES.len(), 7);
        // «Авто» (index 0) maps to 0 Hz (auto).
        assert_eq!(FIXED_RATES[0], 0);
        // Остальные индексы — реальные (non-zero) частоты для Fixed-режима.
        for &hz in &FIXED_RATES[1..] {
            assert!((44_100..=192_000).contains(&hz));
        }
    }

    #[test]
    fn build_capabilities_hw_stereo_marks_all_ok() {
        let d = mock_device(
            "hw:PCH,0",
            "Intel HDA",
            2,
            DeviceCategory::Hardware,
            vec![44100, 48000, 96000],
            vec![SampleFormat::I32],
            true,
        );
        let caps = build_capabilities(&d);
        assert_eq!(caps.len(), 6, "rovisions must have exactly 6 rows");
        // Row 0: Тип — hardware → ok
        assert_eq!(caps[0].label.as_str(), "Тип устройства");
        assert!(caps[0].ok);
        assert_eq!(caps[0].value.as_str(), "Аппаратное (hw:*)");
        // Row 1: Каналы — 2 → «стерео»
        assert_eq!(caps[1].label.as_str(), "Каналы");
        assert!(caps[1].ok);
        assert_eq!(caps[1].value.as_str(), "2 (стерео)");
        // Row 2: Частоты — non-empty → ok
        assert_eq!(caps[2].label.as_str(), "Частоты");
        assert!(caps[2].ok);
        // Row 3: Форматы — ok
        assert_eq!(caps[3].label.as_str(), "Форматы");
        assert!(caps[3].ok);
        // Row 4: Exclusive — capable
        assert_eq!(caps[4].label.as_str(), "Exclusive");
        assert!(caps[4].ok);
        assert_eq!(caps[4].value.as_str(), "поддерживается");
        // Row 5: DSD (DoP) — Intel HDA без DoP → ok=false
        assert_eq!(caps[5].label.as_str(), "DSD (DoP)");
        assert!(!caps[5].ok);
        assert_eq!(caps[5].value.as_str(), "—");
    }

    #[test]
    fn build_capabilities_dop_device_shows_container_rate() {
        let d = mock_device(
            "hw:DAC,0",
            "HiFi DAC",
            2,
            DeviceCategory::Hardware,
            // DoP container rates: 176400 → supported.
            vec![44100, 48000, 176400],
            vec![SampleFormat::I32],
            true,
        );
        let caps = build_capabilities(&d);
        let dop = caps.iter().find(|c| c.label.as_str() == "DSD (DoP)").unwrap();
        assert!(dop.ok, "device supports 176400 → DoP must be ok");
        assert_eq!(dop.value.as_str(), "176.4 kHz");
    }

    #[test]
    fn build_capabilities_server_proxy_marks_not_hardware() {
        let d = mock_device(
            "pipewire",
            "PipeWire (Software)",
            99,
            DeviceCategory::ServerProxy,
            vec![],
            vec![],
            false,
        );
        let caps = build_capabilities(&d);
        assert_eq!(caps[0].value.as_str(), "Программное (сервер звука)");
        // Каналы 99 → вне известных конфигураций (2..=8, 1) — просто число
        assert_eq!(caps[1].value.as_str(), "99");
        // Exclusive не поддерживается
        assert!(!caps[4].ok);
        assert_eq!(caps[4].value.as_str(), "не поддерживается");
    }

    #[test]
    fn build_capabilities_empty_rates_marks_freq_not_ok() {
        let d = mock_device("dmix", "dmix", 2, DeviceCategory::Virtual, vec![], vec![], false);
        let caps = build_capabilities(&d);
        let freq = caps.iter().find(|c| c.label.as_str() == "Частоты").unwrap();
        assert!(!freq.ok);
    }

    #[test]
    fn audio_filter_hardware_only_keeps_hw_drops_software() {
        let hw = mock_device("hw:PCH,0", "Intel HDA", 2, DeviceCategory::Hardware, vec![], vec![], true);
        let sp = mock_device("pipewire", "PipeWire", 2, DeviceCategory::ServerProxy, vec![], vec![], false);
        let virt = mock_device("dmix", "dmix", 2, DeviceCategory::Virtual, vec![], vec![], false);
        assert!(audio_filter_matches(&hw, true, false));
        assert!(!audio_filter_matches(&sp, true, false));
        assert!(!audio_filter_matches(&virt, true, false));
        // Фильтр выключен — проходят все категории.
        assert!(audio_filter_matches(&hw, false, false));
        assert!(audio_filter_matches(&sp, false, false));
    }

    #[test]
    fn audio_filter_stereo_only_keeps_2ch_drops_multichannel() {
        let stereo = mock_device("hw:PCH,0", "Intel HDA", 2, DeviceCategory::Hardware, vec![], vec![], true);
        let surround = mock_device("hw:DAC,0", "HiFi DAC", 6, DeviceCategory::Hardware, vec![], vec![], true);
        assert!(audio_filter_matches(&stereo, false, true));
        assert!(!audio_filter_matches(&surround, false, true));
        // Оба фильтра сразу: hw + стерео.
        assert!(audio_filter_matches(&stereo, true, true));
        assert!(!audio_filter_matches(&surround, true, true));
        let virtual_mono = mock_device("dmix", "dmix", 1, DeviceCategory::Virtual, vec![], vec![], false);
        assert!(!audio_filter_matches(&virtual_mono, false, true));
        assert!(!audio_filter_matches(&virtual_mono, true, true));
    }

    #[test]
    fn audio_filter_no_presets_accepts_anything() {
        let hw = mock_device("hw:PCH,0", "Intel HDA", 2, DeviceCategory::Hardware, vec![], vec![], true);
        let sp = mock_device("pipewire", "PipeWire", 99, DeviceCategory::ServerProxy, vec![], vec![], false);
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
        let suffix = music_player_rs::audio::output::SERVER_NODE_SUFFIX;
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

    #[test]
    fn resampler_fixed_fallback_guard() {
        // When mode == Fixed && fallback == Fail → forced Nearest.
        let g = |m, fb| super::resampler_fixed_fallback_guard(m, fb);
        use music_player_rs::settings::{FallbackPolicy, ResamplerMode};
        assert_eq!(
            g(ResamplerMode::Fixed, FallbackPolicy::Fail),
            FallbackPolicy::Nearest
        );
        // Fallback unchanged for other combinations.
        assert_eq!(
            g(ResamplerMode::Fixed, FallbackPolicy::Nearest),
            FallbackPolicy::Nearest,
        );
        assert_eq!(
            g(ResamplerMode::Fixed, FallbackPolicy::DeviceDefault),
            FallbackPolicy::DeviceDefault,
        );
        assert_eq!(
            g(ResamplerMode::Auto, FallbackPolicy::Fail),
            FallbackPolicy::Fail,
        );
        assert_eq!(
            g(ResamplerMode::Native, FallbackPolicy::Fail),
            FallbackPolicy::Fail,
        );
    }
}
