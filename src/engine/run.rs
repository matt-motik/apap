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

use crate::audio::backend::exclusive::DeviceClaim;
use crate::audio::backend::catalog::DeviceCatalog;
use crate::audio::backend::HwDeviceId;
use crate::audio::clock::ClockInstant;
use crate::audio::error::{CaptureFailure, EngineFault, FileError, OpenError, Reaction, StreamFault};
use crate::audio::format::{SampleRate, SourceFormat};
use crate::audio::path::plan::{plan, plan_with_device, with_gain};
use crate::audio::path::signal::{DynamicPathState, RtPriorityState, SignalPath};
use crate::audio::path::{Blocked, OpenedFacts, OsStage, PathPlan};
use crate::audio::player::{Player, StreamDesc};
use crate::audio::reservation::LostNotify;
use crate::audio::session_build::SpawnDecoderError;
use crate::audio::verdict::{verdict, BadgeState, GainState};
use crate::engine::deps::EngineDeps;
use crate::engine::exclusive_path::{self, ExclusiveOpenReason, ExclusivePath};
use crate::engine::legacy_path::{legacy_audio, opened_facts_from_desc, shared_caps_from_desc, stream_format_from_desc};
use crate::engine::lock::{lock_step, resolve_device, LockEffect, LockInput, LockState, ReacquirePoint, Resolved};
use crate::engine::messages::{
    EngineCmd, EngineEvent, InternalMsg, LegacyAudio, Notice, SkipReason, TrackRef, TransportState,
};
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

