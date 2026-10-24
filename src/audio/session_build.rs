//! Сборка сессии воспроизведения: план ring/рендера, ring, декодер (§6.18).

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use cpal::SampleFormat;

use super::backend::exclusive::{DopRenderAny, ExclusivePcm, ExclusiveRender, OptimalRender, StrictRender};
use super::backend::DeviceBufferActual;
use super::decoder::AudioSource;
use super::error::EngineFault;
use super::format::OutputSampleFormat;
use super::output::{dop_render_for, pcm_render_for, RawRender, Resampler, StreamBuildError};
use super::path::transform::TransformKind;
use super::path::{PathPlan, Route};
use super::render::dop::DopRender;
use super::render::gain::{AtomicGain, GainStage, NoGain};
use super::render::out::{S16Le, S24Le, S24_3Le, S32Le, F32Le};
use super::render::pcm::{PcmRender, PcmSample};
use super::render::tpdf::Tpdf;
use super::render::{prime_frames, RenderCore};
use super::session::{RingPayload, SessionShared};
use super::worker::{
    start_fill_frames, DecodeConfig, DecodeEvent, DecodeLoop, DecodeWorker, ExactFeed, FloatFeed,
    VizTap,
};
use crate::engine::spawner::ThreadSpawner;
use crate::settings::clamp_ring_buffer_ms;
use crate::settings::playback::{ModeKind, SrcFilter};
use crate::settings::ResamplerAlgorithm;

/// Ёмкость `PendingTail` в кадрах: один декодированный пакет (§6.18).
const DECODE_TAIL_FRAMES: usize = 8192;

/// Сколько ждать стартового заполнения ring до отказа открытия (§6.18).
const START_FILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Interleaved-sample capacity for the producer/consumer ring (ТЗ A2.0 §5.2).
///
/// Derived from the requested depth in milliseconds and the device geometry,
/// then floored at `max(4096, 2 cpal-buffer periods)` so the ring can never be
/// shorter than two output callbacks (which would guarantee underruns).
pub(crate) fn ring_capacity(
    out_rate: u32,
    out_ch: usize,
    ring_buffer_ms: u32,
    buffer_frames: Option<u32>,
) -> usize {
    let ch = out_ch.max(1);
    let ms = clamp_ring_buffer_ms(ring_buffer_ms) as u64;
    let samples = out_rate as u64 * ch as u64 * ms / 1000;
    let buffer_floor = buffer_frames
        .map(|f| f as usize * ch * 2)
        .unwrap_or(0);
    (samples as usize).max(buffer_floor).max(4096)
}

/// Тип ring сессии и вид рендера (§6.10, ADR-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RingKind {
    /// DoP-слова в i32-ring, маркеры ставит `DopRender` (§6.15).
    Dop,
    /// Точный PCM без ресемплинга в i32-ring.
    Exact(RingPayload),
    /// f32 после SRC / DSD→PCM / float-источника.
    Float,
}

/// Параметры сборки рендера, фиксируемые на время сессии (ОВС-18, ТЗ-8).
#[derive(Debug, Clone, Copy)]
pub(crate) struct EnginePlan {
    pub(crate) kind: RingKind,
    pub(crate) rate: u32,
    pub(crate) channels: usize,
    /// Bit-perfect: рендер без ступени громкости (`NoGain`, ОВС-18).
    pub(crate) no_gain: bool,
    /// Seed TPDF; `None` — дизеринг выключен.
    pub(crate) dither_seed: Option<u32>,
}

/// Вид рендера эксклюзивного потока (§2.9, ТЗ-140).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// используется ExclusivePath (шаг 44)
#[allow(dead_code)]
pub(crate) enum ExclusiveRenderKind {
    /// Оптимальный, PCM, ступень `AtomicGain`.
    OptimalPcm,
    /// Оптимальный с `volume_lock`: ступени громкости нет (ADR-23, ОВС-19).
    OptimalPcmLocked,
    /// Строгий: PCM без ступени громкости (И-Т6).
    StrictPcm,
    /// DoP в Оптимальном режиме: громкости нет (ADR-12).
    OptimalDop,
    /// DoP в Строгом режиме: громкости нет (ADR-12, И-Т6).
    StrictDop,
}

/// Отказ [`exclusive_session_plan`]: план не применим к эксклюзивному выводу
/// (§6.18 п. 5–6, ТЗ-137, ТЗ-140).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// используется ExclusivePath (шаг 44)
#[allow(dead_code)]
pub(crate) enum ExclusivePlanError {
    /// Совместимый режим не строит эксклюзивный план (ADR-04): у Shared нет
    /// `DeviceBufferActual`, вызывающая сторона не должна звать эту функцию
    /// для Совместимого режима.
    NotExclusive,
    /// Строгий режим бит-перфект: ring `Float` невозможен (И-Т6) — Строгий
    /// не ресемплирует и не переквантует после декодера.
    StrictFloatRing,
}

