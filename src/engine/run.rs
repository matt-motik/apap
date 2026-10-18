//! Цикл потока `apap-engine` (§6.1, ADR-01, ADR-02, ADR-20, ТЗ-103). Мост С3:
//! `Engine` строится внутри самого потока (`Player` не обязан быть `Send`,
//! §2.10) и держит легаси `Player` целиком вместо `SignalPath`/сессии (С4/С5).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use std::sync::Mutex;

use crate::audio::backend::exclusive::DeviceClaim;
use crate::audio::backend::catalog::DeviceCatalog;
use crate::audio::backend::HwDeviceId;
use crate::audio::clock::ClockInstant;
use crate::audio::error::{CaptureFailure, EngineFault, FileError, OpenError, Reaction, StreamFault};
use crate::audio::format::{SampleRate, SourceFormat};
use crate::audio::path::plan::{plan, with_gain};
use crate::audio::path::signal::{DynamicPathState, SignalPath};
use crate::audio::path::{Blocked, OpenedFacts, OsStage, PathPlan};
use crate::audio::player::{Player, ReservationEvent, StreamDesc};
use crate::audio::reservation::LostNotify;
use crate::audio::session_build::SpawnDecoderError;
use crate::audio::verdict::{verdict, BadgeState, GainState};
use crate::audio::worker::VizTap;
use crate::engine::deps::EngineDeps;
use crate::engine::exclusive_path::{self, ExclusiveOpenReason, ExclusivePath};
use crate::engine::legacy_path::{legacy_audio, opened_facts_from_desc, shared_caps_from_desc, stream_format_from_desc};
use crate::engine::lock::{lock_step, resolve_device, LockEffect, LockInput, LockState, ReacquirePoint, Resolved};
use crate::engine::messages::{EngineCmd, EngineEvent, InternalMsg, LegacyAudio, Notice, SkipReason, TransportState};
use crate::persist::state_file::LegacyPlaybackState;
use crate::settings::params::{ApplyKind, ModeSettingsDiff};
use crate::settings::playback::{Dither, ModeKind, ModeSettings, PathPolicy, Volume};

/// Минимальный интервал между событиями `Position` (§6.1, И-Р13).
const POSITION_INTERVAL: Duration = Duration::from_millis(100);

/// Решение `EngineCmd::SetModeSettings` по диффу активного режима и
/// состоянию транспорта (§6.18, И-Р26). `Store` — без переоткрытия: пустой
/// дифф и изменения с `ApplyKind::Memory` не трогают `Player`, а на
/// остановленном транспорте нет сессии для переоткрытия, поэтому параметры
/// только копируются в `Player` для следующего `Open`. `ReopenAtPosition` и
/// `SwitchDevice` — одно переоткрытие текущего трека старым путём;
/// `SwitchDevice` поглощает остальные изменения активного режима, т.к. он
/// сильнее по порядку `ApplyKind` (И-Р26).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModeApplyAction {
    Store,
    ReopenAtPosition,
    SwitchDevice,
}

/// Чистая часть решения `SetModeSettings` (§6.18, И-Р26), без доступа к
/// `Player` — тестируется без реального аудио-устройства (ТЗ-114).
/// Изменения неактивного режима (`diff.strongest(active)` не их учитывает)
/// не дают эффекта.
pub(crate) fn mode_apply_action(diff: Option<&ModeSettingsDiff>, active: ModeKind, stopped: bool) -> ModeApplyAction {
    match diff.and_then(|d| d.strongest(active)) {
        None | Some(ApplyKind::Memory) => ModeApplyAction::Store,
        Some(ApplyKind::ReopenAtPosition) | Some(ApplyKind::SwitchDevice) if stopped => ModeApplyAction::Store,
        Some(ApplyKind::ReopenAtPosition) => ModeApplyAction::ReopenAtPosition,
        Some(ApplyKind::SwitchDevice) => ModeApplyAction::SwitchDevice,
    }
}

/// Действующая громкость по правилу источников громкости (§6.18, ОВС-18…
/// ОВС-20, ADR-23, ТЗ-41, ТЗ-140), без доступа к `Player` — тестируется без
/// реального аудио-устройства (ТЗ-114). Mute гасит сигнал в любом режиме
/// (ОВС-18). Иначе в Строгом режиме и в Оптимальном с «Фиксировать громкость
/// на 100 %» (`volume_lock`) ступени громкости нет — эффективная громкость
/// `1.0` независимо от запрошенной (ADR-23, ТЗ-41, ТЗ-140); в Совместимом и
/// в Оптимальном без фиксации громкость — запрошенное значение.
pub(crate) fn effective_gain(settings: &ModeSettings, active: ModeKind, volume_gain: f32, muted: bool) -> f32 {
    if muted {
        return 0.0;
    }
    match active {
        ModeKind::Strict => 1.0,
        ModeKind::Optimal if settings.optimal.volume_lock => 1.0,
        _ => volume_gain,
    }
}

/// Запасная частота потока для [`crate::audio::backend::os_probe::OsLevelProbe::probe`],
/// если `StreamDesc::rate` не заполнена (нулевая/`StreamDesc::default()`,
/// ТЗ-57, §6.3) — то же значение, что приватная `legacy_path::FALLBACK_RATE`
/// (своя копия здесь, т.к. та константа не публична); убирается вместе со
/// `StreamDesc` на этапе С6.
const FALLBACK_STREAM_RATE: SampleRate = match SampleRate::new(44_100) {
    Some(rate) => rate,
    None => panic!("FALLBACK_STREAM_RATE_HZ must be non-zero"),
};

/// Собранный `SignalPath` и материалы для его пересборки без повторного
/// обращения к устройству (§6.3, §6.18 шаг 28, 50, ADR-20, ТЗ-52, ТЗ-137):
/// `plan` — план ДО `with_gain` (шаги 1–4), `facts` — факты открытого
/// потока; оба нужны `rebuild_with_gain` (пересборка на `SetVolume`/
/// `SetMuted`), который эту структуру уже не строит с нуля. Используется и
/// для Совместимого режима (`Engine::active_path`), и для Эксклюзивного
/// пути (`Engine::exclusive_signal`).
struct ActivePath {
    /// План ДО `with_gain` — пересборка на `SetVolume`/`SetMuted` строит
    /// гейн-транспорт заново от него, не трогая шаги 1–4 (`rebuild_with_gain`).
    plan: PathPlan,
    /// Факты открытого потока — переиспользуются пересборкой на
    /// `SetVolume`/`SetMuted` без повторного обращения к устройству
    /// (`rebuild_with_gain`).
    facts: OpenedFacts,
    path: Arc<SignalPath>,
    badge: BadgeState,
}

/// Строит `SignalPath` Совместимого режима из уже открытого `StreamDesc`
/// (§6.3, §6.18 шаг 28, ADR-20, ТЗ-52, ТЗ-57): чистая функция без доступа к
/// `Player`/`Engine` — тестируется без реального аудио-устройства (ТЗ-114).
/// `src` — формат источника с шага 3 (`SourceOpener::probe`); план строится
/// без усиления (шаги 1–4, §6.3), `with_gain` добавляет громкость/mute/
/// дизеринг (шаги 5–6) для текущего `gain`. `Err(Blocked)` — план не
/// построен (несовместимый формат), вызывающая сторона останавливает
/// воспроизведение и шлёт `Skipped(Incompatible)`.
#[expect(
    clippy::result_large_err,
    reason = "сигнатура `plan()` (§6.3): Blocked несёт partial PathPlan (§3.1, ТЗ-80); вызывается раз на открытие трека"
)]
fn build_compatible_path(
    src: &SourceFormat,
    desc: &StreamDesc,
    os: OsStage,
    settings: &ModeSettings,
    gain: GainState,
    generation: u64,
) -> Result<ActivePath, Blocked> {
    let caps = shared_caps_from_desc(desc);
    let policy = PathPolicy::active(settings, ModeKind::Compatible);
    let base = plan(src, &caps, policy)?;
    let gained = with_gain(&base, gain, policy);
    let facts = opened_facts_from_desc(desc, os);
    let path = Arc::new(SignalPath::from_opened(gained, facts.clone(), generation));
    let badge = verdict(&path, gain, None);
    Ok(ActivePath { plan: base, facts, path, badge })
}

/// Пересобирает `SignalPath` на новую громкость/mute без переоткрытия
/// устройства (§6.3, §6.18 шаг 50, ADR-20, ТЗ-38, ТЗ-60, ТЗ-137): `with_gain`
/// по тому же `plan` (шаги 1–4, ДО усиления) пересчитывает гейн-транспорт
/// (шаги 5–6) для актуального `gain` под политику режима `mode`; `OpenedFacts`
/// и устройство не меняются — `from_opened` переиспользует `active.facts`
/// целиком, без обращения к `Player`/`ExclusivePath`. Новое `generation` —
/// путь меняется без переоткрытия потока, UI должен отличать пересобранный
/// путь от предыдущего по номеру поколения (§5). Второй элемент пары —
/// изменился ли вердикт (`BadgeState`) относительно `active.badge`; вызывающая
/// сторона (`Engine::refresh_path_gain`) шлёт `EngineEvent::BadgeChanged`
/// только при `true` (ТЗ-60). `mode` — `ModeKind::Compatible` для
/// `Engine::active_path`, активный режим для `Engine::exclusive_signal`.
fn rebuild_with_gain(
    active: ActivePath,
    settings: &ModeSettings,
    mode: ModeKind,
    gain: GainState,
    generation: u64,
) -> (ActivePath, bool) {
    let policy = PathPolicy::active(settings, mode);
    let gained = with_gain(&active.plan, gain, policy);
    let path = Arc::new(SignalPath::from_opened(gained, active.facts.clone(), generation));
    let badge = verdict(&path, gain, None);
    let changed = badge != active.badge;
    (ActivePath { plan: active.plan, facts: active.facts, path, badge }, changed)
}

/// Сид TPDF-дизеринга по пути трека (§6.18 шаг 45, ТЗ-8): та же формула, что
/// у легаси `Player` (`src/audio/player.rs`), продублирована здесь — функция
/// приватна в том модуле, а `player.rs` не входит в вайтлист этого шага.
fn track_seed(path: &Path) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() >> (64 - 30)) as u32 | 1
}

/// Секунды в кадры по частоте дискретизации цели (§6.18 шаг 45): та же
/// формула, что у легаси `Player` (`src/audio/player.rs`), продублирована по
/// той же причине, что `track_seed`.
fn secs_to_frames(secs: f64, rate: u32) -> u64 {
    let frames = (secs.max(0.0) * f64::from(rate)).round();
    if frames.is_finite() {
        frames as u64
    } else {
        0
    }
}

/// Классифицирует причину отказа `exclusive_path::open` в `OpenError`
/// (§6.18, ADR-14, ТЗ-86). `Configure`/`BufferUnavailable`/`Start` — отказ
/// драйвера/устройства на шагах `configure`/`start` (ТЗ-113, ТЗ-131): ближе
/// к `OpenError` для них нет типизированного `CaptureFailure` (тот же
/// мост-приём, что у `reopen_with_legacy` и `SetDevice` в этом файле) — им
/// присвоена потеря устройства. `Decode(Open(msg))` — декодер не открыл
/// источник текстом ошибки (ТЗ-86, тот же мост-приём, что у легаси-ветки
/// `EngineCmd::Open`) — тот же вариант `File(Unsupported)`.
/// `Decode(Spawn(fault))` — поток декодера не запущен — переносит уже
/// типизированный `EngineFault` в `Internal` без моста. `Plan`/`Render`/
/// `Feed` — рассогласование плана сессии с фактическим буфером устройства
/// или источником декодера: это ошибка сборки пути внутри движка, а не
/// отказ оборудования или пользовательского файла, поэтому классифицируется
/// как `Internal`; `EngineFault` не содержит варианта именно под
/// рассогласование плана — `DriverFormatUnstable` зарезервирован под другой
/// случай (повторное расхождение `hw_params` в Оптимальном режиме, ОВС-10
/// п. 5) и не подходит, поэтому использован ближайший по смыслу вариант
/// `SpawnFailed`.
fn exclusive_open_error(reason: ExclusiveOpenReason) -> OpenError {
    match reason {
        ExclusiveOpenReason::Configure(_) | ExclusiveOpenReason::BufferUnavailable | ExclusiveOpenReason::Start(_) => {
            OpenError::DeviceLost
        }
        ExclusiveOpenReason::Plan(_) | ExclusiveOpenReason::Render(_) | ExclusiveOpenReason::Feed(_) => {
            OpenError::Internal(EngineFault::SpawnFailed { what: "exclusive-session-assembly" })
        }
        ExclusiveOpenReason::Decode(SpawnDecoderError::Spawn(fault)) => OpenError::Internal(fault),
        ExclusiveOpenReason::Decode(SpawnDecoderError::Open(msg)) => {
            OpenError::File(FileError::Unsupported { codec: msg })
        }
    }
}

