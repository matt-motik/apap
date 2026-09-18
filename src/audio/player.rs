use std::path::Path;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat};

use super::decoder::{AudioSource, Decoder, TrackInfo};
use super::dsd::{DecodeMode, DsdDecoder};
use super::output::{
    build_stream_rt, select_output_for, FallbackReason, OutputRequest, OutputSpec, Resampler,
};
use super::worker::{PlaybackWorker, RtConsumer, RtShared, VizTap, WorkerCmd};
use crate::settings::{
    clamp_ring_buffer_ms, ClockFamily, DsdMode, ExclusiveMode, FallbackPolicy,
    FallbackRatePolicy, ResamplerAlgorithm, ResamplerDither, ResamplerMode, RING_BUFFER_MS_DEFAULT,
};

/// Zero-allocation LCG PRNG for TPDF dithering in the real-time audio path.
///
/// TPDF (triangular probability density function) dither is generated as the
/// sum of two independent uniforms, producing a triangular distribution over
/// [-1, 1). It runs inside the cpal callback, so it must never allocate or
/// block — which is why `rand::thread_rng()` is forbidden here (TЗ/AGENTS).
#[derive(Debug, Clone, Copy)]
pub struct TpdfRng {
    state: u32,
}

impl TpdfRng {
    pub fn new() -> Self {
        Self { state: 0x9E37_79B9 }
    }

    /// (Re)seed the generator. Called per-track in `Player::open` so each
    /// track starts with a fresh, deterministic-but-random noise sequence.
    pub fn reseed(&mut self, seed: u32) {
        self.state = seed | 1;
    }

    /// Uniform sample in [0, 1): xorshift32, 24 mantissa bits (zero-alloc).
    #[inline]
    fn next_uniform(&mut self) -> f32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        (x >> 8) as f32 / 16_777_216.0
    }

    /// TPDF sample in (-1, 1): `u1 + u2 - 1`.
    #[inline]
    pub fn next_tpdf(&mut self) -> f32 {
        self.next_uniform() + self.next_uniform() - 1.0
    }
}

impl Default for TpdfRng {
    fn default() -> Self {
        Self::new()
    }
}

/// Dither-mode index stored in [`RtShared`] (maps from [`ResamplerDither`];
/// kept as a plain u8 so the audio callback can read it through an
/// [`std::sync::atomic::AtomicU8`] with a single relaxed load).
pub(crate) const DITHER_INDEX_TPDF: u8 = 0;
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

