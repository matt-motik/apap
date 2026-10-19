use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat};

use super::decoder::{AudioSource, Decoder, TrackInfo};
use super::dsd::{DecodeMode, DsdDecoder};
use super::error::{EngineFault, FileError};
use super::output::{
    build_output_stream_raw, select_output_for, FallbackReason, OutputRequest, OutputSpec,
    PcmOpenError, RawRender, Resampler, StreamBuildError,
};
use super::render::prime_frames;
use super::session::{RingPayload, SessionShared};
use super::session_build::{
    build_render, ring_capacity, spawn_decoder, EngineFeed, EnginePlan, EngineRing, RingKind,
    SpawnDecoderError,
};
use super::worker::{DecodeEvent, DecodeWorker, ExactFeed, FloatFeed, VizTap};
use crate::engine::spawner::{StdSpawner, ThreadSpawner};
use crate::settings::{
    clamp_ring_buffer_ms, ClockFamily, DsdMode, FallbackPolicy,
    FallbackRatePolicy, ResamplerAlgorithm, ResamplerDither, ResamplerMode, RING_BUFFER_MS_DEFAULT,
};

/// Индекс режима дизеринга из [`ResamplerDither`]; фиксируется при сборке
/// рендера (seed TPDF, ТЗ-8).
const DITHER_INDEX_TPDF: u8 = 0;
const DITHER_INDEX_OFF: u8 = 2;

