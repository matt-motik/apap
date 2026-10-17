//! Автомат замка устройства режима (§2.6, ADR-20, ADR-24). Состояние —
//! `LockState`, переходы — `LockInput`, побочные действия для движка —
//! `LockEffect`. `ReacquirePoint` — точка, куда встать после автоматического
//! повторного захвата (ТЗ-126). Шаг `lock_step` реализуется отдельно.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use smallvec::{smallvec, SmallVec};

use crate::audio::backend::exclusive::ClaimInfo;
use crate::audio::backend::HwDeviceId;
use crate::audio::clock::ClockInstant;
use crate::audio::error::CaptureFailure;
use crate::engine::messages::TrackRef;
use crate::settings::playback::ModeKind;

/// Состояние замка устройства режима (§2.6).
#[derive(Clone, PartialEq, Debug)]
pub enum LockState {
    /// Совместимый режим: виден, неактивен, подсказка (ТЗ-50).
    NotApplicable,
    Open,
    /// Ожидание резервирования, не более 5 с (ОВ-34).
    Acquiring { started: ClockInstant }, // инжектируемые часы (ADR-20, ОВС-10 п. 7)
    Held { info: ClaimInfo },
    Failed(CaptureFailure),
}

/// Входы автомата замка. Play, пауза, стоп и смена трека сюда не входят (ТЗ-43, ТЗ-125).
#[derive(Clone, PartialEq, Debug)]
pub enum LockInput {
    Startup,
    ModeSelected(ModeKind),
    DeviceSelected(HwDeviceId),
    UserClose,
    UserOpen,
    CaptureAndPlay,
    DeviceReappeared(HwDeviceId),
    RetryCapture,
    CaptureResult(Result<ClaimInfo, CaptureFailure>),
    DeviceLost,
    Shutdown,
}

/// Точка, куда встать после автоматического повторного захвата (ТЗ-126).
#[derive(Clone, PartialEq, Debug)]
pub struct ReacquirePoint {
    pub device: HwDeviceId,
    pub track: TrackRef,
    pub frame: u64,
}

/// Побочные действия замка для движка (§2.6).
#[derive(Clone, PartialEq, Debug)]
pub enum LockEffect {
    Acquire(HwDeviceId),
    Release,
    StopPlayback,
    ForgetReacquire,
}

/// Строка таблицы «любое (Совм.) `ModeSelected(Optimal/Strict)` → `Acquiring`,
/// `Acquire(dev)`» и «`Open`/`Failed` + `Startup`/`ModeSelected(E)`/`UserClose`/
/// `CaptureAndPlay`/`RetryCapture` → `Acquiring`, `Acquire(dev)`» (§6.19): обе
/// используют уже разрешённое движком устройство `dev` (ADR-24), поэтому
/// `dev = None` даёт `Failed(NoDeviceSelected)` без эффектов (ТЗ-144).
fn acquire_or_fail(dev: Option<&HwDeviceId>) -> (LockState, SmallVec<[LockEffect; 2]>) {
    match dev {
        Some(d) => (
            LockState::Acquiring { started: ClockInstant::START },
            smallvec![LockEffect::Acquire(d.clone())],
        ),
        None => (LockState::Failed(CaptureFailure::NoDeviceSelected), SmallVec::new()),
    }
}

