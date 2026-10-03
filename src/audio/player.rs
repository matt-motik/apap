use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat};

use super::decoder::{AudioSource, Decoder, TrackInfo};
use super::dsd::{DecodeMode, DsdDecoder};
use super::clock::{Clock, ClockInstant, MonotonicClock};
use super::error::CaptureFailure;
use super::reservation::gate::{ExclusiveGate, GateEvent, GateStatus, OpenOutcome};
use super::output::{
    build_output_stream_raw, dop_render_for, pcm_render_for, select_output_for, FallbackReason,
    OutputRequest, OutputSpec, RawRender, Resampler, StreamBuildError,
};
use super::render::gain::{AtomicGain, NoGain};
use super::render::pcm::PcmSample;
use super::render::tpdf::Tpdf;
use super::render::{prime_frames, RenderCore};
use super::session::{RingPayload, SessionShared};
use super::worker::{
    start_fill_frames, DecodeConfig, DecodeEvent, DecodeLoop, DecodeWorker, ExactFeed, FloatFeed,
    VizTap,
};
use crate::settings::{
    clamp_ring_buffer_ms, ClockFamily, DsdMode, ExclusiveMode, FallbackPolicy,
    FallbackRatePolicy, ResamplerAlgorithm, ResamplerDither, ResamplerMode, RING_BUFFER_MS_DEFAULT,
};

/// Индекс режима дизеринга из [`ResamplerDither`]; фиксируется при сборке
/// рендера (seed TPDF, ТЗ-8).
const DITHER_INDEX_TPDF: u8 = 0;
const DITHER_INDEX_TRIANGULAR: u8 = 1;
const DITHER_INDEX_OFF: u8 = 2;

/// Map a config [`ResamplerDither`] to the atomically-stored index.
fn dither_index(d: ResamplerDither) -> u8 {
    match d {
        ResamplerDither::Tpdf => DITHER_INDEX_TPDF,
        ResamplerDither::Triangular => DITHER_INDEX_TRIANGULAR,
        ResamplerDither::Off => DITHER_INDEX_OFF,
    }
}

/// Deterministic per-track seed for the dither PRNG, derived from the path so
/// the noise sequence is stable across runs but unique per file.
fn track_seed(path: &Path) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() >> (64 - 30)) as u32 | 1
}

/// Interleaved-sample capacity for the producer/consumer ring (ТЗ A2.0 §5.2).
///
/// Derived from the requested depth in milliseconds and the device geometry,
/// then floored at `max(4096, 2 cpal-buffer periods)` so the ring can never be
/// shorter than two output callbacks (which would guarantee underruns).
fn ring_capacity(
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

/// True for `.dsf`/`.dff` paths (case-insensitive).
fn is_dsd_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("dsf") || e.eq_ignore_ascii_case("dff"))
        .unwrap_or(false)
}

/// Короткое имя sample-формата для [`StreamDesc`] (⚠ не форматировать,
/// используется как человекочитаемая метка в UI и bp-report).
fn format_name(f: &SampleFormat) -> &'static str {
    match f {
        SampleFormat::F32 => "F32",
        SampleFormat::I32 => "I32",
        SampleFormat::I24 => "I24",
        SampleFormat::I16 => "I16",
        SampleFormat::U8 => "U8",
        _ => "?",
    }
}

/// Test-only seam: lets §11.4 tests force the first N `build_stream_rt`
/// attempts in `start_engine` to fail (e.g. to simulate a device that rejects
/// an exclusive stream). Absent from production builds.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct TestHooks {
    /// Remaining forced build failures; consumed one per `start_engine` call.
    pub build_failures: AtomicUsize,
}

/// Описание текущего audio-потока (ТЗ A3.0 §4.4): геометрия + деградации.
/// Собирается в `open_pcm`/`open_dop`, обогащается DSD-полями в
/// `open_dsd_with_chain`; читается UI-менеджерами для badge'ов и bp-report.
#[derive(Debug, Clone, Default)]
pub struct StreamDesc {
    pub device: String,
    pub rate: u32,
    pub channels: u16,
    pub format: &'static str,
    pub exclusive: bool,
    /// Exclusive был запрошен (политикой), но не выдан (серверный узел
    /// или устройство отклонило поток) — см. `stream_desc.exclusive_fallback`.
    pub exclusive_fallback: bool,
    pub resampled: bool,
    pub source_rate: u32,
    pub source_channels: usize,
    pub dsd_mode: Option<DsdMode>,
    pub dsd_preferred: Option<DsdMode>,
    pub dsd_fallback_reason: Option<String>,
    pub fallback: Option<FallbackReason>,
}

/// Открытие трека, отложенное до резервирования карты: намерения транспорта,
/// поступившие во время ожидания, применяются после открытия (ТЗ-119).
#[derive(Debug, Clone)]
struct PendingOpen {
    path: PathBuf,
    play: bool,
    seek: f64,
}

/// Итог опроса резервирования на тике приложения (§8 С1).
#[derive(Debug, Clone, PartialEq)]
pub enum ReservationEvent {
    /// Резервирование получено, поток открыт.
    Opened,
    /// Захват не удался: воспроизведение остановлено, отката в Shared нет (ТЗ-122).
    Failed(String),
    /// `NameLost`: PCM закрыт немедленно (И-Р20).
    Lost(String),
}

/// Текст отказа захвата для строки состояния.
pub fn capture_message(f: &CaptureFailure) -> String {
    match f {
        CaptureFailure::ReservationDenied { owner: Some(app) } => {
            format!("Устройство занято: {app} не освобождает его")
        }
        CaptureFailure::ReservationDenied { owner: None } => "Устройство занято другим приложением".into(),
        CaptureFailure::OwnerNotResponding => "Владелец устройства не отвечает".into(),
        CaptureFailure::NoSessionBus => "Нет сессионной шины D-Bus: резервирование устройства невозможно".into(),
        CaptureFailure::BusyOutsideProtocol => "Устройство занято вне протокола резервирования".into(),
        CaptureFailure::DriverRefused { errno } => format!("Драйвер отказал в открытии устройства (errno {errno})"),
        CaptureFailure::LostOnReopen => "Устройство потеряно при переоткрытии".into(),
        CaptureFailure::DeviceDisconnected => "Устройство отключено".into(),
        CaptureFailure::ReservationLost => "Резервирование перехвачено".into(),
        CaptureFailure::PlatformUnsupported => "Монопольный режим не поддерживается на этой платформе".into(),
    }
}

/// Ворота резервирования поверх сессионной шины (ADR-08).
#[cfg(target_os = "linux")]
fn system_gate() -> Option<ExclusiveGate> {
    use super::reservation::dbus::{DbusReserveBus, SystemServerProbe};
    use super::reservation::{BusReservationService, ReserveBus};
    let (tx, rx) = std::sync::mpsc::channel();
    let bus: Arc<dyn ReserveBus> = Arc::new(DbusReserveBus::new());
    let service = BusReservationService::new(bus, Box::new(SystemServerProbe), tx);
    Some(ExclusiveGate::new(Box::new(service), rx))
}

#[cfg(not(target_os = "linux"))]
fn system_gate() -> Option<ExclusiveGate> {
    None
}

