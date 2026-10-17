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

#[cfg(test)]
mod tests {
    use super::*;

    /// Устройство режима, используемое как `dev` (разрешённое движком, ADR-24).
    fn dev_a() -> HwDeviceId {
        "hw:CARD=X,DEV=0".parse().expect("валидный HwDeviceId")
    }

    /// Второе устройство — для входов, несущих собственное устройство
    /// (`DeviceSelected`, `DeviceReappeared`, `CaptureResult`).
    fn dev_b() -> HwDeviceId {
        "hw:CARD=Y,DEV=0".parse().expect("валидный HwDeviceId")
    }

    /// Представительный набор состояний замка, включая несколько вариантов
    /// `CaptureFailure` (§2.6).
    fn state_samples() -> Vec<LockState> {
        vec![
            LockState::NotApplicable,
            LockState::Open,
            LockState::Acquiring { started: ClockInstant::START },
            LockState::Held { info: ClaimInfo { device: dev_a() } },
            LockState::Failed(CaptureFailure::DeviceDisconnected),
            LockState::Failed(CaptureFailure::NoDeviceSelected),
            LockState::Failed(CaptureFailure::ReservationDenied { owner: None }),
            LockState::Failed(CaptureFailure::OwnerNotResponding),
            LockState::Failed(CaptureFailure::NoSessionBus),
            LockState::Failed(CaptureFailure::BusyOutsideProtocol),
            LockState::Failed(CaptureFailure::DriverRefused { errno: 0 }),
            LockState::Failed(CaptureFailure::LostOnReopen),
            LockState::Failed(CaptureFailure::ReservationLost),
            LockState::Failed(CaptureFailure::PlatformUnsupported),
        ]
    }

    /// Представительный набор входов, по одному значению на вариант `LockInput`.
    fn input_samples() -> Vec<LockInput> {
        vec![
            LockInput::Startup,
            LockInput::ModeSelected(ModeKind::Compatible),
            LockInput::ModeSelected(ModeKind::Optimal),
            LockInput::ModeSelected(ModeKind::Strict),
            LockInput::DeviceSelected(dev_b()),
            LockInput::UserClose,
            LockInput::UserOpen,
            LockInput::CaptureAndPlay,
            LockInput::DeviceReappeared(dev_b()),
            LockInput::RetryCapture,
            LockInput::CaptureResult(Ok(ClaimInfo { device: dev_b() })),
            LockInput::CaptureResult(Err(CaptureFailure::DeviceDisconnected)),
            LockInput::DeviceLost,
            LockInput::Shutdown,
        ]
    }

    /// Тест §7.2 `lock_step_is_total`: перебирает все пары «состояние × вход ×
    /// режим» (с `dev` и `None`, и `Some`) и проверяет, что функция не паникует
    /// и возвращает не более 3 эффектов (ТЗ-42…46, 50, 125, 144, И-Т17). Предел —
    /// 3, а не ёмкость `SmallVec<[LockEffect; 2]>`: строки таблицы §6.19 с
    /// `ModeSelected(Compatible)`/`DeviceSelected`/`UserOpen` из `Held` дают
    /// `StopPlayback` + `Release` + `ForgetReacquire`/`Acquire` — тройку эффектов,
    /// обслуживаемую переполнением `SmallVec` на кучу, а не инлайн-ёмкостью.
    #[test]
    fn lock_step_is_total() {
        let devices = [None, Some(dev_a())];
        for state in &state_samples() {
            for input in &input_samples() {
                for &mode in &[ModeKind::Compatible, ModeKind::Optimal, ModeKind::Strict] {
                    for dev in &devices {
                        let (_, effects) = lock_step(state, input.clone(), mode, dev.as_ref());
                        assert!(
                            effects.len() <= 3,
                            "больше 3 эффектов: state={state:?} input={input:?} mode={mode:?} dev={dev:?}"
                        );
                    }
                }
            }
        }
    }

    /// Совместимый режим: любой вход, кроме `ModeSelected(Optimal/Strict)`,
    /// даёт `NotApplicable` без эффектов независимо от состояния (ТЗ-50).
    #[test]
    fn compatible_mode_gives_not_applicable() {
        let inputs = [
            LockInput::Startup,
            LockInput::ModeSelected(ModeKind::Compatible),
            LockInput::DeviceSelected(dev_b()),
            LockInput::UserClose,
            LockInput::UserOpen,
            LockInput::CaptureAndPlay,
            LockInput::DeviceReappeared(dev_b()),
            LockInput::RetryCapture,
            LockInput::CaptureResult(Ok(ClaimInfo { device: dev_b() })),
            LockInput::CaptureResult(Err(CaptureFailure::DeviceDisconnected)),
            LockInput::DeviceLost,
            LockInput::Shutdown,
        ];
        for state in &state_samples() {
            for input in &inputs {
                let (new_state, effects) = lock_step(state, input.clone(), ModeKind::Compatible, Some(&dev_a()));
                assert_eq!(new_state, LockState::NotApplicable, "input={input:?} state={state:?}");
                assert!(effects.is_empty(), "input={input:?} state={state:?}");
            }
        }
    }