/// Map a config [`ResamplerDither`] to the atomically-stored index.
fn dither_index(d: ResamplerDither) -> u8 {
    match d {
        ResamplerDither::Tpdf => DITHER_INDEX_TPDF,
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

/// Секунды → кадр сессии (отрицательные и нечисловые — 0).
fn secs_to_frames(secs: f64, rate: u32) -> u64 {
    let frames = (secs.max(0.0) * f64::from(rate)).round();
    if frames.is_finite() {
        frames as u64
    } else {
        0
    }
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
    pub device_desc: String,
    pub last_error: Option<String>,
    /// Current track info (duration/rate), kept for `snapshot`.
    info: Option<TrackInfo>,
    /// Path of the currently loaded track: lets `reopen_current` rebuild the
    /// stream after a parameter change (bit-perfect/dither, ТЗ-8).
    current_path: Option<PathBuf>,
    preferred_device: Option<String>,
    /// Resampling algorithm used by every new stream (ТЗ 5.1 §8.3).
    resampler_algo: ResamplerAlgorithm,
    /// DSD output mode (Pcm / Native / DoP, ТЗ 5.1 §8.2).
    dsd_mode: DsdMode,
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
    /// Подмена запуска потоков движком (ТЗ-88, ADR-20); по умолчанию — боевой
    /// `StdSpawner`.
    spawner: Arc<dyn ThreadSpawner>,
    /// Отказ запуска потока `apap-decode` последнего `open` (ТЗ-88, §6.18
    /// шаг 19): движок читает его через [`Player::take_fault`], чтобы отличить
    /// `OpenFailed(Internal(..))` от классификации по файлу.
    last_fault: Option<EngineFault>,
    /// Приёмник событий декодера, оставленный живым после `Ready` (ТЗ-87,
    /// §6.18 шаг 19): мид-трековый `DecodeEvent::Failed` до этого терялся —
    /// воркер уходил в EOF, и трек выглядел нормально доигранным. Опрашивается
    /// неблокирующе через [`Player::poll_decode_failure`].
    decode_events: Option<mpsc::Receiver<DecodeEvent>>,
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
            device_desc,
            last_error: None,
            info: None,
            current_path: None,
            preferred_device: None,
            resampler_algo: ResamplerAlgorithm::SincMedium,
            dsd_mode: DsdMode::Pcm,
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
            spawner: Arc::new(StdSpawner),
            last_fault: None,
            decode_events: None,
        }
    }

    /// Подмена запуска потоков (ТЗ-88, ADR-20): движок передаёт свой `ThreadSpawner`.
    pub fn set_spawner(&mut self, spawner: Arc<dyn ThreadSpawner>) {
        self.spawner = spawner;
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

    /// Release the cpal stream and the worker while keeping the shared
    /// transport state (position/flags) so the UI does not reset. The ALSA
    /// `hw:*` PCM is snd_pcm_close'd on Drop → the node returns to the
    /// PipeWire/wireplumber graph (V5.1-B5).
    pub fn release_engine(&mut self) {
        self.stream = None;
        self.worker = None;
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
        self.last_fault = None;
        self.open_at(path, 0.0)
    }

    /// Забирает отказ запуска `apap-decode` последнего `open`, если он был
    /// (ТЗ-88, §6.18 шаг 19): движок различает `OpenFailed(Internal(..))` и
    /// классификацию по файлу (`SkipReason::File`) по наличию этого отказа.
    pub fn take_fault(&mut self) -> Option<EngineFault> {
        self.last_fault.take()
    }

    /// Неблокирующий опрос мид-трекового отказа декодера (ТЗ-87, §6.18
    /// шаг 19): `Ready` игнорируется, пустой/отключённый канал — `None`.
    /// Вызывается на каждой итерации цикла движка ([`Engine::poll_session`]),
    /// пока трек играет — до этого `DecodeEvent::Failed` после `Ready` терялся,
    /// и воркер молча уходил в EOF (трек выглядел доигранным штатно).
    pub fn poll_decode_failure(&mut self) -> Option<FileError> {
        let rx = self.decode_events.as_ref()?;
        loop {
            match rx.try_recv() {
                Ok(DecodeEvent::Ready) => continue,
                Ok(DecodeEvent::Failed(e)) => return Some(e),
                Err(_) => return None,
            }
        }
    }

    /// [`Player::open`] с началом сессии в `start_secs` (фаза 1 seek, §6.16):
    /// декодер встаёт на позицию до стартового заполнения (§6.18).
    fn open_at(&mut self, path: &Path, start_secs: f64) -> Result<TrackInfo, String> {
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

    /// Собрать поток и запустить `apap-decode`. Отказ сборки Exclusive-потока
    /// возвращается как ошибка: отката в Shared нет ни при каком
    /// `ExclusiveMode` (ТЗ-119, ТЗ-122; DoP — ТЗ-1). Поток возвращается после
    /// стартового заполнения ring (§6.18). Резервирование карты и повторы
    /// `EBUSY` (ТЗ-48, ТЗ-122) ведёт `AlsaExclusiveBackend` (§8 С6) — старый
    /// cpal-путь `hw:` резервирования не делает.
    fn start_engine(
        &mut self,
        feed: EngineFeed,
        plan: EnginePlan,
        shared: Arc<SessionShared>,
        mut spec: OutputSpec,
        start_frame: u64,
    ) -> Result<cpal::Stream, String> {
        let (stream, ring, cap_frames) = self
            .build_engine_stream(&plan, &shared, &mut spec)
            .map_err(|e| e.message)?;
        match self.spawn_decoder(feed, ring, &plan, &shared, cap_frames, start_frame) {
            Ok((worker, rx)) => {
                self.worker = Some(worker);
                self.decode_events = Some(rx);
                Ok(stream)
            }
            Err(e) => {
                drop(stream);
                Err(e)
            }
        }
    }

    /// Поток `apap-decode` над ring сессии; возврат — после `Ready` (§6.18).
    /// Тонкая обёртка над [`session_build::spawn_decoder`]: отказ запуска
    /// потока (`ThreadSpawner`) сохраняется в `last_fault` (ТЗ-88, §6.18 шаг
    /// 19) — движок отличает его от классификации по файлу.
    fn spawn_decoder(
        &mut self,
        feed: EngineFeed,
        ring: EngineRing,
        plan: &EnginePlan,
        shared: &Arc<SessionShared>,
        cap_frames: usize,
        start_frame: u64,
    ) -> Result<(DecodeWorker, mpsc::Receiver<DecodeEvent>), String> {
        match spawn_decoder(
            self.spawner.as_ref(),
            self.viz_tap.clone(),
            feed,
            ring,
            plan,
            shared,
            cap_frames,
            start_frame,
        ) {
            Ok(pair) => Ok(pair),
            Err(SpawnDecoderError::Spawn(e)) => {
                self.last_fault = Some(e.clone());
                Err(format!("cannot spawn decode thread: {e:?}"))
            }
            Err(SpawnDecoderError::Open(msg)) => Err(msg),
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
                        && e.open != PcmOpenError::Busy
                    {
                        spec.config.buffer_size = BufferSize::Default;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Standard path: PCM files or DSD decoded to PCM via the CIC cascade.
    /// The device/config are negotiated through [`select_output_for`] so
    /// `FallbackPolicy`/`ResamplerMode` are applied (ТЗ A3.0
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
            exclusive: false,
            exclusive_fallback: false,
            resampled,
            source_rate: src_rate,
            source_channels: src_ch,
            dsd_mode: None,
            dsd_preferred: None,
            dsd_fallback_reason: None,
            fallback: spec.fallback,
        });

        let start_frame = secs_to_frames(start_secs, out_rate);
        let stream = self.start_engine(feed, plan, shared.clone(), spec, start_frame)?;
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
            fallback: self.fallback_policy,
            resampler: self.resampler_mode,
            fallback_rate: self.fallback_rate,
            clock_family: self.prefer_family,
            fixed_rate: self.fixed_rate,
        };
        let mut spec = select_output_for(&req)?;

        // DoP только в Exclusive (ТЗ-1); старый путь — только Совместимый
        // (Shared, §8 С6), поэтому цепочка DSD всегда переходит к PCM.
        dop_requires_exclusive(false)?;
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
            exclusive: false,
            exclusive_fallback: false,
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
            Ok(s) => s,
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

    /// Переоткрыть текущий трек с текущей позиции, сохранив play/pause:
    /// режим усиления и дизеринг фиксируются при сборке рендера (ОВС-18, ТЗ-8).
    fn reopen_current(&mut self) {
        if self.stream.is_none() {
            return;
        }
        let was_playing = self.is_playing();
        let pos = if self.at_end() { 0.0 } else { self.resume_pos_secs() };
        let Some(path) = self.current_path.clone() else {
            self.last_error = Some("no track loaded".into());
            return;
        };
        if let Err(e) = self.open_at(&path, pos) {
            self.last_error = Some(e);
            return;
        }
        if was_playing {
            self.play();
        }
    }

    /// Current playback position in seconds (used to restore the resume
    /// position when reopening the track with new parameters).
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

    /// Start/resume playback. A finished track is rewound and replayed.
    pub fn play(&mut self) {
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

    /// Pause/resume the current track (rewinds if it had finished). Пауза
    /// гасит `SessionShared::playing` (колбэк пишет тишину); поток вывода не
    /// освобождается (ТЗ-43).
    pub fn toggle(&mut self) {
        if self.is_playing() {
            if let Some(shared) = &self.shared {
                shared.playing.store(false, Ordering::Release);
            }
        } else {
            self.play();
        }
    }

    /// Stop playback and rewind to the start (фаза 1 seek, §6.16).
    pub fn stop(&mut self) {
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

    /// Shared `VizTap` handle for the Exclusive path (§6.18): the
    /// non-`Clone` `rtrb::Producer` lives in this one `Arc`, so both paths
    /// reuse it instead of duplicating a tap — safe since Compatible and
    /// Exclusive sessions are mutually exclusive.
    pub(crate) fn viz_tap_handle(&self) -> VizTap {
        self.viz_tap.clone()
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
            device_desc: String::from("test"),
            last_error: None,
            info: None,
            current_path: None,
            preferred_device: None,
            resampler_algo: ResamplerAlgorithm::SincMedium,
            dsd_mode: DsdMode::Pcm,
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
            spawner: Arc::new(StdSpawner),
            last_fault: None,
            decode_events: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decoder::TrackInfo;
    use crate::audio::output::pcm_render_for;
    use crate::audio::render::gain::NoGain;
    use crate::audio::render::tpdf::Tpdf;
    use crate::audio::render::RenderCore;
    use crate::audio::worker::{DecodeConfig, DecodeLoop};

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

    /// DSD-файл (`.dsf`/`.dff`) из окружения; None → тест пропускается.
    fn env_dsd_path() -> Option<std::path::PathBuf> {
        let p = std::env::var_os("MUSIC_DSD_TEST_FILE")?;
        Some(std::path::PathBuf::from(p))
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