/// Owns audio playback: a [`PlaybackWorker`] thread decoding into a lock-free
/// ring and the cpal output stream whose real-time callback only consumes that
/// ring through [`RtConsumer`]. All control happens on the caller's thread via
/// the [`RtShared`] atomics and [`WorkerCmd`] messages (ТЗ A2.0 §5).
pub struct Player {
    /// Stream geometry + transport/control atomics shared with the worker and
    /// the real-time consumer. `None` until a track is opened.
    shared: Option<Arc<RtShared>>,
    worker: Option<PlaybackWorker>,
    /// Persistent visualizer tap holder; survives worker re-creation on every
    /// `open` (the producer itself is not cloneable).
    viz_tap: VizTap,
    pub stream: Option<cpal::Stream>,
    pub device_desc: String,
    pub last_error: Option<String>,
    /// Current track info (duration/rate), kept for `snapshot`.
    info: Option<TrackInfo>,
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
            viz_tap: Arc::new(Mutex::new(None)),
            stream: None,
            device_desc,
            last_error: None,
            info: None,
            preferred_device: None,
            resampler_algo: ResamplerAlgorithm::SincMedium,
            dsd_mode: DsdMode::Pcm,
            exclusive_mode: ExclusiveMode::Auto,
            fallback_policy: FallbackPolicy::Nearest,
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
            self.open(path)?;
            if pos > 0.0 {
                self.seek(pos);
            }
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
        if is_dsd_path(path) {
            self.open_dsd_with_chain(path)
        } else {
            self.open_pcm(path)
        }
    }

    /// DSD playback through the preference chain (ТЗ A3.0 §4.1–4.2).
    fn open_dsd_with_chain(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let preferred = self.dsd_mode;
        let chain: &[DsdMode] = match preferred {
            DsdMode::Native => &[DsdMode::Native, DsdMode::DoP, DsdMode::Pcm],
            DsdMode::DoP => &[DsdMode::DoP, DsdMode::Pcm],
            DsdMode::Pcm => &[DsdMode::Pcm],
        };
        let strict = self.fallback_policy == FallbackPolicy::Fail;
        let mut last_err: Option<String> = None;

        for &mode in chain {
            match self.try_open_dsd(path, mode) {
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
    fn try_open_dsd(&mut self, path: &Path, mode: DsdMode) -> Result<TrackInfo, String> {
        match mode {
            DsdMode::Native => Err("Native DSD not supported by cpal backend".into()),
            DsdMode::DoP => self.open_dop(path),
            DsdMode::Pcm => self.open_pcm(path),
        }
    }

    /// Publish the persisted control state onto a freshly created [`RtShared`].
    fn apply_state(
        shared: &RtShared,
        volume: f32,
        muted: bool,
        dither_idx: u8,
        bit_perfect: bool,
        viz_active: bool,
    ) {
        shared.set_volume(volume);
        shared.set_muted(muted);
        shared.set_dither_index(dither_idx);
        shared.set_bit_perfect(bit_perfect);
        shared.set_viz_tap_active(viz_active);
        // A freshly opened stream is not "finished": the first `play` must not
        // trigger the rewind/seek path.
        shared.set_finished(false);
    }

    /// Wrap `build_stream_rt` with the §11.4 mock seam. In test builds the
    /// first `build_failures` `start_engine`-iterations are forced to fail
    /// (the device rejects the stream); production builds call through.
    fn build_stream(
        &self,
        spec: &OutputSpec,
        consumer: RtConsumer,
    ) -> Result<cpal::Stream, String> {
        #[cfg(test)]
        {
            let rem = self.test_hooks.build_failures.load(Ordering::Relaxed);
            if rem > 0 {
                self.test_hooks
                    .build_failures
                    .store(rem - 1, Ordering::Relaxed);
                return Err("Mock: build_stream_rt forced to fail (TestHooks)".into());
            }
        }
        build_stream_rt(spec, consumer, None)
    }

    /// Build the consumer stream and spawn the producer worker. On a
    /// `Fixed`-buffer rejection the stream is retried with `Default` (the
    /// source/resampler are only handed to the worker once the stream builds).
    /// When the stream was selected as `exclusive` and the build fails, §4.3
    /// applies: `Auto` downgrades to shared (one retry), `Strict` returns the
    /// error unchanged.
    fn start_engine(
        &mut self,
        source: Box<dyn AudioSource>,
        resampler: Resampler,
        shared: Arc<RtShared>,
        rng: TpdfRng,
        mut spec: OutputSpec,
    ) -> Result<cpal::Stream, String> {
        #[cfg(test)]
        {
            let rem = self.test_hooks.build_failures.load(Ordering::Relaxed);
            if rem > 0 {
                self.test_hooks
                    .build_failures
                    .store(rem - 1, Ordering::Relaxed);
                return Err("Mock: build_stream_rt forced to fail (TestHooks)".into());
            }
        }

        let viz_tap = self.viz_tap.clone();
        let mut exclusive_retried = false;
        loop {
            // Recompute per iteration so the `Default` retry gets a floor that
            // matches the actual (unknown) callback period less aggressively.
            let buffer_frames = match spec.config.buffer_size {
                BufferSize::Fixed(f) => Some(f),
                BufferSize::Default => None,
            };
            let capacity = ring_capacity(
                shared.out_rate(),
                shared.out_ch(),
                self.ring_buffer_ms,
                buffer_frames,
            );
            let (producer, ring) = rtrb::RingBuffer::<f32>::new(capacity);
            let mut consumer = RtConsumer::new(ring, shared.clone());
            consumer.set_tpdf(rng);
            match self.build_stream(&spec, consumer) {
                Ok(stream) => {
                    self.worker = Some(PlaybackWorker::spawn(
                        source,
                        resampler,
                        producer,
                        shared,
                        viz_tap,
                    ));
                    return Ok(stream);
                }
                Err(e) => {
                    if matches!(spec.config.buffer_size, BufferSize::Fixed(_)) {
                        spec.config.buffer_size = BufferSize::Default;
                        continue;
                    }
                    // Exclusive stream rejected: one shared downgrade (Auto) or
                    // fail through (Strict), ТЗ A3.0 §4.3.
                    if spec.exclusive
                        && self.exclusive_mode == ExclusiveMode::Auto
                        && !exclusive_retried
                    {
                        exclusive_retried = true;
                        spec.exclusive = false;
                        spec.sample_format = SampleFormat::F32;
                        spec.config.buffer_size = BufferSize::Default;
                        if let Some(desc) = self.stream_desc.as_mut() {
                            desc.exclusive = false;
                            desc.exclusive_fallback = true;
                        }
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Standard path: PCM files or DSD decoded to PCM via the CIC cascade,
    /// resampled to the device rate. The device/config are negotiated through
    /// [`select_output_for`] so `ExclusiveMode`/`FallbackPolicy`/`ResamplerMode`
    /// are applied (ТЗ A3.0 §2–§4).
    fn open_pcm(&mut self, path: &Path) -> Result<TrackInfo, String> {
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
            resampler: ResamplerMode::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            clock_family: ClockFamily::Auto,
            fixed_rate: 0,
        };
        let spec = select_output_for(&req)?;
        let out_rate = spec.config.sample_rate;
        let out_ch = spec.config.channels as usize;
        self.device_desc = spec.device_name.clone();

        let resampler =
            Resampler::with_algo(src_rate, out_rate, src_ch, out_ch, self.resampler_algo);
        let shared = RtShared::new(out_rate, out_ch, resampler.is_enabled());
        Self::apply_state(
            &shared,
            self.volume,
            self.muted,
            self.dither_idx,
            self.bit_perfect,
            self.viz_active,
        );
        // Fresh dither seed per track: statistically independent noise,
        // reproducible across runs for a given track path.
        let mut rng = TpdfRng::new();
        rng.reseed(track_seed(path));

        self.stream_desc = Some(StreamDesc {
            device: spec.device_name.clone(),
            rate: out_rate,
            channels: out_ch as u16,
            format: format_name(&spec.sample_format),
            exclusive: spec.exclusive,
            exclusive_fallback: req.exclusive != ExclusiveMode::Off && !spec.exclusive,
            resampled: resampler.is_enabled(),
            source_rate: src_rate,
            source_channels: src_ch,
            dsd_mode: None,
            dsd_preferred: None,
            dsd_fallback_reason: None,
            fallback: spec.fallback,
        });

        let resampled = shared.bit_perfect_resampled();
        let stream = self.start_engine(src, resampler, shared.clone(), rng, spec)?;
        if resampled {
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
        self.stream = Some(stream);
        self.shared = Some(shared);
        self.info = Some(info.clone());
        Ok(info)
    }

    /// DoP (DSD over PCM) path: the decoder keeps the raw DSD bytes and packs
    /// them into 24-bit DoP words at the container rate (byte rate / 2, i.e.
    /// two bytes per channel per frame). Any slot mismatch or stream failure
    /// is returned as `Err` — the DSD chain (§4.2) decides whether to fall
    /// back to PCM (honest CIC decoding) without mislabelling the mode.
    fn open_dop(&mut self, path: &Path) -> Result<TrackInfo, String> {
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
            resampler: ResamplerMode::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            clock_family: ClockFamily::Auto,
            fixed_rate: 0,
        };
        let mut spec = select_output_for(&req)?;

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

        // Identity config: source and output rates/channels match, so the
        // resampler degrades to a pure pass-through and the DoP words are
        // delivered verbatim (no volume, no dither — Direct Output).
        let resampler =
            Resampler::with_algo(dop_rate, dop_rate, src_ch, src_ch, self.resampler_algo);
        let shared = RtShared::new(dop_rate, src_ch, resampler.is_enabled());
        Self::apply_state(
            &shared,
            self.volume,
            self.muted,
            self.dither_idx,
            self.bit_perfect,
            self.viz_active,
        );
        let mut rng = TpdfRng::new();
        rng.reseed(track_seed(path));

        self.stream_desc = Some(StreamDesc {
            device: spec.device_name.clone(),
            rate: dop_rate,
            channels: src_ch as u16,
            format: format_name(&spec.sample_format),
            exclusive: spec.exclusive,
            exclusive_fallback: req.exclusive != ExclusiveMode::Off && !spec.exclusive,
            resampled: resampler.is_enabled(),
            source_rate: dop_rate,
            source_channels: src_ch,
            dsd_mode: None,
            dsd_preferred: None,
            dsd_fallback_reason: None,
            fallback: spec.fallback,
        });

        let resampled = shared.bit_perfect_resampled();

        let stream = match self.start_engine(Box::new(dop), resampler, shared.clone(), rng, spec) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[audio] WARN: cannot build DoP stream ({e})");
                return Err(format!("cannot build DoP stream: {e}"));
            }
        };
        if resampled {
            eprintln!(
                "[audio] WARN: device does not support the DoP slot {dop_rate} Hz, \
                 resampling under Bit-perfect mode"
            );
        }
        if let Err(e) = stream.play() {
            eprintln!("[audio] WARN: cannot start DoP stream ({e})");
            return Err(format!("cannot start DoP stream: {e}"));
        }
        self.stream = Some(stream);
        self.shared = Some(shared);
        self.info = Some(info.clone());
        self.last_error = None;
        Ok(info)
    }

    /// Rewind the worker/resampler to the start and clear the finished state.
    fn rewind(&self, shared: &Arc<RtShared>) {
        let generation = shared.begin_seek(0);
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::Seek {
                generation,
                secs: 0.0,
            });
        }
        shared.set_finished(false);
        shared.set_natural_end(false);
    }

    /// Start/resume playback. A finished track is rewound and replayed.
    pub fn play(&mut self) {
        let Some(shared) = self.shared.clone() else {
            self.last_error = Some("no track loaded".into());
            return;
        };
        if shared.finished() {
            self.rewind(&shared);
        }
        shared.set_natural_end(false);
        shared.set_playing(true);
    }

    /// Returns `true` if a decoder (track) is currently loaded.
    pub fn has_decoder(&self) -> bool {
        self.shared.is_some()
    }

    /// Pause/resume the current track (rewinds if it had finished).
    pub fn toggle(&mut self) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if shared.is_playing() {
            shared.set_playing(false);
        } else {
            if shared.finished() {
                self.rewind(&shared);
            }
            shared.set_natural_end(false);
            shared.set_playing(true);
        }
    }

    /// Stop playback, mark the track as finished and rewind to the start.
    pub fn stop(&mut self) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        shared.set_playing(false);
        shared.set_finished(true);
        shared.set_natural_end(false);
        shared.set_pos_frames(0);
        let generation = shared.begin_seek(0);
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::Seek {
                generation,
                secs: 0.0,
            });
        }
    }

    /// Seek to `secs` (clamped to >= 0). The position is re-based optimistically
    /// and finalised by the consumer once the worker acknowledges the seek.
    pub fn seek(&mut self, secs: f64) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let secs = secs.max(0.0);
        let target = (secs * shared.out_rate() as f64) as u64;
        let generation = shared.begin_seek(target);
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::Seek { generation, secs });
        }
        shared.set_pos_frames(target);
        shared.set_finished(false);
        shared.set_natural_end(false);
    }

    /// Set volume, clamped to [0, 1]. Applied inside the audio callback.
    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(shared) = &self.shared {
            shared.set_volume(self.volume);
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
            shared.set_muted(m);
        }
    }

    /// Mute state.
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Toggle mute.
    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(shared) = &self.shared {
            shared.set_muted(self.muted);
        }
    }

    /// Enable/disable bit-perfect (Direct Output) mode. When active, the
    /// consumer bypasses the software volume/mute stage entirely and (at a
    /// native-rate match) delivers the stream untouched.
    pub fn set_bit_perfect(&mut self, enabled: bool) {
        self.bit_perfect = enabled;
        if let Some(shared) = &self.shared {
            shared.set_bit_perfect(enabled);
        }
    }

    /// Current bit-perfect flag.
    pub fn bit_perfect(&self) -> bool {
        self.bit_perfect
    }

    /// True when bit-perfect mode is active while the device forces a software
    /// resample (native rate unsupported), i.e. bit-perfect is not guaranteed.
    /// Drives the «Resample (device limit)» status badge (ТЗ A2.0 §4.1).
    pub fn bit_perfect_resampled(&self) -> bool {
        self.shared
            .as_ref()
            .map(|s| s.bit_perfect_resampled())
            .unwrap_or(false)
    }

    /// Set the dither mode applied during final quantization (i16/u8).
    pub fn set_dither(&mut self, dither: ResamplerDither) {
        self.dither_idx = dither_index(dither);
        if let Some(shared) = &self.shared {
            shared.set_dither_index(self.dither_idx);
        }
    }

    /// Whether the core is actively decoding (not paused/stopped).
    pub fn is_playing(&self) -> bool {
        self.shared
            .as_ref()
            .map(|s| s.is_playing())
            .unwrap_or(false)
    }

    /// True when the current track played to its natural end (EOF).
    pub fn ended(&self) -> bool {
        self.shared
            .as_ref()
            .map(|s| s.natural_end())
            .unwrap_or(false)
    }

    /// Clear the natural-end flag (e.g. after the playlist handled it).
    pub fn clear_end(&mut self) {
        if let Some(shared) = &self.shared {
            shared.set_natural_end(false);
        }
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
            shared.set_viz_tap_active(active);
        }
    }

    /// Return a snapshot for the UI: (playing, pos, duration).
    pub fn snapshot(&self) -> (bool, f64, Option<f64>) {
        let playing = self.is_playing();
        let pos = match &self.shared {
            Some(shared) => shared.pos_frames() as f64 / shared.out_rate().max(1) as f64,
            None => 0.0,
        };
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
            Some(shared) => (shared.out_rate(), shared.out_ch()),
            None => (44_100, 2),
        }
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Producer/Consumer callbacks (ТЗ A2.0 §5.3). ----
//
// The cpal real-time thread only touches the lock-free `RtConsumer` (ring +
// atomics): no `Mutex`, no decoder, no allocation, no logging. Volume/mute/
// bit-perfect/dither come from `RtShared`; samples come from the worker's ring.