    /// `no_device_selected` (ТЗ-144): входы, которые по таблице §6.19 ведут к
    /// захвату через `Acquire(dev)`, при `dev = None` дают `Failed(NoDeviceSelected)`
    /// без эффектов — устройство режима не разрешено движком.
    #[test]
    fn no_device_selected_without_device_fails() {
        // Строка «любое (Совм.) + ModeSelected(Optimal/Strict)».
        for state in &state_samples() {
            for mode in [ModeKind::Optimal, ModeKind::Strict] {
                let (new_state, effects) = lock_step(state, LockInput::ModeSelected(mode), ModeKind::Compatible, None);
                assert_eq!(new_state, LockState::Failed(CaptureFailure::NoDeviceSelected));
                assert!(effects.is_empty());
            }
        }

        // Строка «Open/Failed + Startup/ModeSelected(E)/UserClose/CaptureAndPlay/RetryCapture».
        let inputs = [
            LockInput::Startup,
            LockInput::ModeSelected(ModeKind::Strict),
            LockInput::UserClose,
            LockInput::CaptureAndPlay,
            LockInput::RetryCapture,
        ];
        let states = [LockState::Open, LockState::Failed(CaptureFailure::DeviceDisconnected)];
        for mode in [ModeKind::Optimal, ModeKind::Strict] {
            for state in &states {
                for input in &inputs {
                    let (new_state, effects) = lock_step(state, input.clone(), mode, None);
                    assert_eq!(new_state, LockState::Failed(CaptureFailure::NoDeviceSelected), "input={input:?} state={state:?} mode={mode:?}");
                    assert!(effects.is_empty(), "input={input:?} state={state:?} mode={mode:?}");
                }
            }
        }
    }

    /// `no_device_selected` (ТЗ-144): те же входы при разрешённом `dev = Some(d)`
    /// захватывают устройство как обычно — `Acquiring` + `Acquire(d)`.
    #[test]
    fn no_device_selected_with_device_acquires() {
        for state in &state_samples() {
            for mode in [ModeKind::Optimal, ModeKind::Strict] {
                let (new_state, effects) =
                    lock_step(state, LockInput::ModeSelected(mode), ModeKind::Compatible, Some(&dev_a()));
                assert_eq!(new_state, LockState::Acquiring { started: ClockInstant::START });
                assert_eq!(effects.as_slice(), [LockEffect::Acquire(dev_a())]);
            }
        }

        let inputs = [
            LockInput::Startup,
            LockInput::ModeSelected(ModeKind::Strict),
            LockInput::UserClose,
            LockInput::CaptureAndPlay,
            LockInput::RetryCapture,
        ];
        let states = [LockState::Open, LockState::Failed(CaptureFailure::DeviceDisconnected)];
        for mode in [ModeKind::Optimal, ModeKind::Strict] {
            for state in &states {
                for input in &inputs {
                    let (new_state, effects) = lock_step(state, input.clone(), mode, Some(&dev_a()));
                    assert_eq!(new_state, LockState::Acquiring { started: ClockInstant::START }, "input={input:?} state={state:?} mode={mode:?}");
                    assert_eq!(effects.as_slice(), [LockEffect::Acquire(dev_a())], "input={input:?} state={state:?} mode={mode:?}");
                }
            }
        }
    }

    /// `UserClose` захватывает только из `Open`/`Failed` (§6.19) — перехода из
    /// `Held` в таблице нет, состояние не меняется (ТЗ-43). `UserOpen` в
    /// таблице определён только для `Held` (→ `Open` + `StopPlayback`,
    /// `Release`, `ForgetReacquire`, ТЗ-44) и для `Failed(DeviceDisconnected)`
    /// (→ `Open` + `ForgetReacquire`); из `Open` перехода `UserOpen` в таблице
    /// нет, состояние не меняется.
    #[test]
    fn user_close_from_held_and_user_open_table() {
        for mode in [ModeKind::Optimal, ModeKind::Strict] {
            let held = LockState::Held { info: ClaimInfo { device: dev_a() } };

            let (new_state, effects) = lock_step(&held, LockInput::UserClose, mode, Some(&dev_a()));
            assert_eq!(new_state, held, "mode={mode:?}");
            assert!(effects.is_empty(), "mode={mode:?}");

            let (new_state, effects) = lock_step(&held, LockInput::UserOpen, mode, Some(&dev_a()));
            assert_eq!(new_state, LockState::Open, "mode={mode:?}");
            assert_eq!(
                effects.as_slice(),
                [LockEffect::StopPlayback, LockEffect::Release, LockEffect::ForgetReacquire],
                "mode={mode:?}"
            );

            let failed_disconnected = LockState::Failed(CaptureFailure::DeviceDisconnected);
            let (new_state, effects) = lock_step(&failed_disconnected, LockInput::UserOpen, mode, Some(&dev_a()));
            assert_eq!(new_state, LockState::Open, "mode={mode:?}");
            assert_eq!(effects.as_slice(), [LockEffect::ForgetReacquire], "mode={mode:?}");

            let (new_state, effects) = lock_step(&LockState::Open, LockInput::UserOpen, mode, Some(&dev_a()));
            assert_eq!(new_state, LockState::Open, "mode={mode:?}");
            assert!(effects.is_empty(), "mode={mode:?}");
        }
    }

