//! Автомат замка устройства режима (§2.6, ADR-20, ADR-24). Состояние —
//! `LockState`, переходы — `LockInput`, побочные действия для движка —
//! `LockEffect`. `ReacquirePoint` — точка, куда встать после автоматического
//! повторного захвата (ТЗ-126). Шаг `lock_step` реализуется отдельно.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

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