/// План сессии эксклюзивного вывода: ring/рендер из [`PathPlan`] и
/// фактического буфера устройства (§6.18 п. 5–6, ТЗ-137, ТЗ-140). Строится
/// чистой функцией [`exclusive_session_plan`] без I/O и без аллокации ring —
/// используется `ExclusivePath::open` (шаг 44) для сборки ring и
/// `ExclusiveRender`.
#[derive(Debug, Clone, Copy)]
// используется ExclusivePath (шаг 44)
#[allow(dead_code)]
pub(crate) struct ExclusiveSessionPlan {
    pub(crate) engine: EnginePlan,
    pub(crate) render: ExclusiveRenderKind,
    /// Ёмкость ring в interleaved-сэмплах ([`ring_capacity`]).
    pub(crate) ring_capacity: usize,
    /// Стартовый запас колбэка в кадрах ([`super::render::prime_frames`], §6.14).
    pub(crate) prime_frames: usize,
}

/// План сессии эксклюзивного вывода по плану пути `plan` и фактическому
/// буферу устройства `buffer` (§6.18 п. 5–6, ТЗ-137, ТЗ-140). Чистая функция:
/// не открывает устройство и не аллоцирует ring, только считает параметры.
///
/// `volume_lock` — опция Оптимального режима «фиксировать громкость на
/// 100 %» (ОВС-19); `dither_seed` — seed TPDF трека, `None` — дизеринг
/// выключен (ТЗ-8). Для маршрута DoP `dither_seed` игнорируется — громкости и
/// дизеринга у DoP нет (ADR-12).
///
/// `no_gain` повторяет правило `gain_stage` планировщика (§6.3, ADR-23):
/// Строгий и DoP — всегда без ступени громкости, Оптимальный — без ступени
/// только при `volume_lock` (ОВС-18, ОВС-19).
// используется ExclusivePath (шаг 44)
#[allow(dead_code)]
pub(crate) fn exclusive_session_plan(
    plan: &PathPlan,
    buffer: DeviceBufferActual,
    ring_buffer_ms: u32,
    volume_lock: bool,
    dither_seed: Option<u32>,
) -> Result<ExclusiveSessionPlan, ExclusivePlanError> {
    let is_dop = matches!(plan.route, Route::Dop(_));
    let kind = if is_dop {
        RingKind::Dop
    } else {
        match plan.ring {
            RingPayload::F32 => RingKind::Float,
            RingPayload::ExactI32 { .. } => RingKind::Exact(plan.ring),
        }
    };

    let (render, no_gain) = match (plan.mode, is_dop) {
        (ModeKind::Compatible, _) => return Err(ExclusivePlanError::NotExclusive),
        (ModeKind::Optimal, true) => (ExclusiveRenderKind::OptimalDop, true),
        (ModeKind::Strict, true) => (ExclusiveRenderKind::StrictDop, true),
        (ModeKind::Strict, false) => {
            if kind == RingKind::Float {
                return Err(ExclusivePlanError::StrictFloatRing);
            }
            (ExclusiveRenderKind::StrictPcm, true)
        }
        (ModeKind::Optimal, false) if volume_lock => (ExclusiveRenderKind::OptimalPcmLocked, true),
        (ModeKind::Optimal, false) => (ExclusiveRenderKind::OptimalPcm, false),
    };

    let rate = plan.target.rate.hz();
    let channels = usize::from(plan.target.channels.get());
    let dither_seed = if is_dop { None } else { dither_seed };

    let samples = ring_capacity(rate, channels, ring_buffer_ms, Some(buffer.buffer_frames));
    let cap_frames = samples.checked_div(channels.max(1)).unwrap_or(0);
    let period_frames = usize::try_from(buffer.period_frames).unwrap_or(0);
    let prime = prime_frames(period_frames, rate, cap_frames);

    Ok(ExclusiveSessionPlan {
        engine: EnginePlan { kind, rate, channels, no_gain, dither_seed },
        render,
        ring_capacity: samples,
        prime_frames: prime,
    })
}

/// Producer ring сессии, отдаваемый декодеру.
pub(crate) enum EngineRing {
    I32(rtrb::Producer<i32>),
    F32(rtrb::Producer<f32>),
}

/// Источник для `apap-decode` по типу ring.
pub(crate) enum EngineFeed {
    Exact(ExactFeed),
    Float(FloatFeed),
}

/// PCM-рендер с выбранной ступенью усиления (`NoGain` в bit-perfect, ОВС-18).
fn pcm_render<P: PcmSample>(
    format: SampleFormat,
    core: RenderCore<P>,
    payload: RingPayload,
    tpdf: Tpdf,
    no_gain: bool,
) -> Option<Box<dyn RawRender>> {
    if no_gain {
        pcm_render_for::<P, NoGain>(format, core, payload, tpdf)
    } else {
        pcm_render_for::<P, AtomicGain>(format, core, payload, tpdf)
    }
}