/// Карта ALSA, которую нужно зарезервировать перед открытием (`hw:` в Exclusive).
fn reserved_card(spec: &OutputSpec) -> Option<u32> {
    if !spec.exclusive {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        super::reservation::dbus::alsa_card_index(&spec.device_id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Открытие `hw:` только при удерживаемом резервировании карты (ТЗ-48, §6.6).
/// `Ok(None)` — ждать ответа службы или повторить `EBUSY` на следующем тике
/// (ТЗ-119, ТЗ-122); ошибка — без отката в Shared.
fn gated_build<P>(
    gate: &mut ExclusiveGate,
    card: u32,
    device_name: &str,
    now: ClockInstant,
    build: impl FnOnce() -> Result<P, StreamBuildError>,
) -> Result<Option<P>, String> {
    match gate.begin(card, device_name) {
        Some(GateEvent::Ready) => {}
        Some(GateEvent::Failed(f)) => return Err(capture_message(&f)),
        Some(GateEvent::Lost) | None => return Ok(None),
    }
    let mut message = String::new();
    let outcome = gate.try_open(now, || {
        build().map_err(|e| {
            message = e.message;
            e.open
        })
    });
    match outcome {
        OpenOutcome::Opened(p) => Ok(Some(p)),
        OpenOutcome::Retry | OpenOutcome::NotHeld => Ok(None),
        OpenOutcome::Failed(f) => Err(format!("{}: {message}", capture_message(&f))),
    }
}

/// Ёмкость `PendingTail` в кадрах: один декодированный пакет (§6.18).
const DECODE_TAIL_FRAMES: usize = 8192;

/// Сколько ждать стартового заполнения ring до отказа открытия (§6.18).
const START_FILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Тип ring сессии и вид рендера (§6.10, ADR-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RingKind {
    /// DoP-слова в i32-ring, маркеры ставит `DopRender` (§6.15).
    Dop,
    /// Точный PCM без ресемплинга в i32-ring.
    Exact(RingPayload),
    /// f32 после SRC / DSD→PCM / float-источника.
    Float,
}

/// Параметры сборки рендера, фиксируемые на время сессии (ОВС-18, ТЗ-8).
#[derive(Debug, Clone, Copy)]
struct EnginePlan {
    kind: RingKind,
    rate: u32,
    channels: usize,
    /// Bit-perfect: рендер без ступени громкости (`NoGain`, ОВС-18).
    no_gain: bool,
    /// Seed TPDF; `None` — дизеринг выключен.
    dither_seed: Option<u32>,
}

/// Producer ring сессии, отдаваемый декодеру.
enum EngineRing {
    I32(rtrb::Producer<i32>),
    F32(rtrb::Producer<f32>),
}

/// Источник для `apap-decode` по типу ring.
enum EngineFeed {
    Exact(ExactFeed),
    Float(FloatFeed),
}

/// Секунды → кадр сессии (отрицательные и нечисловые — 0).
fn secs_to_frames(secs: f64, rate: u32) -> u64 {
    let frames = (secs.max(0.0) * f64::from(rate)).round();
    if frames.is_finite() {
        frames as u64
    } else {
        0
    }
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
fn build_render(
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

/// Owns audio playback: поток `apap-decode` пишет `SampleBlock` в
/// типизированный ring, колбэк cpal читает его через `PcmRender`/`DopRender`
/// (§6.10, §6.14, ADR-03). Управление — атомики [`SessionShared`] (§2.9).
pub struct Player {
    /// Атомики сессии, общие с декодером и колбэком. `None` до открытия трека.
    shared: Option<Arc<SessionShared>>,
    worker: Option<DecodeWorker>,
    /// Геометрия ring текущей сессии (позиция в кадрах → секунды).
    out_rate: u32,
    out_ch: usize,
    /// Естественный конец уже обработан плейлистом ([`Player::clear_end`]).
    end_ack: bool,
    /// Persistent visualizer tap holder; survives worker re-creation on every
    /// `open` (the producer itself is not cloneable).
    viz_tap: VizTap,
    pub stream: Option<cpal::Stream>,
    /// Ворота exclusive-открытия `hw:` (ТЗ-48, §6.6). Объявлены после `stream`:
    /// при drop плеера PCM закрывается раньше `ReleaseName` (И-Р1). `None` —
    /// платформа без ReserveDevice1: открытие без резервирования.
    gate: Option<ExclusiveGate>,
    /// Открытие, ждущее резервирования или повтора `EBUSY` (опрос на тике, §8 С1).
    pending: Option<PendingOpen>,
    clock: MonotonicClock,
    pub device_desc: String,
    pub last_error: Option<String>,
    /// Current track info (duration/rate), kept for `snapshot`.
    info: Option<TrackInfo>,
    /// Path of the currently loaded track: lets `play()`/`toggle()` rebuild the
    /// engine after an idle exclusive stream was released (V5.1-B5).
    current_path: Option<PathBuf>,
    preferred_device: Option<String>,
    /// Resampling algorithm used by every new stream (ТЗ 5.1 §8.3).
    resampler_algo: ResamplerAlgorithm,
    /// DSD output mode (Pcm / Native / DoP, ТЗ 5.1 §8.2).
    dsd_mode: DsdMode,
    /// Политика exclusive-доступа (ТЗ A3.0 §2.2): Off / Auto (retry shared) /
    /// Strict (без retry).
    exclusive_mode: ExclusiveMode,
    /// Политика фолбека при несовпадении параметров (ТЗ A3.0 §2.2); для DSD
    /// `Fail` останавливает цепочку на первом провале (§4.2).
    fallback_policy: FallbackPolicy,
    /// Режим ресемплинга для новых потоков (ТЗ A3.0 §2.1). Раньше парсился
    /// из пути (A2.0 §8.2) — теперь задаётся настройками (A3.5 §7.7).
    resampler_mode: ResamplerMode,
    /// Фиксированная частота выхода для `resampler_mode == Fixed` (Гц).
    fixed_rate: u32,
    /// Предпочтительное семейство клока при ресемплинге (ТЗ A3.0 §2.1).
    prefer_family: ClockFamily,
    /// Политика понижения частоты (ТЗ A3.0 §2.1); работает при
    /// `resampler_mode == Auto`.
    fallback_rate: FallbackRatePolicy,
    /// Описание последнего открытого потока (см. [`StreamDesc`]).
    stream_desc: Option<StreamDesc>,
    /// Test-only seam (§11.4) — см. [`TestHooks`].
    #[cfg(test)]
    test_hooks: TestHooks,
    /// Ring depth in milliseconds for new streams (ТЗ A2.0 §5.2).
    ring_buffer_ms: u32,
    /// Desired transport state, persisted across stream re-creation so a new
    /// track inherits volume/mute/dither/bit-perfect/visualizer settings.
    volume: f32,
    muted: bool,
    bit_perfect: bool,
    dither_idx: u8,
    viz_active: bool,
}

impl Player {
    pub fn new() -> Self {
        let host = cpal::default_host();
        let device_desc = host
            .default_output_device()
            .and_then(|d| d.description().ok())
            .map(|d| d.name().to_string())
            .unwrap_or_else(|| "none".to_string());
        Self {
            shared: None,
            worker: None,
            out_rate: 44_100,
            out_ch: 2,
            end_ack: false,
            viz_tap: Arc::new(Mutex::new(None)),
            stream: None,
            gate: system_gate(),
            pending: None,
            clock: MonotonicClock::new(),
            device_desc,
            last_error: None,
            info: None,
            current_path: None,
            preferred_device: None,
            resampler_algo: ResamplerAlgorithm::SincMedium,
            dsd_mode: DsdMode::Pcm,
            exclusive_mode: ExclusiveMode::Auto,
            fallback_policy: FallbackPolicy::Nearest,
            resampler_mode: ResamplerMode::Auto,
            fixed_rate: 0,
            prefer_family: ClockFamily::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            stream_desc: None,
            #[cfg(test)]
            test_hooks: TestHooks::default(),
            ring_buffer_ms: RING_BUFFER_MS_DEFAULT,
            volume: 0.8,
            muted: false,
            bit_perfect: false,
            dither_idx: DITHER_INDEX_TPDF,
            viz_active: false,
        }
    }

    pub fn set_resampler_algorithm(&mut self, algo: ResamplerAlgorithm) {
        self.resampler_algo = algo;
    }

    /// Set the DSD output mode (ТЗ 5.1 §8.2). With A3.4 the mode is a
    /// *preference*: `open` expands it into a chain (§4.2) and falls back down
    /// the chain instead of failing outright (Native → DoP → Pcm).
    pub fn set_dsd_mode(&mut self, mode: DsdMode) {
        self.dsd_mode = mode;
    }

    /// Политика exclusive-доступа (ТЗ A3.0 §2.2): `Strict` = без retry,
    /// `Auto` = при отказе потока один общий retry, `Off` = всегда shared.
    pub fn set_exclusive_mode(&mut self, mode: ExclusiveMode) {
        self.exclusive_mode = mode;
    }

    /// Политика фолбека (ТЗ A3.0 §2.2). `Fail` дополнительно останавливает
    /// DSD-цепочку: после первого провала шага следующие шаги не пробуются.
    pub fn set_fallback_policy(&mut self, policy: FallbackPolicy) {
        self.fallback_policy = policy;
    }

    /// Режим ресемплинга для новых потоков (ТЗ A3.0 §2.1, A3.5 §7.7)
    /// — применяется на следующем `open`/смене устройства.
    pub fn set_resampler_mode(&mut self, mode: ResamplerMode) {
        self.resampler_mode = mode;
    }

    /// Фиксированная частота выхода для `resampler_mode == Fixed` (ТЗ A3.5
    /// §7.7). Затрагивает только следующий `open`.
    pub fn set_fixed_rate(&mut self, rate: u32) {
        self.fixed_rate = rate;
    }

    /// Предпочтительное семейство клока при ресемплинге (ТЗ A3.0 §2.1,
    /// A3.5 §7.7). Применяется на следующем `open`.
    pub fn set_prefer_family(&mut self, family: ClockFamily) {
        self.prefer_family = family;
    }

    /// Политика понижения частоты (ТЗ A3.0 §2.1, A3.5 §7.7); актуальна только
    /// при `resampler_mode == Auto`. Применяется на следующем `open`.
    pub fn set_fallback_rate(&mut self, policy: FallbackRatePolicy) {
        self.fallback_rate = policy;
    }

    /// Описание последнего открытого потока (геометрия + деградации), либо
    /// `None`, если трек ещё не открывался (ТЗ A3.0 §4.4).
    pub fn stream_desc(&self) -> Option<&StreamDesc> {
        self.stream_desc.as_ref()
    }

    /// Ring depth in ms for streams opened from now on (ТЗ A2.0 §5.2).
    /// Clamped into `[RING_BUFFER_MS_MIN..RING_BUFFER_MS_MAX]`; takes effect on
    /// the next `open`/device change.
    pub fn set_ring_buffer_ms(&mut self, ms: u32) {
        self.ring_buffer_ms = clamp_ring_buffer_ms(ms);
    }

    pub fn set_preferred_device(&mut self, name: String) {
        self.preferred_device = Some(name);
    }

    /// Tear down the current engine (stream, then worker) before a new open.
    fn teardown(&mut self) {
        // Drop the stream first so the real-time callback stops touching the
        // ring, then join the worker.
        self.stream = None;
        self.worker = None;
        self.shared = None;
    }

    /// True when the current stream holds an exclusive raw-`hw:` ALSA node
    /// (the only case releasing it is required to unblock the system mixer).
    fn exclusive_held(desc: Option<&StreamDesc>) -> bool {
        desc.map(|d| d.exclusive).unwrap_or(false)
    }

    /// Release the cpal stream and the worker while keeping the shared
    /// transport state (position/flags) so the UI does not reset. The ALSA
    /// `hw:*` PCM is snd_pcm_close'd on Drop → the node returns to the
    /// PipeWire/wireplumber graph (V5.1-B5).
    pub fn release_engine(&mut self) {
        self.pending = None;
        self.release_stream();
        self.worker = None;
    }

    /// Закрыть PCM, затем снять резервирование (И-Р1).
    fn release_stream(&mut self) {
        match self.gate.as_mut() {
            Some(gate) => gate.release(&mut self.stream),
            None => self.stream = None,
        }
    }

    /// Idle hook: drop the engine only when an exclusive raw node was open.
    /// Shared (pipeline) devices are left running to avoid churning the mixer
    /// on every stop.
    pub fn release_if_exclusive(&mut self) {
        if Self::exclusive_held(self.stream_desc.as_ref()) || self.pending.is_some() {
            self.release_engine();
        }
    }

    /// Change the output device. When `path` is given the current track is
    /// reopened on the new device, resumed near `pos` and set playing.
    pub fn set_device(
        &mut self,
        name: String,
        path: Option<&Path>,
        pos: f64,
    ) -> Result<(), String> {
        let was_playing = self.is_playing();
        self.preferred_device = Some(name.clone());
        if let Some(path) = path {
            self.open_at(path, pos)?;
            if was_playing || pos > 0.0 {
                self.play();
            }
        }
        Ok(())
    }

    /// Open `path` for playback. PCM files go through [`Player::open_pcm`];
    /// DSD files are routed into the preference chain (§4.2): `dsd_mode`
    /// expands to `[Native, DoP, Pcm]` / `[DoP, Pcm]` / `[Pcm]` and the first
    /// step that succeeds wins. `FallbackPolicy::Fail` stops the chain after
    /// the first failed step. Returns the track info; errors are strings.
    pub fn open(&mut self, path: &Path) -> Result<TrackInfo, String> {
        self.open_at(path, 0.0)
    }

    /// [`Player::open`] с началом сессии в `start_secs` (фаза 1 seek, §6.16):
    /// декодер встаёт на позицию до стартового заполнения (§6.18).
    fn open_at(&mut self, path: &Path, start_secs: f64) -> Result<TrackInfo, String> {
        self.pending = None;
        if is_dsd_path(path) {
            self.open_dsd_with_chain(path, start_secs)
        } else {
            self.open_pcm(path, start_secs)
        }
    }

    /// DSD playback through the preference chain (ТЗ A3.0 §4.1–4.2).
    fn open_dsd_with_chain(&mut self, path: &Path, start_secs: f64) -> Result<TrackInfo, String> {
        let preferred = self.dsd_mode;
        let chain: &[DsdMode] = match preferred {
            DsdMode::Native => &[DsdMode::Native, DsdMode::DoP, DsdMode::Pcm],
            DsdMode::DoP => &[DsdMode::DoP, DsdMode::Pcm],
            DsdMode::Pcm => &[DsdMode::Pcm],
        };
        let strict = self.fallback_policy == FallbackPolicy::Fail;
        let mut last_err: Option<String> = None;

        for &mode in chain {
            match self.try_open_dsd(path, mode, start_secs) {
                Ok(info) => {
                    if let Some(desc) = self.stream_desc.as_mut() {
                        desc.dsd_mode = Some(mode);
                        desc.dsd_preferred = Some(preferred);
                        if mode != preferred {
                            desc.dsd_fallback_reason =
                                Some(format!("{preferred:?} unavailable"));
                        }
                    }
                    return Ok(info);
                }
                Err(e) => {
                    last_err = Some(e);
                    if strict {
                        break;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| "DSD playback failed".into()))
    }

    /// One step of the DSD chain. `Native` has no cpal backend yet, so it is a
    /// constant error (the chain accounts for it).
    fn try_open_dsd(&mut self, path: &Path, mode: DsdMode, start_secs: f64) -> Result<TrackInfo, String> {
        match mode {
            DsdMode::Native => Err("Native DSD not supported by cpal backend".into()),
            DsdMode::DoP => self.open_dop(path, start_secs),
            DsdMode::Pcm => self.open_pcm(path, start_secs),
        }
    }

    /// Опубликовать сохранённое состояние управления в новой сессии (§2.9).
    fn apply_state(&self, shared: &SessionShared) {
        shared.set_gain(self.volume);
        shared.muted.store(self.muted, Ordering::Release);
        shared
            .viz_tap_active
            .store(self.viz_active, Ordering::Relaxed);
    }

    /// Seed TPDF трека; `None` — дизеринг выключен. Фиксируется при сборке
    /// рендера, смена режима переоткрывает поток (ТЗ-8, §6.14).
    fn dither_seed(&self, path: &Path) -> Option<u32> {
        (self.dither_idx != DITHER_INDEX_OFF).then(|| track_seed(path))
    }

    /// Wrap `build_output_stream_raw` with the §11.4 mock seam. In test builds
    /// the first `build_failures` `start_engine`-iterations are forced to fail
    /// (the device rejects the stream); production builds call through.
    fn build_stream(
        &self,
        spec: &OutputSpec,
        render: Box<dyn RawRender>,
    ) -> Result<cpal::Stream, StreamBuildError> {
        #[cfg(test)]
        {
            let rem = self.test_hooks.build_failures.load(Ordering::Relaxed);
            if rem > 0 {
                self.test_hooks
                    .build_failures
                    .store(rem - 1, Ordering::Relaxed);
                return Err(StreamBuildError::refused("Mock: build_output_stream_raw forced to fail (TestHooks)".into()));
            }
        }
        build_output_stream_raw(spec, render, None)
    }

    /// Собрать поток и запустить `apap-decode`. `hw:` в Exclusive открывается
    /// только через [`ExclusiveGate`] (ТЗ-48); `Ok(None)` — открытие отложено
    /// до резервирования или повтора `EBUSY` (опрос [`Player::poll_reservation`]).
    /// Отказ сборки Exclusive-потока возвращается как ошибка: отката в Shared
    /// нет ни при каком `ExclusiveMode` (ТЗ-119, ТЗ-122; DoP — ТЗ-1).
    /// Поток возвращается после стартового заполнения ring (§6.18).
    fn start_engine(
        &mut self,
        feed: EngineFeed,
        plan: EnginePlan,
        shared: Arc<SessionShared>,
        mut spec: OutputSpec,
        start_frame: u64,
    ) -> Result<Option<cpal::Stream>, String> {
        let card = reserved_card(&spec);
        let now = self.clock.now();
        let mut gate = self.gate.take();
        let built = match (gate.as_mut(), card) {
            (Some(g), Some(card)) => {
                let name = spec.device_name.clone();
                gated_build(g, card, &name, now, || self.build_engine_stream(&plan, &shared, &mut spec))
            }
            (g, _) => {
                // Shared или не `hw:`: резервирование не нужно, удерживаемое снимается.
                if let Some(g) = g {
                    g.release(&mut None::<cpal::Stream>);
                }
                self.build_engine_stream(&plan, &shared, &mut spec)
                    .map(Some)
                    .map_err(|e| e.message)
            }
        };
        self.gate = gate;
        let Some((stream, ring, cap_frames)) = built? else {
            return Ok(None);
        };
        match self.spawn_decoder(feed, ring, &plan, &shared, cap_frames, start_frame) {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(Some(stream))
            }
            Err(e) => {
                // PCM закрывается раньше снятия резервирования (И-Р1).
                let mut stream = Some(stream);
                match self.gate.as_mut() {
                    Some(g) => g.release(&mut stream),
                    None => drop(stream),
                }
                Err(e)
            }
        }
    }

    /// Поток `apap-decode` над ring сессии; возврат — после `Ready` (§6.18).
    /// Ошибка декодирования или таймаут заполнения — отказ открытия (ТЗ-87).
    fn spawn_decoder(
        &self,
        feed: EngineFeed,
        ring: EngineRing,
        plan: &EnginePlan,
        shared: &Arc<SessionShared>,
        cap_frames: usize,
        start_frame: u64,
    ) -> Result<DecodeWorker, String> {
        let (tx, rx) = mpsc::channel();
        let cfg = DecodeConfig {
            channels: plan.channels,
            start_frame,
            start_fill: start_fill_frames(cap_frames, plan.rate),
            tail_samples: DECODE_TAIL_FRAMES.saturating_mul(plan.channels.max(1)),
        };
        let tap = Some(self.viz_tap.clone());
        let spawned = match (feed, ring) {
            // DoP: tap отсутствует (ТЗ-108).
            (EngineFeed::Exact(f), EngineRing::I32(p)) if plan.kind == RingKind::Dop => {
                DecodeWorker::spawn(DecodeLoop::new(f, p, shared.clone(), cfg, tx, None))
            }
            (EngineFeed::Exact(f), EngineRing::I32(p)) => {
                DecodeWorker::spawn(DecodeLoop::new(f, p, shared.clone(), cfg, tx, tap))
            }
            (EngineFeed::Float(f), EngineRing::F32(p)) => {
                DecodeWorker::spawn(DecodeLoop::new(f, p, shared.clone(), cfg, tx, tap))
            }
            _ => return Err("internal: ring type does not match the feed".into()),
        };
        let worker = spawned.map_err(|e| format!("cannot spawn decode thread: {e}"))?;
        match rx.recv_timeout(START_FILL_TIMEOUT) {
            Ok(DecodeEvent::Ready) => Ok(worker),
            Ok(DecodeEvent::Failed(e)) => Err(format!("decoding failed: {e:?}")),
            Err(_) => Err("decoder did not fill the start buffer in time".into()),
        }
    }

    /// Ring + рендер + cpal-поток. При отказе `Fixed`-буфера поток повторяется
    /// с `Default` (источник отдаётся декодеру только после сборки потока).
    /// Возврат: поток, producer ring и ёмкость ring в кадрах.
    fn build_engine_stream(
        &self,
        plan: &EnginePlan,
        shared: &Arc<SessionShared>,
        spec: &mut OutputSpec,
    ) -> Result<(cpal::Stream, EngineRing, usize), StreamBuildError> {
        let ch = plan.channels.max(1);
        loop {
            let buffer_frames = match spec.config.buffer_size {
                BufferSize::Fixed(f) => Some(f),
                BufferSize::Default => None,
            };
            let cap_frames = ring_capacity(plan.rate, ch, self.ring_buffer_ms, buffer_frames) / ch;
            let period = buffer_frames
                .and_then(|f| usize::try_from(f).ok())
                .unwrap_or(0);
            let prime = prime_frames(period, plan.rate, cap_frames);
            let (render, ring) =
                build_render(plan, spec.sample_format, shared, cap_frames.saturating_mul(ch), prime)?;
            match self.build_stream(spec, render) {
                Ok(stream) => return Ok((stream, ring, cap_frames)),
                Err(e) => {
                    // `EBUSY` не лечится сменой буфера: решают ворота (ТЗ-122).
                    if matches!(spec.config.buffer_size, BufferSize::Fixed(_))
                        && e.open != super::reservation::gate::PcmOpenError::Busy
                    {
                        spec.config.buffer_size = BufferSize::Default;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Открытие отложено (ожидание резервирования / повтор `EBUSY`): трек
    /// загружен, поток будет открыт из [`Player::poll_reservation`].
    fn defer_open(&mut self, path: &Path, info: &TrackInfo, start_secs: f64) {
        self.pending = Some(PendingOpen { path: path.to_path_buf(), play: false, seek: start_secs });
        self.info = Some(info.clone());
        self.current_path = Some(path.to_path_buf());
        self.last_error = None;
    }

    /// True while the open waits for the device reservation (UI: «захват…», ТЗ-119).
    pub fn reservation_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Опрос резервирования на тике приложения (§8 С1): не блокирует.
    /// Получено — отложенное открытие выполняется и применяет намерения
    /// транспорта; отказ или `NameLost` — поток закрыт, воспроизведение
    /// остановлено, отката в Shared нет (ТЗ-119, ТЗ-122, И-Р20).
    pub fn poll_reservation(&mut self) -> Option<ReservationEvent> {
        let event = self.gate.as_mut()?.poll();
        match event {
            Some(GateEvent::Lost) => {
                if let Some(shared) = &self.shared {
                    shared.playing.store(false, Ordering::Release);
                }
                self.release_engine();
                let msg = capture_message(&CaptureFailure::ReservationLost);
                self.last_error = Some(msg.clone());
                return Some(ReservationEvent::Lost(msg));
            }
            Some(GateEvent::Failed(f)) => {
                self.pending.take()?;
                let msg = capture_message(&f);
                self.last_error = Some(msg.clone());
                return Some(ReservationEvent::Failed(msg));
            }
            Some(GateEvent::Ready) | None => {}
        }
        let held = self.gate.as_ref().is_some_and(|g| g.status() == GateStatus::Held);
        if !held {
            return None;
        }
        let intent = self.pending.take()?;
        if let Err(e) = self.open_at(&intent.path, intent.seek) {
            self.release_stream();
            self.last_error = Some(e.clone());
            return Some(ReservationEvent::Failed(e));
        }
        if let Some(again) = self.pending.as_mut() {
            // `EBUSY` в пределах окна: повтор на следующем тике.
            again.play = intent.play;
            return None;
        }
        if intent.play {
            self.play();
        }
        Some(ReservationEvent::Opened)
    }

    /// Standard path: PCM files or DSD decoded to PCM via the CIC cascade.
    /// The device/config are negotiated through [`select_output_for`] so
    /// `ExclusiveMode`/`FallbackPolicy`/`ResamplerMode` are applied (ТЗ A3.0
    /// §2–§4). Точный `ExactI32` без ресемплинга идёт в i32-ring, остальное —
    /// в f32-ring через SRC (§6.10, ADR-03). Сессия начинается с `start_secs`
    /// (фаза 1 seek при переоткрытии, §6.16).
    fn open_pcm(&mut self, path: &Path, start_secs: f64) -> Result<TrackInfo, String> {
        let src: Box<dyn AudioSource> = match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("dsf") || e.eq_ignore_ascii_case("dff") => {
                Box::new(DsdDecoder::open(path)?)
            }
            _ => Box::new(Decoder::open(path)?),
        };
        let (src_rate, src_ch) = (src.info().sample_rate, src.info().channels);
        let info = src.info().clone();

        self.teardown();

        let req = OutputRequest {
            track_rate: src_rate,
            track_channels: src_ch,
            preferred_device: self.preferred_device.clone(),
            exclusive: self.exclusive_mode,
            fallback: self.fallback_policy,
            resampler: self.resampler_mode,
            fallback_rate: self.fallback_rate,
            clock_family: self.prefer_family,
            fixed_rate: self.fixed_rate,
        };
        let spec = select_output_for(&req)?;
        let out_rate = spec.config.sample_rate;
        let out_ch = spec.config.channels as usize;
        self.device_desc = spec.device_name.clone();

        let resampler =
            Resampler::with_algo(src_rate, out_rate, src_ch, out_ch, self.resampler_algo);
        let resampled = resampler.is_enabled();
        let payload = src.ring_payload();
        let exact = matches!(payload, RingPayload::ExactI32 { .. }) && !resampled && src_ch == out_ch;
        let (kind, feed) = if exact {
            (RingKind::Exact(payload), EngineFeed::Exact(ExactFeed::new(src, out_rate)))
        } else {
            (
                RingKind::Float,
                EngineFeed::Float(FloatFeed::new(src, resampler, out_rate, src_ch, out_ch)),
            )
        };
        let plan = EnginePlan {
            kind,
            rate: out_rate,
            channels: out_ch,
            no_gain: self.bit_perfect,
            dither_seed: self.dither_seed(path),
        };
        let shared = Arc::new(SessionShared::new());
        self.apply_state(&shared);

        self.stream_desc = Some(StreamDesc {
            device: spec.device_name.clone(),
            rate: out_rate,
            channels: out_ch as u16,
            format: format_name(&spec.sample_format),
            exclusive: spec.exclusive,
            exclusive_fallback: req.exclusive != ExclusiveMode::Off && !spec.exclusive,
            resampled,
            source_rate: src_rate,
            source_channels: src_ch,
            dsd_mode: None,
            dsd_preferred: None,
            dsd_fallback_reason: None,
            fallback: spec.fallback,
        });

        let start_frame = secs_to_frames(start_secs, out_rate);
        let Some(stream) = self.start_engine(feed, plan, shared.clone(), spec, start_frame)? else {
            self.defer_open(path, &info, start_secs);
            return Ok(info);
        };
        if resampled && self.bit_perfect {
            eprintln!(
                "[audio] WARN: device does not support native rate {src_rate} Hz, \
                 resampling to {out_rate} Hz under Bit-perfect mode"
            );
        }
        if let Err(e) = stream.play() {
            self.last_error = Some(format!("Cannot start stream: {e}"));
        } else {
            self.last_error = None;
        }
        self.install(stream, shared, out_rate, out_ch, path, &info);
        Ok(info)
    }

    /// Сохранить собранную сессию как текущую.
    fn install(
        &mut self,
        stream: cpal::Stream,
        shared: Arc<SessionShared>,
        out_rate: u32,
        out_ch: usize,
        path: &Path,
        info: &TrackInfo,
    ) {
        self.stream = Some(stream);
        self.shared = Some(shared);
        self.out_rate = out_rate;
        self.out_ch = out_ch;
        self.end_ack = false;
        self.info = Some(info.clone());
        self.current_path = Some(path.to_path_buf());
    }

    /// DoP (DSD over PCM) path: the decoder packs the raw DSD bytes into
    /// `ExactI32` DoP payloads at the container rate (byte rate / 2, i.e. two
    /// bytes per channel per frame); `DopRender` adds the markers (§6.15). Any
    /// slot mismatch or stream failure is returned as `Err` — the DSD chain
    /// (§4.2) decides whether to fall back to PCM (honest CIC decoding)
    /// without mislabelling the mode.
    fn open_dop(&mut self, path: &Path, start_secs: f64) -> Result<TrackInfo, String> {
        let dop = DsdDecoder::open_with_mode(path, DecodeMode::Dop)?;
        // DSD64: byte rate = dsd_rate / 8 = 352800; DoP carries 2 bytes per
        // channel per 32-bit frame -> container rate 176400 (mpv/mpd framing).
        let dop_rate = dop.info().sample_rate * 4;
        let src_ch = dop.info().channels;
        let info = dop.info().clone();

        self.teardown();
        let req = OutputRequest {
            track_rate: dop_rate,
            track_channels: src_ch,
            preferred_device: self.preferred_device.clone(),
            exclusive: self.exclusive_mode,
            fallback: self.fallback_policy,
            resampler: self.resampler_mode,
            fallback_rate: self.fallback_rate,
            clock_family: self.prefer_family,
            fixed_rate: self.fixed_rate,
        };
        let mut spec = select_output_for(&req)?;

        // DoP только в Exclusive (ТЗ-1): в Shared цепочка DSD переходит к PCM.
        dop_requires_exclusive(spec.exclusive)?;
        // DoP is bit-exact only when the device opens the exact container slot.
        if spec.config.sample_rate != dop_rate || spec.config.channels as usize != src_ch {
            let offered = format!("{} Hz × {} ch", spec.config.sample_rate, spec.config.channels);
            return Err(format!(
                "device does not offer the DoP slot {dop_rate} Hz × {src_ch} ch (offers {offered})"
            ));
        }
        spec.sample_format = SampleFormat::I32;
        spec.is_dop = true;
        self.device_desc = spec.device_name.clone();

        // Direct Output: без громкости и дизеринга, слова DoP доходят без изменений.
        let plan = EnginePlan {
            kind: RingKind::Dop,
            rate: dop_rate,
            channels: src_ch,
            no_gain: true,
            dither_seed: None,
        };
        let shared = Arc::new(SessionShared::new());
        self.apply_state(&shared);

        self.stream_desc = Some(StreamDesc {
            device: spec.device_name.clone(),
            rate: dop_rate,
            channels: src_ch as u16,
            format: format_name(&spec.sample_format),
            exclusive: spec.exclusive,
            exclusive_fallback: req.exclusive != ExclusiveMode::Off && !spec.exclusive,
            resampled: false,
            source_rate: dop_rate,
            source_channels: src_ch,
            dsd_mode: None,
            dsd_preferred: None,
            dsd_fallback_reason: None,
            fallback: spec.fallback,
        });

        let feed = EngineFeed::Exact(ExactFeed::new(Box::new(dop), dop_rate));
        let start_frame = secs_to_frames(start_secs, dop_rate);
        let stream = match self.start_engine(feed, plan, shared.clone(), spec, start_frame) {
            Ok(Some(s)) => s,
            Ok(None) => {
                self.defer_open(path, &info, start_secs);
                return Ok(info);
            }
            Err(e) => {
                eprintln!("[audio] WARN: cannot build DoP stream ({e})");
                return Err(format!("cannot build DoP stream: {e}"));
            }
        };
        if let Err(e) = stream.play() {
            eprintln!("[audio] WARN: cannot start DoP stream ({e})");
            return Err(format!("cannot start DoP stream: {e}"));
        }
        self.install(stream, shared, dop_rate, src_ch, path, &info);
        self.last_error = None;
        Ok(info)
    }

    /// Rebuild a released engine (idle exclusive stream, V5.1-B6): reopen the
    /// saved current track at `secs` (0 = start) so `play()`/`toggle()` can
    /// resume. Сессия начинается с этой позиции (§6.18 `start_frame`).
    fn reopen_and_seek(&mut self, secs: f64) -> Result<(), String> {
        let Some(path) = self.current_path.clone() else {
            return Err("no track loaded".into());
        };
        self.open_at(&path, secs).map(|_| ())
    }

    /// Переоткрыть текущий трек с текущей позиции, сохранив play/pause:
    /// режим усиления и дизеринг фиксируются при сборке рендера (ОВС-18, ТЗ-8).
    fn reopen_current(&mut self) {
        if self.stream.is_none() {
            return;
        }
        let was_playing = self.is_playing();
        let pos = if self.at_end() { 0.0 } else { self.resume_pos_secs() };
        if let Err(e) = self.reopen_and_seek(pos) {
            self.last_error = Some(e);
            return;
        }
        if was_playing {
            self.play();
        }
    }

    /// Current playback position in seconds (used to restore resume position
    /// after an idle exclusive engine was released).
    fn resume_pos_secs(&self) -> f64 {
        self.shared
            .as_ref()
            .map(|s| s.pos_frames.load(Ordering::Relaxed) as f64 / f64::from(self.out_rate.max(1)))
            .unwrap_or(0.0)
    }

    /// Трек доигран: колбэк дошёл до `eof_frame`, и новый seek не запрошен (§6.17).
    fn at_end(&self) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|s| s.ended.load(Ordering::Acquire) && !s.seek_pending())
    }

    /// Start/resume playback. A finished track is rewound and replayed. When
    /// the engine was released (the exclusive node is freed on stop/pause,
    /// V5.1-B6) the current track is reopened at the saved position before
    /// the transport flag is raised.
    pub fn play(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            pending.play = true;
            return;
        }
        if self.stream.is_none() && self.current_path.is_some() {
            let target = if self.at_end() { 0.0 } else { self.resume_pos_secs() };
            if let Err(e) = self.reopen_and_seek(target) {
                self.last_error = Some(e);
                return;
            }
            if let Some(pending) = self.pending.as_mut() {
                pending.play = true;
                return;
            }
        }
        let at_end = self.at_end();
        let Some(shared) = self.shared.as_ref() else {
            self.last_error = Some("no track loaded".into());
            return;
        };
        if at_end {
            // Фаза 1 seek в начало (§6.16).
            shared.request_seek(0);
            shared.pos_frames.store(0, Ordering::Relaxed);
        }
        shared.playing.store(true, Ordering::Release);
        self.end_ack = false;
    }

    /// Returns `true` if a decoder (track) is currently loaded.
    pub fn has_decoder(&self) -> bool {
        self.shared.is_some()
    }

    /// Pause/resume the current track (rewinds if it had finished). Pausing
    /// releases an exclusive raw-`hw:` node immediately so the device returns
    /// to the system mixer; resuming rebuilds the engine and restores the
    /// playback position (V5.1-B6). A fresh reopen also re-broadcasts the DoP
    /// marker preludes, which a plain pause/resume of the same stream loses.
    pub fn toggle(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            pending.play = !pending.play;
            return;
        }
        if self.is_playing() {
            if let Some(shared) = &self.shared {
                shared.playing.store(false, Ordering::Release);
            }
            self.release_if_exclusive();
        } else {
            self.play();
        }
    }

    /// Stop playback and rewind to the start (фаза 1 seek, §6.16).
    pub fn stop(&mut self) {
        // Free an exclusive raw-`hw:` node: while the cpal stream is open the
        // ALSA PCM stays exclusively locked and disappears from the
        // PipeWire/wireplumber mixer (V5.1-B5).
        self.release_if_exclusive();
        let Some(shared) = self.shared.as_ref() else {
            return;
        };
        shared.playing.store(false, Ordering::Release);
        shared.request_seek(0);
        shared.pos_frames.store(0, Ordering::Relaxed);
        self.end_ack = false;
    }

    /// Seek to `secs` (clamped to >= 0): фаза 1 — запрос нового поколения и
    /// оптимистичная позиция; фазы 2–3 выполняют декодер и колбэк (§6.16).
    pub fn seek(&mut self, secs: f64) {
        if let Some(pending) = self.pending.as_mut() {
            pending.seek = secs.max(0.0);
            return;
        }
        let Some(shared) = self.shared.as_ref() else {
            return;
        };
        let target = secs_to_frames(secs, self.out_rate);
        shared.request_seek(target);
        shared.pos_frames.store(target, Ordering::Relaxed);
        self.end_ack = false;
    }

    /// Set volume, clamped to [0, 1]. Applied inside the audio callback
    /// (`AtomicGain`; в bit-perfect рендер собран без ступени громкости, ОВС-18).
    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(shared) = &self.shared {
            shared.set_gain(self.volume);
        }
    }

    /// Current volume in [0, 1].
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Mute/unmute without changing the volume.
    pub fn set_muted(&mut self, m: bool) {
        self.muted = m;
        if let Some(shared) = &self.shared {
            shared.muted.store(m, Ordering::Release);
        }
    }

    /// Mute state.
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Toggle mute.
    pub fn toggle_mute(&mut self) {
        let m = !self.muted;
        self.set_muted(m);
    }

    /// Enable/disable bit-perfect (Direct Output) mode: рендер собирается с
    /// `NoGain` (ОВС-18); смена режима переоткрывает поток с текущей позиции.
    pub fn set_bit_perfect(&mut self, enabled: bool) {
        if self.bit_perfect == enabled {
            return;
        }
        self.bit_perfect = enabled;
        self.reopen_current();
    }

    /// Current bit-perfect flag.
    pub fn bit_perfect(&self) -> bool {
        self.bit_perfect
    }

    /// True when bit-perfect mode is active while the device forces a software
    /// resample (native rate unsupported), i.e. bit-perfect is not guaranteed.
    /// Drives the «Resample (device limit)» status badge (ТЗ A2.0 §4.1).
    pub fn bit_perfect_resampled(&self) -> bool {
        self.shared.is_some()
            && self.bit_perfect
            && self.stream_desc.as_ref().is_some_and(|d| d.resampled)
    }

    /// Set the dither mode. Seed TPDF фиксируется при сборке рендера (ТЗ-8):
    /// смена режима переоткрывает поток с текущей позиции.
    pub fn set_dither(&mut self, dither: ResamplerDither) {
        let idx = dither_index(dither);
        if self.dither_idx == idx {
            return;
        }
        self.dither_idx = idx;
        self.reopen_current();
    }

    /// Whether the core is actively playing (not paused/stopped/finished).
    pub fn is_playing(&self) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|s| s.playing.load(Ordering::Acquire))
            && !self.at_end()
    }

    /// True when the current track played to its natural end (EOF) and the
    /// playlist has not handled it yet ([`Self::clear_end`]).
    pub fn ended(&self) -> bool {
        self.at_end() && !self.end_ack
    }

    /// Acknowledge the natural end (e.g. after the playlist handled it).
    pub fn clear_end(&mut self) {
        self.end_ack = true;
    }

    /// Attach (or detach) the visualizer tap producer. The worker writes
    /// post-resampler PCM into it only while [`Self::set_viz_tap_active`]
    /// keeps it enabled.
    pub fn set_viz_tap(&mut self, tap: Option<rtrb::Producer<f32>>) {
        if let Ok(mut slot) = self.viz_tap.lock() {
            *slot = tap;
        }
    }

    /// Toggle the tap on/off. When off, the worker skips the copy entirely
    /// (zero CPU cost), per ТЗ §13.2.
    pub fn set_viz_tap_active(&mut self, active: bool) {
        self.viz_active = active;
        if let Some(shared) = &self.shared {
            shared.viz_tap_active.store(active, Ordering::Relaxed);
        }
    }

    /// Return a snapshot for the UI: (playing, pos, duration).
    pub fn snapshot(&self) -> (bool, f64, Option<f64>) {
        let playing = self.is_playing();
        let pos = self.resume_pos_secs();
        let duration = self
            .info
            .as_ref()
            .and_then(|i| i.num_frames.map(|n| n as f64 / i.sample_rate as f64));
        (playing, pos, duration)
    }

    /// Output format of the current audio stream: (sample rate, channels).
    /// Used by the visualizer to drive the FFT (tap is post-resampler PCM).
    pub fn format(&self) -> (u32, usize) {
        match &self.shared {
            Some(_) => (self.out_rate, self.out_ch),
            None => (44_100, 2),
        }
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

/// DoP допустим только на Exclusive-выходе (ТЗ-1). Ошибка ведёт цепочку DSD
/// к следующему шагу (DSD→PCM).
fn dop_requires_exclusive(exclusive: bool) -> Result<(), String> {
    if exclusive {
        Ok(())
    } else {
        Err("DoP недоступен: выход открыт не в Exclusive (ТЗ-1)".into())
    }
}

#[cfg(test)]
impl Player {
    /// Test-only: build a `Player` without touching the audio backend.
    fn test_new() -> Self {
        Self {
            shared: None,
            worker: None,
            out_rate: 44_100,
            out_ch: 2,
            end_ack: false,
            viz_tap: Arc::new(Mutex::new(None)),
            stream: None,
            gate: None,
            pending: None,
            clock: MonotonicClock::new(),
            device_desc: String::from("test"),
            last_error: None,
            info: None,
            current_path: None,
            preferred_device: None,
            resampler_algo: ResamplerAlgorithm::SincMedium,
            dsd_mode: DsdMode::Pcm,
            exclusive_mode: ExclusiveMode::Auto,
            fallback_policy: FallbackPolicy::Nearest,
            resampler_mode: ResamplerMode::Auto,
            fixed_rate: 0,
            prefer_family: ClockFamily::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            stream_desc: None,
            #[cfg(test)]
            test_hooks: TestHooks::default(),
            ring_buffer_ms: RING_BUFFER_MS_DEFAULT,
            volume: 0.8,
            muted: false,
            bit_perfect: false,
            dither_idx: DITHER_INDEX_TPDF,
            viz_active: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decoder::TrackInfo;

    fn track_info(frames: usize) -> TrackInfo {
        TrackInfo {
            sample_rate: 44100,
            channels: 1,
            num_frames: Some(frames as u64),
            format_name: "mock".into(),
            bitrate: 0,
            bits: Some(16),
            tags: Default::default(),
        }
    }

    /// A `Player` with a live `SessionShared` but no output engine: enough to
    /// test transport/control state without touching a real backend.
    fn player_with_shared(frames: usize) -> Player {
        let mut p = Player::test_new();
        p.shared = Some(Arc::new(SessionShared::new()));
        p.out_rate = 44_100;
        p.out_ch = 1;
        p.info = Some(track_info(frames));
        p
    }

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

    #[test]
    fn set_ring_buffer_ms_clamps() {
        let mut p = Player::test_new();
        p.set_ring_buffer_ms(50);
        assert_eq!(p.ring_buffer_ms, crate::settings::RING_BUFFER_MS_MIN);
        p.set_ring_buffer_ms(3000);
        assert_eq!(p.ring_buffer_ms, 3000);
    }

    #[test]
    fn play_without_decoder_is_noop() {
        let mut p = Player::test_new();
        p.play();
        assert!(!p.is_playing());
    }

    #[test]
    fn toggle_pauses_and_resumes() {
        let mut p = player_with_shared(1000);
        p.toggle();
        assert!(p.is_playing(), "toggle should start playback");
        p.toggle();
        assert!(!p.is_playing(), "toggle should pause");
    }

    #[test]
    fn stop_rewinds_and_confirms_manual_end() {
        let mut p = player_with_shared(1000);
        p.play();
        p.stop();
        assert!(!p.is_playing());
        assert_eq!(p.snapshot().1, 0.0, "stop must rewind to start");
        let shared = p.shared.as_ref().expect("shared");
        assert!(shared.seek_pending(), "stop requests a seek to 0 (§6.16)");
        assert_eq!(shared.seek_target_frame.load(Ordering::Relaxed), 0);
        assert!(!p.ended(), "manual stop is not a natural end");
    }

    #[test]
    fn play_after_stop_replays_from_start() {
        let mut p = player_with_shared(1000);
        p.play();
        p.stop();
        p.play();
        assert!(p.is_playing());
        assert!(!p.ended());
        assert_eq!(p.snapshot().1, 0.0);
    }

    #[test]
    fn seek_moves_position() {
        let mut p = player_with_shared(100_000);
        p.seek(10.0);
        let (_, pos, _) = p.snapshot();
        assert!((pos - 10.0).abs() < 1e-6, "pos = {pos}");
    }

    #[test]
    fn volume_clamped_to_0_1() {
        let mut p = Player::test_new();
        p.set_volume(2.0);
        assert_eq!(p.volume(), 1.0);
        p.set_volume(-0.5);
        assert_eq!(p.volume(), 0.0);
        p.set_volume(0.6);
        assert_eq!(p.volume(), 0.6);
    }

    #[test]
    fn mute_flags_follow_toggles() {
        let mut p = Player::test_new();
        p.set_muted(true);
        assert!(p.muted());
        p.set_muted(false);
        assert!(!p.muted());
        p.toggle_mute();
        assert!(p.muted());
    }

    #[test]
    fn snapshot_reports_duration() {
        let p = player_with_shared(4_410_000);
        let (_, _, dur) = p.snapshot();
        assert_eq!(dur, Some(100.0));
    }

    #[test]
    fn bit_perfect_resampled_flag_updates_on_toggle() {
        let mut p = Player::test_new();
        // Identity resampler: native-rate match, resampling is unnecessary.
        p.shared = Some(Arc::new(SessionShared::new()));
        p.stream_desc = Some(StreamDesc { resampled: false, ..Default::default() });
        p.set_bit_perfect(true);
        assert!(!p.bit_perfect_resampled());

        // Device rejects the native rate: a real conversion starts, the
        // «bit-perfect not guaranteed» flag must flip (ТЗ A2.0 §4.1).
        p.stream_desc = Some(StreamDesc { resampled: true, ..Default::default() });
        p.set_bit_perfect(true);
        assert!(p.bit_perfect_resampled());

        // Switching bit-perfect off clears the badge even while resampling.
        p.set_bit_perfect(false);
        assert!(!p.bit_perfect_resampled());
    }

    /// PCM file from the environment (ставится вручную, см. §11.4); пропускает
    /// тест, если переменная не задана.
    fn env_pcm_path() -> Option<std::path::PathBuf> {
        let p = std::env::var_os("MUSIC_PCM_TEST_FILE")?;
        Some(std::path::PathBuf::from(p))
    }

    /// DSD-файл (`.dsf`/`.dff`) из окружения; None → тест пропускается.
    fn env_dsd_path() -> Option<std::path::PathBuf> {
        let p = std::env::var_os("MUSIC_DSD_TEST_FILE")?;
        Some(std::path::PathBuf::from(p))
    }

    /// Доступен ли на текущем устройстве по умолчанию exclusive (raw-нода).
    /// `Strict`-запрос успешен с `exclusive == true` только на hardware-узле.
    fn default_exclusive_capable() -> bool {
        let req = OutputRequest {
            track_rate: 44_100,
            track_channels: 2,
            preferred_device: None,
            exclusive: ExclusiveMode::Strict,
            fallback: FallbackPolicy::Nearest,
            resampler: ResamplerMode::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            clock_family: ClockFamily::Auto,
            fixed_rate: 0,
        };
        match select_output_for(&req) {
            Ok(spec) => spec.exclusive,
            Err(_) => false,
        }
    }

    /// Есть ли на текущем устройстве по умолчанию слот DoP для конкретного
    /// трека (контейнерная частота = DSD rate × 4).
    fn dop_slot_available_for(path: &std::path::Path) -> bool {
        let Ok(dop) = DsdDecoder::open_with_mode(path, DecodeMode::Dop) else {
            return false;
        };
        let dop_rate = dop.info().sample_rate * 4;
        let ch = dop.info().channels;
        let req = OutputRequest {
            track_rate: dop_rate,
            track_channels: ch,
            preferred_device: None,
            exclusive: ExclusiveMode::Off,
            fallback: FallbackPolicy::Nearest,
            resampler: ResamplerMode::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            clock_family: ClockFamily::Auto,
            fixed_rate: 0,
        };
        match select_output_for(&req) {
            Ok(spec) => spec.config.sample_rate == dop_rate && spec.config.channels as usize == ch,
            Err(_) => false,
        }
    }

    /// `ExclusiveMode::Strict` никогда не делает retry: ровно одна попытка
    /// сборки, затем Err (ТЗ A3.0 §4.3).
    #[test]
    fn open_with_exclusive_strict_on_server_fails() {
        let Some(path) = env_pcm_path() else {
            return;
        };
        let mut p = Player::test_new();
        p.set_exclusive_mode(ExclusiveMode::Strict);
        p.test_hooks.build_failures.store(999, Ordering::Relaxed);
        assert!(p.open(&path).is_err());
        let remaining = p.test_hooks.build_failures.load(Ordering::Relaxed);
        assert!(999 - remaining <= 1, "Strict must not retry the build");
    }

    /// Решение «освободить движок» принимается строго по флагу exclusive
    /// (V5.1-B5): shared-потоки и пустой descriptor не трогаем.
    #[test]
    fn release_decision_uses_exclusive_flag() {
        let shared_desc = StreamDesc {
            exclusive: false,
            ..Default::default()
        };
        assert!(!Player::exclusive_held(Some(&shared_desc)));
        let excl_desc = StreamDesc {
            exclusive: true,
            ..Default::default()
        };
        assert!(Player::exclusive_held(Some(&excl_desc)));
        assert!(!Player::exclusive_held(None));
    }

    /// `stop()` на exclusive-узле дропает stream+worker и возвращает узел
    /// системному микшеру, сохраняя transport-состояние; `play()`/`toggle()`
    /// переоткрывают движок по сохранённому пути (V5.1-B5). Требует
    /// exclusive-capable устройство по умолчанию и env `PCM_PATH`.
    #[test]
    fn stop_releases_exclusive_engine_and_play_reopens() {
        let Some(path) = env_pcm_path() else {
            return;
        };
        if !default_exclusive_capable() {
            eprintln!("skipped: default device is not exclusive-capable");
            return;
        }
        let mut p = Player::test_new();
        p.set_exclusive_mode(ExclusiveMode::Strict);
        assert!(p.open(&path).is_ok());
        let desc = p.stream_desc().unwrap();
        assert!(desc.exclusive, "this test needs an exclusive stream");
        assert!(p.stream.is_some());
        assert_eq!(p.current_path.as_deref(), Some(path.as_path()));

        p.stop();
        assert!(p.stream.is_none(), "exclusive stream must be released on stop");
        assert!(p.worker.is_none(), "worker must be released on stop");
        assert!(p.has_decoder(), "transport state kept for the loaded track");
        assert_eq!(p.current_path.as_deref(), Some(path.as_path()));
        assert!(!p.is_playing());

        p.play();
        assert!(p.stream.is_some(), "play() must rebuild the engine");
        assert!(p.is_playing());

        p.stop();
        assert!(p.stream.is_none());
    }

    /// Пауза на exclusive-узле освобождает движок (устройство возвращается в
    /// системный микшер немедленно); ресюм переоткрывает поток и
    /// восстанавливает позицию воспроизведения (V5.1-B6). Требует
    /// exclusive-capable устройство по умолчанию и env `PCM_PATH`.
    #[test]
    fn pause_releases_exclusive_and_resume_restores_position() {
        let Some(path) = env_pcm_path() else {
            return;
        };
        if !default_exclusive_capable() {
            eprintln!("skipped: default device is not exclusive-capable");
            return;
        }
        let mut p = Player::test_new();
        p.set_exclusive_mode(ExclusiveMode::Strict);
        assert!(p.open(&path).is_ok());
        assert!(p.stream_desc().unwrap().exclusive);
        p.play();
        assert!(p.is_playing());
        p.seek(10.0);

        p.toggle();
        assert!(!p.is_playing());
        assert!(p.stream.is_none(), "pause must release the exclusive engine");
        assert!(p.has_decoder(), "transport state kept for the loaded track");

        p.toggle();
        assert!(p.is_playing());
        assert!(p.stream.is_some(), "resume must rebuild the engine");
        let (_, pos, _) = p.snapshot();
        assert!(
            (pos - 10.0).abs() < 1.0,
            "resume must restore the paused position, pos = {pos}"
        );

        p.stop();
    }

    /// ТЗ-1, И-Т3 (в объёме старого пути): DoP-поток не строится на выходе,
    /// открытом не в Exclusive.
    /// ТЗ-17 (в объёме старого пути): ±full scale, downmix 5.1→2, SRC синуса
    /// 0.99·fs/2 амплитуды 1.0 (44,1 → 192 кГц) → во всех форматах значения в
    /// границах и нет соседних сэмплов с разностью > 2^N − 2^(N−2).
    #[test]
    fn no_wraparound_under_full_scale_processing() {
        let src_rate = 44_100u32;
        let frames = 4_096usize;
        let f = 0.99 * f64::from(src_rate) / 2.0;
        let mut input = Vec::with_capacity(frames * 6);
        for n in 0..frames {
            // Первая половина — синус у Найквиста, вторая — блоки ±full scale.
            let v = if n < frames / 2 {
                (std::f64::consts::TAU * f * n as f64 / f64::from(src_rate)).sin() as f32
            } else if (n / 256) % 2 == 0 {
                1.0
            } else {
                -1.0
            };
            input.extend(std::iter::repeat_n(v, 6));
        }
        let mut rs = Resampler::with_algo(src_rate, 192_000, 6, 2, ResamplerAlgorithm::SincSlow);
        let mut processed = Vec::new();
        let mut pos = 0;
        let mut out = vec![0.0f32; 8_192];
        while pos < input.len() {
            let pushed = rs.push(&input[pos..]) * 6;
            pos += pushed;
            let n = rs.pull(&mut out, 4_096, false);
            processed.extend_from_slice(&out[..n * 2]);
            assert!(pushed > 0 || n > 0, "ресемплер не продвигается");
        }
        loop {
            let n = rs.pull(&mut out, 4_096, true);
            if n == 0 {
                break;
            }
            processed.extend_from_slice(&out[..n * 2]);
        }
        assert!(processed.iter().any(|x| x.abs() > 1.0), "вход должен давать выбросы за full scale");
        let len = processed.len() / 2 * 2;

        fn check<T: Copy + Into<f64>>(name: &str, out: &[T], bits: u32, lo: f64, hi: f64) {
            let limit = 2f64.powi(bits as i32) - 2f64.powi(bits as i32 - 2);
            for (i, v) in out.iter().enumerate() {
                let x: f64 = (*v).into();
                assert!(x >= lo && x <= hi, "{name}: значение {x} вне границ");
                if i >= 2 {
                    let prev: f64 = out[i - 2].into();
                    assert!((x - prev).abs() <= limit, "{name}: скачок {prev} → {x} (переход через знак)");
                }
            }
        }
        let samples = &processed[..len];
        for dither in [None, Some(1)] {
            let f32_out: Vec<f64> = render_bytes(SampleFormat::F32, samples, dither)
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| f64::from(f32::from_le_bytes(b)) * 32_768.0)
                .collect();
            check("f32", &f32_out, 16, -32_768.0, 32_768.0);
            let i16_out: Vec<i16> = render_bytes(SampleFormat::I16, samples, dither)
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| i16::from_le_bytes(b))
                .collect();
            check("i16", &i16_out, 16, f64::from(i16::MIN), f64::from(i16::MAX));
            let i32_out: Vec<i32> = render_bytes(SampleFormat::I32, samples, dither)
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| i32::from_le_bytes(b))
                .collect();
            check("i32", &i32_out, 32, f64::from(i32::MIN), f64::from(i32::MAX));
        }
    }

    /// Один период `PcmRender` над f32-ring со стереосигналом `samples` (§6.14).
    fn render_bytes(format: SampleFormat, samples: &[f32], dither: Option<u32>) -> Vec<u8> {
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(samples.len().max(1));
        for &x in samples {
            assert!(producer.push(x).is_ok());
        }
        let shared = Arc::new(SessionShared::new());
        shared.playing.store(true, Ordering::Release);
        let core = RenderCore::new(consumer, shared, 2, 0);
        let tpdf = dither.map_or_else(Tpdf::off, Tpdf::with_seed);
        let mut render = pcm_render_for::<f32, NoGain>(format, core, RingPayload::F32, tpdf)
            .expect("рендер для формата");
        let mut out = vec![0u8; samples.len() * format.sample_size()];
        render.render(&mut out);
        out
    }

    #[test]
    fn old_path_dop_requires_exclusive() {
        assert!(dop_requires_exclusive(false).is_err());
        assert!(dop_requires_exclusive(true).is_ok());
    }

    /// Цепочка `Native → [Native, DoP, Pcm]`: Native не реализован, DoP
    /// падает на сборке (mock), выигрывает PCM. ТЗ A3.0 §4.2.
    #[test]
    fn open_dsd_chain_native_prefers_dop_when_native_fails() {
        let Some(path) = env_dsd_path() else {
            return;
        };
        if !dop_slot_available_for(&path) {
            eprintln!("skipped: device lacks the DoP slot for this file");
            return;
        }
        let mut p = Player::test_new();
        p.set_dsd_mode(DsdMode::Native);
        p.set_exclusive_mode(ExclusiveMode::Off);
        p.test_hooks.build_failures.store(1, Ordering::Relaxed);
        assert!(p.open(&path).is_ok());
        let desc = p.stream_desc().unwrap();
        assert_eq!(desc.dsd_mode, Some(DsdMode::Pcm));
        assert_eq!(desc.dsd_preferred, Some(DsdMode::Native));
        assert!(desc.dsd_fallback_reason.is_some());
    }

    /// Цепочка `DoP → [DoP, Pcm]`: DoP падает, выигрывает PCM (§4.2).
    #[test]
    fn open_dsd_chain_dop_falls_to_pcm() {
        let Some(path) = env_dsd_path() else {
            return;
        };
        if !dop_slot_available_for(&path) {
            eprintln!("skipped: device lacks the DoP slot for this file");
            return;
        }
        let mut p = Player::test_new();
        p.set_dsd_mode(DsdMode::DoP);
        p.set_exclusive_mode(ExclusiveMode::Off);
        p.test_hooks.build_failures.store(1, Ordering::Relaxed);
        assert!(p.open(&path).is_ok());
        let desc = p.stream_desc().unwrap();
        assert_eq!(desc.dsd_mode, Some(DsdMode::Pcm));
        assert_eq!(desc.dsd_preferred, Some(DsdMode::DoP));
        assert!(desc.dsd_fallback_reason.is_some());
    }

    /// `FallbackPolicy::Fail` останавливает цепочку на первом провале — DoP
    /// даже не пробуется (§4.1–4.2).
    #[test]
    fn open_dsd_chain_fail_policy_stops_chain() {
        let Some(path) = env_dsd_path() else {
            return;
        };
        let mut p = Player::test_new();
        p.set_dsd_mode(DsdMode::Native);
        p.set_exclusive_mode(ExclusiveMode::Off);
        p.set_fallback_policy(FallbackPolicy::Fail);
        p.test_hooks.build_failures.store(0, Ordering::Relaxed);
        let err = p.open(&path).expect_err("Fail policy must reject the chain");
        assert!(err.contains("Native"), "{err}");
        assert_eq!(
            p.test_hooks.build_failures.load(Ordering::Relaxed),
            0,
            "no step after the failed one may attempt a build"
        );
    }

    mod gated {
        use super::super::{capture_message, gated_build};
        use crate::audio::clock::{Clock, ClockInstant};
        use crate::audio::output::StreamBuildError;
        use crate::audio::reservation::gate::{ExclusiveGate, GateEvent, PcmOpenError};
        use crate::audio::reservation::{BusReservationService, ReserveBus};
        use crate::audio::testing::{BusCall, FakeOwner, FakeReserveBus, FakeServerProbe, ManualClock};
        use std::sync::{mpsc, Arc};
        use std::time::{Duration, Instant};

        fn gate(fake: &FakeReserveBus) -> ExclusiveGate {
            let (tx, rx) = mpsc::channel();
            let bus: Arc<dyn ReserveBus> = Arc::new(fake.clone());
            let service = BusReservationService::new(bus, Box::new(FakeServerProbe::desktop()), tx);
            ExclusiveGate::new(Box::new(service), rx)
        }

        fn wait(g: &mut ExclusiveGate) -> GateEvent {
            let start = Instant::now();
            loop {
                if let Some(ev) = g.poll() {
                    return ev;
                }
                assert!(start.elapsed() < Duration::from_secs(5), "служба не ответила");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn has_open(fake: &FakeReserveBus) -> bool {
            fake.calls().iter().any(|c| matches!(c, BusCall::PcmOpen { .. }))
        }

        /// ТЗ-48, И-Р1: `hw:` не открывается до резервирования; ожидание не блокирует.
        #[test]
        fn exclusive_open_waits_for_reservation() {
            let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
            let mut g = gate(&fake);
            let first = gated_build(&mut g, 1, "DAC", ClockInstant::START, || Ok(fake.open_pcm(1)));
            assert!(matches!(first, Ok(None)));
            assert!(!has_open(&fake), "PCM открыт до резервирования");
            assert_eq!(wait(&mut g), GateEvent::Ready);
            let opened = gated_build(&mut g, 1, "DAC", ClockInstant::START, || Ok(fake.open_pcm(1)));
            assert!(matches!(opened, Ok(Some(_))));
            let calls = fake.calls();
            let req = calls.iter().position(|c| matches!(c, BusCall::RequestName { .. })).unwrap();
            let open = calls.iter().position(|c| matches!(c, BusCall::PcmOpen { .. })).unwrap();
            assert!(req < open, "{calls:?}");
        }

        /// ТЗ-122: `EBUSY` при удерживаемом резервировании повторяется ≤ 1 с,
        /// затем — ошибка без отката в Shared.
        #[test]
        fn exclusive_open_ebusy_retries_then_fails() {
            let clock = ManualClock::new();
            let fake = FakeReserveBus::new(clock.clone(), FakeOwner::Free);
            let mut g = gate(&fake);
            assert!(matches!(gated_build::<()>(&mut g, 0, "DAC", clock.now(), || panic!("сборка до резервирования")), Ok(None)));
            assert_eq!(wait(&mut g), GateEvent::Ready);
            let busy = || Err::<(), _>(StreamBuildError { open: PcmOpenError::Busy, message: "busy".into() });
            let mut retries = 0;
            let err = loop {
                match gated_build(&mut g, 0, "DAC", clock.now(), busy) {
                    Ok(None) => {
                        retries += 1;
                        clock.advance(Duration::from_millis(100));
                    }
                    Ok(Some(())) => panic!("открыто при EBUSY"),
                    Err(e) => break e,
                }
                assert!(retries < 100);
            };
            assert!(retries >= 9, "повторов {retries}");
            assert!(err.contains("вне протокола"), "{err}");
            assert!(!has_open(&fake));
        }

        /// ТЗ-118, ТЗ-122: отказ владельца — ошибка, PCM не открывается.
        #[test]
        fn exclusive_open_denied_does_not_open() {
            let owner = FakeOwner::Denies { app: Some("PipeWire".into()) };
            let fake = FakeReserveBus::new(ManualClock::new(), owner);
            let mut g = gate(&fake);
            assert!(matches!(gated_build::<()>(&mut g, 0, "DAC", ClockInstant::START, || panic!("сборка до резервирования")), Ok(None)));
            let GateEvent::Failed(f) = wait(&mut g) else { panic!("ожидался отказ") };
            assert!(capture_message(&f).contains("PipeWire"), "{}", capture_message(&f));
            assert!(!has_open(&fake));
        }
    }

    // --- Сквозные тесты тракта декодер → ring → колбэк (§7.2) ---------------

    use crate::audio::error::FileError;
    use crate::audio::format::BitDepth;
    use crate::audio::worker::{Feed, Step};

    const E2E_PERIOD: usize = 256;
    const E2E_CHUNK: u64 = 64;
    const E2E_CAP: usize = 4096;
    const E2E_TOTAL: u64 = 1 << 22;

    /// Значение кадра `f` счётчика: ненулевое, ExactI32 с 24 значащими битами.
    fn counter_word(f: u64) -> i32 {
        i32::try_from((f + 1) << 8).expect("счётчик в пределах i32")
    }

    /// Моно-счётчик кадров; после seek `stall` порций приходят пустыми
    /// (медленный источник, §6.17).
    struct E2eFeed {
        pos: u64,
        stall_after_seek: usize,
        stall_left: usize,
    }

    impl Feed<i32> for E2eFeed {
        fn refill(&mut self, out: &mut Vec<i32>) -> Result<bool, FileError> {
            if self.stall_left > 0 {
                self.stall_left -= 1;
                return Ok(true);
            }
            if self.pos >= E2E_TOTAL {
                return Ok(false);
            }
            let end = (self.pos + E2E_CHUNK).min(E2E_TOTAL);
            out.extend((self.pos..end).map(counter_word));
            self.pos = end;
            Ok(true)
        }
        fn seek(&mut self, frame: u64) {
            self.pos = frame;
            self.stall_left = self.stall_after_seek;
        }
    }

    /// Декодер и колбэк I32/`NoGain` над общим ring: выход — исходные слова.
    struct E2e {
        lp: DecodeLoop<i32, E2eFeed>,
        render: Box<dyn RawRender>,
        shared: Arc<SessionShared>,
        _rx: std::sync::mpsc::Receiver<DecodeEvent>,
        buf: Vec<u8>,
    }

    impl E2e {
        fn new(prime: usize, stall_after_seek: usize) -> Self {
            let shared = Arc::new(SessionShared::new());
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(E2E_CAP);
            let (tx, rx) = std::sync::mpsc::channel();
            let feed = E2eFeed { pos: 0, stall_after_seek, stall_left: stall_after_seek };
            let cfg = DecodeConfig { channels: 1, start_frame: 0, start_fill: 0, tail_samples: 64 };
            let lp = DecodeLoop::new(feed, producer, shared.clone(), cfg, tx, None);
            let core = RenderCore::new(consumer, shared.clone(), 1, prime);
            let payload = RingPayload::ExactI32 { valid_bits: BitDepth::new(24).expect("24 бита") };
            let render = pcm_render_for::<i32, NoGain>(SampleFormat::I32, core, payload, Tpdf::off())
                .expect("рендер I32");
            shared.playing.store(true, Ordering::Release);
            Self { lp, render, shared, _rx: rx, buf: vec![0u8; E2E_PERIOD * 4] }
        }

        /// До `steps` итераций декодера (останов на заполненном ring).
        fn pump(&mut self, steps: usize) {
            for _ in 0..steps {
                match self.lp.step() {
                    Step::Wrote(_) => {}
                    Step::RingFull | Step::WaitAck | Step::Ended => break,
                }
            }
        }

        /// Один период колбэка; слова выхода.
        fn period(&mut self) -> Vec<i32> {
            self.render.render(&mut self.buf);
            self.buf
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| i32::from_le_bytes(b))
                .collect()
        }

        fn underruns(&self) -> u32 {
            self.shared.underruns.load(Ordering::Relaxed)
        }
    }

    /// Слова без тишины — непрерывный счётчик от кадра `first`.
    fn assert_continuous_from(words: &[i32], first: u64) {
        let data: Vec<i32> = words.iter().copied().filter(|&w| w != 0).collect();
        assert!(!data.is_empty(), "нет данных");
        for (i, &w) in data.iter().enumerate() {
            assert_eq!(w, counter_word(first + i as u64), "разрыв на позиции {i}");
        }
    }

    /// Детерминированный LCG для кадров seek.
    fn lcg(state: &mut u64) -> u64 {
        *state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        *state >> 33
    }

    #[test]
    fn pause_resume_100x_counter_stream_is_continuous() {
        let mut e = E2e::new(0, 0);
        let mut words = Vec::new();
        for _ in 0..100 {
            e.pump(usize::MAX);
            for _ in 0..3 {
                words.extend(e.period());
            }
            e.shared.playing.store(false, Ordering::Release);
            for _ in 0..3 {
                let silent = e.period();
                assert!(silent.iter().all(|&w| w == 0), "пауза выдаёт тишину");
                words.extend(silent);
            }
            e.shared.playing.store(true, Ordering::Release);
        }
        assert_continuous_from(&words, 0);
        assert_eq!(words.iter().filter(|&&w| w != 0).count(), 100 * 3 * E2E_PERIOD);
        assert_eq!(e.underruns(), 0);
    }

    /// Seek на `target`: фаза 1, фаза 2 в декодере, фаза 3 в колбэке, затем
    /// данные до первого ненулевого слова. Возвращает слова после seek.
    fn seek_and_play(e: &mut E2e, target: u64, periods: usize) -> Vec<i32> {
        e.shared.request_seek(target);
        assert_eq!(e.lp.step(), Step::WaitAck);
        assert!(e.period().iter().all(|&w| w == 0), "фаза 3 — тишина");
        let mut words = Vec::new();
        for _ in 0..periods {
            e.pump(8);
            words.extend(e.period());
        }
        words
    }

    #[test]
    fn seek_1000x_first_sample_is_target() {
        let mut e = E2e::new(512, 0);
        let mut rng = 7u64;
        for _ in 0..1000 {
            let target = lcg(&mut rng) % (E2E_TOTAL - (1 << 16));
            let words = seek_and_play(&mut e, target, 6);
            let first = words.iter().copied().find(|&w| w != 0).expect("данные после seek");
            assert_eq!(first, counter_word(target));
            assert_continuous_from(&words, target);
        }
        assert_eq!(e.underruns(), 0);
    }

    #[test]
    fn no_false_underrun_after_seek_and_start() {
        // Медленный источник: 20 пустых порций после старта и каждого seek.
        let mut e = E2e::new(512, 20);
        let mut words = Vec::new();
        for _ in 0..8 {
            e.pump(8);
            words.extend(e.period());
        }
        assert_eq!(words[0], 0, "до набора prime_frames — тишина");
        assert_continuous_from(&words, 0);
        let mut rng = 11u64;
        for _ in 0..1000 {
            let target = lcg(&mut rng) % (E2E_TOTAL - (1 << 16));
            let words = seek_and_play(&mut e, target, 8);
            assert_eq!(words[0], 0, "до набора prime_frames после seek — тишина");
            assert_continuous_from(&words, target);
        }
        assert_eq!(e.underruns(), 0);
    }

    #[test]
    fn real_starvation_after_priming_counts() {
        let mut e = E2e::new(512, 0);
        let mut words = Vec::new();
        for _ in 0..4 {
            e.pump(8);
            words.extend(e.period());
        }
        assert_eq!(e.underruns(), 0);
        // Декодер встаёт: ring исчерпывается, затем ровно 3 голодных периода.
        let mut starved = 0;
        while starved < 3 {
            let p = e.period();
            if p.contains(&0) {
                starved += 1;
            }
            words.extend(p);
        }
        assert_eq!(e.underruns(), 3);
        // Декодер вернулся: поток продолжается без пропусков, счётчик не растёт.
        for _ in 0..4 {
            e.pump(8);
            words.extend(e.period());
        }
        assert_continuous_from(&words, 0);
        assert_eq!(e.underruns(), 3);
    }
}