/// Dither applicability for the ring-consumer path: skipped in a true
/// bit-perfect passthrough, applied otherwise.
fn dither_enabled_rt(bit_perfect: bool, resampler_enabled: bool, dither_idx: u8) -> bool {
    if dither_idx == DITHER_INDEX_OFF {
        return false;
    }
    if bit_perfect && !resampler_enabled {
        return false;
    }
    true
}

/// At natural EOF (fewer samples than requested and the worker signalled end),
/// stop the transport and flag the end for the playlist.
#[inline]
fn finish_if_eof(consumer: &RtConsumer, produced: usize, requested: usize) {
    if produced < requested && consumer.shared().eof() {
        let shared = consumer.shared();
        shared.set_playing(false);
        shared.set_finished(true);
        shared.set_natural_end(true);
    }
}

/// f32 consumer callback: pull straight into the device buffer.
pub fn audio_callback_f32_rt(consumer: &mut RtConsumer, data: &mut [f32]) {
    if !consumer.shared().is_playing() {
        data.fill(0.0);
        return;
    }
    if !consumer.reconcile_seek() {
        data.fill(0.0);
        return;
    }
    let bit_perfect = consumer.shared().bit_perfect();
    let muted = consumer.shared().muted();
    let volume = consumer.shared().volume();
    let produced = consumer.pull_f32(data);
    let vol = if bit_perfect {
        1.0
    } else if muted {
        0.0
    } else {
        volume
    };
    if vol < 1.0 {
        for s in data.iter_mut().take(produced) {
            *s *= vol;
        }
    }
    if produced < data.len() {
        data[produced..].fill(0.0);
        finish_if_eof(consumer, produced, data.len());
    }
}

