//! Открытие Эксклюзивного пути: захват → `configure` → план сессии → рендер
//! и источник декодера → поток декодера → поток устройства → снэпшот пути
//! (§6.18 п. 1–7, ТЗ-48, ТЗ-50, ТЗ-113).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;

use crate::audio::backend::exclusive::{ConfigureError, DeviceClaim, ExclusiveBackend, StartError, StreamHandle};
use crate::audio::backend::{AccessKind, DeviceBufferActual, DriverFormat, OutputApi};
use crate::audio::decoder::AudioSource;
use crate::audio::error::StreamFault;
use crate::audio::format::StreamFormat;
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
/// (§6.18 п. 5–6).
pub(crate) struct ExclusiveSession {
    worker: DecodeWorker,
    shared: Arc<SessionShared>,
    // читается реакцией на `DecodeEvent::Failed` после старта — следующий шаг
    #[allow(dead_code)]
    decode_events: mpsc::Receiver<DecodeEvent>,
}

/// Конфигурация PCM, установленная последним `configure` (§6.7, §6.18 шаг
/// 5): смена трека на той же конфигурации пропускает повторный `configure`
/// (ТЗ-43, И-Р12), сохраняя значения `driver`/`buffer_actual`, с которыми
/// уже открыт путь.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ConfiguredFormat {
    target: StreamFormat,
    device_buffer: DeviceBuffer,
    driver: DriverFormat,
    buffer_actual: DeviceBufferActual,
}

/// Открытый Эксклюзивный путь: удержанный захват устройства, сессия
/// декодера и поток вывода (§6.18, ТЗ-48). `claim` не теряется ни на одном
/// шаге открытия — при отказе он возвращается в `ExclusiveOpenError`, чтобы
/// замок устройства оставался удержан (§6.19).
pub(crate) struct ExclusivePath {
    claim: DeviceClaim,
    session: ExclusiveSession,
    handle: Box<dyn StreamHandle>,
    /// Частота кадров ring (= частота цели устройства, §6.16, ТЗ-99): секунды
    /// → кадры для `seek` считает вызывающая сторона (`run.rs::secs_to_frames`).
    frame_rate: u32,
    /// Факты открытого потока (§6.18 шаг 50, ТЗ-137): переиспользуются
    /// снимком `ActivePath` Эксклюзивного пути (`run.rs::open_exclusive`,
    /// `rebuild_with_gain`) без повторного обращения к устройству.
    facts: OpenedFacts,
    /// Конфигурация PCM, с которой открыт путь (§6.7, §6.18 шаг 5):
    /// передаётся следующему `open()` как `prev_configured`, чтобы смена
    /// трека на той же конфигурации не повторяла `configure`.
    configured: ConfiguredFormat,
}

impl ExclusivePath {
    /// Возобновить вывод: колбэк перестаёт писать тишину, поток и устройство
    /// остаются открытыми (§6.18, ТЗ-49).
    pub(crate) fn play(&self) {
        self.session.shared.playing.store(true, Ordering::Release);
    }

    /// Поставить на паузу: колбэк пишет тишину вместо ring, поток вывода и
    /// устройство не останавливаются (§6.18, ТЗ-49).
    pub(crate) fn pause(&self) {
        self.session.shared.playing.store(false, Ordering::Release);
    }

    /// Текущее состояние паузы для транспорта (§6.18, ТЗ-49).
    pub(crate) fn is_playing(&self) -> bool {
        self.session.shared.playing.load(Ordering::Acquire)
    }

    /// Запросить seek на Эксклюзивном пути (§6.16, ТЗ-99, ADR-13): фаза 1
    /// рукопожатия — установка целевого кадра и инкремент поколения запроса
    /// (`SessionShared::request_seek`). Фазу 2 (стоп декодера на старом
    /// поколении) выполняет поток декодера (`DecodeWorker::poll_seek`), фазу
    /// 3 (O(1) сброс ring, `pos_frames = target`, повторное заполнение) —
    /// колбэк рендера (`render::begin`). Поток вывода и устройство остаются
    /// открытыми; работает и на паузе (`SessionShared::playing` не меняется).
    pub(crate) fn seek(&self, target_frame: u64) {
        self.session.shared.request_seek(target_frame);
    }

    /// Частота кадров ring (= частота цели устройства, §6.16, ТЗ-99): для
    /// перевода секунд в кадры вызывающей стороной (`run.rs::secs_to_frames`).
    pub(crate) fn frame_rate(&self) -> u32 {
        self.frame_rate
    }

    /// TID потока вывода для запроса RT-приоритета у `rtkit` (§6.8 п. 2,
    /// ТЗ-99, ТЗ-76): `None`, если бэкенд его не публикует.
    pub(crate) fn rt_tid(&self) -> Option<u32> {
        self.handle.rt_tid()
    }

    /// Факты открытого потока (§6.18 шаг 50, ТЗ-137) для снимка `ActivePath`
    /// Эксклюзивного пути без повторного обращения к устройству.
    pub(crate) fn facts(&self) -> &OpenedFacts {
        &self.facts
    }

    /// Конфигурация PCM, с которой открыт путь (§6.7, §6.18 шаг 5): передаётся
    /// следующему `open()` как `prev_configured` при смене трека без закрытия
    /// замка устройства.
    pub(crate) fn configured(&self) -> ConfiguredFormat {
        self.configured
    }

    /// Применяет громкость/mute к разделяемому состоянию сессии (§6.18,
    /// ТЗ-137, ТЗ-140, ОВС-18, ADR-23). Mute гасит рендер любого вида:
    /// ворота тишины `NoGain`/`DopRender` читают `muted` напрямую, минуя
    /// ступень усиления. Громкость читает только ступень `AtomicGain`
    /// (только `OptimalPcm`) — виды рендера без ступени усиления (`StrictPcm`,
    /// `OptimalPcmLocked`, `OptimalDop`, `StrictDop`) её игнорируют.
    pub(crate) fn set_gain(&self, gain: f32, muted: bool) {
        self.session.shared.set_gain(gain);
        self.session.shared.muted.store(muted, Ordering::Release);
    }