/// Цикл обработки команд (мост С3): владеет `Player` целиком, пока
/// `SignalPath`/`BadgeState`/`DeviceCatalog.hw` не появились (С4…С6).
struct Engine {
    player: Player,
    deps: EngineDeps,
    /// `req_gen` и путь последнего успешно открытого трека — для `SetDevice`
    /// (переоткрытие на новом устройстве) и для `req_gen` в `OpenFailed`.
    current: Option<(u64, Arc<Path>)>,
    /// Кэш перечисления Shared-устройств (ADR-16, §2.3); пополняется при
    /// старте движка, по сигналу `DeviceWatcher` и по команде `RefreshDevices`
    /// (ТЗ-104, ТЗ-105, §6.2 п.3).
    catalog: DeviceCatalog,
    /// Флаг «список устройств мог измениться», выставляемый колбэком
    /// `DeviceWatcher` из произвольного потока (ADR-16, ТЗ-105); `poll_session`
    /// снимает его и перечисляет устройства не чаще одного раза на событие.
    devices_dirty: Arc<AtomicBool>,
    /// Явная остановка транспорта (ADR-20, мост С3): легаси `Player` держит
    /// декодер загруженным и после `stop()` (`has_decoder()` остаётся
    /// `true`), поэтому `Stopped` отслеживается здесь, а не выводится из
    /// `Player`. `true` до первого успешного `Open`, после `Stop` и после
    /// отказа/потери резервирования.
    stopped: bool,
    /// Последнее отправленное состояние `Transport` — для дедупликации
    /// (§6.1 п. 3, И-Р13): событие не шлётся повторно с тем же состоянием.
    last_transport: Option<TransportState>,
    /// Последняя отправленная позиция — для дедупликации `Position`.
    last_pos: Option<f64>,
    /// Момент последней отправки `Position` — минимальный интервал 100 мс
    /// (§6.1 п. 4, И-Р13).
    last_pos_at: Option<ClockInstant>,
    /// Мид-трековый отказ декодера уже разобран в текущей сессии (ТЗ-87,
    /// §6.18 шаг 19): `poll_session` уже отправил `Skipped` и не должен затем
    /// отправить `Ended` по тому же `ended()` (мид-трековый отказ — это
    /// пропуск, а не штатный конец трека). Сбрасывается на следующий успешный
    /// `Open`.
    session_failed: bool,
    /// Копия настроек всех режимов (ОВС-14, §6.18): обновляется по
    /// `SetModeSettings` целиком, независимо от того, требует ли изменение
    /// переоткрытия.
    mode_settings: ModeSettings,
    /// Активный режим (ADR-04, §2.2); меняется только `SetActiveMode`.
    active_mode: ModeKind,
    /// Последняя запрошенная через `SetVolume` громкость (§6.18, ОВС-20):
    /// хранится независимо от того, применилась ли она к `Player` —
    /// restore при снятии фиксации/смене режима в `apply_gain`.
    last_volume: f32,
    /// Последнее запрошенное через `SetMuted` состояние mute (ОВС-18):
    /// mute гасит сигнал во всех режимах, `player.set_muted` вызывается
    /// напрямую, вне правила `effective_gain`.
    last_muted: bool,
    /// Возможности бэкенда (§2.3, ТЗ-109): определяет доступность
    /// `active_mode` для проверки в `Open` (§6.18 п. 2). Снимок на момент
    /// запуска потока — платформа не меняется на ходу.
    caps: BackendCaps,
    /// Счётчик поколений `SignalPath` (§5, §6.3, ADR-20): увеличивается на
    /// каждую сборку нового пути (сейчас — только успешный `Open` в
    /// Совместимом режиме, С6 добавит остальные). UI сверяет поколение
    /// `BadgeChanged` с поколением текущего пути, чтобы отбросить устаревшее
    /// событие от уже закрытой сессии.
    path_generation: u64,
    /// Текущий собранный `SignalPath` Совместимого режима (§6.3, §6.18 шаг
    /// 28), если он есть: `None` в Оптимальном/Строгом режиме (пока
    /// исполняются старым путём, С6) и без открытого потока. Очищается во
    /// всех точках конца/отказа сессии — рядом с очисткой `self.current`.
    active_path: Option<ActivePath>,
    /// Открытый Эксклюзивный путь Оптимального/Строгого режима (§6.18 шаг
    /// 45, ТЗ-48, ТЗ-137), если он есть: `None` в Совместимом режиме и без
    /// открытого потока. Очищается рядом с `active_path`/`self.current` во
    /// всех точках конца/отказа сессии.
    exclusive_path: Option<ExclusivePath>,
    /// Снимок `SignalPath` Эксклюзивного пути и материалы для его пересборки
    /// на `SetVolume`/`SetMuted` (§6.18 шаг 50, ТЗ-137), если путь открыт:
    /// аналог `active_path` для Оптимального/Строгого режима. Очищается
    /// везде, где закрывается/берётся `exclusive_path`.
    exclusive_signal: Option<ActivePath>,
    /// Отправитель внутренних сообщений служебных потоков (§2.8, ADR-01):
    /// клонируется для каждого служебного потока (резервирование §6.20,
    /// открытие PCM §6.19, RT-приоритет §6.8 — проводка следующих шагов).
    internal_tx: mpsc::Sender<InternalMsg>,
    /// Приёмник внутренних сообщений (§2.8, ADR-01): вычитывается без
    /// блокировки на каждой итерации цикла (`drain_internal`).
    internal_rx: Receiver<InternalMsg>,
    /// Состояние замка устройства режима (§6.19, ТЗ-118, ADR-01): меняется
    /// только через `lock_input`. Начальное значение соответствует
    /// Совместимому режиму по умолчанию (`active_mode`, §2.2).
    lock: LockState,
    /// Удержанный захват устройства (§6.19, ТЗ-119, ADR-01): `Some`, пока
    /// `lock` в состоянии `Held`. Порядок полей `DeviceClaim` задаёт порядок
    /// освобождения PCM/резервирования (И-Р1) — `self.claim = None` в ответ
    /// на `LockEffect::Release` снимает захват.
    claim: Option<DeviceClaim>,
    /// Поколение попытки захвата (§6.19, ТЗ-123, ADR-01): увеличивается
    /// перед каждым запуском `start_claim` и эффектом `LockEffect::Release`,
    /// чтобы `InternalMsg::ClaimDone` от отменённой или уже снятой попытки
    /// отбрасывалось — тот же приём, что `req_gen` у `Open`.
    claim_attempt: u64,
    /// Точка повторного захвата (§6.20, ТЗ-126); заполняется при потере
    /// устройства — шаг 57.
    reacquire: Option<ReacquirePoint>,
    /// Счётчики динамики текущего потока (§6.17, §6.20, ТЗ-51, ТЗ-76):
    /// `underruns`/`xruns` переносятся из `SessionShared` Эксклюзивного пути
    /// на каждом опросе; сбрасывается на каждый успешный `Open`.
    dynamics: DynamicPathState,
}

impl Engine {
    fn new(deps: EngineDeps, caps: BackendCaps) -> Engine {
        let mut player = Player::new();
        player.set_spawner(Arc::clone(&deps.spawner));
        let player_volume = player.volume();
        let (internal_tx, internal_rx) = mpsc::channel();
        Engine {
            player,
            deps,
            current: None,
            catalog: DeviceCatalog::default(),
            devices_dirty: Arc::new(AtomicBool::new(false)),
            stopped: true,
            last_transport: None,
            last_pos: None,
            last_pos_at: None,
            session_failed: false,
            mode_settings: ModeSettings::default(),
            active_mode: ModeKind::default(),
            last_volume: player_volume,
            last_muted: false,
            caps,
            path_generation: 0,
            active_path: None,
            exclusive_path: None,
            exclusive_signal: None,
            internal_tx,
            internal_rx,
            lock: LockState::NotApplicable,
            claim: None,
            claim_attempt: 0,
            reacquire: None,
            dynamics: DynamicPathState::default(),
        }
    }