/// i16 consumer callback: pull into scratch, apply volume + optional TPDF.
pub fn audio_callback_i16_rt(consumer: &mut RtConsumer, data: &mut [i16]) {
    if !consumer.shared().is_playing() {
        data.fill(0);
        return;
    }
    if !consumer.reconcile_seek() {
        data.fill(0);
        return;
    }
    let bit_perfect = consumer.shared().bit_perfect();
    let vol = if bit_perfect {
        1.0
    } else if consumer.shared().muted() {
        0.0
    } else {
        consumer.shared().volume()
    };
    let dither_idx = consumer.shared().dither_index();
    let apply_dither =
        dither_enabled_rt(bit_perfect, consumer.shared().resampler_enabled(), dither_idx);
    let dither_amp = dither_amplitude(dither_idx);
    let produced = consumer.pull_scratch(data.len());
    let mut tpdf = consumer.tpdf();
    for (dst, &src) in data.iter_mut().zip(consumer.scratch().iter()).take(produced) {
        let mut x = src.clamp(-1.0, 1.0) * vol * 32767.0;
        if apply_dither {
            x += tpdf.next_tpdf() * dither_amp;
        }
        *dst = x.round().clamp(-32768.0, 32767.0) as i16;
    }
    consumer.set_tpdf(tpdf);
    for s in data.iter_mut().skip(produced) {
        *s = 0;
    }
    if produced < data.len() {
        finish_if_eof(consumer, produced, data.len());
    }
}