/// Отложенный `Open`, пришедший, пока замок ещё `Acquiring` (AM1.0-B24,
/// ТЗ-20, §6.18 «Смена режима»): запрос параметров `EngineCmd::Open`
/// сохраняется здесь вместо немедленного отказа `OpenFailed
/// (NoDeviceSelected)` и доигрывается в `handle_internal` по приходу
/// `InternalMsg::ClaimDone` для текущей попытки захвата — «`Open` ждёт
/// итога захвата». Более новый `Open` заменяет более старый без события
/// для заменённого (UI отфильтровывает устаревший `req_gen` как и для
/// обычного `OpenFailed`/`Opened`).
struct DeferredOpen {
    req_gen: u64,
    path: Arc<Path>,
    src: SourceFormat,
    start_secs: f64,
    autoplay: bool,
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

/// Обратное преобразование к [`secs_to_frames`] (§6.20 п. 4, ТЗ-126):
/// переоткрытие трека из `ReacquirePoint` принимает позицию в секундах
/// (мост С3, `open_exclusive`), а точка хранит кадры с частотой на момент
/// сохранения (`ReacquirePoint::rate`). Нулевая частота (точки не было или
/// путь не был открыт) даёт безопасный `0.0`, а не деление на ноль.
fn frames_to_secs(frame: u64, rate: u32) -> f64 {
    if rate == 0 {
        return 0.0;
    }
    frame as f64 / f64::from(rate)
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
    /// Последнее запрошенное через `SetVizTap` состояние визуализатора
    /// (§6.18): зеркалит `self.player`, чтобы применить его к только что
    /// открытому Эксклюзивному пути в `open_exclusive` так же, как
    /// `Player::apply_state` делает это для Совместимого пути.
    viz_tap_active: bool,
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
    /// Номер карты текущей попытки/удержанного захвата (§6.19, §6.20,
    /// ТЗ-126): записывается в `start_claim` вместе с запуском захвата,
    /// сравнивается с `InternalMsg::ReservationLost { card }`, чтобы отбросить
    /// сигнал об отключённой карте, захват для которой уже снят или сменился
    /// (та же причина, что у `claim_attempt`, но для номера карты — `ClaimInfo`
    /// его не хранит). Сбрасывается в `None` эффектом `LockEffect::Release`.
    claim_card: Option<u32>,
    /// Точка повторного захвата (§6.20 п. 2, ТЗ-51, ТЗ-126): заполняется
    /// движком (`save_reacquire_point`) до `lock_input(DeviceLost)`, пока
    /// `self.claim`/`self.exclusive_path` ещё не очищены эффектами
    /// `StopPlayback`/`Release` перехода `Held → Failed(DeviceDisconnected)`.
    reacquire: Option<ReacquirePoint>,
    /// Отложенный `Open`, пришедший при захвате в процессе (`Acquiring`,
    /// AM1.0-B24, ТЗ-20, §6.18 «Смена режима»): заполняется в
    /// `open_exclusive`, забирается и доигрывается в `handle_internal` по
    /// `InternalMsg::ClaimDone`. Очищается `stop_playback`/
    /// `LockEffect::Release`, чтобы не доиграть запрос, относящийся к уже
    /// остановленному/отпущенному замку.
    deferred_open: Option<DeferredOpen>,
    /// Счётчики динамики текущего потока (§6.17, §6.20, ТЗ-51, ТЗ-76):
    /// `underruns`/`xruns` переносятся из `SessionShared` Эксклюзивного пути
    /// на каждом опросе; сбрасывается на каждый успешный `Open`.
    dynamics: DynamicPathState,
    /// Последняя отправленная в UI копия `self.dynamics` — для дедупликации
    /// `EngineEvent::DynamicsChanged` (ТЗ-68, §6.18 п.3, И-Р13). `None` до
    /// первой отправки.
    last_sent_dynamics: Option<DynamicPathState>,
    /// Приоритет, который движок запрашивает у `rtkit` для потока вывода
    /// Эксклюзивного пути (§6.8 п. 1, 3, ТЗ-99, ТЗ-76): результат
    /// однократного `deps.rt.init()` при запуске движка. `None` — `rtkit`
    /// RT не даёт вовсе (нет шины/демона или нулевой лимит) — `request_rt_priority`
    /// сразу отдаёт `Denied`, не обращаясь к `deps.rt.request`.
    rt_prio: Option<u32>,
}

impl Engine {
    fn new(deps: EngineDeps, caps: BackendCaps) -> Engine {
        let mut player = Player::new();
        player.set_spawner(Arc::clone(&deps.spawner));
        let player_volume = player.volume();
        let (internal_tx, internal_rx) = mpsc::channel();
        // Однократно при запуске движка (§6.8 п. 1, 3, ТЗ-99): лимиты
        // `rtkit`/`RLIMIT_RTTIME` читаются и ставятся один раз на сессию, а
        // не на каждое открытие Эксклюзивного пути.
        let rt_prio = deps.rt.init();
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
            viz_tap_active: false,
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
            claim_card: None,
            reacquire: None,
            deferred_open: None,
            dynamics: DynamicPathState::default(),
            last_sent_dynamics: None,
            rt_prio,
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
                        self.lock_input(LockInput::CaptureResult(Ok(info.clone())));
                        match self.deferred_open.take() {
                            // `Open`, пришедший во время захвата (AM1.0-B24,
                            // ТЗ-20, §6.18 «Смена режима»), важнее точки
                            // повторного захвата — она явно отбрасывается,
                            // чтобы трек не открылся дважды (сначала из
                            // `reacquire`, затем поверх него из `Open`).
                            Some(d) if matches!(self.lock, LockState::Held { .. }) => {
                                self.reacquire = None;
                                self.open_exclusive(d.req_gen, d.path, d.src, d.start_secs, d.autoplay);
                            }
                            // Переоткрытие трека из точки повторного захвата —
                            // только после того, как замок принял успех и перешёл
                            // в `Held` (§6.20 п. 4, ТЗ-126).
                            _ => self.reopen_after_reacquire(&info.device),
                        }
                    }
                    Err(e) => {
                        // Отказ захвата снимает автоплей сохранённой точки:
                        // дальнейшее переоткрытие (ручной `RetryCapture` или
                        // автоматический повторный захват вернувшегося
                        // устройства) — всегда на паузе (ТЗ-126, §6.20).
                        if let Some(point) = self.reacquire.as_mut() {
                            point.autoplay = false;
                        }
                        self.lock_input(LockInput::CaptureResult(Err(e.clone())));
                        if let Some(d) = self.deferred_open.take() {
                            // `Open`, ждавший итога захвата, получает отказ
                            // явно (AM1.0-B24, ТЗ-20, §6.18 «Смена режима»):
                            // легаси-поток уже остановлен эффектами замка —
                            // `stop_playback` переводит транспорт в `Stopped`,
                            // чтобы UI не остался в «играет».
                            self.deps.events.emit(EngineEvent::OpenFailed { req_gen: d.req_gen, err: OpenError::Capture(e) });
                            self.stop_playback();
                        }
                    }
                }
            }
            // Потеря имени `ReserveDevice1` удержанной карты (§6.20, ADR-08,
            // И-Р20): карта сверяется с `self.claim_card`, чтобы сигнал от
            // уже отпущенной/сменённой попытки захвата (гонка с `Release`)
            // не уронил не ту сессию (ТЗ-123). Точка повторного захвата
            // сохраняется до `lock_input`, как и для `fault` в `poll_session`.
            InternalMsg::ReservationLost { card } => {
                if Some(card) != self.claim_card {
                    return;
                }
                self.save_reacquire_point();
                self.lock_input(LockInput::DeviceLost);
                self.deps.events.emit(EngineEvent::DeviceLost { reaction: Reaction::StopWithError });
            }
            // Ответ устаревшей (закрытой/переоткрытой) сессии отбрасывается
            // молча — как и `ClaimDone` на отменённую попытку (§6.8, ТЗ-99).
            InternalMsg::RtPriority { session, granted } => {
                if session != self.path_generation {
                    return;
                }
                self.dynamics.rt_priority = match granted {
                    Some(prio) => RtPriorityState::Granted(prio),
                    None => RtPriorityState::Denied,
                };
                self.emit_dynamics_if_changed();
            }
        }
    }

    /// Запрашивает RT-приоритет потока вывода Эксклюзивного пути у `rtkit`
    /// (§6.8 п. 1-3, ТЗ-99, ТЗ-76): без TID потока (бэкенд его не публикует)
    /// или без приоритета от `self.rt_prio` (однократный `deps.rt.init()` при
    /// запуске движка не дал RT вовсе) запрос не имеет смысла — сразу
    /// `Denied`. Иначе запрос с приоритетом `self.rt_prio` уходит в служебный
    /// поток через `deps.rt`, а ответ приходит позже через
    /// `InternalMsg::RtPriority { session, .. }` с текущим `path_generation`
    /// как `session`, чтобы ответ устаревшей сессии был отброшен.
    fn request_rt_priority(&mut self, session: u64, tid: Option<u32>) {
        let (Some(tid), Some(prio)) = (tid, self.rt_prio) else {
            self.dynamics.rt_priority = RtPriorityState::Denied;
            return;
        };
        self.dynamics.rt_priority = RtPriorityState::Pending;
        let tx = self.internal_tx.clone();
        self.deps.rt.request(
            tid,
            prio,
            Box::new(move |granted| {
                let _ = tx.send(InternalMsg::RtPriority { session, granted });
            }),
        );
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
    ///
    /// Для `ModeSelected(m)` сам переход `lock_step` считается ещё при
    /// старом `self.active_mode` (нужно для AM1.0-B13 — см. комментарий у
    /// `EngineCmd::SetActiveMode`), но ПЕРЕД выполнением его эффектов поле
    /// `self.active_mode` переставляется на `m` (AM1.0-B15, §6.19, ТЗ-118):
    /// эффект `Acquire(dev)` (`start_claim`) может синхронно, тем же стеком
    /// вызовов, вернуть `LockInput::CaptureResult` (бэкенд недоступен,
    /// устройство пропало из каталога, отказ `spawner.spawn`) — этот
    /// рекурсивный вызов `lock_input` обязан увидеть уже целевой режим,
    /// иначе он попал бы в ветку `lock_step` для Совместимого режима
    /// («любой вход → `NotApplicable`») и терял бы настоящий переход
    /// `Acquiring → Failed`/`Held`.
    fn lock_input(&mut self, input: LockInput) {
        // AM1.0-B15 (§6.19, ТЗ-118): запоминается здесь, а не после
        // `lock_step` ниже — `input` туда уходит по значению (не `Copy`,
        // в отличие от `ModeKind`), повторно сматчить его после вызова
        // нельзя.
        let mode_selected = matches!(input, LockInput::ModeSelected(_));
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
        // AM1.0-B16 (ADR-24, ТЗ-126): устройство, взятое именно из настроек
        // режима (`Resolved::Selected` либо ветка без разрешения выше), а не
        // найденное автовыбором — используется ниже для одноразовой миграции
        // легаси численного id на канонический. Для автовыбора (`auto_selected`
        // уже заполнен) и для отсутствующего устройства остаётся `None`.
        let from_settings = if auto_selected.is_none() { device.clone() } else { None };
        // AM1.0-B16 (§6.19, ADR-24, ТЗ-126): устройство из настроек режима
        // могло быть сохранено в устаревшем численном формате cpal
        // (`hw:CARD=<N>,DEV=<D>`, номер карты вместо стабильного id) —
        // приводится к канонической форме каталога здесь же, чтобы
        // `LockState`/события ниже несли уже канонический id.
        let device = device.map(|d| match self.catalog.canonical_hw(&d) {
            Some(found) => found.id.clone(),
            None => d,
        });
        // AM1.0-B16 (ADR-24, ТЗ-126): если устройство режима пришло из
        // `settings.toml` в устаревшем численном формате, а каталог привёл
        // его здесь к другому (каноническому) id, настройки режима мигрируют
        // на канонический id один раз — миграция не требует эффекта `Acquire`
        // (это переименование того же физического устройства, а не новый
        // захват) и шлёт `DeviceAutoSelected`, чтобы приложение перезаписало
        // `settings.toml` (та же персистентность, что и для автовыбора ниже).
        // После миграции сохранённый id совпадает с каноническим, поэтому
        // повторный вход с тем же устройством уже не даёт расхождения и
        // событие не шлётся снова.
        if let (Some(from), Some(canonical)) = (from_settings.as_ref(), device.as_ref()) {
            if from != canonical {
                let migrated = match device_mode {
                    ModeKind::Optimal => {
                        self.mode_settings.optimal.device = Some(canonical.clone());
                        true
                    }
                    ModeKind::Strict => {
                        self.mode_settings.strict.device = Some(canonical.clone());
                        true
                    }
                    ModeKind::Compatible => false,
                };
                if migrated {
                    self.deps
                        .events
                        .emit(EngineEvent::DeviceAutoSelected { mode: device_mode, device: canonical.clone() });
                }
            }
        }

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

        // AM1.0-B15 (§6.19, ТЗ-118): переставляется на целевой режим ДО
        // выполнения эффектов — см. доккомент функции выше. `EngineCmd::
        // SetActiveMode` ниже присвоит `self.active_mode` то же значение
        // `m` ещё раз (идемпотентно) и вызовет `store_active_mode`/
        // `apply_gain` уже после всех входов замка этого перехода.
        // `device_mode` здесь равно целевому `m` — см. его вычисление выше.
        if mode_selected {
            self.active_mode = device_mode;
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
                    self.claim_card = None;
                    // Отложенный `Open` относился к снимаемому замку
                    // (AM1.0-B24, ТЗ-20, §6.18 «Смена режима») — без
                    // удержания его больше некому доиграть.
                    self.deferred_open = None;
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
        // Отложенный `Open` (AM1.0-B24, ТЗ-20, §6.18 «Смена режима») не
        // переживает остановку воспроизведения — доигрывать его уже не для
        // чего.
        self.deferred_open = None;
        self.refresh_transport();
    }

    /// Строит точку повторного захвата для `device`/`autoplay` из текущего
    /// трека и позиции Эксклюзивного пути (§6.20 п. 2, ТЗ-51, ТЗ-126, ТЗ-137)
    /// — общая часть `save_reacquire_point` (потеря устройства) и
    /// `save_switch_device_reacquire_point` (смена устройства активного
    /// режима). Без открытого трека (`self.current`) сохранять нечего —
    /// `None`; без открытого Эксклюзивного пути позиция/частота — `(0, 0)`
    /// (трек открывается с начала).
    fn build_reacquire_point(&self, device: HwDeviceId, autoplay: bool) -> Option<ReacquirePoint> {
        let (_, path) = self.current.as_ref()?;
        let (frame, rate) = self
            .exclusive_path
            .as_ref()
            .map_or((0, 0), |p| (secs_to_frames(p.position_secs(), p.frame_rate()), p.frame_rate()));
        Some(ReacquirePoint { device, track: TrackRef { id: None, path: Arc::clone(path) }, frame, rate, autoplay })
    }

    /// Сохраняет точку повторного захвата (§6.20 п. 2, ТЗ-51, ТЗ-126) перед
    /// `lock_input(DeviceLost)`: вызывающая сторона обязана вызвать её ДО
    /// `lock_input`, иначе эффекты `StopPlayback`/`Release` перехода `Held →
    /// Failed(DeviceDisconnected)` уже закроют `self.exclusive_path` (источник
    /// позиции) и снимут замок (источник устройства). Устройство читается из
    /// `self.lock` (`LockState::Held`), а не из `self.claim` — пока
    /// Эксклюзивный путь открыт, захват живёт внутри него, и `self.claim`
    /// пуст (И-Р1, §6.19). Без удержанного замка или без открытого трека
    /// сохранять нечего (например, сигнал пришёл не о текущей удержанной
    /// карте) — тихий no-op, `self.reacquire` не трогается. `autoplay: false`
    /// безусловно — автоплей после потери устройства запрещён (ТЗ-126),
    /// независимо от того, играл трек в момент потери.
    fn save_reacquire_point(&mut self) {
        let LockState::Held { info } = &self.lock else { return };
        let Some(point) = self.build_reacquire_point(info.device.clone(), false) else { return };
        self.reacquire = Some(point);
    }

    /// Сохраняет точку повторного захвата перед сменой устройства активного
    /// Эксклюзивного режима (`LockInput::DeviceSelected(new_dev)`, §6.18,
    /// §6.19, ТЗ-137): вызывающая сторона обязана вызвать её ДО
    /// `lock_input`, как и `save_reacquire_point` — иначе эффект
    /// `StopPlayback` уже закроет `self.exclusive_path` (источник позиции и
    /// игра/пауза). В отличие от потери устройства (ТЗ-126, всегда пауза),
    /// смена устройства продолжает воспроизведение, если оно шло (ТЗ-137,
    /// «продолжение с той же позиции») — `autoplay = exclusive_path.
    /// is_playing()`. Устройство точки — НОВОЕ (`new_dev`), на которое идёт
    /// захват, а не старое удержанное: `reopen_after_reacquire` сверяет точку
    /// с устройством из `ClaimDone`, которым будет новое. Без удержанного
    /// замка (переход `DeviceSelected` из других состояний не закрывает
    /// открытый трек) — тихий no-op.
    ///
    /// Если захват нового устройства откажет (`ClaimDone(Err)`), точка не
    /// забирается — `reopen_after_reacquire` вызывается только из успешной
    /// ветки `ClaimDone`. Она остаётся для последующего `RetryCapture`/
    /// `DeviceReappeared` на то же устройство, но ветка отказа `ClaimDone`
    /// сбрасывает её `autoplay` — после отказа переоткрытие только на паузе
    /// (ТЗ-126).
    fn save_switch_device_reacquire_point(&mut self, new_dev: HwDeviceId) {
        let LockState::Held { .. } = &self.lock else { return };
        let autoplay = self.exclusive_path.as_ref().is_some_and(ExclusivePath::is_playing);
        // AM1.0-B16 (ADR-24, ТЗ-126): `new_dev` идёт прямо из настроек
        // режима и может быть в устаревшем численном формате — приводится к
        // каноническому id каталога, если он найден, чтобы точка совпадала
        // с каноническим устройством из `ClaimDone` (см. `start_claim`).
        let canonical = self.catalog.canonical_hw(&new_dev).map_or(new_dev, |found| found.id.clone());
        self.reacquire = self.build_reacquire_point(canonical, autoplay);
    }

    /// Автоматический повторный захват вернувшегося устройства (§6.20 п. 4,
    /// ТЗ-126): вызывается после изменения каталога Exclusive-устройств
    /// (`refresh_devices`). Замок должен всё ещё быть `Failed
    /// (DeviceDisconnected)` — иначе пользователь уже открыл замок или
    /// сменил режим/устройство, и `ForgetReacquire` уже забыл точку
    /// (ТЗ-123); устройство точки должно быть среди перечисленных —
    /// `lock_step` сам не сверяет `d` с точкой (см. доккомент
    /// `DeviceReappeared` в `lock.rs`), эту сверку обязан сделать движок
    /// до вызова `lock_input`. При совпадении — `DeviceReappeared(d)` даёт
    /// эффект `Acquire(d)` (захват по §6.6); переоткрытие трека на
    /// сохранённой позиции — в `reopen_after_reacquire`, после успешного
    /// `ClaimDone`.
    fn try_reacquire_reappeared_device(&mut self) {
        let LockState::Failed(CaptureFailure::DeviceDisconnected) = &self.lock else { return };
        let Some(point) = self.reacquire.as_ref() else { return };
        // `canonical_hw` (AM1.0-B16, ADR-24, ТЗ-126) допускает устаревший
        // численный id точки повторного захвата наравне с каноническим;
        // сохранённая точка приводится к найденному каноническому id, чтобы
        // `reopen_after_reacquire` ниже сравнивала её с устройством
        // `ClaimDone` (тоже каноническим после `start_claim`) без ложного
        // расхождения форм одного и того же устройства.
        let Some(found) = self.catalog.canonical_hw(&point.device) else {
            return;
        };
        let dev = found.id.clone();
        if let Some(stored) = self.reacquire.as_mut() {
            stored.device = dev.clone();
        }
        self.lock_input(LockInput::DeviceReappeared(dev));
    }

    /// Переоткрытие трека после успешного повторного захвата (§6.20 п. 4,
    /// ТЗ-126): вызывается из `handle_internal` сразу после того, как
    /// `ClaimDone` перевело замок в `Held`. Точка повторного захвата
    /// забирается (`Option::take`) и, если её устройство не совпадает с
    /// только что захваченным (например, захват успел пойти на другое
    /// устройство — ручной `RetryCapture`/`SetActiveMode` поверх старого
    /// `Failed` без явного `ForgetReacquire`, см. таблицу §6.19), точка
    /// возвращается нетронутой и ничего не открывается — иначе трек
    /// переоткрывается на сохранённой позиции (`ReacquirePoint::frame`/
    /// `rate` → секунды, `frames_to_secs`) на Эксклюзивном пути с
    /// `autoplay: point.autoplay` — всегда `false` после потери устройства
    /// (ТЗ-126, запуск воспроизведения — явное решение пользователя) и
    /// игра/пауза на момент смены устройства активного режима (ТЗ-137, см.
    /// `save_switch_device_reacquire_point`).
    fn reopen_after_reacquire(&mut self, claimed: &HwDeviceId) {
        let Some(point) = self.reacquire.take() else { return };
        if point.device != *claimed {
            self.reacquire = Some(point);
            return;
        }
        let req_gen = self.current.as_ref().map_or(0, |(gen, _)| *gen);
        let path = Arc::clone(&point.track.path);
        let start_secs = frames_to_secs(point.frame, point.rate);
        let src = match self.deps.sources.probe(&path) {
            Ok(src) => src,
            Err(e) => {
                self.deps
                    .events
                    .emit(EngineEvent::Skipped { req_gen, path, reason: SkipReason::File(e) });
                return;
            }
        };
        let autoplay = point.autoplay;
        self.open_exclusive(req_gen, path, src, start_secs, autoplay);
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
        // `ReservationLost` не выдумывается (ТЗ-126, §6.19). `canonical_hw`
        // (AM1.0-B16, ADR-24) допускает устаревший численный id сохранённого
        // устройства наравне с каноническим — захват идёт по каноническому
        // `found.id`, чтобы `ClaimInfo::device` и дальнейшие сравнения с
        // каталогом совпадали.
        let Some(found) = self.catalog.canonical_hw(&dev) else {
            self.lock_input(LockInput::CaptureResult(Err(CaptureFailure::DeviceDisconnected)));
            return;
        };
        let canonical_dev = found.id.clone();
        let device_name = found.name.clone();
        let card = found.card;
        self.claim_card = Some(card);

        let lost_tx = self.internal_tx.clone();
        let on_lost: LostNotify = Box::new(move || {
            let _ = lost_tx.send(InternalMsg::ReservationLost { card });
        });
        let job = backend.claim_job(canonical_dev, device_name, on_lost);

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
    /// быть удержан `EngineCmd::Lock`/`lock_step` до `Open` (§6.19).
    /// Исключение — `Acquiring` (AM1.0-B24, ТЗ-20, §6.18 «Смена режима»):
    /// захват ещё в процессе (например, только что запущен `SetActiveMode`),
    /// и `Open` не отказывает немедленно, а ждёт итога захвата — запрос
    /// сохраняется в `self.deferred_open` и доигрывается из
    /// `InternalMsg::ClaimDone` в `handle_internal`. Смена трека при уже
    /// открытом Эксклюзивном пути (новый `Open` без промежуточного `Stop`)
    /// закрывает старый путь первой же строкой — иначе `self.claim` пуст
    /// (его держит старый `ExclusivePath`) и новое открытие отказало бы
    /// `NoDeviceSelected`, хотя замок удержан. `autoplay` — пуск
    /// воспроизведения сразу после открытия (ТЗ-49): поток вывода
    /// `ExclusiveBackend::start` поднимается всегда, пауза регулируется
    /// `SessionShared::playing`, а не остановкой потока.
    fn open_exclusive(&mut self, req_gen: u64, path: Arc<Path>, src: SourceFormat, start_secs: f64, autoplay: bool) {
        if matches!(self.lock, LockState::Acquiring { .. }) {
            // Более новый `Open` заменяет отложенный без события для
            // заменённого — UI отфильтровывает устаревший `req_gen`, как и
            // для обычного `OpenFailed`/`Opened` (AM1.0-B24, ТЗ-20).
            self.deferred_open = Some(DeferredOpen { req_gen, path, src, start_secs, autoplay });
            return;
        }
        let LockState::Held { .. } = &self.lock else {
            let reason = match &self.lock {
                LockState::Failed(reason) => reason.clone(),
                _ => CaptureFailure::NoDeviceSelected,
            };
            self.deps.events.emit(EngineEvent::OpenFailed { req_gen, err: OpenError::Capture(reason) });
            return;
        };
        // §6.7, §6.18 шаг 5 (ТЗ-43, И-Р12): конфигурация PCM предыдущего
        // пути сохраняется до его `close()` — смена трека на той же
        // конфигурации не повторяет `configure` в `exclusive_path::open`.
        let prev_configured = self.exclusive_path.as_ref().map(|p| p.configured());
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
        // Устройство для шага 1 (маршрут DoP, §6.9) — фактически захваченное
        // устройство режима (ADR-24), а не только настроенное `optimal.device`/
        // `strict.device`: при автовыборе (`Resolved::AutoSelected`) захват
        // мог быть открыт на другом устройстве, чем записано в настройках.
        let dev_id = claim.info().device.clone();
        let dev_opts = self.mode_settings.device_opts(&dev_id);
        let base = match plan_with_device(&src, &caps, policy, Some((&dev_id, dev_opts))) {
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

        // Эксклюзивный путь делит общий `VizTap`-хэндл с `Player` (§6.18):
        // ровно один `Producer` на всё приложение, активен у той сессии,
        // что сейчас открыта — Совместимый и Эксклюзивный пути взаимно
        // исключительны, поэтому общий `Arc` не требует арбитража.
        let tap = self.player.viz_tap_handle();
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
            prev_configured,
        ) {
            Ok((opened, signal)) => {
                self.path_generation = generation;
                let badge = verdict(&signal, gain, None);
                let signal = Arc::new(signal);
                // §6.18 шаг 50 (ТЗ-137, ТЗ-140, ОВС-18): громкость/mute
                // применяются до пуска воспроизведения — без этого автоплей
                // стартовал бы с единичным усилением (щелчок громкости).
                opened.set_gain(effective_gain(&self.mode_settings, self.active_mode, self.last_volume, false), self.last_muted);
                // Публикует сохранённое состояние визуализатора в новой
                // сессии (§6.18) — аналог `Player::apply_state` для
                // Эксклюзивного пути: без этого спектр молчал бы до
                // следующего `SetVizTap` от UI.
                opened.set_viz_tap_active(self.viz_tap_active);
                self.exclusive_signal =
                    Some(ActivePath { plan: base, facts: opened.facts().clone(), path: Arc::clone(&signal), badge: badge.clone() });
                let rt_tid = opened.rt_tid();
                if autoplay {
                    opened.play();
                }
                self.exclusive_path = Some(opened);
                self.current = Some((req_gen, Arc::clone(&path)));
                self.stopped = false;
                self.last_pos = None;
                self.session_failed = false;
                self.dynamics = DynamicPathState::default();
                // Свежий поток начинает с «нулевой» динамики (ТЗ-68): без
                // сброса последней отправленной копии первое же отличие
                // (например, RT-приоритет ниже) не дошло бы до UI.
                self.last_sent_dynamics = Some(DynamicPathState::default());
                self.request_rt_priority(generation, rt_tid);
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
                // источника сохраняется (шаг 28) — нужен для `plan()`
                // Совместимого режима (`build_compatible_path`).
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
                        self.last_sent_dynamics = Some(DynamicPathState::default());
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
            EngineCmd::SetVizTap(on) => {
                self.viz_tap_active = on;
                self.player.set_viz_tap_active(on);
                if let Some(path) = &self.exclusive_path {
                    path.set_viz_tap_active(on);
                }
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
                    // при `self.stopped` сегодня). Старый путь обслуживает
                    // только Совместимый режим (§8 С6): для Оптимального и
                    // Строгого настройки уже скопированы выше и применятся
                    // собственным выводом на следующем `Open`.
                    ModeApplyAction::Store
                        if self.active_mode == ModeKind::Compatible
                            && matches!(strongest, Some(ApplyKind::ReopenAtPosition) | Some(ApplyKind::SwitchDevice)) =>
                    {
                        let (legacy, device) = legacy_audio(&self.mode_settings);
                        self.apply_legacy_fields(&legacy, device);
                    }
                    // Пустой дифф или изменение с `ApplyKind::Memory` —
                    // настройки уже скопированы выше, больше ничего не нужно.
                    ModeApplyAction::Store => {}
                    // Эксклюзивный путь активного режима (Оптимальный/Строгий,
                    // §6.7, §6.18, ТЗ-131): переоткрытие на текущей позиции
                    // через `open_exclusive`, без снятия резервирования.
                    ModeApplyAction::ReopenAtPosition if self.active_mode != ModeKind::Compatible => {
                        self.reopen_exclusive_at_position();
                    }
                    // Смена устройства активного Эксклюзивного режима (§6.18,
                    // §6.19, ТЗ-137): «освобождение старого, захват нового,
                    // продолжение с той же позиции» — один вход
                    // `LockInput::DeviceSelected` замку, который сам
                    // упорядочивает `StopPlayback, Release, Acquire(dev)` из
                    // `Held` (таблица §6.19), поглощая остальные изменения
                    // этого `SetModeSettings` (И-Р26). `save_switch_device_
                    // reacquire_point` сохраняет позицию и игра/пауза ДО
                    // `lock_input` (`StopPlayback` закрывает путь) — трек
                    // переоткрывается на новом устройстве через `reopen_after_
                    // reacquire` после успешного `ClaimDone` (§6.20 п. 4,
                    // ТЗ-137). Устройство не настроено явно (возврат к
                    // автовыбору) здесь не переоткрывается — автовыбор нового
                    // устройства для этого входа не входит в область этого
                    // шага.
                    ModeApplyAction::SwitchDevice if self.active_mode != ModeKind::Compatible => {
                        let dev = match self.active_mode {
                            ModeKind::Optimal => self.mode_settings.optimal.device.clone(),
                            ModeKind::Strict => self.mode_settings.strict.device.clone(),
                            ModeKind::Compatible => None,
                        };
                        if let Some(dev) = dev {
                            self.save_switch_device_reacquire_point(dev.clone());
                            self.lock_input(LockInput::DeviceSelected(dev));
                        }
                    }
                    ModeApplyAction::ReopenAtPosition | ModeApplyAction::SwitchDevice => {
                        self.apply_active_mode();
                    }
                }
            }
            EngineCmd::SetActiveMode(mode) => {
                if mode != self.active_mode {
                    // Замок переключается ДО смены `self.active_mode`
                    // (AM1.0-B13, ТЗ-21, §6.18, §6.19): `lock_input`
                    // использует текущий `self.active_mode` как «старый»
                    // режим для `lock_step`, а устройство целевого режима
                    // резолвит сам по `LockInput::ModeSelected(mode)`
                    // (ADR-24). Из Эксклюзивного (Оптимальный/Строгий) в
                    // Совместимый при удержанном/ожидающем/отказавшем замке
                    // это даёт `NotApplicable` с эффектами `StopPlayback,
                    // Release, ForgetReacquire` — эксклюзивный путь
                    // закрывается и карта освобождается ДО того, как ниже
                    // `store_active_mode` снимет легаси-поток и впустит
                    // Совместимый путь на следующем `Open`; без этого
                    // порядка карта оставалась занятой эксклюзивно (баг
                    // AM1.0-B13). Для Совместимый → Эксклюзивный сам переход
                    // `lock_step` считается ещё при старом `self.active_mode
                    // == Compatible` (это и даёт `acquire_or_fail`, а не
                    // ветку «любой вход → `NotApplicable`» — `lock_step`
                    // смотрит на свой параметр `mode`, а не на итоговое
                    // значение поля), но `lock_input` переставляет
                    // `self.active_mode` на целевой режим ДО выполнения
                    // эффектов этого перехода, то есть ДО `Acquire(dev)` /
                    // `start_claim` (AM1.0-B15, §6.19, ТЗ-118 — см. доккомент
                    // `lock_input`). Без этого переноса синхронный возврат
                    // `start_claim` тем же стеком вызовов (нет эксклюзивного
                    // бэкенда, устройство исчезло из каталога, отказ
                    // `spawner.spawn`) рекурсивно дёргал бы `lock_input
                    // (CaptureResult(..))`, который увидел бы ещё
                    // `self.active_mode == Compatible` и свалил бы настоящий
                    // переход `Acquiring → Failed`/`Held` в `NotApplicable`
                    // (баг AM1.0-B15). Присвоение `self.active_mode = mode`
                    // ниже, после обоих вызовов `lock_input`, уже
                    // идемпотентно — поле выставлено раньше внутри
                    // `lock_input`. Для
                    // Эксклюзивный ↔ Эксклюзивный (Оптимальный ↔ Строгий) из
                    // `Held`/`Acquiring` таблица §6.19 не даёт эффектов на
                    // `ModeSelected` — устройство само по себе остаётся тем
                    // же. Если же настроенные устройства старого и нового
                    // режима различаются (ТЗ-137, §6.18), ниже отдельным
                    // входом `LockInput::DeviceSelected(dev)` замок
                    // освобождает старое устройство и захватывает новое —
                    // `lock_step` сам упорядочивает `StopPlayback, Release,
                    // Acquire(dev)` из `Held` (§6.19); `save_switch_device_
                    // reacquire_point` перед этим входом сохраняет позицию и
                    // игра/пауза, трек переоткрывается на новом устройстве
                    // после `ClaimDone` (§6.20 п. 4, ТЗ-137).
                    let old_mode = self.active_mode;
                    self.lock_input(LockInput::ModeSelected(mode));
                    if old_mode != ModeKind::Compatible && mode != ModeKind::Compatible {
                        let old_dev = match old_mode {
                            ModeKind::Optimal => self.mode_settings.optimal.device.clone(),
                            ModeKind::Strict => self.mode_settings.strict.device.clone(),
                            ModeKind::Compatible => None,
                        };
                        let new_dev = match mode {
                            ModeKind::Optimal => self.mode_settings.optimal.device.clone(),
                            ModeKind::Strict => self.mode_settings.strict.device.clone(),
                            ModeKind::Compatible => None,
                        };
                        if let (Some(o), Some(n)) = (old_dev, new_dev) {
                            if o != n {
                                self.save_switch_device_reacquire_point(n.clone());
                                self.lock_input(LockInput::DeviceSelected(n));
                            }
                        }
                    }
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

    /// Применяет `self.mode_settings` к старому пути Совместимого режима для
    /// `EngineCmd::SetModeSettings` (ОВС-14, ОВС-17, §6.18, ADR-22): при
    /// `Stopped` — только копия полей `Player` (нет сессии, переоткрывать
    /// нечего — применится на следующем `Open`); иначе одно переоткрытие на
    /// текущей позиции с сохранением играет/на паузе через
    /// `reopen_with_legacy`. Вызывается только для Совместимого режима (§8
    /// С6): ветки Оптимального/Строгого в `SetModeSettings` уходят в
    /// Эксклюзивный путь (`reopen_exclusive_at_position`).
    /// `EngineCmd::SetActiveMode` переоткрытие здесь не запускает — см.
    /// `store_active_mode`.
    fn apply_active_mode(&mut self) {
        let (legacy, device) = legacy_audio(&self.mode_settings);
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
    /// переоткрытия. Применится на следующем `Open`. Поля старого пути
    /// пишутся только при переходе В Совместимый режим (§8 С6): для
    /// Оптимального и Строгого вывод ведёт `AlsaExclusiveBackend`, а
    /// `release_engine` всё равно снимает легаси-поток перед переключением.
    fn store_active_mode(&mut self) {
        self.player.release_engine();
        if self.active_mode == ModeKind::Compatible {
            let (legacy, device) = legacy_audio(&self.mode_settings);
            self.apply_legacy_fields(&legacy, device);
        }
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

    /// Одно переоткрытие Эксклюзивного пути на текущей позиции для
    /// `SetModeSettings` (`ApplyKind::ReopenAtPosition` активного
    /// Оптимального/Строгого режима, §6.7, §6.18, ТЗ-131): позиция и
    /// играет/на паузе сохраняются, как в `reopen_with_legacy`. Повторный
    /// вызов `open_exclusive` сам переносит `configured()` старого пути как
    /// `prev_configured` — `configure` в `exclusive_path::open` выполняется
    /// только при изменении целевого формата или «Буфера устройства» (§6.7),
    /// а резервирование устройства (`self.claim`) не освобождается ни в
    /// одном случае. Без открытой сессии (не должно случаться при
    /// `!self.stopped`, но без паники) — ничего не делает, настройки уже
    /// скопированы в `self.mode_settings` выше.
    fn reopen_exclusive_at_position(&mut self) {
        let Some(exclusive) = self.exclusive_path.as_ref() else { return };
        let Some((req_gen, path)) = self.current.clone() else { return };
        let was_playing = exclusive.is_playing();
        let pos = exclusive.position_secs();
        let src = match self.deps.sources.probe(&path) {
            Ok(src) => src,
            Err(e) => {
                self.deps
                    .events
                    .emit(EngineEvent::Skipped { req_gen, path, reason: SkipReason::File(e) });
                return;
            }
        };
        self.open_exclusive(req_gen, path, src, pos, was_playing);
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
        // Каталог Exclusive-устройств (`hw`) перечисляется тут же, рядом с
        // Shared (§6.2 п. 3, ТЗ-104, ТЗ-105, ADR-16): изменение списка — не
        // только повод переотдать `Devices` в UI, но и триггер автоматического
        // повторного захвата вернувшегося устройства (§6.20 п. 4, ТЗ-126).
        let hw_changed = match self.deps.exclusive.as_deref_mut() {
            Some(backend) => match self.catalog.refresh_hw(backend) {
                Ok(changed) => changed,
                Err(e) => {
                    self.deps.events.emit(EngineEvent::Notice(Notice::DevicesUnavailable(e)));
                    false
                }
            },
            None => false,
        };
        match self.catalog.refresh(self.deps.shared.as_mut()) {
            Ok(changed) => {
                if changed || hw_changed || force_emit {
                    self.deps.events.emit(EngineEvent::Devices(Arc::new(self.catalog.clone())));
                }
            }
            Err(e) => {
                self.deps.events.emit(EngineEvent::Notice(Notice::DevicesUnavailable(e)));
            }
        }
        if hw_changed {
            self.try_reacquire_reappeared_device();
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

    /// Шлёт `EngineEvent::DynamicsChanged`, только если `self.dynamics`
    /// отличается от последней отправленной копии (ТЗ-68, §6.18 п.3,
    /// И-Р13). `session` — `req_gen` текущего открытого трека (`self.current`),
    /// как у `Ended`/`OpenFailed`.
    fn emit_dynamics_if_changed(&mut self) {
        // Без открытого трека динамика не относится ни к какой сессии
        // (§3.5) — событие не шлётся, как `Position`/`Transport` не несут
        // позиции/транспорта вне потока.
        let Some((session, _)) = &self.current else {
            return;
        };
        let session = *session;
        if self.last_sent_dynamics.as_ref() == Some(&self.dynamics) {
            return;
        }
        self.last_sent_dynamics = Some(self.dynamics.clone());
        self.deps
            .events
            .emit(EngineEvent::DynamicsChanged { session, dynamics: self.dynamics.clone() });
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
        // вышел из цикла) — точка повторного захвата сохраняется
        // (`save_reacquire_point`) до `lock_input(DeviceLost)`, который
        // переиспользует переход `Held → Failed(DeviceDisconnected)`
        // (`StopPlayback`, `Release`) вместо дублирования остановки здесь.
        let exclusive_poll =
            self.exclusive_path.as_ref().map(|p| (p.fault(), p.ended(), p.underruns(), p.xruns()));
        if let Some((fault, ended, underruns, xruns)) = exclusive_poll {
            self.dynamics.underruns = underruns;
            self.dynamics.xruns = xruns;
            if fault != StreamFault::None {
                self.save_reacquire_point();
                self.lock_input(LockInput::DeviceLost);
                self.deps.events.emit(EngineEvent::DeviceLost { reaction: Reaction::StopWithError });
            } else if ended {
                if !self.session_failed {
                    if let Some((req_gen, _)) = &self.current {
                        self.deps.events.emit(EngineEvent::Ended { session: *req_gen });
                    }
                }
                // Путь закрывается, а не дропается: захват возвращается в
                // `self.claim`, замок остаётся `Held` и следующий `Open`
                // (переход на следующий трек) находит устройство (§6.19,
                // AM1.0-B17) — как в `stop_playback`.
                if let Some(p) = self.exclusive_path.take() {
                    self.claim = Some(p.close());
                }
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

        self.emit_dynamics_if_changed();
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
    use crate::audio::rt::RtPriorityRequester;
    use crate::audio::testing::{FakeExclusiveBackend, FakeRtRequester, FakeServerProbe};
    use crate::engine::spawner::{FailingSpawner, StdSpawner};
    use crate::platform::fs::{FsPersistStore, MemStore};
    use crate::settings::params::{ModeSettingsUpdate, ParamId};
    use smallvec::SmallVec;
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
            rt: Arc::new(FakeRtRequester::new(None)),
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

    /// AM1.0-B16 (§6.19, ADR-24, ТЗ-126): в `settings.toml` устройство
    /// Строгого режима сохранено в устаревшем численном формате cpal
    /// (`hw:CARD=0,DEV=0`), а каталог перечисляет его канонический id
    /// (`hw:CARD=DX,DEV=0`, card 0, device 0) — захват всё равно проходит
    /// (`Held`), а не падает в `Failed(DeviceDisconnected)`.
    #[test]
    fn lock_input_startup_strict_matches_legacy_numeric_device_id() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let canonical: HwDeviceId = "hw:CARD=DX,DEV=0".parse().expect("hw id");
        let configured: HwDeviceId = "hw:CARD=0,DEV=0".parse().expect("hw id");
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo {
            id: canonical.clone(),
            card: 0,
            device: 0,
            name: "DX".to_string(),
        }];
        engine.active_mode = ModeKind::Strict;
        engine.mode_settings.strict.device = Some(configured);
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);
        let msg = engine.internal_rx.recv().expect("claim done");
        engine.handle_internal(msg);

        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, canonical),
            other => panic!("expected Held, got {other:?}"),
        }
    }

    /// AM1.0-B16 (ADR-24, ТЗ-126): устройство Строгого режима в настройках
    /// сохранено в устаревшем численном формате — `lock_input` мигрирует
    /// `mode_settings.strict.device` на канонический id каталога и шлёт
    /// `DeviceAutoSelected` ровно один раз. Повторный вход (устройство в
    /// настройках уже канонический) не даёт расхождения — события нет.
    #[test]
    fn lock_input_startup_strict_migrates_legacy_device_id_once() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let canonical: HwDeviceId = "hw:CARD=DX,DEV=0".parse().expect("hw id");
        let legacy: HwDeviceId = "hw:CARD=0,DEV=0".parse().expect("hw id");
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo {
            id: canonical.clone(),
            card: 0,
            device: 0,
            name: "DX".to_string(),
        }];
        engine.active_mode = ModeKind::Strict;
        engine.mode_settings.strict.device = Some(legacy);
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);

        assert_eq!(engine.mode_settings.strict.device, Some(canonical.clone()));
        let events = sink.take();
        assert!(events.iter().any(
            |e| matches!(e, EngineEvent::DeviceAutoSelected { mode: ModeKind::Strict, device } if *device == canonical)
        ));

        // Настройки режима уже канонические — повторный вход не находит
        // расхождения и события не шлёт (AM1.0-B16, ТЗ-126).
        engine.lock_input(LockInput::Startup);
        let events = sink.take();
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::DeviceAutoSelected { .. })));
    }

    /// AM1.0-B16 (ADR-24, ТЗ-126): устройство Строгого режима в настройках
    /// уже хранится в каноническом виде — `canonical_hw` возвращает тот же
    /// id, расхождения нет, `DeviceAutoSelected` не шлётся.
    #[test]
    fn lock_input_startup_strict_canonical_device_id_skips_migration_event() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let canonical: HwDeviceId = "hw:CARD=DX,DEV=0".parse().expect("hw id");
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo {
            id: canonical.clone(),
            card: 0,
            device: 0,
            name: "DX".to_string(),
        }];
        engine.active_mode = ModeKind::Strict;
        engine.mode_settings.strict.device = Some(canonical.clone());
        engine.lock = LockState::Open;

        engine.lock_input(LockInput::Startup);

        assert_eq!(engine.mode_settings.strict.device, Some(canonical));
        let events = sink.take();
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::DeviceAutoSelected { .. })));
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
                FakeExclusiveBackend::new().with_acquire(Err(CaptureFailure::BusyOutsideProtocol { owner: None })),
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
            rt: Arc::new(FakeRtRequester::new(None)),
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
            LockState::Failed(CaptureFailure::BusyOutsideProtocol { .. }) => {}
            other => panic!("expected Failed(DeviceDisconnected), got {other:?}"),
        }
        assert!(engine.claim.is_none());
        let events = sink.take();
        assert!(matches!(
            events.as_slice(),
            [EngineEvent::Lock(LockState::Failed(CaptureFailure::BusyOutsideProtocol { .. }))]
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
            rt: Arc::new(FakeRtRequester::new(None)),
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

    /// AM1.0-B24 (ТЗ-20, §6.18 «Смена режима»): `Open`, пришедший пока замок
    /// `Acquiring`, не отказывает немедленно `OpenFailed(NoDeviceSelected)` —
    /// запрос откладывается в `self.deferred_open`. Пришедший затем
    /// `ClaimDone` с отказом захвата для текущей попытки доигрывает его
    /// явным `OpenFailed(Capture(_))` с тем же `req_gen`.
    #[test]
    fn open_while_acquiring_is_deferred_and_fails_with_claim_error() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Strict;
        engine.lock = LockState::Acquiring { started: engine.deps.clock.now() };

        engine.handle(EngineCmd::Open { req_gen: 7, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });

        // `Open` не отказал немедленно — ждёт итога захвата (AM1.0-B24).
        assert!(sink.take().is_empty());
        assert!(engine.deferred_open.is_some());

        engine.handle_internal(InternalMsg::ClaimDone {
            attempt: engine.claim_attempt,
            result: Err(CaptureFailure::BusyOutsideProtocol { owner: None }),
        });

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(
            e,
            EngineEvent::OpenFailed { req_gen: 7, err: OpenError::Capture(CaptureFailure::BusyOutsideProtocol { .. }) }
        )));
        assert!(engine.deferred_open.is_none());
    }

    /// AM1.0-B24 (ТЗ-20, §6.18 «Смена режима»): `EngineCmd::Stop`, пришедший
    /// пока отложенный `Open` ждёт итога захвата, забывает его —
    /// `stop_playback` очищает `self.deferred_open`. Пришедший позже
    /// `ClaimDone` с отказом захвата уже не доигрывает `OpenFailed` для
    /// этого `req_gen`.
    #[test]
    fn open_while_acquiring_then_stop_drops_deferred_open_before_claim_done() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Strict;
        engine.lock = LockState::Acquiring { started: engine.deps.clock.now() };

        engine.handle(EngineCmd::Open { req_gen: 3, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });
        assert!(engine.deferred_open.is_some());

        engine.handle(EngineCmd::Stop);
        assert!(engine.deferred_open.is_none());
        sink.take();

        engine.handle_internal(InternalMsg::ClaimDone {
            attempt: engine.claim_attempt,
            result: Err(CaptureFailure::BusyOutsideProtocol { owner: None }),
        });

        let events = sink.take();
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::OpenFailed { req_gen: 3, .. })));
    }

    /// AM1.0-B24 (ТЗ-20, §6.18 «Смена режима»): `Open`, отложенный на
    /// `Acquiring`, доигрывается успешным `ClaimDone(Ok(claim))` — замок
    /// переходит в `Held`, отложенный запрос заменяет переоткрытие точки
    /// повторного захвата и открывается тем же `req_gen` (`Opened`).
    #[test]
    fn open_while_acquiring_then_claim_done_ok_opens_deferred_request() {
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
            rt: Arc::new(FakeRtRequester::new(None)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Acquiring { started: engine.deps.clock.now() };

        engine.handle(EngineCmd::Open { req_gen: 9, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });
        assert!(engine.deferred_open.is_some());
        sink.take();

        engine.handle_internal(InternalMsg::ClaimDone {
            attempt: engine.claim_attempt,
            result: Ok(DeviceClaim::new(
                None,
                crate::audio::reservation::Reservation::UnavailableNoServer,
                crate::audio::backend::exclusive::ClaimInfo { device: dev },
            )),
        });

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Opened { req_gen: 9, .. })));
        assert!(engine.deferred_open.is_none());
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
            rt: Arc::new(FakeRtRequester::new(None)),
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
            rt: Arc::new(FakeRtRequester::new(None)),
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

    /// Как [`engine_with_exclusive_path_open`], но хэндл журнала фейкового
    /// бэкенда (`FakeExclusiveBackend::calls_handle`) забирается до того,
    /// как бэкенд уходит в `Box<dyn ExclusiveBackend>`, — нужен тестам
    /// AM1.0-B13 (§6.18, §6.19, ТЗ-21), чтобы видеть `ExclusiveCall::Stop`
    /// при закрытии пути эффектом `LockEffect::StopPlayback`.
    fn engine_with_exclusive_path_open_tracked(
        sink: VecSink,
        autoplay: bool,
    ) -> (Engine, Arc<std::sync::Mutex<Vec<crate::audio::testing::ExclusiveCall>>>) {
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new().with_configure(Ok((
            crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                rate: SampleRate::new(44_100).expect("rate"),
                sample: crate::audio::format::OutputSampleFormat::S32Le,
                channels: std::num::NonZeroU8::new(2).expect("channels"),
            }),
            Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
        )));
        let calls = backend.calls_handle();
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
            rt: Arc::new(FakeRtRequester::new(None)),
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
        (engine, calls)
    }

    /// AM1.0-B13 (§6.18, §6.19, ТЗ-21): Оптимальный режим с удержанным
    /// замком и открытым Эксклюзивным путём → `EngineCmd::SetActiveMode
    /// (Compatible)` должен сначала освободить карту (`lock_input` до
    /// смены `self.active_mode`), а не оставить её занятой эксклюзивно, как
    /// было до фикса. Эксклюзивный путь закрывается (`ExclusiveCall::Stop`
    /// в журнале бэкенда), замок уходит в `NotApplicable`, `self.claim`
    /// пуст. Затем `Open` того же трека уже в Совместимом режиме не должен
    /// трогать Эксклюзивный бэкенд — освобождение произошло строго раньше,
    /// чем открылся общий путь.
    #[test]
    fn optimal_held_to_compatible_releases_before_shared() {
        let sink = VecSink::new();
        let (mut engine, calls) = engine_with_exclusive_path_open_tracked(sink.clone(), true);
        sink.take();

        engine.handle(EngineCmd::SetActiveMode(ModeKind::Compatible));

        assert!(engine.exclusive_path.is_none());
        assert!(engine.claim.is_none());
        assert!(matches!(engine.lock, LockState::NotApplicable));
        assert_eq!(engine.active_mode, ModeKind::Compatible);
        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Lock(LockState::NotApplicable))));
        {
            let journal = calls.lock().expect("calls lock");
            assert_eq!(journal.last(), Some(&crate::audio::testing::ExclusiveCall::Stop));
        }

        // Совместимый `Open` того же трека не обращается к Эксклюзивному
        // бэкенду (`Decoder::open` падает первым на несуществующем файле,
        // ещё до вывода) — журнал не растёт после освобождения.
        let journal_len_before = calls.lock().expect("calls lock").len();
        engine.handle(EngineCmd::Open { req_gen: 2, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });
        let journal_after = calls.lock().expect("calls lock");
        assert_eq!(journal_after.len(), journal_len_before);
        assert!(!journal_after[journal_len_before..].iter().any(|c| matches!(
            c,
            crate::audio::testing::ExclusiveCall::Acquire(_)
                | crate::audio::testing::ExclusiveCall::Configure
                | crate::audio::testing::ExclusiveCall::Start
        )));
    }

    /// AM1.0-B13 (§6.18, §6.19, ТЗ-21): Совместимый → Оптимальный с
    /// выбранным устройством режима запускает захват (`Acquiring`,
    /// `Acquire(dev)`) ещё до того, как `self.active_mode` указывает на
    /// новый режим — зеркало `lock_input_startup_acquires_and_holds_device`
    /// (ТЗ-118, ТЗ-119), но через `SetActiveMode` вместо `Startup`.
    #[test]
    fn compatible_to_optimal_starts_capture() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() }];
        engine.active_mode = ModeKind::Compatible;
        engine.lock = LockState::NotApplicable;
        engine.mode_settings.optimal.device = Some(dev.clone());

        engine.handle(EngineCmd::SetActiveMode(ModeKind::Optimal));

        assert!(matches!(engine.lock, LockState::Acquiring { .. }));
        assert_eq!(engine.active_mode, ModeKind::Optimal);
        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Lock(LockState::Acquiring { .. }))));

        let msg = engine.internal_rx.recv().expect("claim done");
        engine.handle_internal(msg);
        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, dev),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(engine.claim.is_some());
    }

    /// AM1.0-B15 (§6.19, ТЗ-118): отказ `spawner.spawn` возвращается
    /// `start_claim` СИНХРОННО, тем же стеком вызовов, что и
    /// `EngineCmd::SetActiveMode` — `start_claim` рекурсивно зовёт
    /// `lock_input(CaptureResult(Err(..)))` прежде, чем вызов `handle`
    /// успевает вернуться. До фикса этот рекурсивный вход застревал на
    /// старом `self.active_mode == Compatible` (поле переставлялось только
    /// после `lock_input` в обработчике `SetActiveMode`) и попадал в ветку
    /// `lock_step` «любой вход в Совместимом режиме → `NotApplicable`»,
    /// теряя настоящий переход `Acquiring → Failed`. После фикса
    /// `lock_input` переставляет `self.active_mode` на целевой режим ДО
    /// выполнения эффектов `ModeSelected`, поэтому рекурсивный вход видит
    /// уже `Strict` и корректно даёт `Failed`, а не `NotApplicable`.
    #[test]
    fn compatible_to_strict_synchronous_spawn_failure_emits_failed() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(FailingSpawner::new(1)));
        let mut engine = Engine::new(deps, BackendCaps::current());
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() }];
        engine.active_mode = ModeKind::Compatible;
        engine.lock = LockState::NotApplicable;
        engine.mode_settings.strict.device = Some(dev.clone());

        engine.handle(EngineCmd::SetActiveMode(ModeKind::Strict));

        assert_eq!(engine.active_mode, ModeKind::Strict);
        match &engine.lock {
            LockState::Failed(CaptureFailure::PlatformUnsupported) => {}
            other => panic!("expected Failed(PlatformUnsupported), got {other:?}"),
        }
        assert!(engine.claim.is_none());
        assert!(engine.internal_rx.try_recv().is_err());
        let events = sink.take();
        assert!(events.iter().any(|e| matches!(
            e,
            EngineEvent::Lock(LockState::Failed(CaptureFailure::PlatformUnsupported))
        )));
    }

    /// Шаг 62 (§6.18, §6.19, ТЗ-137): `SetModeSettings` со `SwitchDevice`
    /// активного Эксклюзивного режима (Оптимальный) — замок видит один вход
    /// `LockInput::DeviceSelected(dev)` из `Held`, который сам упорядочивает
    /// `StopPlayback, Release, Acquire(dev)` (таблица §6.19): открытый
    /// Эксклюзивный путь закрывается (`ExclusiveCall::Stop` в журнале),
    /// старый захват сбрасывается, новое устройство захватывается
    /// (`ExclusiveCall::Acquire(new_dev)`) — замок уходит в `Acquiring`, а
    /// после `ClaimDone` — в `Held` с новым устройством. Шаг 65 (§6.20,
    /// ТЗ-137): точка повторного захвата, сохранённая перед
    /// `DeviceSelected`, переоткрывает трек на новом устройстве сразу после
    /// `ClaimDone` — захват потребляется переоткрытым путём (`self.claim`
    /// снова `None`), а не остаётся свободным до ручного `Open`.
    #[test]
    fn switch_device_held_releases_old_then_acquires_new() {
        let sink = VecSink::new();
        let (mut engine, calls) = engine_with_exclusive_path_open_tracked(sink.clone(), true);
        let new_dev: HwDeviceId = "hw:CARD=RU5,DEV=0".parse().expect("hw id");
        engine
            .catalog
            .hw
            .push(crate::audio::backend::catalog::HwDeviceInfo { id: new_dev.clone(), card: 2, device: 0, name: "RU5".to_string() });
        sink.take();

        let mut settings = engine.mode_settings.clone();
        settings.optimal.device = Some(new_dev.clone());
        let diff = ModeSettingsDiff { changed: SmallVec::from_slice(&[(ModeKind::Optimal, ParamId::Device)]) };
        engine.handle(EngineCmd::SetModeSettings(ModeSettingsUpdate { settings, diff: Some(diff) }));

        assert!(engine.exclusive_path.is_none());
        assert!(engine.claim.is_none());
        assert!(matches!(engine.lock, LockState::Acquiring { .. }));

        // `ClaimDone` приходит из служебного потока только после того, как
        // `claim_job` дописал `ExclusiveCall::Acquire(new_dev)` в журнал
        // (`FakeExclusiveBackend::claim_job`) — ждём его, прежде чем читать
        // журнал, вместо гонки с фоновым потоком.
        let msg = engine.internal_rx.recv().expect("claim done");
        {
            let journal = calls.lock().expect("calls lock");
            assert_eq!(journal.last(), Some(&crate::audio::testing::ExclusiveCall::Acquire(new_dev.clone())));
            assert!(journal.iter().any(|c| matches!(c, crate::audio::testing::ExclusiveCall::Stop)));
        }
        engine.handle_internal(msg);
        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, new_dev),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(engine.claim.is_none());
        let path = engine.exclusive_path.as_ref().expect("track reopened on new device");
        assert_eq!(path.position_secs(), 0.0);
        assert_eq!(engine.last_transport, Some(TransportState::Playing));
    }

    /// Шаг 62 (§6.18, §6.19, ТЗ-137): `SetActiveMode` между двумя
    /// Эксклюзивными режимами (Оптимальный → Строгий) с разными настроенными
    /// устройствами — `ModeSelected` сам по себе не даёт эффектов из `Held`
    /// (устройство могло бы остаться тем же), поэтому `EngineCmd::
    /// SetActiveMode` отдельно шлёт `LockInput::DeviceSelected(new_dev)`:
    /// старое устройство освобождается, новое захватывается — тот же
    /// `Stop`/`Acquire` журнал, что при `SwitchDevice`. Шаг 65 (§6.20,
    /// ТЗ-137): переоткрытие на новом устройстве здесь НЕ происходит —
    /// источник фикстуры (`FloatPcm`) не проходит `plan()` в Строгом режиме
    /// (`FloatUnsupported`, §6.5 шаг 4), поэтому `open_exclusive` возвращает
    /// захват обратно в `self.claim` (эмитит `Skipped`), а не потребляет
    /// его переоткрытым путём. Это ограничение фикстуры источника, а не
    /// поведения переоткрытия — см. тесты с Оптимальным режимом на обеих
    /// сторонах (`switch_device_while_playing_reopens_at_position_playing` и
    /// соседние), где источник совместим и переоткрытие происходит.
    #[test]
    fn optimal_to_strict_different_devices_reacquires() {
        let sink = VecSink::new();
        let (mut engine, calls) = engine_with_exclusive_path_open_tracked(sink.clone(), true);
        let strict_dev: HwDeviceId = "hw:CARD=RU5,DEV=0".parse().expect("hw id");
        engine
            .catalog
            .hw
            .push(crate::audio::backend::catalog::HwDeviceInfo { id: strict_dev.clone(), card: 2, device: 0, name: "RU5".to_string() });
        engine.mode_settings.strict.device = Some(strict_dev.clone());
        sink.take();

        engine.handle(EngineCmd::SetActiveMode(ModeKind::Strict));

        assert_eq!(engine.active_mode, ModeKind::Strict);
        assert!(engine.exclusive_path.is_none());
        assert!(engine.claim.is_none());
        assert!(matches!(engine.lock, LockState::Acquiring { .. }));

        // См. `switch_device_held_releases_old_then_acquires_new`: журнал
        // читается после `ClaimDone`, чтобы не гнаться с фоновым потоком
        // захвата.
        let msg = engine.internal_rx.recv().expect("claim done");
        {
            let journal = calls.lock().expect("calls lock");
            assert_eq!(journal.last(), Some(&crate::audio::testing::ExclusiveCall::Acquire(strict_dev.clone())));
            assert!(journal.iter().any(|c| matches!(c, crate::audio::testing::ExclusiveCall::Stop)));
        }
        engine.handle_internal(msg);
        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, strict_dev),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(engine.claim.is_some());
    }

    /// Шаг 62 (§6.18, §6.19, ТЗ-137): `SetActiveMode` между двумя
    /// Эксклюзивными режимами с ОДИНАКОВЫМ настроенным устройством — ни
    /// `ModeSelected` (таблица §6.19 не даёт эффектов из `Held`), ни
    /// добавленная в шаге 62 проверка диффа устройств (`old_dev == new_dev`)
    /// не должны освобождать/захватывать карту: открытый Эксклюзивный путь
    /// и журнал бэкенда остаются без изменений.
    #[test]
    fn optimal_to_strict_same_device_does_not_switch() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let (mut engine, calls) = engine_with_exclusive_path_open_tracked(sink.clone(), true);
        engine.mode_settings.strict.device = Some(dev.clone());
        sink.take();
        let journal_len_before = calls.lock().expect("calls lock").len();

        engine.handle(EngineCmd::SetActiveMode(ModeKind::Strict));

        assert_eq!(engine.active_mode, ModeKind::Strict);
        assert!(engine.exclusive_path.is_some());
        assert!(engine.claim.is_none());
        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, dev),
            other => panic!("expected Held, got {other:?}"),
        }
        let journal_after = calls.lock().expect("calls lock");
        assert_eq!(journal_after.len(), journal_len_before);
        assert!(!sink.take().iter().any(|e| matches!(e, EngineEvent::Lock(_))));
    }

    /// Шаг 65 (§6.18, §6.19, §6.20, ТЗ-137): `SetModeSettings` со
    /// `SwitchDevice` активного Эксклюзивного режима, трек играет — перед
    /// `LockInput::DeviceSelected` движок сохраняет точку повторного захвата
    /// (`save_switch_device_reacquire_point`) с текущей позицией и
    /// `autoplay: true` (трек играл). После `ClaimDone` на новом устройстве
    /// `reopen_after_reacquire` переоткрывает трек на сохранённой позиции
    /// (1 секунда) и СРАЗУ ИГРАЕТ («продолжение с той же позиции», ТЗ-137) —
    /// в отличие от потери устройства (ТЗ-126), где переоткрытие всегда на
    /// паузе.
    #[test]
    fn switch_device_while_playing_reopens_at_position_playing() {
        let sink = VecSink::new();
        let (mut engine, _calls) = engine_with_exclusive_path_open_tracked(sink.clone(), true);
        assert_eq!(engine.last_transport, Some(TransportState::Playing));
        let rate = engine.exclusive_path.as_ref().expect("exclusive path open").frame_rate();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.pos_frames.store(u64::from(rate), Ordering::Relaxed);

        let new_dev: HwDeviceId = "hw:CARD=RU5,DEV=0".parse().expect("hw id");
        engine
            .catalog
            .hw
            .push(crate::audio::backend::catalog::HwDeviceInfo { id: new_dev.clone(), card: 2, device: 0, name: "RU5".to_string() });
        sink.take();

        let mut settings = engine.mode_settings.clone();
        settings.optimal.device = Some(new_dev.clone());
        let diff = ModeSettingsDiff { changed: SmallVec::from_slice(&[(ModeKind::Optimal, ParamId::Device)]) };
        engine.handle(EngineCmd::SetModeSettings(ModeSettingsUpdate { settings, diff: Some(diff) }));

        match &engine.reacquire {
            Some(point) => {
                assert_eq!(point.device, new_dev);
                assert!(point.autoplay);
            }
            None => panic!("expected reacquire point to be saved before DeviceSelected"),
        }

        let msg = engine.internal_rx.recv().expect("claim done");
        engine.handle_internal(msg);

        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, new_dev),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(engine.reacquire.is_none());
        let path = engine.exclusive_path.as_ref().expect("track reopened");
        assert_eq!(path.position_secs(), 1.0);
        assert_eq!(engine.last_transport, Some(TransportState::Playing));
    }

    /// Шаг 65 (§6.18, §6.19, §6.20, ТЗ-137): тот же переход, что
    /// `switch_device_while_playing_reopens_at_position_playing`, но трек на
    /// паузе в момент смены устройства — сохранённая точка несёт
    /// `autoplay: false`, и переоткрытие на новом устройстве остаётся на
    /// паузе (позиция всё равно переносится).
    #[test]
    fn switch_device_while_paused_reopens_paused() {
        let sink = VecSink::new();
        let (mut engine, _calls) = engine_with_exclusive_path_open_tracked(sink.clone(), false);
        assert_eq!(engine.last_transport, Some(TransportState::Paused));
        let rate = engine.exclusive_path.as_ref().expect("exclusive path open").frame_rate();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.pos_frames.store(u64::from(rate), Ordering::Relaxed);

        let new_dev: HwDeviceId = "hw:CARD=RU5,DEV=0".parse().expect("hw id");
        engine
            .catalog
            .hw
            .push(crate::audio::backend::catalog::HwDeviceInfo { id: new_dev.clone(), card: 2, device: 0, name: "RU5".to_string() });
        sink.take();

        let mut settings = engine.mode_settings.clone();
        settings.optimal.device = Some(new_dev.clone());
        let diff = ModeSettingsDiff { changed: SmallVec::from_slice(&[(ModeKind::Optimal, ParamId::Device)]) };
        engine.handle(EngineCmd::SetModeSettings(ModeSettingsUpdate { settings, diff: Some(diff) }));

        match &engine.reacquire {
            Some(point) => {
                assert_eq!(point.device, new_dev);
                assert!(!point.autoplay);
            }
            None => panic!("expected reacquire point to be saved before DeviceSelected"),
        }

        let msg = engine.internal_rx.recv().expect("claim done");
        engine.handle_internal(msg);

        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, new_dev),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(engine.reacquire.is_none());
        let path = engine.exclusive_path.as_ref().expect("track reopened");
        assert_eq!(path.position_secs(), 1.0);
        assert_eq!(engine.last_transport, Some(TransportState::Paused));
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

    /// ТЗ-68, §6.18 п.3, И-Р13: `poll_session` на открытом Эксклюзивном пути
    /// шлёт `DynamicsChanged` с обновлённым `underruns`, когда счётчик в
    /// `SessionShared` изменился относительно последней отправленной копии.
    #[test]
    fn poll_session_emits_dynamics_changed_once_when_underruns_change() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.underruns.store(3, Ordering::Relaxed);

        engine.poll_session();

        let events = sink.take();
        let dynamics = events.iter().find_map(|e| match e {
            EngineEvent::DynamicsChanged { session, dynamics } => Some((*session, dynamics.clone())),
            _ => None,
        });
        let (session, dynamics) = dynamics.expect("DynamicsChanged emitted");
        assert_eq!(session, 1);
        assert_eq!(dynamics.underruns, 3);
    }

    /// ТЗ-68, §6.18 п.3, И-Р13: без изменения `self.dynamics` относительно
    /// последней отправленной копии повторный опрос не шлёт `DynamicsChanged`
    /// заново (дедупликация, как у `Position`/`Transport`).
    #[test]
    fn poll_session_does_not_repeat_dynamics_changed_without_change() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.underruns.store(3, Ordering::Relaxed);

        engine.poll_session();
        sink.take();
        engine.poll_session();

        let events = sink.take();
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::DynamicsChanged { .. })));
    }

    /// §6.20, ТЗ-51, ТЗ-76, ТЗ-126 (шаг 51, шаг 63): `StreamFault` потока
    /// вывода на открытом Эксклюзивном пути переносит счётчики динамики в
    /// `self.dynamics` и переиспользует `lock_input(DeviceLost)` — переход
    /// `Held → Failed(DeviceDisconnected)` c эффектами `StopPlayback`/
    /// `Release` (§6.19) — и шлёт `DeviceLost { reaction: StopWithError }`
    /// в UI. До `lock_input` сохраняется точка повторного захвата (§6.20
    /// п. 2) с устройством, треком и позицией в кадрах.
    #[test]
    fn poll_session_stream_fault_stops_playback_and_releases_lock() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let rate = engine.exclusive_path.as_ref().expect("exclusive path open").frame_rate();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.pos_frames.store(u64::from(rate), Ordering::Relaxed);
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
        let reacquire = engine.reacquire.as_ref().expect("reacquire point saved");
        assert_eq!(reacquire.device, dev);
        assert_eq!(reacquire.track.path.as_ref(), Path::new("/music/a.flac"));
        assert_eq!(reacquire.frame, u64::from(rate));
    }

    /// §6.20, ADR-08, И-Р20, ТЗ-123, ТЗ-126 (шаг 63): `InternalMsg::
    /// ReservationLost` для карты удержанного замка переиспользует тот же
    /// переход `Held → Failed(DeviceDisconnected)`, что и `StreamFault`, и
    /// сохраняет точку повторного захвата до `lock_input`.
    #[test]
    fn reservation_lost_for_held_card_fails_lock_and_saves_reacquire() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        engine.claim_card = Some(7);

        engine.handle_internal(InternalMsg::ReservationLost { card: 7 });

        let events = sink.take();
        assert!(events
            .iter()
            .any(|e| matches!(e, EngineEvent::DeviceLost { reaction: Reaction::StopWithError })));
        assert!(matches!(engine.lock, LockState::Failed(CaptureFailure::DeviceDisconnected)));
        assert!(engine.exclusive_path.is_none());
        let reacquire = engine.reacquire.as_ref().expect("reacquire point saved");
        assert_eq!(reacquire.device, dev);
        assert_eq!(reacquire.track.path.as_ref(), Path::new("/music/a.flac"));
    }

    /// §6.20, ADR-08, И-Р20, ТЗ-123 (шаг 63): `ReservationLost` для карты, не
    /// совпадающей с удержанной (`self.claim_card`), — сигнал устаревшей/
    /// чужой попытки захвата (гонка) и отбрасывается без изменения замка.
    #[test]
    fn reservation_lost_for_other_card_is_ignored() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink.clone(), true);
        sink.take();
        engine.claim_card = Some(7);

        engine.handle_internal(InternalMsg::ReservationLost { card: 9 });

        let events = sink.take();
        assert!(events.is_empty());
        assert!(matches!(engine.lock, LockState::Held { .. }));
        assert!(engine.exclusive_path.is_some());
        assert!(engine.reacquire.is_none());
    }

    /// Движок после потери устройства (`StreamFault` на открытом Эксклюзивном
    /// пути, как в `poll_session_stream_fault_stops_playback_and_releases_lock`),
    /// с сохранённой точкой повторного захвата — общая отправная точка тестов
    /// шага 64 (§6.20 п. 4, ТЗ-126): `lock` = `Failed(DeviceDisconnected)`,
    /// `reacquire` = `Some(ReacquirePoint { device: RU3, frame: rate, rate })`
    /// (ровно 1 секунда позиции). `enumerate_result` — ответ
    /// `FakeExclusiveBackend::enumerate` на последующий `refresh_devices`
    /// теста (симуляция повторного перечисления карт хоста).
    fn engine_after_device_loss_with_enumerate(
        sink: VecSink,
        enumerate_result: Result<Vec<crate::audio::backend::catalog::HwDeviceInfo>, BackendError>,
    ) -> Engine {
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new()
            .with_configure(Ok((
                crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                    rate: SampleRate::new(44_100).expect("rate"),
                    sample: crate::audio::format::OutputSampleFormat::S32Le,
                    channels: std::num::NonZeroU8::new(2).expect("channels"),
                }),
                Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
            )))
            .with_enumerate(enumerate_result);
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
            rt: Arc::new(FakeRtRequester::new(None)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.catalog.hw = vec![crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() }];
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev.clone() } };
        engine.claim = Some(crate::audio::backend::exclusive::DeviceClaim::new(
            None,
            crate::audio::reservation::Reservation::UnavailableNoServer,
            crate::audio::backend::exclusive::ClaimInfo { device: dev },
        ));
        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });
        assert!(engine.exclusive_path.is_some());

        let rate = engine.exclusive_path.as_ref().expect("exclusive path open").frame_rate();
        let shared = Arc::clone(engine.exclusive_path.as_ref().expect("exclusive path open").shared());
        shared.pos_frames.store(u64::from(rate), Ordering::Relaxed);
        shared.set_fault(StreamFault::DeviceUnavailable);
        engine.poll_session();
        assert!(matches!(engine.lock, LockState::Failed(CaptureFailure::DeviceDisconnected)));
        assert!(engine.reacquire.is_some());
        engine
    }

    /// Шаг 64 (§6.20 п. 4, ТЗ-126): устройство из точки повторного захвата
    /// снова появляется в перечислении (`refresh_devices` → `refresh_hw` →
    /// изменение `catalog.hw`) — движок сам шлёт
    /// `LockInput::DeviceReappeared(dev)`, захват проходит (фейковый бэкенд
    /// по умолчанию успешен), и после `ClaimDone` трек переоткрывается на
    /// сохранённой позиции (1 секунда) НА ПАУЗЕ (`autoplay: false`, несмотря
    /// на то что до потери играл), а точка повторного захвата забирается.
    #[test]
    fn device_reappears_reacquires_and_reopens_paused_at_saved_position() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let hw_info = crate::audio::backend::catalog::HwDeviceInfo { id: dev.clone(), card: 1, device: 0, name: "RU3".to_string() };
        let mut engine = engine_after_device_loss_with_enumerate(sink.clone(), Ok(vec![hw_info]));
        // Карта уже пропала из каталога (предыдущий `refresh_devices` во
        // время отключения убрал её) — текущий вызов видит её возвращение.
        engine.catalog.hw = Vec::new();
        sink.take();

        engine.refresh_devices(false);

        assert!(matches!(engine.lock, LockState::Acquiring { .. }));
        let msg = engine.internal_rx.recv().expect("claim done");
        engine.handle_internal(msg);

        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, dev),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(engine.reacquire.is_none());
        let path = engine.exclusive_path.as_ref().expect("track reopened");
        assert_eq!(path.position_secs(), 1.0);
        assert_eq!(engine.last_transport, Some(TransportState::Paused));
    }

    /// Шаг 64 (§6.20 п. 4, ТЗ-126): перечисление изменилось, но появившееся
    /// устройство — не то, что в точке повторного захвата (например, у
    /// отключённой карты подключился посторонний прибор) — движок не должен
    /// самовольно захватывать чужое устройство: `try_reacquire_reappeared_device`
    /// находит точку, но её `device` отсутствует среди перечисленных, и
    /// `lock_input` не вызывается — замок остаётся `Failed`, точка цела.
    #[test]
    fn device_reappears_does_not_reacquire_different_device() {
        let sink = VecSink::new();
        let other_dev =
            crate::audio::backend::catalog::HwDeviceInfo { id: "hw:CARD=RU5,DEV=0".parse().expect("hw id"), card: 2, device: 0, name: "RU5".to_string() };
        let mut engine = engine_after_device_loss_with_enumerate(sink.clone(), Ok(vec![other_dev]));
        sink.take();

        engine.refresh_devices(false);

        assert!(matches!(engine.lock, LockState::Failed(CaptureFailure::DeviceDisconnected)));
        assert!(engine.reacquire.is_some());
        assert!(engine.exclusive_path.is_none());
    }

    /// Шаг 64 (§6.20 п. 4, ТЗ-126): перечисление не изменилось (тот же
    /// пустой/прежний список устройств) — `refresh_hw` возвращает
    /// `changed == false`, `try_reacquire_reappeared_device` не вызывается
    /// вовсе, замок и точка повторного захвата остаются нетронутыми.
    #[test]
    fn refresh_devices_without_hw_change_does_not_touch_reacquire() {
        let sink = VecSink::new();
        let mut engine = engine_after_device_loss_with_enumerate(sink.clone(), Ok(Vec::new()));
        // Фикстура кладёт RU3 в `catalog.hw` напрямую (нужен для успешного
        // захвата до потери), но бэкенд перечисляет пустой список — чтобы
        // проверить именно случай БЕЗ изменения каталога, здесь каталог
        // приводится к тому же пустому списку, что и у бэкенда.
        engine.catalog.hw = Vec::new();
        sink.take();

        engine.refresh_devices(false);

        assert!(matches!(engine.lock, LockState::Failed(CaptureFailure::DeviceDisconnected)));
        assert!(engine.reacquire.is_some());
        assert!(engine.exclusive_path.is_none());
    }

    /// Шаг 64 (§6.20 п. 4, ТЗ-126): защита в `reopen_after_reacquire` от
    /// переоткрытия на чужом захвате — `ClaimDone` сообщает устройство, не
    /// совпадающее с `self.reacquire.device` (гонка: захват успел пойти на
    /// другое устройство мимо `DeviceReappeared`, например ручной
    /// `RetryCapture` поверх старого `Failed` без `ForgetReacquire`, таблица
    /// §6.19). Точка должна остаться нетронутой, трек не переоткрывается.
    #[test]
    fn reopen_after_reacquire_ignores_claim_for_different_device() {
        let sink = VecSink::new();
        let mut engine = engine_after_device_loss_with_enumerate(sink.clone(), Ok(Vec::new()));
        let other_dev: HwDeviceId = "hw:CARD=RU5,DEV=0".parse().expect("hw id");
        engine.catalog.hw.push(crate::audio::backend::catalog::HwDeviceInfo { id: other_dev.clone(), card: 2, device: 0, name: "RU5".to_string() });
        // Пользователь успел сменить настроенное устройство режима на другое
        // — `RetryCapture` разрешится на него, а не на устройство точки
        // повторного захвата (ещё RU3).
        engine.mode_settings.optimal.device = Some(other_dev.clone());
        sink.take();

        // Ручной повторный запрос захвата на другое устройство, минуя
        // `DeviceReappeared` (`RetryCapture` из `Failed` — таблица §6.19 даёт
        // `Acquire(dev)` без `ForgetReacquire`).
        engine.lock_input(LockInput::RetryCapture);
        assert!(matches!(engine.lock, LockState::Acquiring { .. }));
        let msg = engine.internal_rx.recv().expect("claim done");
        engine.handle_internal(msg);

        match &engine.lock {
            LockState::Held { info } => assert_eq!(info.device, other_dev),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(engine.reacquire.is_some(), "точка повторного захвата старого устройства не должна быть потеряна");
        assert!(engine.exclusive_path.is_none(), "трек не должен переоткрываться на чужом захвате");
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
        // §6.19, AM1.0-B17: захват вернулся — следующий трек откроется.
        assert!(engine.claim.is_some(), "claim must be returned after track end");
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
            rt: Arc::new(FakeRtRequester::new(None)),
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

    /// §6.8 п. 2-3 (ТЗ-99, ТЗ-76): TID потока вывода Эксклюзивного пути уходит
    /// в `rt`-зависимость, результат возвращается через `InternalMsg::RtPriority`
    /// с `session == path_generation` — после `drain_internal` приоритет в
    /// `DynamicPathState` становится `Granted`.
    #[test]
    fn rt_priority_granted_updates_dynamics_after_exclusive_open() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new()
            .with_configure(Ok((
                crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                    rate: SampleRate::new(44_100).expect("rate"),
                    sample: crate::audio::format::OutputSampleFormat::S32Le,
                    channels: std::num::NonZeroU8::new(2).expect("channels"),
                }),
                Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
            )))
            .with_start_tid(Some(4242));
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(SourceFormat {
                container: Container::Wav,
                codec: Codec::Wav,
                lossy: false,
                rate: SampleRate::new(44_100).expect("rate"),
                layout: ChannelLayout::stereo(),
                kind: SourceKind::FloatPcm { double: false },
            })),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
            rt: Arc::new(FakeRtRequester::new(Some(20))),
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
        assert!(engine.exclusive_path.is_some());
        engine.drain_internal();

        assert_eq!(engine.dynamics.rt_priority, RtPriorityState::Granted(20));
    }

    /// §6.8 п. 2 (ТЗ-99, ТЗ-76): бэкенд не публикует TID потока вывода
    /// (`rt_tid() == None`) — запрос у `rtkit` не имеет смысла, приоритет
    /// сразу `Denied` без похода в `rt`-зависимость.
    #[test]
    fn rt_priority_denied_when_backend_reports_no_tid() {
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
            sources: Arc::new(FakeSource::with_format(SourceFormat {
                container: Container::Wav,
                codec: Codec::Wav,
                lossy: false,
                rate: SampleRate::new(44_100).expect("rate"),
                layout: ChannelLayout::stereo(),
                kind: SourceKind::FloatPcm { double: false },
            })),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
            rt: Arc::new(FakeRtRequester::new(Some(20))),
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
        assert!(engine.exclusive_path.is_some());

        assert_eq!(engine.dynamics.rt_priority, RtPriorityState::Denied);
    }

    /// §6.8 п. 1, 3 (ТЗ-99, ТЗ-76): приоритет, выданный `deps.rt.init()` при
    /// запуске движка, уходит в `request` как есть (не константа
    /// `RT_PRIORITY_CAP`) — `rtkit` мог дать меньший `MaxRealtimePriority`.
    #[test]
    fn rt_priority_requests_init_result_not_the_cap() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new()
            .with_configure(Ok((
                crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                    rate: SampleRate::new(44_100).expect("rate"),
                    sample: crate::audio::format::OutputSampleFormat::S32Le,
                    channels: std::num::NonZeroU8::new(2).expect("channels"),
                }),
                Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
            )))
            .with_start_tid(Some(4242));
        let mem = MemStore::new();
        let rt = Arc::new(FakeRtRequester::new(Some(5)).with_init(Some(5)));
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(SourceFormat {
                container: Container::Wav,
                codec: Codec::Wav,
                lossy: false,
                rate: SampleRate::new(44_100).expect("rate"),
                layout: ChannelLayout::stereo(),
                kind: SourceKind::FloatPcm { double: false },
            })),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
            rt: Arc::clone(&rt) as Arc<dyn RtPriorityRequester>,
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
        assert!(engine.exclusive_path.is_some());
        engine.drain_internal();

        assert_eq!(rt.requests(), vec![(4242, 5)]);
        assert_eq!(engine.dynamics.rt_priority, RtPriorityState::Granted(5));
    }

    /// §6.8 п. 1 (ТЗ-99): `deps.rt.init()` не дал RT вовсе (нет шины/демона
    /// `rtkit` или нулевой лимит) — `request_rt_priority` сразу `Denied`,
    /// даже при известном TID потока, без похода в `deps.rt.request`.
    #[test]
    fn rt_priority_denied_when_init_gives_no_rt() {
        let sink = VecSink::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let backend = FakeExclusiveBackend::new()
            .with_configure(Ok((
                crate::audio::backend::DriverFormat::Confirmed(crate::audio::format::StreamFormat {
                    rate: SampleRate::new(44_100).expect("rate"),
                    sample: crate::audio::format::OutputSampleFormat::S32Le,
                    channels: std::num::NonZeroU8::new(2).expect("channels"),
                }),
                Some(crate::audio::backend::DeviceBufferActual { period_frames: 256, buffer_frames: 1024 }),
            )))
            .with_start_tid(Some(4242));
        let mem = MemStore::new();
        let rt = Arc::new(FakeRtRequester::new(Some(20)).with_init(None));
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(SourceFormat {
                container: Container::Wav,
                codec: Codec::Wav,
                lossy: false,
                rate: SampleRate::new(44_100).expect("rate"),
                layout: ChannelLayout::stereo(),
                kind: SourceKind::FloatPcm { double: false },
            })),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
            rt: Arc::clone(&rt) as Arc<dyn RtPriorityRequester>,
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
        assert!(engine.exclusive_path.is_some());

        assert_eq!(rt.requests(), Vec::<(u32, u32)>::new());
        assert_eq!(engine.dynamics.rt_priority, RtPriorityState::Denied);
    }

    /// §6.8 (ТЗ-99): ответ `InternalMsg::RtPriority` устаревшей сессии
    /// (`session != path_generation`) отбрасывается молча — приоритет в
    /// `DynamicPathState` не меняется, как и `ClaimDone` на отменённую
    /// попытку.
    #[test]
    fn rt_priority_stale_session_is_ignored() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink), Arc::new(StdSpawner));
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.path_generation = 5;

        engine.handle_internal(InternalMsg::RtPriority { session: 1, granted: Some(10) });

        assert_eq!(engine.dynamics.rt_priority, RtPriorityState::NotRequested);
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
            rt: Arc::new(FakeRtRequester::new(None)),
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
            rt: Arc::new(FakeRtRequester::new(None)),
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

    /// §6.7, §6.18 шаг 5 (ТЗ-43, И-Р12): смена трека на той же конфигурации
    /// PCM (`plan.target` и «Буфер устройства» не изменились) не повторяет
    /// `ExclusiveBackend::configure` — второй `Open` переиспользует
    /// `driver`/`buffer_actual` первого открытия. Замок остаётся удержан:
    /// журнал бэкенда не содержит `Acquire` (claim не освобождался и не
    /// захватывался заново).
    #[test]
    fn open_exclusive_same_format_track_change_skips_configure() {
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
        // Хэндл журнала берётся до того, как `backend` уходит в `Box<dyn
        // ExclusiveBackend>` внутри `EngineDeps` (§7.1).
        let calls = backend.calls_handle();
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
            rt: Arc::new(FakeRtRequester::new(None)),
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
        engine.handle(EngineCmd::Open { req_gen: 2, path: Arc::from(Path::new("/music/b.flac")), start_secs: 0.0, autoplay: true });

        assert!(engine.exclusive_path.is_some());
        let journal = calls.lock().expect("calls lock");
        assert_eq!(journal.iter().filter(|c| **c == crate::audio::testing::ExclusiveCall::Configure).count(), 1);
        assert!(!journal.iter().any(|c| matches!(c, crate::audio::testing::ExclusiveCall::Acquire(_))));
    }

    /// §6.7, §6.18 шаг 5 (ТЗ-43): смена трека с другой «Буфером устройства»
    /// отличается от текущей конфигурации PCM — второй `Open` вызывает
    /// `ExclusiveBackend::configure` ещё раз (hw_params переустанавливаются),
    /// замок остаётся удержан (claim не терялся между открытиями).
    #[test]
    fn open_exclusive_different_device_buffer_track_change_reconfigures() {
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
        let calls = backend.calls_handle();
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
            rt: Arc::new(FakeRtRequester::new(None)),
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
        engine.mode_settings.optimal.device_buffer = crate::settings::playback::DeviceBuffer::Ms100;
        engine.handle(EngineCmd::Open { req_gen: 2, path: Arc::from(Path::new("/music/b.flac")), start_secs: 0.0, autoplay: true });

        assert!(engine.exclusive_path.is_some());
        let journal = calls.lock().expect("calls lock");
        assert_eq!(journal.iter().filter(|c| **c == crate::audio::testing::ExclusiveCall::Configure).count(), 2);
        assert!(!journal.iter().any(|c| matches!(c, crate::audio::testing::ExclusiveCall::Acquire(_))));
    }

    /// §6.7, §6.18 шаг 54 (ТЗ-131, И-Р26): `SetModeSettings` меняет «Буфер
    /// устройства» активного Оптимального режима — переоткрытие Эксклюзивного
    /// пути (`ApplyKind::ReopenAtPosition`) на текущем треке без освобождения
    /// `DeviceClaim` (журнал бэкенда не содержит `Acquire`), с повторным
    /// `configure` (цель `ConfiguredFormat` отличается «Буфером устройства»)
    /// и с сохранением паузы (переоткрытие передаёт прежний `is_playing()`
    /// как `autoplay`).
    #[test]
    fn set_mode_settings_device_buffer_change_reopens_exclusive_path_at_position() {
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
        // Хэндл журнала берётся до того, как `backend` уходит в `Box<dyn
        // ExclusiveBackend>` внутри `EngineDeps` (§7.1).
        let calls = backend.calls_handle();
        let float_format = SourceFormat {
            container: Container::Wav,
            codec: Codec::Wav,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::FloatPcm { double: false },
        };
        let source = Arc::new(FakeSource::with_format(float_format));
        let probes = Arc::clone(&source);
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: source,
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
            rt: Arc::new(FakeRtRequester::new(None)),
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
        engine.handle(EngineCmd::Pause);
        assert!(!engine.exclusive_path.as_ref().expect("exclusive path open").is_playing());

        let mut settings = engine.mode_settings.clone();
        settings.optimal.device_buffer = crate::settings::playback::DeviceBuffer::Ms100;
        let diff = ModeSettingsDiff { changed: SmallVec::from_slice(&[(ModeKind::Optimal, ParamId::DeviceBuffer)]) };
        engine.handle(EngineCmd::SetModeSettings(ModeSettingsUpdate { settings, diff: Some(diff) }));

        assert!(engine.exclusive_path.is_some());
        assert!(!engine.exclusive_path.as_ref().expect("exclusive path open").is_playing());
        assert_eq!(probes.probes(), 2);
        let journal = calls.lock().expect("calls lock");
        assert_eq!(journal.iter().filter(|c| **c == crate::audio::testing::ExclusiveCall::Configure).count(), 2);
        assert!(!journal.iter().any(|c| matches!(c, crate::audio::testing::ExclusiveCall::Acquire(_))));
    }

    /// §7.2 (ОВС-17, ТЗ-145, ADR-25): смена подокументной опции устройства
    /// «ЦАП понимает DoP» (`ModeSettings.devices: BTreeMap<HwDeviceId,
    /// DeviceOpts>`, по умолчанию `dop = true`) через `SetModeSettings` во
    /// время воспроизведения DSD в Оптимальном режиме меняет маршрут плана
    /// (`Dop` → `DsdToPcm`, §6.9) и даёт переоткрытие Эксклюзивного пути на
    /// текущей позиции (`ParamId::DeviceDop` → `ApplyKind::ReopenAtPosition`),
    /// без освобождения `DeviceClaim` (журнал бэкенда не содержит `Acquire`),
    /// с повторным `configure` (целевой формат потока меняется вместе с
    /// маршрутом). Реальный дифф для изменения `devices` помечает `DeviceDop`
    /// сразу в обоих режимах (`changed_for_devices`, `settings/params.rs`).
    #[test]
    fn device_dop_change_reopens_at_position() {
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
        // Хэндл журнала берётся до того, как `backend` уходит в `Box<dyn
        // ExclusiveBackend>` внутри `EngineDeps` (§7.1).
        let calls = backend.calls_handle();
        let dsd_format = SourceFormat {
            container: Container::Dsf,
            codec: Codec::Dsf,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Dsd { dsd: crate::audio::format::DsdRate::Dsd64 },
        };
        let source = Arc::new(FakeSource::with_format(dsd_format));
        let probes = Arc::clone(&source);
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: source,
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
            rt: Arc::new(FakeRtRequester::new(None)),
        };
        let mut engine = Engine::new(deps, BackendCaps::current());
        engine.active_mode = ModeKind::Optimal;
        engine.mode_settings.optimal.device = Some(dev.clone());
        engine.lock = LockState::Held { info: crate::audio::backend::exclusive::ClaimInfo { device: dev.clone() } };
        engine.claim = Some(crate::audio::backend::exclusive::DeviceClaim::new(
            None,
            crate::audio::reservation::Reservation::UnavailableNoServer,
            crate::audio::backend::exclusive::ClaimInfo { device: dev.clone() },
        ));

        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 0.0, autoplay: true });
        engine.handle(EngineCmd::Pause);
        assert!(!engine.exclusive_path.as_ref().expect("exclusive path open").is_playing());

        let mut settings = engine.mode_settings.clone();
        settings.devices.insert(dev.clone(), crate::settings::playback::DeviceOpts { dop: false });
        let diff = ModeSettingsDiff {
            changed: SmallVec::from_slice(&[(ModeKind::Optimal, ParamId::DeviceDop), (ModeKind::Strict, ParamId::DeviceDop)]),
        };
        engine.handle(EngineCmd::SetModeSettings(ModeSettingsUpdate { settings, diff: Some(diff) }));

        assert!(engine.exclusive_path.is_some());
        assert!(!engine.exclusive_path.as_ref().expect("exclusive path open").is_playing());
        assert_eq!(probes.probes(), 2);
        let journal = calls.lock().expect("calls lock");
        assert_eq!(journal.iter().filter(|c| **c == crate::audio::testing::ExclusiveCall::Configure).count(), 2);
        assert!(!journal.iter().any(|c| matches!(c, crate::audio::testing::ExclusiveCall::Acquire(_))));
        assert!(!engine.mode_settings.device_opts(&dev).dop);
    }

    /// §6.7, §6.18 шаг 55 (ТЗ-131, И-Р26, §7.2
    /// `audio_param_change_reopens_at_position_preserving_pause`): одно
    /// `SetModeSettings`, меняющее сразу два параметра звука активного
    /// Оптимального режима с `ApplyKind::ReopenAtPosition` («Фильтр SRC» и
    /// «Дизеринг»), даёт не более одного переоткрытия Эксклюзивного пути —
    /// `strongest(active)` сводит разницу к одному действию независимо от
    /// числа изменившихся параметров одного класса. Переоткрытие сохраняет
    /// позицию (`start_frame` = прежней позиции в кадрах, записанной
    /// синхронно в `DecodeLoop::new` до спавна потока декодера, И-Р13) и
    /// состояние паузы (`autoplay = false`), не освобождая `DeviceClaim`
    /// (журнал бэкенда не содержит `Acquire`).
    #[test]
    fn audio_param_change_reopens_at_position_preserving_pause() {
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
        // Хэндл журнала берётся до того, как `backend` уходит в `Box<dyn
        // ExclusiveBackend>` внутри `EngineDeps` (§7.1).
        let calls = backend.calls_handle();
        let float_format = SourceFormat {
            container: Container::Wav,
            codec: Codec::Wav,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::FloatPcm { double: false },
        };
        let source = Arc::new(FakeSource::with_format(float_format));
        let probes = Arc::clone(&source);
        let mem = MemStore::new();
        let deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(backend)),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: source,
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(sink.clone()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
            rt: Arc::new(FakeRtRequester::new(None)),
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

        // Открытие с позиции 1:00 (ТЗ-131, §7.2): `start_frame` уходит в
        // `DecodeLoop::new`, который кладёт его в `pos_frames` синхронно, до
        // спавна потока декодера — позиция наблюдаема сразу после `handle`,
        // без опроса/поллинга.
        engine.handle(EngineCmd::Open { req_gen: 1, path: Arc::from(Path::new("/music/a.flac")), start_secs: 60.0, autoplay: true });
        engine.handle(EngineCmd::Pause);
        assert!(!engine.exclusive_path.as_ref().expect("exclusive path open").is_playing());
        let pos_before = engine.exclusive_path.as_ref().expect("exclusive path open").position_secs();
        assert_eq!(pos_before, 60.0);

        let mut settings = engine.mode_settings.clone();
        settings.optimal.src_filter = crate::settings::playback::SrcFilter::VerySlow;
        settings.optimal.dither = Dither::Off;
        let diff = ModeSettingsDiff {
            changed: SmallVec::from_slice(&[
                (ModeKind::Optimal, ParamId::SrcFilter),
                (ModeKind::Optimal, ParamId::DitherKind),
            ]),
        };
        engine.handle(EngineCmd::SetModeSettings(ModeSettingsUpdate { settings, diff: Some(diff) }));

        assert!(engine.exclusive_path.is_some());
        // Пауза сохранена: переоткрытие передало прежний `is_playing()` как
        // `autoplay = false`.
        assert!(!engine.exclusive_path.as_ref().expect("exclusive path open").is_playing());
        // Позиция сохранена ровно: `start_frame` переоткрытия — та же позиция
        // в кадрах, что была до него (ТЗ-131).
        assert_eq!(engine.exclusive_path.as_ref().expect("exclusive path open").position_secs(), pos_before);
        // Ровно одно переоткрытие, а не два (по одному на каждый изменённый
        // параметр одного класса `ReopenAtPosition`): один дополнительный
        // `probe` источника на `reopen_exclusive_at_position`.
        assert_eq!(probes.probes(), 2);
        let journal = calls.lock().expect("calls lock");
        assert_eq!(journal.iter().filter(|c| **c == crate::audio::testing::ExclusiveCall::Start).count(), 2);
        assert!(!journal.iter().any(|c| matches!(c, crate::audio::testing::ExclusiveCall::Acquire(_))));
    }

    /// §6.7, §6.18 шаг 54 (ТЗ-131, И-Р26): изменение параметра активного
    /// Оптимального режима без влияния на поток (`ApplyKind::Memory`, здесь —
    /// «DSD выше возможностей ЦАП») не переоткрывает Эксклюзивный путь — ни
    /// повторного `configure`, ни повторного открытия источника.
    #[test]
    fn set_mode_settings_memory_change_does_not_reopen_exclusive_path() {
        let sink = VecSink::new();
        let mut engine = engine_with_exclusive_path_open(sink, true);
        let configured_before = engine.exclusive_path.as_ref().expect("exclusive path open").configured();

        let settings = engine.mode_settings.clone();
        let diff = ModeSettingsDiff { changed: SmallVec::from_slice(&[(ModeKind::Optimal, ParamId::DsdAboveDac)]) };
        engine.handle(EngineCmd::SetModeSettings(ModeSettingsUpdate { settings, diff: Some(diff) }));

        assert!(engine.exclusive_path.is_some());
        assert_eq!(engine.exclusive_path.as_ref().expect("exclusive path open").configured(), configured_before);
    }
}