    /// Цикл движка (§6.1): команда — сразу `handle`, тайм-аут 20 мс —
    /// следующая итерация; после каждой обработанной команды и на каждом
    /// тайм-ауте — опрос сессии `poll_session` (`Position`/`Transport`/
    /// `Ended`/резервирование, §6.1 п. 1–5, И-Р13, ТЗ-60, ТЗ-102), затем
    /// вычитка сообщений служебных потоков `drain_internal` (§2.8, ADR-01).
    /// Ни опрос, ни вычитка не выполняются после `Shutdown`/`Disconnected` —
    /// цикл уже завершается.
    ///
    /// Перед циклом (§6.2 п.3, ТЗ-104, ТЗ-105): запускается `DeviceWatcher` и
    /// выполняется первое перечисление устройств — UI должен получить ответ
    /// при старте даже при пустом списке (заменяет прежний busy-wait пробник
    /// вывода на стороне UI).
    fn run(mut self, rx: Receiver<EngineCmd>) {
        let dirty = Arc::clone(&self.devices_dirty);
        self.deps.watcher.start(Box::new(move || {
            dirty.store(true, Ordering::Release);
        }));
        self.refresh_devices(true);

        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(cmd) => {
                    if self.handle(cmd) {
                        break;
                    }
                    self.poll_session();
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.poll_session();
                }
                // UI исчез без Shutdown (§6.1): реагируем так же, как на Shutdown.
                Err(RecvTimeoutError::Disconnected) => {
                    self.handle(EngineCmd::Shutdown);
                    break;
                }
            }
            self.drain_internal();
        }
    }

    /// Отдельный отправитель внутренних сообщений для служебных потоков
    /// (§2.8, ADR-01): клонируется, т.к. каждый служебный поток держит свою
    /// копию. Используется проводкой следующих шагов (§6.19, §6.20, §6.8).
    #[allow(dead_code)]
    fn internal_sender(&self) -> mpsc::Sender<InternalMsg> {
        self.internal_tx.clone()
    }

    /// Вычитывает все сообщения служебных потоков, пришедшие с прошлой
    /// итерации (§2.8, ADR-01): `try_recv` в цикле до `Empty` — не блокирует
    /// цикл движка. Канал не может быть отключён, пока `Engine` держит
    /// `internal_tx` сам, поэтому `Disconnected` здесь не требует отдельной
    /// обработки — цикл просто не находит сообщений.
    fn drain_internal(&mut self) {
        while let Ok(msg) = self.internal_rx.try_recv() {
            self.handle_internal(msg);
        }
    }

    /// Обрабатывает одно сообщение служебного потока (§2.8, ADR-01).
    fn handle_internal(&mut self, msg: InternalMsg) {
        match msg {
            // Ответ устаревшей (отменённой повторным захватом) или уже
            // снятой попытки отбрасывается молча (§6.19, ТЗ-123): захват
            // внутри сам освобождается через `Drop DeviceClaim`, если он был
            // получен (§2.3, И-Р1).
            InternalMsg::ClaimDone { attempt, result } => {
                if attempt != self.claim_attempt || !matches!(self.lock, LockState::Acquiring { .. }) {
                    return;
                }
                match result {
                    Ok(claim) => {
                        let info = claim.info().clone();
                        self.claim = Some(claim);
                        self.lock_input(LockInput::CaptureResult(Ok(info)));
                    }
                    Err(e) => self.lock_input(LockInput::CaptureResult(Err(e))),
                }
            }
            // Потеря резервирования вне захвата замка — обработка приходит с
            // проводкой замка (§6.20).
            InternalMsg::ReservationLost { .. } => {}
            // Итог RT-приоритета не влияет на поведение движка (§6.8).
            InternalMsg::RtPriority { .. } => {}
        }
    }

    /// Шаг состояния замка устройства режима (§6.19, ТЗ-118, ADR-01):
    /// применяет чистую `lock_step`, подставляя реальное время начала
    /// попытки (`lock_step` заполняет `Acquiring.started` плейсхолдером
    /// `ClockInstant::START` — здесь он переписывается `deps.clock.now()`,
    /// как `poll_session` берёт `now` для `Position`), затем выполняет
    /// эффекты. Устройство режима резолвится до `lock_step` (ADR-24, ТЗ-143,
    /// ТЗ-144, §6.18): для `ModeSelected(m)` целевой режим — `m`, устройство
    /// берётся из его настроек (а не из `self.active_mode`, который на этот
    /// момент ещё не сменился), для прочих входов — из настроек текущего
    /// `self.active_mode`.
    fn lock_input(&mut self, input: LockInput) {
        // Режим, чьё устройство нужно разрешить: для `ModeSelected(m)` —
        // целевой `m` (устройство режима, в который переключаются), иначе —
        // текущий активный режим (ADR-24).
        let device_mode = match input {
            LockInput::ModeSelected(m) => m,
            _ => self.active_mode,
        };
        // Разрешение (и тем самым опрос системного выхода в `resolve_device`)
        // выполняется только для входов, которые по таблице §6.19 вообще
        // могут дать эффект `Acquire(dev)` с этим `dev` — остальные
        // (`Shutdown`, `CaptureResult`, `UserOpen`, `DeviceLost`,
        // `DeviceSelected`, `DeviceReappeared`, …) либо не захватывают,
        // либо несут собственное устройство во входе, минуя `dev` (ТЗ-32):
        // опрос для них был бы лишним обращением к системному аудиосерверу.
        let needs_resolution = matches!(
            input,
            LockInput::ModeSelected(ModeKind::Optimal) | LockInput::ModeSelected(ModeKind::Strict)
        ) || (self.active_mode != ModeKind::Compatible
            && matches!(
                input,
                LockInput::Startup | LockInput::UserClose | LockInput::CaptureAndPlay | LockInput::RetryCapture
            ));

        // Автовыбор устройства для записи в копию опций режима и события
        // `DeviceAutoSelected`, но только если он действительно привёл к
        // захвату (ТЗ-143) — выставляется после вызова `lock_step` ниже.
        let mut auto_selected = None;
        let device = if needs_resolution {
            let opts_device = match device_mode {
                ModeKind::Compatible => None,
                ModeKind::Optimal => self.mode_settings.optimal.device.as_ref(),
                ModeKind::Strict => self.mode_settings.strict.device.as_ref(),
            };
            match resolve_device(opts_device, &*self.deps.server_probe, &self.catalog) {
                Resolved::Selected(d) => Some(d),
                Resolved::AutoSelected(d) => {
                    auto_selected = Some(d.clone());
                    Some(d)
                }
                Resolved::None => None,
            }
        } else {
            match device_mode {
                ModeKind::Compatible => None,
                ModeKind::Optimal => self.mode_settings.optimal.device.clone(),
                ModeKind::Strict => self.mode_settings.strict.device.clone(),
            }
        };

        let (mut state, effects) = lock_step(&self.lock, input, self.active_mode, device.as_ref());
        if let LockState::Acquiring { .. } = state {
            state = LockState::Acquiring { started: self.deps.clock.now() };
        }
        let changed = state != self.lock;
        self.lock = state;
        if changed {
            self.deps.events.emit(EngineEvent::Lock(self.lock.clone()));
        }

        // Автовыбор подтверждён реальным захватом (ТЗ-143): копия в опциях
        // режима обновляется и шлётся `DeviceAutoSelected`, которое
        // приложение должно сохранить в настройках (персистентность —
        // следующий шаг). Без эффекта `Acquire` (например, устройство нашёл
        // автовыбор, но вход всё равно не привёл к захвату) событие не шлётся.
        if let Some(d) = auto_selected {
            if effects.iter().any(|effect| matches!(effect, LockEffect::Acquire(_))) {
                match device_mode {
                    ModeKind::Optimal => self.mode_settings.optimal.device = Some(d.clone()),
                    ModeKind::Strict => self.mode_settings.strict.device = Some(d.clone()),
                    ModeKind::Compatible => {}
                }
                self.deps.events.emit(EngineEvent::DeviceAutoSelected { mode: device_mode, device: d });
            }
        }

        for effect in effects {
            match effect {
                LockEffect::Acquire(dev) => self.start_claim(dev),
                LockEffect::StopPlayback => self.stop_playback(),
                LockEffect::Release => {
                    // Попытка захвата может быть ещё не завершена (замок
                    // открывается во время `Acquiring`): доклад `ClaimDone`
                    // от неё должен считаться устаревшим, как при повторном
                    // захвате (§6.19, ТЗ-123) — её `DeviceClaim`, если захват
                    // всё же удался, дропается сам при игнорировании
                    // (`Drop DeviceClaim`, И-Р1), а не попадает в `self.claim`.
                    self.claim_attempt += 1;
                    self.claim = None;
                }
                LockEffect::ForgetReacquire => self.reacquire = None,
            }
        }
    }

    /// Останавливает активное воспроизведение (§6.19, ТЗ-120, ТЗ-121): общий
    /// путь для `EngineCmd::Stop` и эффекта `LockEffect::StopPlayback` замка —
    /// поток вывода должен остановиться до освобождения PCM эффектом
    /// `LockEffect::Release` (И-Р1), иначе звук обрывается после закрытия
    /// устройства. Без открытой сессии (`Player` уже остановлен) —
    /// безопасный no-op: `player.stop()`/`refresh_transport()` идемпотентны.
    /// Эксклюзивный путь закрывается, а не дропается: `ExclusivePath::close`
    /// возвращает захват устройства обратно в `self.claim` — замок остаётся
    /// `Held` (И-Р1, §6.19: `Held ⇔ self.claim.is_some()` без открытого пути),
    /// `LockEffect::Release` снимает его отдельно, если решит освободить замок.
    fn stop_playback(&mut self) {
        self.player.stop();
        self.stopped = true;
        self.active_path = None;
        if let Some(p) = self.exclusive_path.take() {
            self.claim = Some(p.close());
        }
        self.exclusive_signal = None;
        self.refresh_transport();
    }

    /// Запуск захвата устройства `dev` в служебном потоке (§6.19, ТЗ-119,
    /// ADR-01, §6.6). Без эксклюзивного бэкенда на платформе захват
    /// невозможен — `CaptureFailure::PlatformUnsupported`, тот же вариант,
    /// что показывает UI для платформ без ALSA (`player.rs`). Отказ ОС в
    /// выделении служебного потока (`spawner.spawn` вернул `SpawnFailed`)
    /// заводится тем же вариантом: отдельного кода для отказа планировщика
    /// в `CaptureFailure` нет, а структурно это тоже недоступность
    /// монопольного захвата здесь и сейчас.
    fn start_claim(&mut self, dev: HwDeviceId) {
        self.claim_attempt += 1;
        let attempt = self.claim_attempt;

        let Some(backend) = self.deps.exclusive.as_ref() else {
            self.lock_input(LockInput::CaptureResult(Err(CaptureFailure::PlatformUnsupported)));
            return;
        };

        // Устройства нет в каталоге — оно отключено; номер карты для
        // `ReservationLost` не выдумывается (ТЗ-126, §6.19).
        let Some(found) = self.catalog.hw.iter().find(|d| d.id == dev) else {
            self.lock_input(LockInput::CaptureResult(Err(CaptureFailure::DeviceDisconnected)));
            return;
        };
        let device_name = found.name.clone();
        let card = found.card;

        let lost_tx = self.internal_tx.clone();
        let on_lost: LostNotify = Box::new(move || {
            let _ = lost_tx.send(InternalMsg::ReservationLost { card });
        });
        let job = backend.claim_job(dev, device_name, on_lost);

        let done_tx = self.internal_tx.clone();
        let spawned = self.deps.spawner.spawn(
            "apap-claim",
            Box::new(move || {
                let result = job();
                let _ = done_tx.send(InternalMsg::ClaimDone { attempt, result });
            }),
        );
        if let Err(EngineFault::SpawnFailed { .. }) = spawned {
            self.lock_input(LockInput::CaptureResult(Err(CaptureFailure::PlatformUnsupported)));
        }
    }

    /// Открытие в Оптимальном/Строгом режиме (§6.18 шаг 45, 48, ТЗ-48, ТЗ-49,
    /// ТЗ-137): диспетчер к Эксклюзивному пути. Без удержанного замка
    /// (`self.lock` не `Held`) — отказ без попытки захвата: замок уже должен
    /// быть удержан `EngineCmd::Lock`/`lock_step` до `Open` (§6.19). Смена
    /// трека при уже открытом Эксклюзивном пути (новый `Open` без
    /// промежуточного `Stop`) закрывает старый путь первой же строкой —
    /// иначе `self.claim` пуст (его держит старый `ExclusivePath`) и новое
    /// открытие отказало бы `NoDeviceSelected`, хотя замок удержан. `autoplay`
    /// — пуск воспроизведения сразу после открытия (ТЗ-49): поток вывода
    /// `ExclusiveBackend::start` поднимается всегда, пауза регулируется
    /// `SessionShared::playing`, а не остановкой потока.
    fn open_exclusive(&mut self, req_gen: u64, path: Arc<Path>, src: SourceFormat, start_secs: f64, autoplay: bool) {
        let LockState::Held { .. } = &self.lock else {
            let reason = match &self.lock {
                LockState::Failed(reason) => reason.clone(),
                _ => CaptureFailure::NoDeviceSelected,
            };
            self.deps.events.emit(EngineEvent::OpenFailed { req_gen, err: OpenError::Capture(reason) });
            return;
        };
        if let Some(p) = self.exclusive_path.take() {
            self.claim = Some(p.close());
        }
        self.exclusive_signal = None;
        // Инвариант §6.19: `Held` всегда соответствует `self.claim.is_some()`.
        // Расхождение здесь — ошибка движка, не повод для паники: явный отказ
        // вместо unreachable!()/unwrap() (deny clippy выше).
        let Some(mut claim) = self.claim.take() else {
            self.deps
                .events
                .emit(EngineEvent::OpenFailed { req_gen, err: OpenError::Capture(CaptureFailure::NoDeviceSelected) });
            return;
        };
        let caps = {
            let Some(backend) = self.deps.exclusive.as_deref_mut() else {
                self.claim = Some(claim);
                self.deps.events.emit(EngineEvent::OpenFailed {
                    req_gen,
                    err: OpenError::Capture(CaptureFailure::PlatformUnsupported),
                });
                return;
            };
            match backend.device_caps(&mut claim) {
                Ok(caps) => caps,
                Err(e) => {
                    self.claim = Some(claim);
                    self.deps.events.emit(EngineEvent::OpenFailed { req_gen, err: OpenError::Capture(e) });
                    return;
                }
            }
        };

        let policy = PathPolicy::active(&self.mode_settings, self.active_mode);
        let base = match plan(&src, &caps, policy) {
            Ok(p) => p,
            Err(blocked) => {
                self.claim = Some(claim);
                self.deps.events.emit(EngineEvent::Skipped { req_gen, path, reason: SkipReason::Incompatible(blocked) });
                return;
            }
        };
        let gain = self.gain_state();
        let plan_built = with_gain(&base, gain, policy);

        let source = match self.deps.sources.open_for_route(&path, &plan_built.route) {
            Ok(source) => source,
            Err(e) => {
                self.claim = Some(claim);
                self.deps.events.emit(EngineEvent::Skipped { req_gen, path, reason: SkipReason::File(e) });
                return;
            }
        };
        let info = source.info().clone();

        // Параметры сессии по активному режиму (ОВС-14, §6.18): Строгий —
        // без фиксации громкости и без дизеринга (`StrictOpts` не несёт этих
        // полей, ТЗ-8), Оптимальный — из `OptimalOpts` («Фиксировать громкость
        // на 100 %», дизеринг TPDF с сидом по пути трека, как у легаси
        // `Player`, см. `track_seed`).
        let (device_buffer, ring_buffer_ms, volume_lock, dither_seed) = if self.active_mode == ModeKind::Strict {
            let opts = &self.mode_settings.strict;
            (opts.device_buffer, u32::from(opts.buffer.get()), false, None)
        } else {
            let opts = &self.mode_settings.optimal;
            let seed = (opts.dither == Dither::Tpdf).then(|| track_seed(&path));
            (opts.device_buffer, u32::from(opts.buffer.get()), opts.volume_lock, seed)
        };

        // Визуализация Эксклюзивного пути не проведена в этом шаге —
        // `AttachVizTap`/`SetVizTap` не меняют эту сессию (вне объёма шага
        // 45); пустой tap — поток декодера не блокируется и не паникует.
        let tap: VizTap = Arc::new(Mutex::new(None));
        let stream_format = Some(plan_built.target);
        let start_frame = secs_to_frames(start_secs, plan_built.target.rate.hz());
        let generation = self.path_generation.saturating_add(1);

        let Some(backend) = self.deps.exclusive.as_deref_mut() else {
            self.claim = Some(claim);
            self.deps
                .events
                .emit(EngineEvent::OpenFailed { req_gen, err: OpenError::Capture(CaptureFailure::PlatformUnsupported) });
            return;
        };
        match exclusive_path::open(
            backend,
            claim,
            plan_built,
            source,
            self.deps.spawner.as_ref(),
            tap,
            device_buffer,
            ring_buffer_ms,
            volume_lock,
            dither_seed,
            start_frame,
            generation,
        ) {
            Ok((opened, signal)) => {
                self.path_generation = generation;
                let badge = verdict(&signal, gain, None);
                let signal = Arc::new(signal);
                // §6.18 шаг 50 (ТЗ-137, ТЗ-140, ОВС-18): громкость/mute
                // применяются до пуска воспроизведения — без этого автоплей
                // стартовал бы с единичным усилением (щелчок громкости).
                opened.set_gain(effective_gain(&self.mode_settings, self.active_mode, self.last_volume, false), self.last_muted);
                self.exclusive_signal =
                    Some(ActivePath { plan: base, facts: opened.facts().clone(), path: Arc::clone(&signal), badge: badge.clone() });
                if autoplay {
                    opened.play();
                }
                self.exclusive_path = Some(opened);
                self.current = Some((req_gen, Arc::clone(&path)));
                self.stopped = false;
                self.last_pos = None;
                self.session_failed = false;
                self.dynamics = DynamicPathState::default();
                self.deps.events.emit(EngineEvent::Opened { req_gen, info, stream_format, path: Some(signal), badge });
                self.refresh_transport();
            }
            Err(err) => {
                // §6.19: замок остаётся удержан — claim не теряется ни при
                // одном отказе `exclusive_path::open` (возвращается в
                // `ExclusiveOpenError`); причина классифицируется в
                // `OpenError` через `exclusive_open_error` (§6.18, ADR-14,
                // ТЗ-86).
                self.claim = Some(err.claim);
                self.deps
                    .events
                    .emit(EngineEvent::OpenFailed { req_gen, err: exclusive_open_error(err.reason) });
            }
        }
    }

    /// Обрабатывает одну команду. Возвращает `true`, если цикл должен
    /// завершиться (`Shutdown`).
    fn handle(&mut self, cmd: EngineCmd) -> bool {
        match cmd {
            EngineCmd::Open { req_gen, path, start_secs, autoplay } => {
                // §6.18 п. 2 (ТЗ-109, ТЗ-111): режим недоступен на платформе —
                // без захвата и без открытия файла, сохранённый активный
                // режим не меняется (§6.2 п. 5).
                if !self.caps.available(self.active_mode) {
                    self.deps.events.emit(EngineEvent::OpenFailed {
                        req_gen,
                        err: OpenError::ModeUnavailable(self.active_mode),
                    });
                    return false;
                }
                // §6.18 шаг 3: сначала только заголовок через `SourceOpener`,
                // без касания `Player` (ТЗ-86, ТЗ-103, ADR-20). Ошибка —
                // классифицированный `FileError`, сразу `Skipped`. Формат
                // источника сохраняется (шаг 28) — нужен для `plan()` моста
                // Совместимого режима.
                let src = match self.deps.sources.probe(&path) {
                    Ok(src) => src,
                    Err(e) => {
                        self.deps.events.emit(EngineEvent::Skipped {
                            req_gen,
                            path,
                            reason: SkipReason::File(e),
                        });
                        return false;
                    }
                };
                // §6.18 шаг 45 (ТЗ-48, ТЗ-137): Оптимальный/Строгий режим —
                // Эксклюзивный путь, старый `Player` не открывается. Совместимый
                // режим не меняется (прежний путь ниже).
                if self.active_mode != ModeKind::Compatible {
                    self.open_exclusive(req_gen, path, src, start_secs, autoplay);
                    return false;
                }
                match self.player.open(&path) {
                    Ok(info) => {
                        if start_secs > 0.0 {
                            self.player.seek(start_secs);
                        }
                        if autoplay {
                            self.player.play();
                        }
                        self.current = Some((req_gen, Arc::clone(&path)));
                        self.stopped = false;
                        self.last_pos = None;
                        self.session_failed = false;
                        self.dynamics = DynamicPathState::default();
                        let stream = self.player.stream_desc().cloned();
                        // §6.3, §6.18 шаг 28 (ADR-20, ТЗ-52, ТЗ-57): Совместимый
                        // режим — `SignalPath` строится мостом из `StreamDesc`
                        // старого пути. Оптимальный/Строгий пока исполняются
                        // старым путём без снимка (С6); без потока — нет пути
                        // (ТЗ-56).
                        let compatible_desc =
                            if self.active_mode == ModeKind::Compatible { stream.clone() } else { None };
                        let mut path_arc = None;
                        let mut badge = BadgeState::NoStream;
                        if let Some(desc) = compatible_desc {
                            let generation = self.path_generation.saturating_add(1);
                            let gain = self.gain_state();
                            let stream_rate = SampleRate::new(desc.rate).unwrap_or(FALLBACK_STREAM_RATE);
                            let os = self.deps.os_probe.probe(&self.mode_settings.compatible.device, stream_rate);
                            match build_compatible_path(&src, &desc, os, &self.mode_settings, gain, generation) {
                                Ok(active) => {
                                    self.path_generation = generation;
                                    path_arc = Some(Arc::clone(&active.path));
                                    badge = active.badge.clone();
                                    self.active_path = Some(active);
                                }
                                Err(blocked) => {
                                    self.player.stop();
                                    self.stopped = true;
                                    self.current = None;
                                    self.active_path = None;
                                    self.deps.events.emit(EngineEvent::Skipped {
                                        req_gen,
                                        path,
                                        reason: SkipReason::Incompatible(blocked),
                                    });
                                    return false;
                                }
                            }
                        } else {
                            self.active_path = None;
                        }
                        let stream_format = stream.as_ref().map(stream_format_from_desc);
                        self.deps.events.emit(EngineEvent::Opened {
                            req_gen,
                            info,
                            stream_format,
                            path: path_arc,
                            badge,
                        });
                        self.refresh_transport();
                    }
                    Err(msg) => {
                        // §6.18 шаг 19 (ТЗ-87, ТЗ-88): отказ запуска потока
                        // `apap-decode` (`ThreadSpawner`) — внутренняя ошибка
                        // движка, а не файла; иначе — мост С3: легаси `Player`
                        // пока отдаёт только текст, заворачиваем в `Unsupported`.
                        match self.player.take_fault() {
                            Some(fault) => {
                                self.deps.events.emit(EngineEvent::OpenFailed {
                                    req_gen,
                                    err: OpenError::Internal(fault),
                                });
                            }
                            None => {
                                self.deps.events.emit(EngineEvent::Skipped {
                                    req_gen,
                                    path,
                                    reason: SkipReason::File(FileError::Unsupported { codec: msg }),
                                });
                            }
                        }
                    }
                }
            }
            EngineCmd::Play => {
                if let Some(p) = &self.exclusive_path {
                    p.play();
                } else {
                    self.player.play();
                }
                self.stopped = false;
                self.refresh_transport();
            }
            EngineCmd::Pause => {
                // Пауза переключается только из «играет»: команда `Pause` не
                // должна запускать воспроизведение, если оно уже на паузе или
                // остановлено. Легаси `Player::toggle` переключает play/pause;
                // Эксклюзивный путь не трогает поток вывода (§6.18, ТЗ-49) —
                // пауза гасит `SessionShared::playing`, колбэк пишет тишину.
                if let Some(p) = &self.exclusive_path {
                    if p.is_playing() {
                        p.pause();
                    }
                } else if self.player.is_playing() {
                    self.player.toggle();
                }
                self.refresh_transport();
            }
            EngineCmd::Stop => {
                self.stop_playback();
            }
            EngineCmd::Seek { secs } => {
                // §6.16, ТЗ-99, ADR-13: на Эксклюзивном пути seek — 3-фазное
                // рукопожатие (`ExclusivePath::seek` — фаза 1), поток и
                // устройство не переоткрываются; легаси `Player::seek` —
                // прежний путь Совместимого режима.
                if let Some(p) = &self.exclusive_path {
                    p.seek(secs_to_frames(secs, p.frame_rate()));
                } else {
                    self.player.seek(secs);
                }
            }
            EngineCmd::SetVolume(v) => {
                self.last_volume = v;
                self.apply_gain();
                self.refresh_path_gain();
            }
            EngineCmd::SetMuted(m) => {
                // Mute гасит сигнал во всех режимах (ОВС-18) напрямую через
                // `Player`, минуя `effective_gain`/`apply_gain`.
                self.last_muted = m;
                self.player.set_muted(m);
                self.apply_exclusive_gain();
                self.refresh_path_gain();
            }
            // §6.19, ТЗ-118, ADR-01: ввод замка устройства режима из UI.
            EngineCmd::Lock(input) => {
                self.lock_input(input);
            }
            EngineCmd::SetDevice { id } => {
                let (req_gen, path) = match &self.current {
                    Some((gen, path)) => (*gen, Some(path.as_ref())),
                    None => (0, None),
                };
                let (_, pos, _) = self.player.snapshot();
                match self.player.set_device(id, path, pos) {
                    Ok(()) => self.refresh_transport(),
                    Err(_msg) => {
                        // Мост С3: `set_device` возвращает текст ошибки, а не
                        // классифицированный `CaptureFailure` (§6.6 — будущий
                        // шаг); ближайший по смыслу вариант — потеря устройства.
                        self.active_path = None;
                        self.deps
                            .events
                            .emit(EngineEvent::OpenFailed { req_gen, err: OpenError::DeviceLost });
                    }
                }
            }
            EngineCmd::RefreshDevices => {
                self.refresh_devices(false);
            }
            EngineCmd::ReleaseExclusive => {
                self.player.release_if_exclusive();
                self.refresh_transport();
            }
            EngineCmd::SetVizTap(on) => {
                self.player.set_viz_tap_active(on);
            }
            EngineCmd::AttachVizTap(producer) => {
                self.player.set_viz_tap(producer);
            }
            EngineCmd::SetModeSettings(update) => {
                self.mode_settings = update.settings;
                // Снятие/включение `volume_lock` активного режима меняет
                // эффективную громкость без отдельного `SetVolume` (§6.18,
                // ОВС-19).
                self.apply_gain();
                let strongest = update.diff.as_ref().and_then(|d| d.strongest(self.active_mode));
                match mode_apply_action(update.diff.as_ref(), self.active_mode, self.stopped) {
                    // `Store`, вызванный остановленным транспортом при
                    // изменении с реальным эффектом (§6.18): нет сессии для
                    // переоткрытия — чистая запись параметров старого пути,
                    // применится на следующем `Open` (как `apply_active_mode`
                    // при `self.stopped` сегодня).
                    ModeApplyAction::Store
                        if matches!(strongest, Some(ApplyKind::ReopenAtPosition) | Some(ApplyKind::SwitchDevice)) =>
                    {
                        let (legacy, device) = legacy_audio(&self.mode_settings, self.active_mode);
                        self.apply_legacy_fields(&legacy, device);
                    }
                    // Пустой дифф или изменение с `ApplyKind::Memory` —
                    // настройки уже скопированы выше, больше ничего не нужно.
                    ModeApplyAction::Store => {}
                    ModeApplyAction::ReopenAtPosition | ModeApplyAction::SwitchDevice => {
                        self.apply_active_mode();
                    }
                }
            }
            EngineCmd::SetActiveMode(mode) => {
                if mode != self.active_mode {
                    self.active_mode = mode;
                    self.store_active_mode();
                    self.apply_gain();
                }
            }
            EngineCmd::Shutdown => {
                self.shutdown();
                return true;
            }
        }
        false
    }

    /// Пересчитывает и применяет громкость к `Player` по
    /// `effective_gain(mode_settings, active_mode, last_volume, last_muted)`
    /// (§6.18, ОВС-18…ОВС-20, ADR-23): вызывается при `SetVolume`, смене
    /// активного режима и `SetModeSettings`, чтобы снятие фиксации громкости
    /// или выход из Строгого режима восстанавливал сохранённое значение
    /// `last_volume` без отдельного `SetVolume` от UI.
    fn apply_gain(&mut self) {
        let gain = effective_gain(&self.mode_settings, self.active_mode, self.last_volume, self.last_muted);
        self.player.set_volume(gain);
        self.apply_exclusive_gain();
    }

    /// Применяет громкость/mute к Эксклюзивному пути, если он открыт (§6.18
    /// шаг 50, ТЗ-137, ТЗ-140, ОВС-18, ADR-23): громкость — `effective_gain`
    /// с `muted = false` (Строгий/зафиксированная громкость Оптимального
    /// всегда 1.0 независимо от mute), mute — отдельным флагом
    /// `ExclusivePath::set_gain`, который гасит рендер любого вида, в т.ч. без
    /// ступени усиления. Вызывается из `apply_gain` и `EngineCmd::SetMuted`.
    fn apply_exclusive_gain(&mut self) {
        let Some(path) = &self.exclusive_path else { return };
        let gain = effective_gain(&self.mode_settings, self.active_mode, self.last_volume, false);
        path.set_gain(gain, self.last_muted);
    }

    /// `GainState` для `verdict`/`with_gain` (§6.3, §6.18 шаг 28, ADR-23) по
    /// тому же правилу источников громкости, что `apply_gain` (mute — вне
    /// `effective_gain`, см. его doc-комментарий). `100 %`/без mute —
    /// `GainState::Unity`, а не `Scaled(100)`: `verdict` помечает `Scaled(_)`
    /// как причину «не bit-perfect» независимо от процента, а `with_gain`
    /// уже не добавляет `Volume`-трансформ при `100` — `Unity` здесь
    /// сохраняет согласованность бейджа с реально построенным планом.
    fn gain_state(&self) -> GainState {
        if self.last_muted {
            return GainState::Muted;
        }
        let frac = effective_gain(&self.mode_settings, self.active_mode, self.last_volume, false);
        let pct = LegacyPlaybackState::pct_from_gain(frac);
        match Volume::new(pct) {
            Some(v) if v.get() != 100 => GainState::Scaled(v),
            _ => GainState::Unity,
        }
    }

    /// Пересобирает `self.active_path` после `SetVolume`/`SetMuted` (§6.3,
    /// §6.18, ADR-20, ТЗ-38, ТЗ-60): без активного пути (Оптимальный/Строгий
    /// режим, нет открытого потока) не делает ничего — нет `SignalPath` для
    /// пересборки. Поколение растёт при каждом вызове независимо от того,
    /// изменился ли вердикт — путь всё равно пересобран (новый гейн-транспорт
    /// в плане); `BadgeChanged` шлётся только при изменении самого бейджа,
    /// чтобы UI не перерисовывал неизменную плашку.
    fn refresh_path_gain(&mut self) {
        let gain = self.gain_state();
        if let Some(active) = self.active_path.take() {
            let generation = self.path_generation.saturating_add(1);
            let (rebuilt, changed) = rebuild_with_gain(active, &self.mode_settings, ModeKind::Compatible, gain, generation);
            self.path_generation = generation;
            let badge = rebuilt.badge.clone();
            self.active_path = Some(rebuilt);
            if changed {
                self.deps.events.emit(EngineEvent::BadgeChanged(badge));
            }
        }
        // Эксклюзивный путь (§6.18 шаг 50, ТЗ-137): тот же пересчёт бейджа без
        // переоткрытия устройства, своё поколение `path_generation`.
        if let Some(active) = self.exclusive_signal.take() {
            let generation = self.path_generation.saturating_add(1);
            let (rebuilt, changed) = rebuild_with_gain(active, &self.mode_settings, self.active_mode, gain, generation);
            self.path_generation = generation;
            let badge = rebuilt.badge.clone();
            self.exclusive_signal = Some(rebuilt);
            if changed {
                self.deps.events.emit(EngineEvent::BadgeChanged(badge));
            }
        }
    }

    /// Применяет `self.mode_settings`/`self.active_mode` к старому пути для
    /// `EngineCmd::SetModeSettings` (ОВС-14, ОВС-17, §6.18, ADR-22): при
    /// `Stopped` — только копия полей `Player` (нет сессии, переоткрывать
    /// нечего — применится на следующем `Open`); иначе одно переоткрытие на
    /// текущей позиции с сохранением играет/на паузе через
    /// `reopen_with_legacy`. `EngineCmd::SetActiveMode` переоткрытие здесь не
    /// запускает — см. `store_active_mode`.
    fn apply_active_mode(&mut self) {
        let (legacy, device) = legacy_audio(&self.mode_settings, self.active_mode);
        if self.stopped {
            self.apply_legacy_fields(&legacy, device);
        } else {
            self.reopen_with_legacy(legacy, device);
        }
    }

    /// Смена активного режима без переоткрытия движком (ТЗ-20, ОВ-10, §6.18
    /// строка 2399, ТС-9): по спецификации UI сама шлёт следом `Open` того
    /// же трека с текущей позиции — переоткрытие остаётся за ней. Если
    /// переоткрыть здесь (как `apply_active_mode`), а затем снова в
    /// `Open` от UI, получится два переоткрытия подряд — слышимый щелчок.
    /// Поэтому только копия полей старого пути: `release_engine` снимает
    /// поток (если он был открыт; на уже остановленном/безпотоковом движке
    /// не делает ничего), после чего `set_bit_perfect`/`set_dither` в
    /// `apply_legacy_fields` — чистая запись без собственного
    /// переоткрытия. Применится на следующем `Open`.
    fn store_active_mode(&mut self) {
        let (legacy, device) = legacy_audio(&self.mode_settings, self.active_mode);
        self.player.release_engine();
        self.apply_legacy_fields(&legacy, device);
    }

    /// Пишет поля `Player`, соответствующие старому пути (ТЗ-134, И-Р24).
    /// Сеттеры `bit_perfect`/`dither` переоткрывают поток сами, если он
    /// сейчас открыт (`self.player`'s `reopen_current`) — вызывающая сторона
    /// обязана сначала `release_engine`, если нужно ровно одно переоткрытие
    /// (см. `reopen_with_legacy`).
    fn apply_legacy_fields(&mut self, legacy: &LegacyAudio, device: Option<String>) {
        if let Some(name) = device {
            self.player.set_preferred_device(name);
        }
        self.player.set_exclusive_mode(legacy.exclusive_mode);
        self.player.set_fallback_policy(legacy.fallback_policy);
        self.player.set_dsd_mode(legacy.dsd_mode);
        self.player.set_resampler_mode(legacy.resampler_mode);
        self.player.set_resampler_algorithm(legacy.resampler_algorithm);
        self.player.set_fixed_rate(legacy.fixed_rate);
        self.player.set_prefer_family(legacy.prefer_family);
        self.player.set_fallback_rate(legacy.fallback_rate);
        self.player.set_ring_buffer_ms(legacy.ring_buffer_ms);
        self.player.set_bit_perfect(legacy.bit_perfect);
        self.player.set_dither(legacy.dither);
    }

    /// Одно переоткрытие текущего трека с новыми параметрами старого пути
    /// (`ApplyKind::ReopenAtPosition`/`SwitchDevice`, §6.18): `release_engine`
    /// снимает поток заранее, поэтому сеттеры `bit_perfect`/`dither` в
    /// `apply_legacy_fields` становятся чистой записью (их собственное
    /// `reopen_current` не срабатывает при снятом потоке) — переоткрытие
    /// ниже остаётся единственным, даже если изменились оба поля сразу
    /// (например, при смене режима). Позиция и играет/на паузе — как в
    /// `EngineCmd::Open`.
    fn reopen_with_legacy(&mut self, legacy: LegacyAudio, device: Option<String>) {
        let Some((req_gen, path)) = self.current.clone() else {
            self.apply_legacy_fields(&legacy, device);
            return;
        };
        let was_playing = self.player.is_playing();
        let (_, pos, _) = self.player.snapshot();
        self.player.release_engine();
        self.apply_legacy_fields(&legacy, device);
        match self.player.open(&path) {
            Ok(_info) => {
                if pos > 0.0 {
                    self.player.seek(pos);
                }
                if was_playing {
                    self.player.play();
                }
                self.refresh_transport();
            }
            Err(_msg) => {
                // Мост С3: как `SetDevice` — текст ошибки без классификации
                // (§6.6 — будущий шаг), ближайший по смыслу вариант — потеря
                // устройства.
                self.stopped = true;
                self.active_path = None;
                self.deps
                    .events
                    .emit(EngineEvent::OpenFailed { req_gen, err: OpenError::DeviceLost });
                self.refresh_transport();
            }
        }
    }

    /// Выход (§6.28 п. 2 в рамках моста, ТЗ-45): остановка транспорта
    /// (колбэк пишет тишину) → `release_engine` — сначала поток вывода
    /// (закрыть PCM, затем снять резервирование, И-Р1), затем декодер →
    /// `ShutdownComplete`. `CancelTest` и `lock_step(Shutdown)` появятся с
    /// тестом и замком (С8/С9). `PersistStore::save` здесь не вызывается:
    /// данных `bp_tests.toml`/`track_marks.toml` в движке моста ещё нет, а
    /// запись пустого содержимого затёрла бы файлы; запись добавляется вместе
    /// с их владельцем. `settings.toml`/`state.toml` пишет UI (ОВС-16).
    fn shutdown(&mut self) {
        if self.player.is_playing() {
            self.player.toggle();
        }
        self.player.release_engine();
        self.current = None;
        self.stopped = true;
        self.active_path = None;
        if let Some(p) = self.exclusive_path.take() {
            self.claim = Some(p.close());
        }
        self.exclusive_signal = None;
        self.deps.events.emit(EngineEvent::ShutdownComplete);
    }

    /// Перечисляет Shared-устройства и уведомляет UI (ТЗ-104, ТЗ-105, ADR-16,
    /// §6.2 п.3). `force_emit` шлёт `Devices` даже без изменения списка —
    /// нужно для первого перечисления при старте движка; по `RefreshDevices`
    /// и по сигналу `DeviceWatcher` событие шлётся только при изменении.
    fn refresh_devices(&mut self, force_emit: bool) {
        match self.catalog.refresh(self.deps.shared.as_mut()) {
            Ok(changed) => {
                if changed || force_emit {
                    self.deps.events.emit(EngineEvent::Devices(Arc::new(self.catalog.clone())));
                }
            }
            Err(e) => {
                self.deps.events.emit(EngineEvent::Notice(Notice::DevicesUnavailable(e)));
            }
        }
    }

    /// `Transport` по текущему состоянию (мост С3): `self.stopped` имеет
    /// приоритет (у легаси `Player` декодер остаётся загруженным и после
    /// `stop()`, `has_decoder()` не различает «остановлено» и «на паузе»);
    /// иначе — Эксклюзивный путь, если он открыт (§6.18, ТЗ-49): `Playing`
    /// или `Paused` по `SessionShared::playing`, поток вывода открыт в обоих
    /// случаях, `has_decoder()` легаси `Player` тут не при чём. Без
    /// Эксклюзивного пути — легаси `Player`: `Playing`, если идёт
    /// воспроизведение, `Paused`, если трек загружен, но не играет, иначе
    /// `Stopped`. Шлёт событие только при изменении состояния (§6.1 п. 3,
    /// И-Р13) — повторный вызов с тем же состоянием не даёт дубликата.
    fn refresh_transport(&mut self) {
        let state = if self.stopped {
            TransportState::Stopped
        } else if let Some(p) = &self.exclusive_path {
            if p.is_playing() {
                TransportState::Playing
            } else {
                TransportState::Paused
            }
        } else if self.player.is_playing() {
            TransportState::Playing
        } else if self.player.has_decoder() {
            TransportState::Paused
        } else {
            TransportState::Stopped
        };
        if self.last_transport != Some(state) {
            self.last_transport = Some(state);
            self.deps.events.emit(EngineEvent::Transport { state });
        }
    }

    /// Опрос сессии на каждой итерации цикла (§6.1 п. 1–5, И-Р13, ТЗ-60,
    /// ТЗ-102): только чтение состояния `Player`/`ExclusivePath`, без I/O
    /// (перечисление устройств по сигналу `DeviceWatcher` — исключение,
    /// ТЗ-105). Порядок — каталог устройств, резервирование, мид-трековый
    /// отказ декодера, затем потеря устройства/конец трека Эксклюзивного
    /// пути (StreamFault, §6.20, ТЗ-51, ТЗ-76) или конец трека Совместимого,
    /// затем транспорт, затем позиция.
    fn poll_session(&mut self) {
        if self.devices_dirty.swap(false, Ordering::Acquire) {
            self.refresh_devices(false);
        }

        if let Some(ev) = self.player.poll_reservation() {
            self.deps.events.emit(EngineEvent::Notice(Notice::Reservation(ev.clone())));
            match ev {
                ReservationEvent::Opened => {
                    self.stopped = false;
                    self.refresh_transport();
                }
                ReservationEvent::Failed(_) => {
                    self.player.stop();
                    self.stopped = true;
                    self.active_path = None;
                    let req_gen = self.current.as_ref().map_or(0, |(gen, _)| *gen);
                    // Мост С3: легаси `Player` отдаёт только текст ошибки;
                    // точная классификация `CaptureFailure` — следующий этап.
                    self.deps.events.emit(EngineEvent::OpenFailed {
                        req_gen,
                        err: OpenError::Capture(CaptureFailure::ReservationDenied { owner: None }),
                    });
                    self.refresh_transport();
                }
                ReservationEvent::Lost(_) => {
                    // `Player` уже освободил движок внутри себя (И-Р20).
                    self.stopped = true;
                    self.active_path = None;
                    let req_gen = self.current.as_ref().map_or(0, |(gen, _)| *gen);
                    self.deps.events.emit(EngineEvent::OpenFailed {
                        req_gen,
                        err: OpenError::Capture(CaptureFailure::ReservationLost),
                    });
                    self.refresh_transport();
                }
            }
        }

        // §6.18 шаг 19 (ТЗ-87): мид-трековый отказ декодера после `Ready` —
        // `DecodeWorker` после `Failed` сам доходит до EOF (`shared.ended`),
        // поэтому без этой проверки трек выглядел бы штатно доигранным.
        // Это пропуск, а не конец трека — `session_failed` гасит `Ended` ниже.
        if let Some(e) = self.player.poll_decode_failure() {
            if let Some((req_gen, path)) = &self.current {
                let reason = match e {
                    // Кадр отказа недоступен дёшево на этой границе (мост С3,
                    // без `as`-приведений позиции): фиксируется 0, точная
                    // позиция — следующий этап.
                    FileError::Io(kind) => FileError::ReadDuringPlayback { at_frame: 0, kind },
                    other => other,
                };
                self.deps.events.emit(EngineEvent::Skipped {
                    req_gen: *req_gen,
                    path: Arc::clone(path),
                    reason: SkipReason::File(reason),
                });
            }
            self.session_failed = true;
        }

        // Эксклюзивный путь (§6.20, ТЗ-51, ТЗ-76): счётчики динамики
        // переносятся из `SessionShared` каждый опрос; `fault` — признак
        // потери устройства (`DeviceUnavailable`/`Stalled`, поток вывода уже
        // вышел из цикла) обрабатывается через `lock_input(DeviceLost)` —
        // переиспользует переход `Held → Failed(DeviceDisconnected)`
        // (`StopPlayback`, `Release`) вместо дублирования остановки здесь.
        let exclusive_poll =
            self.exclusive_path.as_ref().map(|p| (p.fault(), p.ended(), p.underruns(), p.xruns()));
        if let Some((fault, ended, underruns, xruns)) = exclusive_poll {
            self.dynamics.underruns = underruns;
            self.dynamics.xruns = xruns;
            if fault != StreamFault::None {
                self.lock_input(LockInput::DeviceLost);
                self.deps.events.emit(EngineEvent::DeviceLost { reaction: Reaction::StopWithError });
            } else if ended {
                if !self.session_failed {
                    if let Some((req_gen, _)) = &self.current {
                        self.deps.events.emit(EngineEvent::Ended { session: *req_gen });
                    }
                }
                self.exclusive_path = None;
                self.exclusive_signal = None;
                self.active_path = None;
            }
        } else if self.player.ended() {
            if !self.session_failed {
                if let Some((req_gen, _)) = &self.current {
                    self.deps.events.emit(EngineEvent::Ended { session: *req_gen });
                }
            }
            self.active_path = None;
            // Движок — единственный потребитель флага конца трека теперь
            // (легаси UI-слой флаг больше не читает).
            self.player.clear_end();
        }

        self.refresh_transport();

        let pos = if let Some(path) = &self.exclusive_path {
            path.position_secs()
        } else {
            self.player.snapshot().1
        };
        let now = self.deps.clock.now();
        let due = self.last_pos_at.is_none_or(|at| now.saturating_since(at) >= POSITION_INTERVAL);
        if due && self.last_pos != Some(pos) {
            self.last_pos = Some(pos);
            self.last_pos_at = Some(now);
            self.deps.events.emit(EngineEvent::Position { secs: pos });
        }
    }
}