/// Чистая функция перехода замка устройства режима (§2.6, §6.19). `dev` —
/// устройство режима после разрешения движком (ADR-24): выбранное в настройках
/// или найденное автовыбором; `None` — выбрать не удалось (ТЗ-144).
///
/// Функция тотальна: для пар «состояние × вход», не упомянутых в таблице §6.19,
/// возвращается неизменное состояние без эффектов («для прочих сочетаний
/// состояние не меняется и эффектов нет»).
///
/// Открытые решения реализации (консервативный выбор там, где таблица не
/// уточняет конкретное устройство):
/// - `Acquiring.started` ставится в `ClockInstant::START` — таблица §6.19 не
///   передаёт в `lock_step` инжектируемые часы (ОВС-10 п. 7 требует их только
///   для отсчёта тайм-аута захвата в движке, не в этой чистой функции);
///   настоящее время захвата должен выставить вызывающий код движка при
///   обработке эффекта `Acquire`.
/// - `DeviceReappeared(d)` переводит `Failed(DeviceDisconnected)` в `Acquiring`
///   с `Acquire(d)` без сверки `d` с устройством сохранённой `ReacquirePoint`:
///   по §6.20 эту проверку выполняет движок до вызова `lock_step`.
pub fn lock_step(
    state: &LockState,
    input: LockInput,
    mode: ModeKind,
    dev: Option<&HwDeviceId>,
) -> (LockState, SmallVec<[LockEffect; 2]>) {
    // В Совместимом режиме любой вход, кроме выбора Exclusive-режима, даёт
    // `NotApplicable` без эффектов, независимо от текущего состояния (ТЗ-50).
    if mode == ModeKind::Compatible {
        return match input {
            LockInput::ModeSelected(ModeKind::Optimal) | LockInput::ModeSelected(ModeKind::Strict) => {
                acquire_or_fail(dev)
            }
            _ => (LockState::NotApplicable, SmallVec::new()),
        };
    }

    // Далее mode ∈ {Optimal, Strict} (Exclusive).
    match input {
        LockInput::Shutdown => {
            (LockState::Open, smallvec![LockEffect::StopPlayback, LockEffect::Release])
        }
        LockInput::ModeSelected(ModeKind::Compatible) => match state {
            LockState::Held { .. } | LockState::Acquiring { .. } | LockState::Failed(_) => (
                LockState::NotApplicable,
                smallvec![LockEffect::StopPlayback, LockEffect::Release, LockEffect::ForgetReacquire],
            ),
            LockState::Open | LockState::NotApplicable => (state.clone(), SmallVec::new()),
        },
        LockInput::Startup
        | LockInput::ModeSelected(ModeKind::Optimal)
        | LockInput::ModeSelected(ModeKind::Strict)
        | LockInput::UserClose
        | LockInput::CaptureAndPlay
        | LockInput::RetryCapture => match state {
            LockState::Open | LockState::Failed(_) => acquire_or_fail(dev),
            _ => (state.clone(), SmallVec::new()),
        },
        LockInput::DeviceSelected(d_prime) => match state {
            LockState::Held { .. } => (
                LockState::Acquiring { started: ClockInstant::START },
                smallvec![LockEffect::StopPlayback, LockEffect::Release, LockEffect::Acquire(d_prime)],
            ),
            LockState::Open | LockState::Failed(_) => (
                LockState::Acquiring { started: ClockInstant::START },
                smallvec![LockEffect::Acquire(d_prime)],
            ),
            LockState::NotApplicable | LockState::Acquiring { .. } => (state.clone(), SmallVec::new()),
        },
        LockInput::CaptureResult(Ok(info)) => match state {
            LockState::Acquiring { .. } => (LockState::Held { info }, SmallVec::new()),
            _ => (state.clone(), SmallVec::new()),
        },
        LockInput::CaptureResult(Err(failure)) => match state {
            LockState::Acquiring { .. } => (LockState::Failed(failure), SmallVec::new()),
            _ => (state.clone(), SmallVec::new()),
        },
        LockInput::UserOpen => match state {
            LockState::Held { .. } => (
                LockState::Open,
                smallvec![LockEffect::StopPlayback, LockEffect::Release, LockEffect::ForgetReacquire],
            ),
            LockState::Failed(CaptureFailure::DeviceDisconnected) => {
                (LockState::Open, smallvec![LockEffect::ForgetReacquire])
            }
            _ => (state.clone(), SmallVec::new()),
        },
        LockInput::DeviceLost => match state {
            LockState::Held { .. } => (
                LockState::Failed(CaptureFailure::DeviceDisconnected),
                smallvec![LockEffect::StopPlayback, LockEffect::Release],
            ),
            _ => (state.clone(), SmallVec::new()),
        },
        LockInput::DeviceReappeared(d) => match state {
            LockState::Failed(CaptureFailure::DeviceDisconnected) => (
                LockState::Acquiring { started: ClockInstant::START },
                smallvec![LockEffect::Acquire(d)],
            ),
            _ => (state.clone(), SmallVec::new()),
        },
    }
}