/// Ring ёмкостью `capacity` сэмплов и рендер над его consumer (§6.14, §6.15).
/// Формат устройства без рендера для этого вида ring — отказ сборки.
pub(crate) fn build_render(
    plan: &EnginePlan,
    format: SampleFormat,
    shared: &Arc<SessionShared>,
    capacity: usize,
    prime: usize,
) -> Result<(Box<dyn RawRender>, EngineRing), StreamBuildError> {
    let tpdf = plan.dither_seed.map_or_else(Tpdf::off, Tpdf::with_seed);
    let (render, ring) = match plan.kind {
        RingKind::Dop => {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.channels, prime);
            (dop_render_for(format, core), EngineRing::I32(producer))
        }
        RingKind::Exact(payload) => {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.channels, prime);
            (pcm_render(format, core, payload, tpdf, plan.no_gain), EngineRing::I32(producer))
        }
        RingKind::Float => {
            let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.channels, prime);
            (
                pcm_render(format, core, RingPayload::F32, tpdf, plan.no_gain),
                EngineRing::F32(producer),
            )
        }
    };
    let render = render.ok_or_else(|| {
        StreamBuildError::refused(format!("no renderer for {format:?} with {:?} ring", plan.kind))
    })?;
    Ok((render, ring))
}

/// Отказ [`build_exclusive_render`]: формат устройства не поддержан видом
/// рендера, либо вид ring плана не совместим с видом рендера (§2.9, §6.15,
/// §6.18 п. 4, ТЗ-140).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// используется ExclusivePath (шаг 44)
#[allow(dead_code)]
pub(crate) enum ExclusiveRenderError {
    /// Формат устройства не несёт этот вид рендера: DoP определён только для
    /// 24/32-битных целых контейнеров `S24_3Le`/`S24Le`/`S32Le` (§6.15) —
    /// `S16Le` и `F32` отказ.
    UnsupportedFormat { format: OutputSampleFormat, kind: ExclusiveRenderKind },
    /// Вид ring плана не соответствует виду рендера (внутренняя ошибка
    /// сборки: `exclusive_session_plan` не производит такие пары, кроме
    /// отсечённого там же `StrictPcm` + `Float`, И-Т6).
    RingMismatch { render: ExclusiveRenderKind, ring: RingKind },
}

impl std::fmt::Display for ExclusiveRenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::i18n::errors::exclusive_render_error_text(self))
    }
}

/// PCM-рендер эксклюзивного вывода со ступенью усиления `G` по формату
/// устройства (§2.9, §6.14). Исчерпывает все пять `OutputSampleFormat` —
/// `None` недостижим, `Option` сохранён для единообразия с [`dop_any`].
fn exclusive_pcm<P: PcmSample, G: GainStage>(
    format: OutputSampleFormat,
    core: RenderCore<P>,
    payload: RingPayload,
    tpdf: Tpdf,
) -> Option<ExclusivePcm<G>> {
    Some(match format {
        OutputSampleFormat::S16Le => ExclusivePcm::new(PcmRender::<P, S16Le, G>::new(core, payload, tpdf)),
        OutputSampleFormat::S24_3Le => {
            ExclusivePcm::new(PcmRender::<P, S24_3Le, G>::new(core, payload, tpdf))
        }
        OutputSampleFormat::S24Le => ExclusivePcm::new(PcmRender::<P, S24Le, G>::new(core, payload, tpdf)),
        OutputSampleFormat::S32Le => ExclusivePcm::new(PcmRender::<P, S32Le, G>::new(core, payload, tpdf)),
        OutputSampleFormat::F32 => ExclusivePcm::new(PcmRender::<P, F32Le, G>::new(core, payload, tpdf)),
    })
}

/// DoP-рендер эксклюзивного вывода по формату устройства (§2.9, §6.15):
/// DoP-слово несут только 24/32-битные целые контейнеры `S24_3Le`/`S24Le`/
/// `S32Le`; `S16Le` и `F32` отказ.
fn dop_any(format: OutputSampleFormat, core: RenderCore<i32>) -> Option<DopRenderAny> {
    match format {
        OutputSampleFormat::S24_3Le => Some(DopRenderAny::new(DopRender::<S24_3Le>::new(core))),
        OutputSampleFormat::S24Le => Some(DopRenderAny::new(DopRender::<S24Le>::new(core))),
        OutputSampleFormat::S32Le => Some(DopRenderAny::new(DopRender::<S32Le>::new(core))),
        OutputSampleFormat::S16Le | OutputSampleFormat::F32 => None,
    }
}