pub use crate::audio::backend::BackendCaps;

/// Handle движка в UI (§2.8, ADR-01): хранит неблокирующий `Sender` и
/// `JoinHandle` для выхода (ТЗ-45).
pub struct EngineHandle {
    tx: mpsc::Sender<EngineCmd>,
    join: Option<JoinHandle<()>>,
}

impl EngineHandle {
    /// Запускает поток `apap-engine` через `deps.spawner` (ТЗ-88, ADR-20) и
    /// возвращает синхронно `BackendCaps` платформы из `deps.caps` (§2.8,
    /// ОВС-12, ТЗ-109).
    pub fn spawn(deps: EngineDeps) -> Result<(EngineHandle, BackendCaps), EngineFault> {
        let caps = deps.caps;
        let spawner = Arc::clone(&deps.spawner);
        let (tx, rx) = mpsc::channel();
        let join = spawner.spawn(
            "apap-engine",
            Box::new(move || {
                let engine = Engine::new(deps, caps);
                engine.run(rx);
            }),
        )?;
        Ok((EngineHandle { tx, join: Some(join) }, caps))
    }

    /// Неблокирующая отправка команды (ADR-01). `false`, если движок уже
    /// завершился (получатель отключён).
    pub fn send(&self, cmd: EngineCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }

    /// Отдельный отправитель команд для владельцев вне `MusicApp` (ADR-01):
    /// не держит `JoinHandle`, поэтому его `Drop` не останавливает движок.
    pub fn sender(&self) -> EngineSender {
        EngineSender {
            tx: self.tx.clone(),
        }
    }

    /// Дожидается завершения потока движка. Берёт `JoinHandle` из `self`,
    /// чтобы `Drop` не пытался join повторно.
    pub fn join(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Неблокирующий отправитель команд движка (ADR-01): `Clone + Send`,
/// выход и `join` остаются за [`EngineHandle`].
#[derive(Clone)]
pub struct EngineSender {
    tx: mpsc::Sender<EngineCmd>,
}

impl EngineSender {
    /// Неблокирующая отправка команды (ADR-01). `false`, если движок уже
    /// завершился (получатель отключён).
    pub fn send(&self, cmd: EngineCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(EngineCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::audio::backend::catalog::FakeDeviceWatcher;
    use crate::audio::backend::os_probe::FakeOsProbe;
    use crate::audio::backend::shared::{fake_device, FakeSharedBackend};
    use crate::audio::backend::{BackendError, SharedDeviceId};
    use crate::audio::path::OsStage;
    use crate::audio::format::{ChannelLayout, Codec, Container, SampleRate, SourceFormat, SourceKind};
    use crate::audio::clock::MonotonicClock;
    use crate::engine::sink::VecSink;
    use crate::engine::source::FakeSource;
    use crate::audio::testing::{FakeExclusiveBackend, FakeServerProbe};
    use crate::engine::spawner::{FailingSpawner, StdSpawner};
    use crate::platform::fs::{FsPersistStore, MemStore};
    use std::path::PathBuf;

    fn pcm_format() -> SourceFormat {
        SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: crate::audio::format::BitDepth::new(16).expect("bits") },
        }
    }

    fn fake_deps(events: Box<dyn crate::engine::sink::EventSink>, spawner: Arc<dyn crate::engine::spawner::ThreadSpawner>) -> EngineDeps {
        fake_deps_with(events, spawner, Box::new(FakeSharedBackend::with_devices(Vec::new())), Box::new(FakeDeviceWatcher::new()))
    }

    fn fake_deps_with(
        events: Box<dyn crate::engine::sink::EventSink>,
        spawner: Arc<dyn crate::engine::spawner::ThreadSpawner>,
        shared: Box<dyn crate::audio::backend::shared::SharedBackend>,
        watcher: Box<dyn crate::audio::backend::catalog::DeviceWatcher>,
    ) -> EngineDeps {
        let mem = MemStore::new();
        EngineDeps {
            shared,
            exclusive: Some(Box::new(FakeExclusiveBackend::new())),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher,
            sources: Arc::new(FakeSource::with_format(pcm_format())),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events,
            spawner,
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
        }
    }

    #[test]
    fn engine_spawn_and_shutdown() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));