    /// Включает/выключает публикацию кадров в общий `VizTap` (§6.18):
    /// аналог `Player::set_viz_tap_active` для Эксклюзивного пути. Декодер
    /// проверяет этот флаг в своём потоке (`DecodeLoop::mirror_tap`), не в
    /// RT-колбэке вывода.
    pub(crate) fn set_viz_tap_active(&self, active: bool) {
        self.session.shared.viz_tap_active.store(active, Ordering::Relaxed);
    }

    /// Доступ к разделяемому состоянию сессии для тестов (§6.16, ТЗ-99):
    /// позволяет проверить фазу 1 рукопожатия seek без реального потока
    /// вывода.
    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Arc<SessionShared> {
        &self.session.shared
    }

    /// Позиция воспроизведения в секундах (§6.17, ТЗ-51): кадры ring,
    /// пересчитанные по частоте цели устройства (аналог
    /// `Player::resume_pos_secs`).
    pub(crate) fn position_secs(&self) -> f64 {
        self.session.shared.pos_frames.load(Ordering::Relaxed) as f64 / f64::from(self.frame_rate.max(1))
    }

    /// Конец трека: декодер дошёл до EOF и seek не в процессе (§6.17, ТЗ-51)
    /// — аналог `Player::at_end`. Без флага подтверждения — приёмная сторона
    /// (`run.rs::poll_session`) сразу закрывает путь при обнаружении.
    pub(crate) fn ended(&self) -> bool {
        self.session.shared.ended.load(Ordering::Acquire) && !self.session.shared.seek_pending()
    }

    /// Код отказа потока вывода (§6.20, ТЗ-51, ТЗ-76): `StreamFault::None`,
    /// пока поток работает штатно.
    pub(crate) fn fault(&self) -> StreamFault {
        self.session.shared.fault()
    }

    /// Нехватка данных в ring с начала сессии (§6.17, ТЗ-76).
    pub(crate) fn underruns(&self) -> u32 {
        self.session.shared.underruns.load(Ordering::Relaxed)
    }

    /// EPIPE потока вывода с начала сессии (§6.20, ТЗ-76).
    pub(crate) fn xruns(&self) -> u32 {
        self.session.shared.xruns.load(Ordering::Relaxed)
    }

    /// Закрыть путь (§6.18, ТЗ-43): сначала поток вывода — колбэк
    /// перестаёт читать ring и возвращает PCM в `claim`, затем поток
    /// декодера. Захват возвращается вызывающей стороне: замок устройства
    /// остаётся удержан (§6.19), закрыть его — отдельное решение.
    pub(crate) fn close(self) -> DeviceClaim {
        let ExclusivePath { mut claim, mut session, handle, frame_rate: _, facts: _, configured: _ } = self;
        *claim.pcm_mut() = handle.stop();
        session.worker.stop();
        claim
    }
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
        use crate::i18n::errors as i18n_errors;
        match self {
            ExclusiveOpenReason::Configure(e) => write!(f, "configure: {}", e.0),
            ExclusiveOpenReason::BufferUnavailable => {
                write!(f, "{}", i18n_errors::EXCLUSIVE_BUFFER_UNAVAILABLE)
            }
            ExclusiveOpenReason::Plan(e) => write!(f, "{}", i18n_errors::exclusive_plan_failed_text(&format!("{e:?}"))),
            ExclusiveOpenReason::Render(e) => write!(f, "{}", i18n_errors::exclusive_render_failed_text(&e.to_string())),
            ExclusiveOpenReason::Feed(e) => write!(f, "{}", i18n_errors::exclusive_feed_failed_text(&e.to_string())),
            ExclusiveOpenReason::Decode(SpawnDecoderError::Spawn(e)) => {
                write!(f, "{}", i18n_errors::exclusive_decode_spawn_failed_text(&format!("{e:?}")))
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
/// 1. `configure` — только если `plan.target`/`device_buffer` отличаются от
///    `prev_configured` (§6.7, §6.18 шаг 5, ТЗ-43, И-Р12): смена трека на той
///    же конфигурации PCM переиспользует `driver`/`buffer_actual` предыдущего
///    открытия без повторного `hw_params`. `prev_configured` — `None` при
///    первом открытии после `acquire` (`ClaimInfo` формат потока не хранит).
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
    prev_configured: Option<ConfiguredFormat>,
) -> Result<(ExclusivePath, SignalPath), ExclusiveOpenError> {
    let frame_rate = plan.target.rate.hz();
    let reuse = prev_configured
        .filter(|prev| prev.target == plan.target && prev.device_buffer == device_buffer);
    let (driver, buffer, configured) = match reuse {
        Some(prev) => (prev.driver, prev.buffer_actual, prev),
        None => {
            let (driver, buffer_actual) = match backend.configure(&mut claim, plan.target, device_buffer) {
                Ok(ok) => ok,
                Err(e) => return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::Configure(e) }),
            };
            let Some(buffer) = buffer_actual else {
                return Err(ExclusiveOpenError { claim, reason: ExclusiveOpenReason::BufferUnavailable });
            };
            let configured = ConfiguredFormat { target: plan.target, device_buffer, driver, buffer_actual: buffer };
            (driver, buffer, configured)
        }
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
    let path = ExclusivePath { claim, session, handle, frame_rate, facts: facts.clone(), configured };
    let signal = SignalPath::from_opened(plan, facts, generation);
    Ok((path, signal))
}