/// u8 consumer callback (silence = 128, samples centered at 0.5).
pub fn audio_callback_u8_rt(consumer: &mut RtConsumer, data: &mut [u8]) {
    if !consumer.shared().is_playing() {
        data.fill(128);
        return;
    }
    if !consumer.reconcile_seek() {
        data.fill(128);
        return;
    }
    let bit_perfect = consumer.shared().bit_perfect();
    let vol = if bit_perfect {
        1.0
    } else if consumer.shared().muted() {
        0.0
    } else {
        consumer.shared().volume()
    };
    let dither_idx = consumer.shared().dither_index();
    let apply_dither =
        dither_enabled_rt(bit_perfect, consumer.shared().resampler_enabled(), dither_idx);
    let dither_amp = dither_amplitude(dither_idx);
    let produced = consumer.pull_scratch(data.len());
    let mut tpdf = consumer.tpdf();
    for (dst, &src) in data.iter_mut().zip(consumer.scratch().iter()).take(produced) {
        let mut x = (src.clamp(-1.0, 1.0) * vol * 0.5 + 0.5) * 255.0;
        if apply_dither {
            x += tpdf.next_tpdf() * dither_amp;
        }
        *dst = x.round().clamp(0.0, 255.0) as u8;
    }
    consumer.set_tpdf(tpdf);
    for s in data.iter_mut().skip(produced) {
        *s = 128;
    }
    if produced < data.len() {
        finish_if_eof(consumer, produced, data.len());
    }
}

/// i32 PCM consumer callback (native-I32 hardware node), same as i16 scaled.
pub fn audio_callback_i32_pcm_rt(consumer: &mut RtConsumer, data: &mut [i32]) {
    if !consumer.shared().is_playing() {
        data.fill(0);
        return;
    }
    if !consumer.reconcile_seek() {
        data.fill(0);
        return;
    }
    let bit_perfect = consumer.shared().bit_perfect();
    let vol = if bit_perfect {
        1.0
    } else if consumer.shared().muted() {
        0.0
    } else {
        consumer.shared().volume()
    };
    let dither_idx = consumer.shared().dither_index();
    let apply_dither =
        dither_enabled_rt(bit_perfect, consumer.shared().resampler_enabled(), dither_idx);
    let dither_amp = dither_amplitude(dither_idx);
    let produced = consumer.pull_scratch(data.len());
    const I32_MAX: f64 = 2_147_483_647.0;
    let mut tpdf = consumer.tpdf();
    for (dst, &src) in data.iter_mut().zip(consumer.scratch().iter()).take(produced) {
        let mut x = src.clamp(-1.0, 1.0) as f64 * vol as f64 * I32_MAX;
        if apply_dither {
            x += tpdf.next_tpdf() as f64 * dither_amp as f64;
        }
        *dst = x.round().clamp(-I32_MAX - 1.0, I32_MAX) as i32;
    }
    consumer.set_tpdf(tpdf);
    for s in data.iter_mut().skip(produced) {
        *s = 0;
    }
    if produced < data.len() {
        finish_if_eof(consumer, produced, data.len());
    }
}