        let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
        assert!(handle.send(EngineCmd::SetVolume(0.5)));
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::ShutdownComplete)));
    }

    #[test]
    fn engine_spawn_failure_reported() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink), Arc::new(FailingSpawner::new(1)));

        match EngineHandle::spawn(deps) {
            Err(EngineFault::SpawnFailed { what: "apap-engine" }) => {}
            Err(other) => panic!("expected SpawnFailed, got {other:?}"),
            Ok(_) => panic!("expected spawn to fail"),
        }
    }

    #[test]
    fn engine_startup_emits_devices_once() {
        let sink = VecSink::new();
        let shared = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        let deps =
            fake_deps_with(Box::new(sink.clone()), Arc::new(StdSpawner), Box::new(shared), Box::new(FakeDeviceWatcher::new()));

        let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        let devices: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Devices(catalog) => Some(catalog.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].shared.len(), 1);
        assert_eq!(devices[0].shared[0].id, SharedDeviceId::new("hw:0"));
    }

    #[test]
    fn engine_device_watcher_fire_unchanged_emits_no_duplicate() {
        let sink = VecSink::new();
        let shared = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        let watcher = FakeDeviceWatcher::new();
        let trigger = watcher.trigger_handle();
        let deps =
            fake_deps_with(Box::new(sink.clone()), Arc::new(StdSpawner), Box::new(shared), Box::new(watcher));

        let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
        trigger.fire();
        assert!(handle.send(EngineCmd::SetVolume(0.5)));
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        let devices_count = events.iter().filter(|e| matches!(e, EngineEvent::Devices(_))).count();
        assert_eq!(devices_count, 1);
    }

    #[test]
    fn engine_startup_device_error_emits_notice() {
        let sink = VecSink::new();
        let shared = FakeSharedBackend::with_error(BackendError::Unavailable("no host".to_string()));
        let deps =
            fake_deps_with(Box::new(sink.clone()), Arc::new(StdSpawner), Box::new(shared), Box::new(FakeDeviceWatcher::new()));

        let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(
            e,
            EngineEvent::Notice(Notice::DevicesUnavailable(BackendError::Unavailable(msg))) if msg == "no host"
        )));
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::Devices(_))));
    }

    /// `StreamDesc` старого пути, точно совпадающий с `pcm_format()` (ТЗ-52,
    /// §6.3 шаг 28): частота, каналы и формат равны источнику, поэтому
    /// `plan()` не добавляет `Resample`/`Downmix` — только безлоссовый
    /// `IntToFloat` (16 бит → F32), не влияющий на бейдж (ТЗ-53).
    fn matching_desc() -> StreamDesc {
        StreamDesc { device: "pipewire".to_owned(), rate: 44_100, channels: 2, format: "F32", ..StreamDesc::default() }
    }

    #[test]
    fn build_compatible_path_ok_reports_clean_shared_badge() {
        let src = pcm_format();
        let desc = matching_desc();
        let settings = ModeSettings::default();
        let active =
            build_compatible_path(&src, &desc, OsStage::Unknown, &settings, GainState::Unity, 1).expect("plan ok");
        assert_eq!(active.badge, BadgeState::Shared(crate::audio::verdict::SharedBadge::Clean));
        assert_eq!(active.path.generation, 1);
    }

    #[test]
    fn build_compatible_path_muted_badge_is_not_bit_perfect() {
        let src = pcm_format();
        let desc = matching_desc();
        let settings = ModeSettings::default();
        let active =
            build_compatible_path(&src, &desc, OsStage::Unknown, &settings, GainState::Muted, 7).expect("plan ok");
        assert!(matches!(active.badge, BadgeState::Shared(crate::audio::verdict::SharedBadge::NotBitPerfect(_))));
        assert_eq!(active.path.generation, 7);
    }

    #[test]
    fn build_compatible_path_never_blocks_on_channel_mismatch() {
        // Совместимый режим не блокирует (ТЗ-52): несовпадение каналов
        // источника (стерео) и устройства старого пути (моно) даёт
        // `Downmix`, а не `Blocked` — блокировка в `plan()` только для
        // Строгого режима (§6.3).
        let src = pcm_format();
        let desc = StreamDesc { device: "pipewire".to_owned(), rate: 44_100, channels: 1, format: "F32", ..StreamDesc::default() };
        let settings = ModeSettings::default();
        let active = build_compatible_path(&src, &desc, OsStage::Unknown, &settings, GainState::Unity, 1)
            .expect("Совместимый режим не блокирует");
        assert_eq!(active.plan.target.channels.get(), 1);
    }

    #[test]
    fn volume_rebuilds_path_without_reopen() {
        let src = pcm_format();
        let desc = matching_desc();
        let settings = ModeSettings::default();
        let active =
            build_compatible_path(&src, &desc, OsStage::Unknown, &settings, GainState::Unity, 1).expect("plan ok");

        let opened_facts = active.facts.clone();
        let scaled = GainState::Scaled(Volume::new(50).expect("valid percent"));
        let (rebuilt, changed) = rebuild_with_gain(active, &settings, ModeKind::Compatible, scaled, 2);

        assert_eq!(rebuilt.path.generation, 2);
        assert_eq!(rebuilt.facts, opened_facts);
        assert!(matches!(rebuilt.badge, BadgeState::Shared(crate::audio::verdict::SharedBadge::NotBitPerfect(_))));
        assert!(changed);
    }

    #[test]
    fn badge_changed_only_on_verdict_change() {
        let src = pcm_format();
        let desc = matching_desc();
        let settings = ModeSettings::default();
        let active =
            build_compatible_path(&src, &desc, OsStage::Unknown, &settings, GainState::Unity, 1).expect("plan ok");

        let scaled_50 = GainState::Scaled(Volume::new(50).expect("valid percent"));
        let (at_50, changed_to_scaled) = rebuild_with_gain(active, &settings, ModeKind::Compatible, scaled_50, 2);
        assert!(changed_to_scaled);

        let scaled_40 = GainState::Scaled(Volume::new(40).expect("valid percent"));
        let (at_40, changed_within_scaled) = rebuild_with_gain(at_50, &settings, ModeKind::Compatible, scaled_40, 3);
        assert!(!changed_within_scaled);

        let (_at_unity, changed_back_to_unity) = rebuild_with_gain(at_40, &settings, ModeKind::Compatible, GainState::Unity, 4);
        assert!(changed_back_to_unity);
    }

    #[test]
    fn mute_unmute_restores_clean_badge() {
        let src = pcm_format();
        let desc = matching_desc();
        let settings = ModeSettings::default();
        let active =
            build_compatible_path(&src, &desc, OsStage::Unknown, &settings, GainState::Unity, 1).expect("plan ok");

        let (muted, changed_to_muted) = rebuild_with_gain(active, &settings, ModeKind::Compatible, GainState::Muted, 2);
        assert!(changed_to_muted);
        assert!(matches!(muted.badge, BadgeState::Shared(crate::audio::verdict::SharedBadge::NotBitPerfect(_))));

        let (unmuted, changed_back) = rebuild_with_gain(muted, &settings, ModeKind::Compatible, GainState::Unity, 3);
        assert!(changed_back);
        assert_eq!(unmuted.badge, BadgeState::Shared(crate::audio::verdict::SharedBadge::Clean));
    }

    #[test]
    fn drain_internal_consumes_all_pending_messages() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let tx = engine.internal_sender();
        tx.send(InternalMsg::RtPriority { session: 1, granted: Some(5) }).expect("send ok");
        tx.send(InternalMsg::ReservationLost { card: 2 }).expect("send ok");

        engine.drain_internal();

        assert!(matches!(engine.internal_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn claim_done_for_unknown_attempt_is_ignored() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());

        engine.handle_internal(InternalMsg::ClaimDone {
            attempt: 42,
            result: Err(CaptureFailure::NoDeviceSelected),
        });

        assert!(sink.take().is_empty());
    }

    /// §6.19 (ТЗ-118, ТЗ-119, ADR-01): `Startup` в Оптимальном режиме с
    /// выбранным устройством захватывает его через служебный поток — `Lock`
    /// проходит `Acquiring` → `Held`, захват остаётся в `self.claim`.
    #[test]
    fn lock_input_startup_acquires_and_holds_device() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() }];
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);

        let acquiring = sink.take();
        assert!(matches!(acquiring.as_slice(), [EngineEvent::Lock(LockState::Acquiring { .. })]));
        assert!(matches!(engine.lock, LockState::Acquiring { .. }));

        let msg = engine.internal_rx.recv().expect("claim done");
        engine.handle_internal(msg);

        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, dev),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(engine.claim.is_some());
        let held = sink.take();
        assert!(matches!(held.as_slice(), [EngineEvent::Lock(LockState::Held { .. })]));
    }

    /// §6.19 (ТЗ-118, ТЗ-119, ADR-01): отказ захвата, возвращённый
    /// `ExclusiveBackend::claim_job`, переводит замок в `Failed` и не
    /// оставляет захвата.
    #[test]
    fn lock_input_scripted_acquire_failure_emits_failed() {
        let sink = VecSink::new();
        let mem = MemStore::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(
                FakeExclusiveBackend::new().with_acquire(Err(CaptureFailure::BusyOutsideProtocol)),
            )),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(pcm_format())),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() }];
        engine.active_mode = ModeKind::Strict;
        engine.mode_settings.strict.device = Some(dev.clone());
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);
        sink.take();
        let msg = engine.internal_rx.recv().expect("claim done");
        engine.handle_internal(msg);

        match &engine.lock {
            LockState::Failed(CaptureFailure::BusyOutsideProtocol) => {}
            other => panic!("expected Failed(DeviceDisconnected), got {other:?}"),
        }
        assert!(engine.claim.is_none());
        let events = sink.take();
        assert!(matches!(
            events.as_slice(),
            [EngineEvent::Lock(LockState::Failed(CaptureFailure::BusyOutsideProtocol))]
        ));
    }

    /// §6.18 (ADR-24, ТЗ-143, ТЗ-144): нет выбранного устройства режима, зонд
    /// сообщает карту системного выхода, совпадающую с записью каталога —
    /// `resolve_device` автовыбирает устройство, оно пишется в копию опций
    /// режима и шлётся `DeviceAutoSelected`, захват уходит в `Acquiring`.
    #[test]
    fn lock_input_auto_selects_device_from_server_probe() {
        let sink = VecSink::new();
        let mem = MemStore::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(FakeExclusiveBackend::new())),
            server_probe: Box::new(
                FakeServerProbe::desktop().with_sink_card(Some(crate::audio::reservation::SinkCard { card: 1, device: Some(0) })),
            ),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(pcm_format())),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() }];
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = None;
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);

        assert_eq!(engine.mode_settings.optimal.device, Some(dev.clone()));
        assert!(matches!(engine.lock, LockState::Acquiring { .. }));
        let events = sink.take();
        assert!(events.iter().any(
            |e| matches!(e, EngineEvent::DeviceAutoSelected { mode: ModeKind::Optimal, device } if *device == dev)
        ));
    }

    /// §6.18 (ADR-24, ТЗ-32): нет выбранного устройства режима, зонд не
    /// сообщает карту системного выхода — `resolve_device` возвращает
    /// `None`, замок падает в `Failed(NoDeviceSelected)` без захвата:
    /// `start_claim` не вызывается, служебный поток не запускается.
    #[test]
    fn lock_input_without_resolved_device_fails_without_claim() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = None;
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);

        match &engine.lock {
            LockState::Failed(CaptureFailure::NoDeviceSelected) => {}
            other => panic!("expected Failed(NoDeviceSelected), got {other:?}"),
        }
        assert!(engine.claim.is_none());
        assert!(engine.internal_rx.try_recv().is_err());
        let events = sink.take();
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::DeviceAutoSelected { .. })));
    }

    /// §6.18 (ADR-24, ТЗ-143): устройство режима уже выбрано — `resolve_device`
    /// возвращает `Selected`, автовыбора нет и `DeviceAutoSelected` не шлётся.
    #[test]
    fn lock_input_with_selected_device_skips_auto_select_event() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() }];
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);

        assert!(matches!(engine.lock, LockState::Acquiring { .. }));
        let events = sink.take();
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::DeviceAutoSelected { .. })));
    }

    /// §6.19 (ТЗ-123): `ClaimDone` от отменённой попытки (несовпадающий
    /// `attempt`) отбрасывается, даже когда замок сейчас `Acquiring`.
    #[test]
    fn claim_done_stale_attempt_while_acquiring_is_ignored() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.lock = LockState::Acquiring { started: engine.deps.clock.now() };
        engine.claim_attempt = 1;

        engine.handle_internal(InternalMsg::ClaimDone {
            attempt: 99,
            result: Err(CaptureFailure::NoDeviceSelected),
        });

        assert!(sink.take().is_empty());
        assert!(engine.claim.is_none());
        assert!(matches!(engine.lock, LockState::Acquiring { .. }));
    }

    /// §6.19 (ТЗ-120, ТЗ-121): `UserOpen` из `Held` останавливает
    /// воспроизведение, освобождает захват и забывает точку повторного
    /// захвата — `Lock(Open)`, `claim` и `reacquire` пустые.
    #[test]
    fn lock_input_user_open_from_held_stops_and_releases() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev } };

        engine.lock_input(LockInput::UserOpen);

        assert!(matches!(engine.lock, LockState::Open));
        assert!(engine.claim.is_none());
        assert!(engine.reacquire.is_none());
        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Lock(LockState::Open))));
    }

    /// §6.19 (ТЗ-120, ТЗ-121): смена режима на Совместимый из `Held` даёт
    /// `Lock(NotApplicable)` и освобождает захват тем же набором эффектов.
    #[test]
    fn lock_input_mode_selected_compatible_from_held_releases() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev } };

        engine.lock_input(LockInput::ModeSelected(ModeKind::Compatible));

        assert!(matches!(engine.lock, LockState::NotApplicable));
        assert!(engine.claim.is_none());
        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Lock(LockState::NotApplicable))));
    }

    /// §6.19 (ТЗ-123): `Release` во время ещё не завершённого захвата
    /// (`UserOpen` пришёл, пока замок `Acquiring`) бампает `claim_attempt` —
    /// доклад `ClaimDone` от этой попытки приходит позже и должен быть
    /// отброшен как устаревший, не оставляя захвата.
    #[test]
    fn release_during_in_flight_capture_drops_stale_claim_done() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() }];
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);
        assert!(matches!(engine.lock, LockState::Acquiring { .. }));
        sink.take();
        let stale = engine.internal_rx.recv().expect("claim done");

        // Пользователь открывает замок, пока захват ещё не завершился:
        // `lock_step` не определяет переход `UserOpen` из `Acquiring`
        // (§6.19), поэтому замок переводится прямо через `Held`-эффекты, не
        // дожидаясь завершения попытки — моделируем это напрямую через
        // `lock_input` после ручного перевода в `Held`.
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev } };
        engine.lock_input(LockInput::UserOpen);
        sink.take();

        engine.handle_internal(stale);

        assert!(engine.claim.is_none());
        assert!(matches!(engine.lock, LockState::Open));
    }

    /// §6.18 шаг 45 (ТЗ-48, ТЗ-137): `Open` в Оптимальном режиме с удержанным
    /// замком диспетчеризуется в `ExclusivePath` — `EngineEvent::Opened` с
    /// `path.device.accepted` из `DriverFormat::Confirmed`, старый `Player`
    /// не используется (без него тест не открыл бы файл).
    ///
    /// Источник — `FloatPcm` (WAV float32): у `SilentSource` нет своего
    /// `ring_payload()` (дефолт трейта — `F32`, §6.18 шаг 6), а `plan()` для
    /// целого PCM-источника без ресемплинга выбирает ring `ExactI32`
    /// (`f32_payload`, §6.3 шаг 7) — для фейкового источника это разные
    /// нагрузки, и `exclusive_feed` отказал бы `PayloadMismatch`. Float-
    /// источник даёт `plan()` ring `F32` при любом целом формате устройства
    /// (`f32_payload` истинна для `!SourceKind::Pcm`) — совпадает с фейком.
    #[test]
    fn open_in_optimal_mode_dispatches_to_exclusive_path_confirmed() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new().with_configure(Ok((
            crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                rate: SampleRate::new(44_100).expect("rate"),
                sample: crate::audio::format::OutputSampleFormat::S32Le,
                channels: std::num::NonZeroU8::new(2).expect("channels"),
            }),
            Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
        )));
        let float_format = SourceFormat {
            container: Container::Wav,
            codec: Codec::Wav,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::FloatPcm { double: false },
        };
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(float_format)),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev.clone() } };
        engine.claim = Some(crate::audio::backend::exclusive::DeviceClaim::new(
            None,
            crate::audio::reservation::Reservation::UnavailableNoServer,
            crate::audio::backend::exclusive::ClaimInfo { device: dev },
        ));

        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });

        let events = sink.take();
        match events.first() {
            Some(EngineEvent::Opened { path: Some(signal), .. }) => {
                assert!(signal.device.accepted.is_some());
            }
            other => panic!("expected Opened event first, got {other:?}"),
        }
        // Захват переходит в `ExclusivePath` (§6.19): `self.claim` после
        // успешного открытия пуст — замок держит сессию, не `claim`.
        assert!(engine.claim.is_none());
        assert!(engine.exclusive_path.is_some());
    }

    /// Собирает движок с уже открытым Эксклюзивным путём (фейковый бэкенд,
    /// `FloatPcm`-источник — как в `open_in_optimal_mode_dispatches_to_exclusive_path_confirmed`)
    /// для тестов транспорта (§6.18, ТЗ-49, ТЗ-43, шаг 48).
    fn engine_with_exclusive_path_open(sink: VecSink, autoplay: bool) -> Engine {
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new().with_configure(Ok((
            crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                rate: SampleRate::new(44_100).expect("rate"),
                sample: crate::audio::format::OutputSampleFormat::S32Le,
                channels: std::num::NonZeroU8::new(2).expect("channels"),
            }),
            Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
        )));
        let float_format = SourceFormat {
            container: Container::Wav,
            codec: Codec::Wav,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::FloatPcm { double: false },
        };
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(float_format)),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev.clone() } };
        engine.claim = Some(crate::audio::backend::exclusive::DeviceClaim::new(
            None,
            crate::audio::reservation::Reservation::UnavailableNoServer,
            crate::audio::backend::exclusive::ClaimInfo { device: dev },
        ));

        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay });
        assert!(engine.exclusive_path.is_some());
        engine
    }

    /// §6.18, ТЗ-49 (шаг 48): открытие с `autoplay: true` сразу даёт
    /// `Playing` через `ExclusivePath::play`; `Pause` переводит на
    /// `SessionShared::playing = false` без остановки потока вывода
    /// (`Paused`); `Play` снова включает вывод (`Playing`).
    #[test]
    fn exclusive_path_transport_play_pause_play() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink, true);

        assert_eq!(engine.last_transport, Some(TransportState::Playing));

        engine.handle(EngineCmd::Pause);
        assert_eq!(engine.last_transport, Some(TransportState::Paused));

        engine.handle(EngineCmd::Play);
        assert_eq!(engine.last_transport, Some(TransportState::Playing));
    }

    /// §6.18, ТЗ-43, ТЗ-120, ТЗ-121 (шаг 48): `Stop` на открытом Эксклюзивном
    /// пути закрывает его (`ExclusivePath::close`), а не дропает — захват
    /// возвращается в `self.claim`, замок остаётся удержан (И-Р1, §6.19:
    /// `Held ⇔ self.claim.is_some()` без открытого пути).
    #[test]
    fn stop_closes_exclusive_path_and_keeps_claim_held() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink, true);

        engine.handle(EngineCmd::Stop);

        assert!(engine.exclusive_path.is_none());
        assert!(engine.claim.is_some());
        assert_eq!(engine.last_transport, Some(TransportState::Stopped));
    }

    /// §6.18, ТЗ-48, ТЗ-113 (шаг 48): повторный `Open` (смена трека) на уже
    /// открытом Эксклюзивном пути закрывает старый путь и возвращает захват в
    /// `self.claim` первой строкой `open_exclusive` — иначе `self.claim` был
    /// бы пуст (его держит старый `ExclusivePath`) и второе открытие отказало
    /// бы `OpenFailed(Capture(NoDeviceSelected))`, хотя замок удержан.
    #[test]
    fn second_open_while_exclusive_path_open_succeeds() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();

        engine.handle(EngineCmd::Open { req_gen: 2, path: Arc::from(Path::new("/music/b.flac")), start_secs: 0.0, autoplay: true });

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Opened { .. })));
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::OpenFailed { .. })));
        assert!(engine.exclusive_path.is_some());
        assert!(engine.claim.is_none());
    }

    /// §6.16, ТЗ-99, ADR-13 (шаг 49): `EngineCmd::Seek` на открытом
    /// Эксклюзивном пути — фаза 1 рукопожатия (`ExclusivePath::seek`), а не
    /// легаси `Player::seek`: целевой кадр считается по частоте ring, запрос
    /// виден через `SessionShared`.
    #[test]
    fn seek_on_exclusive_path_requests_seek_by_frame() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink, true);
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());

        engine.handle(EngineCmd::Seek { secs: 1.5 });

        assert_eq!(shared.seek_requested.load(std::sync::atomic::Ordering::Acquire), 1);
        let rate = engine.exclusive_path.as_ref().expect("exclusive path open").frame_rate();
        let expected = secs_to_frames(1.5, rate);
        assert_eq!(shared.seek_target_frame.load(std::sync::atomic::Ordering::Relaxed), expected);
    }

    /// §6.16, ТЗ-99 (шаг 49): seek на паузе тоже запрашивает фазу 1 и не
    /// трогает состояние транспорта — пауза остаётся паузой.
    #[test]
    fn seek_on_exclusive_path_while_paused_does_not_change_transport() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink, true);
        engine.handle(EngineCmd::Pause);
        assert_eq!(engine.last_transport, Some(TransportState::Paused));
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());

        engine.handle(EngineCmd::Seek { secs: 2.0 });

        assert_eq!(shared.seek_requested.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(engine.last_transport, Some(TransportState::Paused));
        assert!(!engine.exclusive_path.as_ref().expect("exclusive path open").is_playing());
    }

    /// §6.18 шаг 50, ТЗ-137, ОВС-18 (ADR-23): `EngineCmd::SetVolume` на
    /// открытом Эксклюзивном пути без фиксации громкости (`volume_lock`)
    /// применяет `effective_gain` к разделяемому состоянию сессии через
    /// `ExclusivePath::set_gain`, минуя переоткрытие устройства.
    #[test]
    fn set_volume_applies_gain_to_exclusive_path() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink, true);
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());

        engine.handle(EngineCmd::SetVolume(0.5));

        let expected = effective_gain(&engine.mode_settings, engine.active_mode, 0.5, false);
        assert_eq!(shared.gain(), expected);
        assert!(!shared.muted.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// §6.18 шаг 50, ТЗ-140, ОВС-18 (ADR-23): `EngineCmd::SetMuted` гасит
    /// Эксклюзивный путь через флаг `muted`, не трогая сохранённый
    /// коэффициент громкости (ворота тишины `NoGain`/`DopRender` читают
    /// `muted` напрямую, независимо от вида рендера).
    #[test]
    fn set_muted_gates_exclusive_path_without_changing_gain() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink, true);
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        let gain_before = shared.gain();

        engine.handle(EngineCmd::SetMuted(true));

        assert!(shared.muted.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(shared.gain(), gain_before);
    }

    /// §6.17, ТЗ-51 (шаг 51): `poll_session` на открытом Эксклюзивном пути
    /// берёт позицию из `ExclusivePath::position_secs` (кадры ring по
    /// частоте цели устройства), а не из легаси `Player`.
    #[test]
    fn poll_session_reports_exclusive_path_position() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();
        let rate = engine.exclusive_path.as_ref().expect("exclusive path open").frame_rate();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.pos_frames.store(u64::from(rate) * 2, Ordering::Relaxed);

        engine.poll_session();

        let events = sink.take();
        let pos = events.iter().find_map(|e| match e {
            EngineEvent::Position { secs } => Some(*secs),
            _ => None,
        });
        assert_eq!(pos, Some(2.0));
    }

    /// §6.20, ТЗ-51, ТЗ-76 (шаг 51): `StreamFault` потока вывода на открытом
    /// Эксклюзивном пути переносит счётчики динамики в `self.dynamics` и
    /// переиспользует `lock_input(DeviceLost)` — переход `Held →
    /// Failed(DeviceDisconnected)` c эффектами `StopPlayback`/`Release`
    /// (§6.19) — и шлёт `DeviceLost { reaction: StopWithError }` в UI.
    #[test]
    fn poll_session_stream_fault_stops_playback_and_releases_lock() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.underruns.store(3, Ordering::Relaxed);
        shared.xruns.store(5, Ordering::Relaxed);
        shared.set_fault(StreamFault::DeviceUnavailable);

        engine.poll_session();

        assert_eq!(engine.dynamics.underruns, 3);
        assert_eq!(engine.dynamics.xruns, 5);
        let events = sink.take();
        assert!(events
            .iter()
            .any(|e| matches!(e, EngineEvent::DeviceLost { reaction: Reaction::StopWithError })));
        assert!(matches!(engine.lock, LockState::Failed(CaptureFailure::DeviceDisconnected)));
        assert!(engine.exclusive_path.is_none());
        assert_eq!(engine.last_transport, Some(TransportState::Stopped));
    }

    /// §6.17, ТЗ-51 (шаг 51): штатный конец трека на Эксклюзивном пути
    /// (`SessionShared::ended`) шлёт `Ended` c `req_gen` текущей сессии и
    /// закрывает путь — аналог обработки `Player::ended` в Совместимом
    /// режиме.
    #[test]
    fn poll_session_exclusive_path_ended_emits_ended_and_clears_path() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.ended.store(true, Ordering::Release);

        engine.poll_session();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Ended { session: 1 })));
        assert!(engine.exclusive_path.is_none());
    }

    /// §6.18 п. 6, ТЗ-113 (шаг 47): `open_exclusive` открывает источник через
    /// `open_for_route` с маршрутом из построенного плана, а не через
    /// маршрут-агностичный `open`. Для PCM-источника без DSD план строит
    /// `Route::Pcm` — фейк фиксирует именно его.
    #[test]
    fn open_exclusive_opens_source_via_plan_route() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new().with_configure(Ok((
            crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                rate: SampleRate::new(44_100).expect("rate"),
                sample: crate::audio::format::OutputSampleFormat::S32Le,
                channels: std::num::NonZeroU8::new(2).expect("channels"),
            }),
            Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
        )));
        let float_format = SourceFormat {
            container: Container::Wav,
            codec: Codec::Wav,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::FloatPcm { double: false },
        };
        let fake_source = Arc::new(FakeSource::with_format(float_format));
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::clone(&fake_source) as Arc<dyn crate::engine::source::SourceOpener>,
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev.clone() } };
        engine.claim = Some(crate::audio::backend::exclusive::DeviceClaim::new(
            None,
            crate::audio::reservation::Reservation::UnavailableNoServer,
            crate::audio::backend::exclusive::ClaimInfo { device: dev },
        ));

        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });

        assert_eq!(fake_source.last_route(), Some(crate::audio::path::Route::Pcm));
        assert_eq!(fake_source.opens(), 1);
    }

    /// §6.18 шаг 45 (ТЗ-48, ТЗ-137): `Open` в Строгом режиме без удержанного
    /// замка отказывает без попытки захвата — `OpenFailed(Capture(_))` до
    /// обращения к `ExclusiveBackend` (гвард — первая операция
    /// `open_exclusive`, раньше `self.deps.exclusive` не трогается): `claim`
    /// остаётся `None`, замок не меняется.
    #[test]
    fn open_in_strict_mode_without_held_lock_fails_without_capture_attempt() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Strict;
        engine.lock = LockState::Open;

        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });

        let events = sink.take();
        assert!(matches!(
            events.as_slice(),
            [EngineEvent::OpenFailed { err: OpenError::Capture(CaptureFailure::NoDeviceSelected), .. }]
        ));
        assert!(engine.claim.is_none());
        assert!(matches!(engine.lock, LockState::Open));
    }

    /// §6.18 шаг 46 (ADR-14, ТЗ-86): отказ `configure` на Эксклюзивном пути
    /// (`ExclusiveOpenReason::Configure`) классифицируется в
    /// `OpenError::DeviceLost` через `exclusive_open_error`; замок остаётся
    /// удержан — `claim` возвращается в `self.claim`, а не теряется (§6.19).
    #[test]
    fn open_exclusive_configure_failure_maps_to_device_lost_and_keeps_claim() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new()
            .with_configure(Err(crate::audio::backend::exclusive::ConfigureError("hw_params busy".to_owned())));
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(pcm_format())),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev.clone() } };
        engine.claim = Some(crate::audio::backend::exclusive::DeviceClaim::new(
            None,
            crate::audio::reservation::Reservation::UnavailableNoServer,
            crate::audio::backend::exclusive::ClaimInfo { device: dev },
        ));

        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });

        let events = sink.take();
        assert!(matches!(events.as_slice(), [EngineEvent::OpenFailed { err: OpenError::DeviceLost, .. }]));
        assert!(engine.claim.is_some());
        assert!(engine.exclusive_path.is_none());
    }

    /// §6.18 шаг 46 (ADR-14, ТЗ-86): рассогласование плана сессии с
    /// источником декодера (`ExclusiveOpenReason::Feed`, здесь —
    /// `PayloadMismatch`: целый ring плана против `ring_payload()` фейкового
    /// источника, дефолт трейта — `F32`, как и у `open_in_optimal_mode_
    /// dispatches_to_exclusive_path_confirmed`, но без переключения на
    /// float-источник) классифицируется как внутренняя ошибка сборки пути —
    /// `OpenError::Internal`; замок остаётся удержан (§6.19).
    #[test]
    fn open_exclusive_feed_mismatch_maps_to_internal_and_keeps_claim() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new().with_configure(Ok((
            crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                rate: SampleRate::new(44_100).expect("rate"),
                sample: crate::audio::format::OutputSampleFormat::S32Le,
                channels: std::num::NonZeroU8::new(2).expect("channels"),
            }),
            Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
        )));
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(pcm_format())),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev.clone() } };
        engine.claim = Some(crate::audio::backend::exclusive::DeviceClaim::new(
            None,
            crate::audio::reservation::Reservation::UnavailableNoServer,
            crate::audio::backend::exclusive::ClaimInfo { device: dev },
        ));

        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });

        let events = sink.take();
        assert!(matches!(
            events.as_slice(),
            [EngineEvent::OpenFailed { err: OpenError::Internal(EngineFault::SpawnFailed { .. }), .. }]
        ));
        assert!(engine.claim.is_some());
        assert!(engine.exclusive_path.is_none());
    }
}