/// Ring и [`ExclusiveRender`] эксклюзивного потока по плану `plan` и формату
/// устройства `format` (§2.9, §6.15, §6.18 п. 4, ТЗ-140). Чистая функция:
/// только аллокация ring и подбор мономорфизированного рендера, без I/O.
// используется ExclusivePath (шаг 44)
#[allow(dead_code)]
pub(crate) fn build_exclusive_render(
    plan: &ExclusiveSessionPlan,
    format: OutputSampleFormat,
    shared: &Arc<SessionShared>,
) -> Result<(ExclusiveRender, EngineRing), ExclusiveRenderError> {
    let tpdf = plan.engine.dither_seed.map_or_else(Tpdf::off, Tpdf::with_seed);
    let unsupported =
        |kind: ExclusiveRenderKind| ExclusiveRenderError::UnsupportedFormat { format, kind };

    match (plan.render, plan.engine.kind) {
        (ExclusiveRenderKind::OptimalDop, RingKind::Dop) => {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(plan.ring_capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.engine.channels, plan.prime_frames);
            let dop = dop_any(format, core).ok_or_else(|| unsupported(plan.render))?;
            Ok((ExclusiveRender::Optimal(OptimalRender::Dop(dop)), EngineRing::I32(producer)))
        }
        (ExclusiveRenderKind::StrictDop, RingKind::Dop) => {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(plan.ring_capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.engine.channels, plan.prime_frames);
            let dop = dop_any(format, core).ok_or_else(|| unsupported(plan.render))?;
            Ok((ExclusiveRender::Strict(StrictRender::Dop(dop)), EngineRing::I32(producer)))
        }
        (ExclusiveRenderKind::OptimalPcm, RingKind::Exact(payload)) => {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(plan.ring_capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.engine.channels, plan.prime_frames);
            let pcm = exclusive_pcm::<i32, AtomicGain>(format, core, payload, tpdf)
                .ok_or_else(|| unsupported(plan.render))?;
            Ok((ExclusiveRender::Optimal(OptimalRender::Pcm(pcm)), EngineRing::I32(producer)))
        }
        (ExclusiveRenderKind::OptimalPcm, RingKind::Float) => {
            let (producer, consumer) = rtrb::RingBuffer::<f32>::new(plan.ring_capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.engine.channels, plan.prime_frames);
            let pcm = exclusive_pcm::<f32, AtomicGain>(format, core, RingPayload::F32, tpdf)
                .ok_or_else(|| unsupported(plan.render))?;
            Ok((ExclusiveRender::Optimal(OptimalRender::Pcm(pcm)), EngineRing::F32(producer)))
        }
        (ExclusiveRenderKind::OptimalPcmLocked, RingKind::Exact(payload)) => {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(plan.ring_capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.engine.channels, plan.prime_frames);
            let pcm = exclusive_pcm::<i32, NoGain>(format, core, payload, tpdf)
                .ok_or_else(|| unsupported(plan.render))?;
            Ok((ExclusiveRender::Optimal(OptimalRender::PcmLocked(pcm)), EngineRing::I32(producer)))
        }
        (ExclusiveRenderKind::OptimalPcmLocked, RingKind::Float) => {
            let (producer, consumer) = rtrb::RingBuffer::<f32>::new(plan.ring_capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.engine.channels, plan.prime_frames);
            let pcm = exclusive_pcm::<f32, NoGain>(format, core, RingPayload::F32, tpdf)
                .ok_or_else(|| unsupported(plan.render))?;
            Ok((ExclusiveRender::Optimal(OptimalRender::PcmLocked(pcm)), EngineRing::F32(producer)))
        }
        (ExclusiveRenderKind::StrictPcm, RingKind::Exact(payload)) => {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(plan.ring_capacity);
            let core = RenderCore::new(consumer, shared.clone(), plan.engine.channels, plan.prime_frames);
            let pcm = exclusive_pcm::<i32, NoGain>(format, core, payload, tpdf)
                .ok_or_else(|| unsupported(plan.render))?;
            Ok((ExclusiveRender::Strict(StrictRender::Pcm(pcm)), EngineRing::I32(producer)))
        }
        (render, ring) => Err(ExclusiveRenderError::RingMismatch { render, ring }),
    }
}

/// Отказ [`exclusive_feed`]: открытый источник не соответствует плану
/// (§6.18 п. 4, ТЗ-48, ТЗ-113).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// используется ExclusivePath (шаг 44)
#[allow(dead_code)]
pub(crate) enum ExclusiveFeedError {
    /// Частота или число каналов открытого источника не совпали с планом
    /// (ТЗ-113): для DoP `got_rate` — это частота источника × 4 (контейнер
    /// DoP, §6.15), для `Exact` — частота источника как есть (без SRC,
    /// ТЗ-48).
    SourceMismatch { expected_rate: u32, expected_channels: usize, got_rate: u32, got_channels: usize },
    /// Ring `Exact`: `AudioSource::ring_payload()` не совпал с payload плана —
    /// без молчаливого отказа от бит-точности (ТЗ-48).
    PayloadMismatch,
}

impl std::fmt::Display for ExclusiveFeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::i18n::errors::exclusive_feed_error_text(self))
    }
}

