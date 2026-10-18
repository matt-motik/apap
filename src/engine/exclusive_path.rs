//! Открытие Эксклюзивного пути: захват → `configure` → план сессии → рендер
//! и источник декодера → поток декодера → поток устройства → снэпшот пути
//! (§6.18 п. 1–7, ТЗ-48, ТЗ-50, ТЗ-113).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::sync::mpsc;
use std::sync::Arc;

use crate::audio::backend::exclusive::{ConfigureError, DeviceClaim, ExclusiveBackend, StartError, StreamHandle};
use crate::audio::backend::{AccessKind, OutputApi};
use crate::audio::decoder::AudioSource;
use crate::audio::path::signal::SignalPath;
use crate::audio::path::{OpenedFacts, OsStage, PathPlan, ReservationView};
use crate::audio::session::SessionShared;
use crate::audio::session_build::{
    build_exclusive_render, exclusive_feed, exclusive_session_plan, spawn_decoder,
    ExclusiveFeedError, ExclusivePlanError, ExclusiveRenderError, SpawnDecoderError,
};
use crate::audio::worker::{DecodeEvent, DecodeWorker, VizTap};
use crate::engine::spawner::ThreadSpawner;
use crate::settings::playback::DeviceBuffer;

/// Поток декодера и общее состояние сессии, открытой на Эксклюзивном пути
/// (§6.18 п. 5–6). Приёмник `decode_events` держится, чтобы вызывающая
/// сторона могла реагировать на `DecodeEvent::Failed` после старта.
// поля читаются опросом/остановкой сессии — следующие шаги С6
#[allow(dead_code)]
pub(crate) struct ExclusiveSession {
    worker: DecodeWorker,
    shared: Arc<SessionShared>,
    decode_events: mpsc::Receiver<DecodeEvent>,
}

/// Открытый Эксклюзивный путь: удержанный захват устройства, сессия
/// декодера и поток вывода (§6.18, ТЗ-48). `claim` не теряется ни на одном
/// шаге открытия — при отказе он возвращается в `ExclusiveOpenError`, чтобы
/// замок устройства оставался удержан (§6.19).
// поля читаются остановкой/переоткрытием сессии — следующие шаги С6
#[allow(dead_code)]
pub(crate) struct ExclusivePath {
    claim: DeviceClaim,
    session: ExclusiveSession,
    handle: Box<dyn StreamHandle>,
}

/// Причина отказа открытия Эксклюзивного пути (§6.18). Без `Debug`/`Clone`:
/// оборачивает `SpawnDecoderError`, у которого таких реализаций нет (как и у
/// `DeviceClaim`, владеющего резервированием).
pub(crate) enum ExclusiveOpenReason {
    /// `ExclusiveBackend::configure` не смог установить hw_params (ТЗ-113).
    Configure(ConfigureError),
    /// `configure` не вернул фактический буфер/период устройства (ТЗ-131):
    /// без них план сессии и период потока вывода не построить.
    BufferUnavailable,
    /// Эксклюзивный план ring/рендера не построен (§6.18 п. 2).
    Plan(ExclusivePlanError),
    /// Рендер/ring не построены под фактический формат устройства (§6.18 п. 3).
    Render(ExclusiveRenderError),
    /// Источник декодера не совпадает с планом сессии (§6.18 п. 4).
    Feed(ExclusiveFeedError),
    /// Поток декодера не запущен или не заполнил стартовый ring (§6.18 п. 5).
    Decode(SpawnDecoderError),
    /// `ExclusiveBackend::start` не смог поднять поток вывода (§6.18 п. 7).
    Start(StartError),
}

impl std::fmt::Display for ExclusiveOpenReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExclusiveOpenReason::Configure(e) => write!(f, "configure: {}", e.0),
            ExclusiveOpenReason::BufferUnavailable => {
                write!(f, "configure не вернул фактический буфер устройства (ТЗ-131)")
            }
            ExclusiveOpenReason::Plan(e) => write!(f, "план сессии: {e:?}"),
            ExclusiveOpenReason::Render(e) => write!(f, "рендер: {e}"),
            ExclusiveOpenReason::Feed(e) => write!(f, "источник декодера: {e}"),
            ExclusiveOpenReason::Decode(SpawnDecoderError::Spawn(e)) => {
                write!(f, "поток декодера: {e:?}")
            }
            ExclusiveOpenReason::Decode(SpawnDecoderError::Open(msg)) => write!(f, "{msg}"),
            ExclusiveOpenReason::Start(e) => write!(f, "start: {}", e.0),
        }
    }
}