    /// `CaptureAndPlay` и `RetryCapture` входят в строку «`Open`/`Failed` +
    /// `Startup`/`ModeSelected(E)`/`UserClose`/`CaptureAndPlay`/`RetryCapture`
    /// → `Acquiring`, `Acquire(dev)`» (§6.19, ТЗ-42, ТЗ-125).
    #[test]
    fn capture_and_play_and_retry_capture_acquire() {
        let states = [LockState::Open, LockState::Failed(CaptureFailure::OwnerNotResponding)];
        let inputs = [LockInput::CaptureAndPlay, LockInput::RetryCapture];
        for mode in [ModeKind::Optimal, ModeKind::Strict] {
            for state in &states {
                for input in &inputs {
                    let (new_state, effects) = lock_step(state, input.clone(), mode, Some(&dev_a()));
                    assert_eq!(
                        new_state,
                        LockState::Acquiring { started: ClockInstant::START },
                        "input={input:?} state={state:?} mode={mode:?}"
                    );
                    assert_eq!(
                        effects.as_slice(),
                        [LockEffect::Acquire(dev_a())],
                        "input={input:?} state={state:?} mode={mode:?}"
                    );
                }
            }
        }
    }

    /// `DeviceLost` из `Held` → `Failed(DeviceDisconnected)` + `StopPlayback`,
    /// `Release` (§6.19, ТЗ-51); из прочих состояний перехода в таблице нет.
    #[test]
    fn device_lost_from_held_fails() {
        for mode in [ModeKind::Optimal, ModeKind::Strict] {
            let held = LockState::Held { info: ClaimInfo { device: dev_a() } };
            let (new_state, effects) = lock_step(&held, LockInput::DeviceLost, mode, Some(&dev_a()));
            assert_eq!(new_state, LockState::Failed(CaptureFailure::DeviceDisconnected), "mode={mode:?}");
            assert_eq!(effects.as_slice(), [LockEffect::StopPlayback, LockEffect::Release], "mode={mode:?}");

            let (new_state, effects) = lock_step(&LockState::Open, LockInput::DeviceLost, mode, Some(&dev_a()));
            assert_eq!(new_state, LockState::Open, "mode={mode:?}");
            assert!(effects.is_empty(), "mode={mode:?}");
        }
    }

    /// `DeviceReappeared(d)` захватывает только из `Failed(DeviceDisconnected)`
    /// (§6.19, ТЗ-126); из прочих `Failed`-вариантов перехода в таблице нет.
    #[test]
    fn device_reappeared_from_failed_disconnected_acquires() {
        for mode in [ModeKind::Optimal, ModeKind::Strict] {
            let failed_disconnected = LockState::Failed(CaptureFailure::DeviceDisconnected);
            let (new_state, effects) =
                lock_step(&failed_disconnected, LockInput::DeviceReappeared(dev_b()), mode, Some(&dev_a()));
            assert_eq!(new_state, LockState::Acquiring { started: ClockInstant::START }, "mode={mode:?}");
            assert_eq!(effects.as_slice(), [LockEffect::Acquire(dev_b())], "mode={mode:?}");

            let failed_other = LockState::Failed(CaptureFailure::NoDeviceSelected);
            let (new_state, effects) =
                lock_step(&failed_other, LockInput::DeviceReappeared(dev_b()), mode, Some(&dev_a()));
            assert_eq!(new_state, failed_other, "mode={mode:?}");
            assert!(effects.is_empty(), "mode={mode:?}");
        }
    }

    /// `CaptureResult(Ok(info))`/`CaptureResult(Err(f))` из `Acquiring` →
    /// `Held { info }` / `Failed(f)` без эффектов (§6.19).
    #[test]
    fn capture_result_resolves_acquiring() {
        for mode in [ModeKind::Optimal, ModeKind::Strict] {
            let acquiring = LockState::Acquiring { started: ClockInstant::START };

            let (new_state, effects) =
                lock_step(&acquiring, LockInput::CaptureResult(Ok(ClaimInfo { device: dev_b() })), mode, Some(&dev_a()));
            assert_eq!(new_state, LockState::Held { info: ClaimInfo { device: dev_b() } }, "mode={mode:?}");
            assert!(effects.is_empty(), "mode={mode:?}");

            let (new_state, effects) = lock_step(
                &acquiring,
                LockInput::CaptureResult(Err(CaptureFailure::OwnerNotResponding)),
                mode,
                Some(&dev_a()),
            );
            assert_eq!(new_state, LockState::Failed(CaptureFailure::OwnerNotResponding), "mode={mode:?}");
            assert!(effects.is_empty(), "mode={mode:?}");
        }
    }
}