/// `SrcFilter` → `ResamplerAlgorithm`: больше тапов windowed-sinc ⇒ круче
/// срез АЧХ (§2.2). Единое соответствие для эксклюзивного и Совместимого пути.
pub(crate) fn src_filter_to_algorithm(filter: SrcFilter) -> ResamplerAlgorithm {
    match filter {
        SrcFilter::Steep => ResamplerAlgorithm::SincSlow,
        SrcFilter::Slow => ResamplerAlgorithm::SincMedium,
        SrcFilter::VerySlow => ResamplerAlgorithm::SincFast,
    }
}

/// Алгоритм ресемплера для SRC-преобразования плана `plan` (§6.18 п. 4).
/// Нет преобразования `Resample` в `plan.transforms` — частоты совпадают, и
/// подходит дефолт `Resampler::new` (`ResamplerAlgorithm::Linear`).
fn exclusive_resample_algo(plan: &PathPlan) -> ResamplerAlgorithm {
    plan.transforms
        .iter()
        .find_map(|t| match &t.kind {
            TransformKind::Resample { filter, .. } => Some(src_filter_to_algorithm(*filter)),
            _ => None,
        })
        .unwrap_or(ResamplerAlgorithm::Linear)
}

/// Источник для `apap-decode` эксклюзивного вывода по плану пути `plan`,
/// плану сессии `session` и уже открытому `src` (§6.18 п. 4, ТЗ-48, ТЗ-113).
/// `src` открывается вызывающей стороной декодером, соответствующим
/// `plan.route` (PCM-декодер / `DsdDecoder` в PCM-режиме / `DsdDecoder` в
/// `DecodeMode::Dop`). Чистая функция: не открывает устройство, не аллоцирует
/// ring, только проверяет совместимость и собирает `EngineFeed` (мирроит
/// `Player::open_pcm`/`open_dop`).
///
/// `RingKind::Dop` — DoP-контейнер: частота источника × 4 должна дать частоту
/// ring плана (§6.15, И-Т3). `RingKind::Exact` — без ресемплинга: частота,
/// число каналов и `ring_payload()` источника должны совпасть с планом 1:1
/// (ТЗ-48, Строгий/Оптимальный без ресемплинга бит-точны только при точном
/// совпадении). `RingKind::Float` — SRC / DSD→PCM / float-источник: алгоритм
/// ресемплера берётся из `Resample`-преобразования плана
/// ([`exclusive_resample_algo`]).
// используется ExclusivePath (шаг 44)
#[allow(dead_code)]
pub(crate) fn exclusive_feed(
    plan: &PathPlan,
    session: &ExclusiveSessionPlan,
    src: Box<dyn AudioSource>,
) -> Result<EngineFeed, ExclusiveFeedError> {
    let src_rate = src.info().sample_rate;
    let src_ch = src.info().channels;
    let engine = &session.engine;

    match engine.kind {
        RingKind::Dop => {
            let got_rate = src_rate.saturating_mul(4);
            if got_rate != engine.rate || src_ch != engine.channels {
                return Err(ExclusiveFeedError::SourceMismatch {
                    expected_rate: engine.rate,
                    expected_channels: engine.channels,
                    got_rate,
                    got_channels: src_ch,
                });
            }
            Ok(EngineFeed::Exact(ExactFeed::new(src, engine.rate)))
        }
        RingKind::Exact(payload) => {
            if src_rate != engine.rate || src_ch != engine.channels {
                return Err(ExclusiveFeedError::SourceMismatch {
                    expected_rate: engine.rate,
                    expected_channels: engine.channels,
                    got_rate: src_rate,
                    got_channels: src_ch,
                });
            }
            if src.ring_payload() != payload {
                return Err(ExclusiveFeedError::PayloadMismatch);
            }
            Ok(EngineFeed::Exact(ExactFeed::new(src, engine.rate)))
        }
        RingKind::Float => {
            let algo = exclusive_resample_algo(plan);
            let resampler = Resampler::with_algo(src_rate, engine.rate, src_ch, engine.channels, algo);
            Ok(EngineFeed::Float(FloatFeed::new(src, resampler, engine.rate, src_ch, engine.channels)))
        }
    }
}

/// Отказ [`spawn_decoder`]: различает отказ запуска потока (`ThreadSpawner`,
/// сохраняется в `Player::last_fault`, ТЗ-88) и прочие отказы открытия
/// (готовое сообщение для пользователя).
pub(crate) enum SpawnDecoderError {
    Spawn(EngineFault),
    Open(String),
}