/// Отказ открытия Эксклюзивного пути с возвращённым захватом (§6.19):
/// замок устройства остаётся удержан, вызывающая сторона решает, закрыть
/// его или повторить открытие с другим планом.
pub(crate) struct ExclusiveOpenError {
    pub(crate) claim: DeviceClaim,
    // читается более точной классификацией `OpenError` — следующий шаг
    #[allow(dead_code)]
    pub(crate) reason: ExclusiveOpenReason,
}

/// Открытие Эксклюзивного пути по уже построенному плану и уже открытому
/// источнику (§6.18 п. 1–7, ТЗ-48, ТЗ-50, ТЗ-113):
///
/// 1. `configure` — безусловно: `ClaimInfo` не хранит формат потока, поэтому
///    пропустить этот шаг нет возможности.
/// 2. `exclusive_session_plan` — план ring/рендера по фактическому буферу.
/// 3. `build_exclusive_render` + `exclusive_feed` — рендер, ring и источник
///    декодера под этот план.
/// 4. `spawn_decoder` — поток декодера, ждёт стартовое заполнение ring.
/// 5. `ExclusiveBackend::start` — поток вывода.
/// 6. `OpenedFacts` → `SignalPath::from_opened` — снэпшот пути для UI.
///
/// `driver` из `configure` переносится в `OpenedFacts` как есть:
/// `DriverFormat::Unreadable` — легитимный исход (ОВ-14), не причина отказа.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open(
    backend: &mut dyn ExclusiveBackend,
    mut claim: DeviceClaim,
    plan: PathPlan,
    source: Box<dyn AudioSource>,
    spawner: &dyn ThreadSpawner,
    tap: VizTap,
    device_buffer: DeviceBuffer,
    ring_buffer_ms: u32,
    volume_lock: bool,
    dither_seed: Option<u32>,
    start_frame: u64,
    generation: u64,
) -> Result<(ExclusivePath, SignalPath), ExclusiveOpenError> {
    let (driver, buffer_actual) = match backend.configure(&mut claim, plan.target, device_buffer) {
        Ok(ok) => ok,
        Err(e) => return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::Configure(e) }),
    };
    let Some(buffer) = buffer_actual else {
        return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::BufferUnavailable });
    };

    let session_plan = match exclusive_session_plan(&plan, buffer, ring_buffer_ms, volume_lock, dither_seed) {
        Ok(p) => p,
        Err(e) => return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::Plan(e) }),
    };

    let shared = Arc::new(SessionShared::new());

    let (render, ring) = match build_exclusive_render(&session_plan, plan.target.sample, &shared) {
        Ok(r) => r,
        Err(e) => return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::Render(e) }),
    };

    let feed = match exclusive_feed(&plan, &session_plan, source) {
        Ok(f) => f,
        Err(e) => return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::Feed(e) }),
    };

    let cap_frames = session_plan.ring_capacity.checked_div(session_plan.engine.channels.max(1)).unwrap_or(0);
    let (worker, decode_events) =
        match spawn_decoder(spawner, tap, feed, ring, &session_plan.engine, &shared, cap_frames, start_frame) {
            Ok(w) => w,
            Err(e) => return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::Decode(e) }),
        };

    let handle = match backend.start(&mut claim, render, plan.target, buffer.period_frames, Arc::clone(&shared)) {
        Ok(h) => h,
        Err(e) => return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::Start(e) }),
    };

    let facts = OpenedFacts {
        access: AccessKind::Exclusive,
        api: OutputApi::AlsaHw,
        device_name: claim.info().device.to_string(),
        driver,
        reservation: ReservationView::Held,
        os: OsStage::NotInvolved,
        device_buffer: Some(buffer),
    };

    let session = ExclusiveSession { worker, shared, decode_events };
    let path = ExclusivePath { claim, session, handle };
    let signal = SignalPath::from_opened(plan, facts, generation);
    Ok((path, signal))
}