/// i32 DoP consumer callback: left-align the 24-bit DoP word (`<< 8`), verbatim.
pub fn audio_callback_i32_dop_rt(consumer: &mut RtConsumer, data: &mut [i32]) {
    if !consumer.shared().is_playing() {
        data.fill(0);
        return;
    }
    if !consumer.reconcile_seek() {
        data.fill(0);
        return;
    }
    let produced = consumer.pull_scratch(data.len());
    for (dst, &src) in data.iter_mut().zip(consumer.scratch().iter()).take(produced) {
        *dst = ((src.clamp(0.0, 16_777_215.0) as u32) << 8) as i32;
    }
    for s in data.iter_mut().skip(produced) {
        *s = 0;
    }
    if produced < data.len() {
        finish_if_eof(consumer, produced, data.len());
    }
}

/// Per-sample dither amplitude for the two supported PDF shapes:
/// full TPDF peak = 1 LSB, triangular peak = 0.5 LSB (half-amplitude).
fn dither_amplitude(idx: u8) -> f32 {
    if idx == DITHER_INDEX_TRIANGULAR {
        0.5
    } else {
        1.0
    }
}

#[cfg(test)]
impl Player {
    /// Test-only: build a `Player` without touching the audio backend.
    fn test_new() -> Self {
        Self {
            shared: None,
            worker: None,
            viz_tap: Arc::new(Mutex::new(None)),
            stream: None,
            device_desc: String::from("test"),
            last_error: None,
            info: None,
            preferred_device: None,
            resampler_algo: ResamplerAlgorithm::SincMedium,
            dsd_mode: DsdMode::Pcm,
            exclusive_mode: ExclusiveMode::Auto,
            fallback_policy: FallbackPolicy::Nearest,
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

// Keep stream alive (no-op guard used by the main loop).
#[allow(dead_code)]
/// Keep the stream object alive for its intended lifetime (used by the
/// startup probe, which must hold the stream until the device is checked).
pub fn keep_alive(_s: &cpal::Stream) {}

#[cfg(test)]
mod alloc_tracking {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Cranked up only while a zero-allocation test runs; keeps the thread-local
    /// counter free of background churn from the parallel test harness.
    pub static TRACKING: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static ALLOC_COUNT: Cell<u64> = const { Cell::new(0) };
    }

    /// Total allocations on the current thread since the counter was armed.
    pub fn alloc_count() -> u64 {
        ALLOC_COUNT.with(|c| c.get())
    }

    pub fn reset_count() {
        ALLOC_COUNT.with(|c| c.set(0));
    }

    /// Thin wrapper over `System` that counts allocations on the measuring
    /// thread. Test-only: the real-time callbacks must allocate zero bytes
    /// (ТЗ A2.0 §3.4).
    pub struct CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if TRACKING.load(Ordering::Relaxed) {
                ALLOC_COUNT.with(|c| c.set(c.get() + 1));
            }
            // SAFETY: delegates to the underlying system allocator.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: delegates to the underlying system allocator.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if TRACKING.load(Ordering::Relaxed) {
                ALLOC_COUNT.with(|c| c.set(c.get() + 1));
            }
            // SAFETY: delegates to the underlying system allocator.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

#[cfg(test)]
#[global_allocator]
static GLOBAL_ALLOC: alloc_tracking::CountingAllocator = alloc_tracking::CountingAllocator;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decoder::TrackInfo;
    use crate::audio::worker::{RtShared, MAX_OUT_SAMPLES};

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

    /// A `Player` with a live `RtShared` but no output engine: enough to test
    /// transport/control state without touching a real backend.
    fn player_with_shared(frames: usize) -> Player {
        let mut p = Player::test_new();
        p.shared = Some(RtShared::new(44100, 1, false));
        p.info = Some(track_info(frames));
        p
    }

    /// `RtConsumer` with a pre-filled ring and playing transport.
    fn rt_consumer_with(samples: &[f32], out_ch: usize) -> RtConsumer {
        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(samples.len().max(64) + 1024);
        let shared = RtShared::new(44100, out_ch, false);
        for &s in samples {
            let _ = prod.push(s);
        }
        let mut c = RtConsumer::new(cons, shared);
        c.shared().set_playing(true);
        c
    }

    /// `RtConsumer` with `n` constant `0.5` samples queued.
    fn rt_consumer_filled(n: usize, out_ch: usize) -> RtConsumer {
        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(n + 1024);
        let shared = RtShared::new(44100, out_ch, false);
        for _ in 0..n {
            let _ = prod.push(0.5);
        }
        let mut c = RtConsumer::new(cons, shared);
        c.shared().set_playing(true);
        c
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
        assert!(shared.finished());
        assert!(!shared.natural_end(), "manual stop is not a natural end");
    }

    #[test]
    fn play_after_stop_replays_from_start() {
        let mut p = player_with_shared(1000);
        p.play();
        p.stop();
        p.play();
        assert!(p.is_playing());
        let shared = p.shared.as_ref().expect("shared");
        assert!(!shared.finished());
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
    fn tpdf_samples_stay_in_unit_range() {
        let mut rng = TpdfRng::new();
        rng.reseed(1);
        for _ in 0..100_000 {
            let s = rng.next_tpdf();
            assert!(s > -1.0 && s < 1.0, "TPDF sample out of range: {s}");
        }
    }

    #[test]
    fn tpdf_mean_is_zero() {
        let mut rng = TpdfRng::new();
        rng.reseed(2);
        const N: i64 = 10_000_000;
        let mut sum = 0.0f64;
        for _ in 0..N {
            sum += rng.next_tpdf() as f64;
        }
        let mean = sum / N as f64;
        assert!(mean.abs() < 1e-3, "TPDF mean not centred: {mean}");
    }

    // ---- Producer/Consumer callback tests (ТЗ A2.0 §5.3) ----

    #[test]
    fn rt_f32_applies_volume_and_silences_when_paused() {
        let mut c = rt_consumer_with(&[0.5, 0.5, 0.5, 0.5], 1);
        c.shared().set_volume(0.5);
        let mut data = [0.0f32; 4];
        audio_callback_f32_rt(&mut c, &mut data);
        assert_eq!(data, [0.25, 0.25, 0.25, 0.25]);

        c.shared().set_playing(false);
        let mut paused = [1.0f32; 2];
        audio_callback_f32_rt(&mut c, &mut paused);
        assert_eq!(paused, [0.0, 0.0], "paused consumer emits silence");
    }

    #[test]
    fn rt_callback_advances_position_by_frames() {
        let mut c = rt_consumer_filled(1024, 1);
        let mut data = [0.0f32; 256];
        audio_callback_f32_rt(&mut c, &mut data);
        assert_eq!(c.shared().pos_frames(), 256);
    }

    #[test]
    fn rt_callback_marks_natural_end_at_eof() {
        let mut c = rt_consumer_with(&[0.5, 0.5], 1);
        c.shared().set_eof(true);
        let mut data = [0.0f32; 8];
        audio_callback_f32_rt(&mut c, &mut data);
        assert!(!c.shared().is_playing(), "EOF must stop playback");
        assert!(c.shared().finished());
        assert!(c.shared().natural_end());
    }

    #[test]
    fn rt_i16_scales_to_full_scale() {
        let mut c = rt_consumer_with(&[1.0, 1.0], 1);
        c.shared().set_volume(1.0);
        c.shared().set_dither_index(DITHER_INDEX_OFF);
        let mut data = [0i16; 2];
        audio_callback_i16_rt(&mut c, &mut data);
        assert_eq!(data, [32767, 32767]);
    }

    #[test]
    fn rt_i16_clamps_and_scales_volume() {
        let mut c = rt_consumer_with(&[0.25; 256], 1);
        c.shared().set_volume(0.5);
        c.shared().set_dither_index(DITHER_INDEX_OFF);
        let mut data = [0i16; 256];
        audio_callback_i16_rt(&mut c, &mut data);
        let expected = (0.25f32 * 0.5 * 32767.0).round() as i16;
        assert!(data.iter().all(|s| *s == expected), "got {:?}", &data[..4]);
    }

    #[test]
    fn rt_u8_uses_128_silence_center() {
        let mut c = rt_consumer_with(&[0.0, 0.0], 1);
        c.shared().set_dither_index(DITHER_INDEX_OFF);
        let mut data = [0u8; 2];
        audio_callback_u8_rt(&mut c, &mut data);
        // 0.0 maps to the midpoint (~128).
        assert!(data.iter().all(|s| (127..=128).contains(s)));
    }

    #[test]
    fn rt_i32_pcm_is_not_dop_packed() {
        // The ADI-2 regression: the old i32 callback was the DoP packer
        // (`(src << 8)`), producing silence for plain PCM. A positive sample
        // must come out as a positive i32 value, not a left-shifted byte.
        let mut c = rt_consumer_with(&[0.25; 64], 1);
        c.shared().set_volume(1.0);
        c.shared().set_dither_index(DITHER_INDEX_OFF);
        let mut data = [0i32; 64];
        audio_callback_i32_pcm_rt(&mut c, &mut data);
        let expected = (0.25f32 * 2_147_483_647.0).round() as i32;
        assert_eq!(data[0], expected);
        assert!(data[0] > 0 && data[0] != 0x4000_0000_i32, "DoP packing leaked");
    }

    #[test]
    fn rt_i32_dop_left_aligns_marker() {
        let mut c = rt_consumer_with(&[16_777_215.0, 0.0], 1);
        let mut data = [0i32; 2];
        audio_callback_i32_dop_rt(&mut c, &mut data);
        assert_eq!(data[0], (16_777_215u32 << 8) as i32);
        assert_eq!(data[1], 0);
    }

    #[test]
    fn rt_bit_perfect_bypasses_software_volume() {
        let mut c = rt_consumer_with(&[0.25; 256], 1);
        c.shared().set_volume(0.5);
        c.shared().set_bit_perfect(true);
        let mut data = [0.0f32; 256];
        audio_callback_f32_rt(&mut c, &mut data);
        assert!(data.iter().all(|s| (*s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn rt_bit_perfect_ignores_mute() {
        let mut c = rt_consumer_with(&[0.25; 64], 1);
        c.shared().set_volume(0.3);
        c.shared().set_muted(true);
        c.shared().set_bit_perfect(true);
        let mut data = [0.0f32; 64];
        audio_callback_f32_rt(&mut c, &mut data);
        assert!(data.iter().any(|s| *s != 0.0), "mute must be ignored");
        assert!(data.iter().all(|s| (*s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn rt_tpdf_dither_adds_noise_to_quantization() {
        let mut c = rt_consumer_with(&[0.25; 256], 1);
        c.shared().set_volume(1.0);
        c.shared().set_dither_index(DITHER_INDEX_TPDF);
        let mut data = [0i16; 256];
        audio_callback_i16_rt(&mut c, &mut data);
        let unique: std::collections::BTreeSet<i16> = data.iter().copied().collect();
        assert!(unique.len() > 1, "dithering must make quantized samples vary");
    }

    #[test]
    fn rt_scratch_is_pinned_and_clamped() {
        let mut c = rt_consumer_filled(MAX_OUT_SAMPLES + 512, 1);
        assert_eq!(c.scratch_capacity(), MAX_OUT_SAMPLES);
        let n = c.pull_scratch(MAX_OUT_SAMPLES + 256);
        assert!(n <= MAX_OUT_SAMPLES, "oversized pull must clamp to the pool");
        assert_eq!(c.scratch_capacity(), MAX_OUT_SAMPLES, "pool must not grow");
    }

    #[test]
    fn bit_perfect_resampled_flag_updates_on_toggle() {
        let mut p = Player::test_new();
        // Identity resampler: native-rate match, resampling is unnecessary.
        p.shared = Some(RtShared::new(44100, 1, false));
        p.set_bit_perfect(true);
        assert!(!p.bit_perfect_resampled());

        // Device rejects the native rate: a real conversion starts, the
        // «bit-perfect not guaranteed» flag must flip (ТЗ A2.0 §4.1).
        p.shared = Some(RtShared::new(44100, 1, true));
        p.set_bit_perfect(true);
        assert!(p.bit_perfect_resampled());

        // Switching bit-perfect off clears the badge even while resampling.
        p.set_bit_perfect(false);
        assert!(!p.bit_perfect_resampled());
    }

    // ---- Zero-allocation detector (ТЗ A2.0 §3.4) ----

    /// Run `f` with allocation counting armed; returns the number of new
    /// allocations performed on this thread during the call.
    fn zero_alloc_run<F: FnOnce()>(f: F) -> u64 {
        use super::alloc_tracking as at;
        at::TRACKING.store(true, std::sync::atomic::Ordering::SeqCst);
        at::reset_count();
        f();
        let n = at::alloc_count();
        at::TRACKING.store(false, std::sync::atomic::Ordering::SeqCst);
        n
    }

    #[test]
    fn all_callbacks_perform_zero_allocations() {
        let mut c_f32 = rt_consumer_filled(32768, 1);
        let mut c_i16 = rt_consumer_filled(32768, 1);
        let mut c_u8 = rt_consumer_filled(32768, 1);
        let mut c_i32 = rt_consumer_filled(32768, 1);
        let mut c_dop = rt_consumer_filled(32768, 1);
        let mut b_f32 = vec![0.0f32; 4096];
        let mut b_i16 = vec![0i16; 4096];
        let mut b_u8 = vec![0u8; 4096];
        let mut b_i32 = vec![0i32; 4096];
        let mut b_dop = vec![0i32; 4096];
        // Warm-up: any lazily-initialised state (iterator glue, TLS, etc.) must
        // settle before the zero-allocation assertion (ТЗ A2.0 §3.4).
        audio_callback_f32_rt(&mut c_f32, &mut b_f32);
        audio_callback_i16_rt(&mut c_i16, &mut b_i16);
        audio_callback_u8_rt(&mut c_u8, &mut b_u8);
        audio_callback_i32_pcm_rt(&mut c_i32, &mut b_i32);
        audio_callback_i32_dop_rt(&mut c_dop, &mut b_dop);

        assert_eq!(
            zero_alloc_run(|| audio_callback_f32_rt(&mut c_f32, &mut b_f32)),
            0,
            "f32"
        );
        assert_eq!(
            zero_alloc_run(|| audio_callback_i16_rt(&mut c_i16, &mut b_i16)),
            0,
            "i16"
        );
        assert_eq!(
            zero_alloc_run(|| audio_callback_u8_rt(&mut c_u8, &mut b_u8)),
            0,
            "u8"
        );
        assert_eq!(
            zero_alloc_run(|| audio_callback_i32_pcm_rt(&mut c_i32, &mut b_i32)),
            0,
            "i32_pcm"
        );
        assert_eq!(
            zero_alloc_run(|| audio_callback_i32_dop_rt(&mut c_dop, &mut b_dop)),
            0,
            "i32_dop"
        );
    }
}