/// Поток `apap-decode` над ring сессии; возврат — после `Ready` (§6.18).
/// Ошибка декодирования или таймаут заполнения — отказ открытия (ТЗ-87).
/// Отказ самого запуска потока (`ThreadSpawner`) возвращается как
/// [`SpawnDecoderError::Spawn`] (ТЗ-88, §6.18 шаг 19) — вызывающая сторона
/// сохраняет его в `last_fault`, отличая от классификации по файлу.
/// Приёмник `rx` возвращается вместе с воркером: он остаётся живым после
/// `Ready`, чтобы мид-трековый `Failed` не терялся
/// (`Player::poll_decode_failure`).
// Свободная функция собирает параметры сессии, которыми раньше владел
// `Player` через `self` (ADR-24) — восьмой параметр не повод вводить
// промежуточную структуру.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_decoder(
    spawner: &dyn ThreadSpawner,
    tap: VizTap,
    feed: EngineFeed,
    ring: EngineRing,
    plan: &EnginePlan,
    shared: &Arc<SessionShared>,
    cap_frames: usize,
    start_frame: u64,
) -> Result<(DecodeWorker, mpsc::Receiver<DecodeEvent>), SpawnDecoderError> {
    let (tx, rx) = mpsc::channel();
    let cfg = DecodeConfig {
        channels: plan.channels,
        rate: plan.rate,
        start_frame,
        start_fill: start_fill_frames(cap_frames, plan.rate),
        tail_samples: DECODE_TAIL_FRAMES.saturating_mul(plan.channels.max(1)),
    };
    let tap = Some(tap);
    let spawned = match (feed, ring) {
        // DoP: tap отсутствует (ТЗ-108).
        (EngineFeed::Exact(f), EngineRing::I32(p)) if plan.kind == RingKind::Dop => {
            DecodeWorker::spawn(spawner, DecodeLoop::new(f, p, shared.clone(), cfg, tx, None))
        }
        (EngineFeed::Exact(f), EngineRing::I32(p)) => {
            DecodeWorker::spawn(spawner, DecodeLoop::new(f, p, shared.clone(), cfg, tx, tap))
        }
        (EngineFeed::Float(f), EngineRing::F32(p)) => {
            DecodeWorker::spawn(spawner, DecodeLoop::new(f, p, shared.clone(), cfg, tx, tap))
        }
        _ => return Err(SpawnDecoderError::Open("internal: ring type does not match the feed".into())),
    };
    let worker = match spawned {
        Ok(w) => w,
        Err(e) => return Err(SpawnDecoderError::Spawn(e)),
    };
    match rx.recv_timeout(START_FILL_TIMEOUT) {
        Ok(DecodeEvent::Ready) => Ok((worker, rx)),
        Ok(DecodeEvent::Failed(e)) => Err(SpawnDecoderError::Open(format!("decoding failed: {e:?}"))),
        Err(_) => Err(SpawnDecoderError::Open("decoder did not fill the start buffer in time".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_capacity_scales_with_ms_and_geometry() {
        assert_eq!(ring_capacity(48_000, 2, 1000, None), 96_000);
        assert_eq!(ring_capacity(44_100, 2, 1500, None), 132_300);
        assert_eq!(ring_capacity(8_000, 1, 100, None), 4096);
    }

    #[test]
    fn ring_capacity_respects_cpal_buffer_floor() {
        // At 8 kHz stereo the 100 ms request is only 1600 samples, so the
        // Fixed(2048)-frame floor (2 periods × 2 ch = 8192) wins.
        assert_eq!(ring_capacity(8_000, 2, 100, Some(2048)), 2048 * 2 * 2);
    }

    use std::num::NonZeroU8;

    use smallvec::SmallVec;

    use crate::audio::backend::AccessKind;
    use crate::audio::format::{
        BitDepth, ChannelLayout, Codec, Container, DsdRate, OutputSampleFormat, SampleRate,
        SourceFormat, SourceKind, StreamFormat,
    };
    use crate::audio::path::{DacCaps, DopRoute};

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap_or_else(|| panic!("bad rate {hz}"))
    }

    /// Минимальный PCM-план (режим и маршрут подставляет вызывающий тест).
    fn pcm_plan(mode: ModeKind) -> PathPlan {
        PathPlan {
            mode,
            source: SourceFormat {
                container: Container::Flac,
                codec: Codec::Flac,
                lossy: false,
                rate: sr(44_100),
                layout: ChannelLayout::stereo(),
                kind: SourceKind::Pcm { bits: BitDepth::new(16).unwrap_or_else(|| panic!("bad bits")) },
            },
            route: Route::Pcm,
            transforms: SmallVec::new(),
            target: StreamFormat {
                rate: sr(44_100),
                sample: OutputSampleFormat::S24Le,
                channels: NonZeroU8::new(2).unwrap_or_else(|| panic!("bad channels")),
            },
            ring: RingPayload::ExactI32 {
                valid_bits: BitDepth::new(16).unwrap_or_else(|| panic!("bad bits")),
            },
            dsd_chain: None,
            dac: DacCaps::default(),
        }
    }

    /// Минимальный DoP-план: источник DSD64, контейнер PCM 176 400 Гц
    /// (И-Т3).
    fn dop_plan(mode: ModeKind) -> PathPlan {
        let pcm_rate = sr(176_400);
        let route = DopRoute::new(DsdRate::Dsd64, pcm_rate, AccessKind::Exclusive)
            .unwrap_or_else(|| panic!("DopRoute::new returned None for Exclusive"));
        PathPlan {
            mode,
            source: SourceFormat {
                container: Container::Dsf,
                codec: Codec::Dsf,
                lossy: false,
                rate: sr(DsdRate::Dsd64.bit_rate_hz()),
                layout: ChannelLayout::stereo(),
                kind: SourceKind::Dsd { dsd: DsdRate::Dsd64 },
            },
            route: Route::Dop(route),
            transforms: SmallVec::new(),
            target: StreamFormat {
                rate: pcm_rate,
                sample: OutputSampleFormat::S24Le,
                channels: NonZeroU8::new(2).unwrap_or_else(|| panic!("bad channels")),
            },
            ring: RingPayload::ExactI32 {
                valid_bits: BitDepth::new(24).unwrap_or_else(|| panic!("bad bits")),
            },
            dsd_chain: None,
            dac: DacCaps::default(),
        }
    }

    fn buf() -> DeviceBufferActual {
        DeviceBufferActual { period_frames: 512, buffer_frames: 2048 }
    }

    #[test]
    fn exclusive_session_plan_optimal_has_gain() {
        let plan = pcm_plan(ModeKind::Optimal);
        let out = exclusive_session_plan(&plan, buf(), 200, false, Some(7))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out.render, ExclusiveRenderKind::OptimalPcm);
        assert!(!out.engine.no_gain);
        assert_eq!(out.engine.dither_seed, Some(7));
        assert_eq!(out.engine.rate, 44_100);
        assert_eq!(out.engine.channels, 2);
        // ring_capacity(44_100, 2, 200, Some(2048)) = 44_100*2*200/1000 = 17_640,
        // выше пола max(4096, 2*2048*2=8192).
        assert_eq!(out.ring_capacity, 17_640);
        let expected_prime = prime_frames(512, 44_100, out.ring_capacity / 2);
        assert_eq!(out.prime_frames, expected_prime);
    }

    #[test]
    fn exclusive_session_plan_optimal_locked_has_no_gain() {
        let plan = pcm_plan(ModeKind::Optimal);
        let out = exclusive_session_plan(&plan, buf(), 200, true, Some(7))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out.render, ExclusiveRenderKind::OptimalPcmLocked);
        assert!(out.engine.no_gain);
        assert_eq!(out.engine.dither_seed, Some(7));
    }

    #[test]
    fn exclusive_session_plan_strict_has_no_gain() {
        let plan = pcm_plan(ModeKind::Strict);
        let out = exclusive_session_plan(&plan, buf(), 200, false, Some(7))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out.render, ExclusiveRenderKind::StrictPcm);
        assert!(out.engine.no_gain);
    }

    #[test]
    fn exclusive_session_plan_dop_has_no_gain_and_no_dither() {
        let plan = dop_plan(ModeKind::Optimal);
        let out = exclusive_session_plan(&plan, buf(), 200, false, Some(7))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out.render, ExclusiveRenderKind::OptimalDop);
        assert!(out.engine.no_gain);
        assert_eq!(out.engine.dither_seed, None);
        assert_eq!(out.engine.kind, RingKind::Dop);

        // Совместимый режим не строит эксклюзивный план (ADR-04).
        let compat = pcm_plan(ModeKind::Compatible);
        let err = exclusive_session_plan(&compat, buf(), 200, false, None)
            .expect_err("Compatible must not build an exclusive plan");
        assert_eq!(err, ExclusivePlanError::NotExclusive);
    }

    fn exclusive_plan(engine: EnginePlan, render: ExclusiveRenderKind) -> ExclusiveSessionPlan {
        ExclusiveSessionPlan { engine, render, ring_capacity: 4096, prime_frames: 0 }
    }

    #[test]
    fn build_exclusive_render_strict_pcm_s24_3_exact_uses_i32_ring() {
        let engine = EnginePlan {
            kind: RingKind::Exact(RingPayload::ExactI32 {
                valid_bits: BitDepth::new(24).unwrap_or_else(|| panic!("bad bits")),
            }),
            rate: 176_400,
            channels: 2,
            no_gain: true,
            dither_seed: None,
        };
        let plan = exclusive_plan(engine, ExclusiveRenderKind::StrictPcm);
        let shared = Arc::new(SessionShared::new());
        let (render, ring) = build_exclusive_render(&plan, OutputSampleFormat::S24_3Le, &shared)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(render, ExclusiveRender::Strict(StrictRender::Pcm(_))));
        assert!(matches!(ring, EngineRing::I32(_)));
    }

    #[test]
    fn build_exclusive_render_optimal_pcm_float_ring() {
        let engine = EnginePlan {
            kind: RingKind::Float,
            rate: 44_100,
            channels: 2,
            no_gain: false,
            dither_seed: Some(7),
        };
        let plan = exclusive_plan(engine, ExclusiveRenderKind::OptimalPcm);
        let shared = Arc::new(SessionShared::new());
        let (render, ring) = build_exclusive_render(&plan, OutputSampleFormat::S32Le, &shared)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(render, ExclusiveRender::Optimal(OptimalRender::Pcm(_))));
        assert!(matches!(ring, EngineRing::F32(_)));
    }

    #[test]
    fn build_exclusive_render_dop_s16le_is_unsupported() {
        let engine = EnginePlan {
            kind: RingKind::Dop,
            rate: 176_400,
            channels: 2,
            no_gain: true,
            dither_seed: None,
        };
        let plan = exclusive_plan(engine, ExclusiveRenderKind::OptimalDop);
        let shared = Arc::new(SessionShared::new());
        let Err(err) = build_exclusive_render(&plan, OutputSampleFormat::S16Le, &shared) else {
            panic!("S16Le must not carry DoP (§6.15)");
        };
        assert_eq!(
            err,
            ExclusiveRenderError::UnsupportedFormat {
                format: OutputSampleFormat::S16Le,
                kind: ExclusiveRenderKind::OptimalDop,
            }
        );
    }

    /// Фейковый `AudioSource` для тестов [`exclusive_feed`]: отдаёт заданные
    /// частоту/каналы/payload, блок данных не нужен — `exclusive_feed` читает
    /// только `info()`/`ring_payload()` и строит `EngineFeed`, не читая кадры.
    struct FakeSource {
        info: crate::audio::decoder::TrackInfo,
        payload: RingPayload,
    }

    impl FakeSource {
        fn new(sample_rate: u32, channels: usize, payload: RingPayload) -> Self {
            Self {
                info: crate::audio::decoder::TrackInfo {
                    sample_rate,
                    channels,
                    num_frames: Some(0),
                    format_name: "fake".into(),
                    bitrate: 0,
                    bits: None,
                    tags: Default::default(),
                },
                payload,
            }
        }
    }

    impl AudioSource for FakeSource {
        fn next_block(
            &mut self,
        ) -> Result<Option<crate::audio::format::SampleBlock<'_>>, crate::audio::error::FileError>
        {
            Ok(None)
        }

        fn info(&self) -> &crate::audio::decoder::TrackInfo {
            &self.info
        }

        fn eof(&self) -> bool {
            true
        }

        fn ring_payload(&self) -> RingPayload {
            self.payload
        }
    }

    #[test]
    fn exclusive_feed_exact_match_builds_exact() {
        let plan = pcm_plan(ModeKind::Optimal);
        let RingPayload::ExactI32 { valid_bits } = plan.ring else {
            panic!("pcm_plan(Optimal) must carry RingPayload::ExactI32");
        };
        let engine = EnginePlan {
            kind: RingKind::Exact(RingPayload::ExactI32 { valid_bits }),
            rate: 44_100,
            channels: 2,
            no_gain: false,
            dither_seed: None,
        };
        let session = exclusive_plan(engine, ExclusiveRenderKind::OptimalPcm);
        let src = Box::new(FakeSource::new(44_100, 2, RingPayload::ExactI32 { valid_bits }));
        let feed = exclusive_feed(&plan, &session, src).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(feed, EngineFeed::Exact(_)));
    }

    #[test]
    fn exclusive_feed_exact_rate_mismatch_is_rejected() {
        let plan = pcm_plan(ModeKind::Optimal);
        let RingPayload::ExactI32 { valid_bits } = plan.ring else {
            panic!("pcm_plan(Optimal) must carry RingPayload::ExactI32");
        };
        let engine = EnginePlan {
            kind: RingKind::Exact(RingPayload::ExactI32 { valid_bits }),
            rate: 44_100,
            channels: 2,
            no_gain: false,
            dither_seed: None,
        };
        let session = exclusive_plan(engine, ExclusiveRenderKind::OptimalPcm);
        let src = Box::new(FakeSource::new(48_000, 2, RingPayload::ExactI32 { valid_bits }));
        let Err(err) = exclusive_feed(&plan, &session, src) else {
            panic!("48 kHz source must not pass as 44.1 kHz plan (ТЗ-48)");
        };
        assert_eq!(
            err,
            ExclusiveFeedError::SourceMismatch {
                expected_rate: 44_100,
                expected_channels: 2,
                got_rate: 48_000,
                got_channels: 2,
            }
        );
    }

    #[test]
    fn exclusive_feed_float_ring_builds_float() {
        let plan = PathPlan { ring: RingPayload::F32, ..pcm_plan(ModeKind::Optimal) };
        let engine = EnginePlan {
            kind: RingKind::Float,
            rate: 44_100,
            channels: 2,
            no_gain: false,
            dither_seed: None,
        };
        let session = exclusive_plan(engine, ExclusiveRenderKind::OptimalPcm);
        let src = Box::new(FakeSource::new(44_100, 2, RingPayload::F32));
        let feed = exclusive_feed(&plan, &session, src).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(feed, EngineFeed::Float(_)));
    }
}
