use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

use super::decoder::{AudioSource, Decoder, TrackInfo};
use super::dsd::{DsdDecoder, DecodeMode};
use super::output::{build_stream, select_output, Resampler};
use crate::settings::{DsdMode, ResamplerAlgorithm, ResamplerDither};

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

    /// (Re)seed the generator. Called per-track in [`Player::open`] so each
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

/// Dither-mode index stored in [`PlaybackCore::dither`] (maps from
/// [`ResamplerDither`]; kept as a plain u8 so the audio callback can read it
/// through an [`Arc<AtomicU8>`] with a single relaxed load).
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

/// Hard ceiling for the preallocated f32 scratch pool (interleaved samples).
/// Large enough to cover any realistic `cpal` callback buffer (65536 samples ≈
/// 8K stereo frames @44.1 kHz); pinned once at track open so the real-time
/// callbacks never reallocate (ТЗ A2.0 §3.1).
const MAX_OUT_SAMPLES: usize = 1 << 16;

/// Shared state accessed both by the UI thread and the audio callback.
pub struct PlaybackCore {
    pub decoder: Option<Box<dyn AudioSource>>,
    pub resampler: Option<Resampler>,
    pub volume: f32,
    pub muted: bool,
    /// Bit-perfect (Direct Output) mode: software volume/mute are bypassed;
    /// the stream is delivered untouched. Read in the audio callback with a
    /// relaxed load; written via [`Player::set_bit_perfect`].
    pub bit_perfect: Arc<AtomicBool>,
    /// Dither mode index (see `dither_index`). Read in the audio callback;
    /// written via [`Player::set_dither`].
    pub dither: Arc<AtomicU8>,
    /// TPDF noise generator used during quantization (i16/u8 conversion).
    /// Touched only inside the audio callback (guarded by the core lock).
    pub tpdf: TpdfRng,
    pub playing: bool,
    pub finished: bool,
    /// True when the track reached its natural end-of-stream (as opposed to a
    /// manual Stop). Drives playlist auto-advance only.
    pub natural_end: bool,
    pub pos_secs: f64,
    pub out_rate: u32,
    pub out_ch: usize,
    scratch: Vec<f32>,
    /// Visualizer tap: copy of the post-resampler PCM (before volume) is
    /// pushed here by the audio callback when `viz_tap_active`. Only
    /// non-blocking `push` is used — the callback never blocks.
    pub viz_tap: Option<rtrb::Producer<f32>>,
    pub viz_tap_active: bool,
}

impl PlaybackCore {
    pub(crate) fn new() -> Self {
        Self {
            decoder: None,
            resampler: None,
            volume: 0.8,
            muted: false,
            bit_perfect: Arc::new(AtomicBool::new(false)),
            dither: Arc::new(AtomicU8::new(DITHER_INDEX_TPDF)),
            tpdf: TpdfRng::new(),
            playing: false,
            finished: true,
            natural_end: false,
            pos_secs: 0.0,
            out_rate: 44100,
            out_ch: 2,
            scratch: Vec::new(),
            viz_tap: None,
            viz_tap_active: false,
        }
    }

    fn fill(&mut self, out: &mut [f32]) -> usize {
        if self.decoder.is_none() {
            return 0;
        }
        let out_ch = self.out_ch;
        let mut written_samples = 0usize;
        loop {
            if written_samples >= out.len() {
                break;
            }
            let remaining_frames = (out.len() - written_samples) / out_ch;
            if remaining_frames == 0 {
                break;
            }
            let eof = self.decoder.as_ref().map(|d| d.eof()).unwrap_or(true);
            match &mut self.resampler {
                Some(res) => {
                    // Keep at least one source frame buffered for interpolation.
                    let mut guard = 0;
                    while res.buffered_frames() < 2 && !eof && guard < 1024 {
                        guard += 1;
                        let Some(s) = self.decoder.as_mut().and_then(|d| d.next_frames()) else {
                            break;
                        };
                        res.push(s);
                    }
                    let produced_frames =
                        res.pull(&mut out[written_samples..], remaining_frames, eof);
                    if produced_frames == 0 {
                        break;
                    }
                    written_samples += produced_frames * out_ch;
                }
                None => break,
            }
        }
        written_samples
    }

    fn scratch_f32(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.scratch)
    }

    /// (Re)pins the scratch pool to [`MAX_OUT_SAMPLES`] at track open. After
    /// this call every audio callback uses a slice of the preallocated buffer
    /// and never allocates (ТЗ A2.0 §3.1).
    fn prepare_scratch(&mut self) {
        self.scratch = vec![0.0f32; MAX_OUT_SAMPLES];
    }

    fn scratch_release(&mut self, buf: Vec<f32>) {
        let mut buf = buf;
        buf.clear();
        if buf.capacity() > 128 * 1024 {
            buf.shrink_to(64 * 1024);
        }
        self.scratch = buf;
    }

    fn duration_secs(&self) -> Option<f64> {
        self.decoder.as_ref().and_then(|d| d.duration_secs())
    }
}

/// Owns audio playback: the shared [`PlaybackCore`] (decoder + resampler +
/// transport state, also read by the cpal callback) and the running output
/// stream. All control happens on the caller's thread; only the cpal
/// callback touches the core concurrently.
pub struct Player {
    pub core: Arc<Mutex<PlaybackCore>>,
    pub stream: Option<cpal::Stream>,
    pub device_desc: String,
    pub last_error: Option<String>,
    preferred_device: Option<String>,
    /// Resampling algorithm used by every new stream (ТЗ 5.1 §8.3).
    resampler_algo: ResamplerAlgorithm,
    /// DSD output mode (Pcm / Native / DoP, ТЗ 5.1 §8.2).
    dsd_mode: DsdMode,
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
            core: Arc::new(Mutex::new(PlaybackCore::new())),
            stream: None,
            device_desc,
            last_error: None,
            preferred_device: None,
            resampler_algo: ResamplerAlgorithm::SincMedium,
            dsd_mode: DsdMode::Pcm,
        }
    }

    pub fn set_resampler_algorithm(&mut self, algo: ResamplerAlgorithm) {
        self.resampler_algo = algo;
    }

    /// Set the DSD output mode (ТЗ 5.1 §8.2). `Native` makes `open` fail
    /// (cpal has no native-DSD backend), `DoP` enables the raw-DSD/DoP path.
    pub fn set_dsd_mode(&mut self, mode: DsdMode) {
        self.dsd_mode = mode;
    }

    pub fn set_preferred_device(&mut self, name: String) {
        self.preferred_device = Some(name);
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

    /// Open `path` for playback: pick an output device/format via
    /// [`select_output`](crate::audio::output::select_output), build the stream
    /// and reset transport state. Returns the track's format info.
    /// Errors (unsupported file, no device) are returned as strings.
    pub fn open(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let is_dsd = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dsf") || e.eq_ignore_ascii_case("dff"))
            .unwrap_or(false);

        if is_dsd && self.dsd_mode == DsdMode::Native {
            // cpal has no native-DSD device backend; fail loudly (ТЗ 5.1 §8.2)
            // instead of silently decoding to PCM.
            eprintln!("[audio] WARN: DSD native output is not supported by the cpal backend");
            return Err("DSD native output not supported by cpal".into());
        }

        if is_dsd && self.dsd_mode == DsdMode::DoP {
            return self.open_dop(path);
        }

        self.open_pcm(path)
    }

    /// Standard path (existing behaviour): PCM files or DSD decoded to PCM via
    /// the CIC cascade, resampled to the device rate.
    fn open_pcm(&mut self, path: &Path) -> Result<TrackInfo, String> {
        // Open and probe outside the audio-thread lock so the callback never
        // blocks on slow I/O.
        let src: Box<dyn AudioSource> = match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("dsf") || e.eq_ignore_ascii_case("dff") => {
                Box::new(DsdDecoder::open(path)?)
            }
            _ => Box::new(Decoder::open(path)?),
        };
        let (src_rate, src_ch) = (src.info().sample_rate, src.info().channels);
        let info = src.info().clone();

        // Rebuild the output stream for the current track parameters.
        self.stream = None;
        let spec = select_output(src_rate, src_ch, self.preferred_device.as_deref())?;
        let out_rate = spec.config.sample_rate;
        let out_ch = spec.config.channels as usize;
        self.device_desc = spec.device_name.clone();

        {
            let mut core = match self.core.lock() {
                Ok(c) => c,
                Err(_) => return Err("audio core poisoned; cannot open track".into()),
            };
            core.resampler = Some(Resampler::with_algo(
                src_rate,
                out_rate,
                src_ch,
                out_ch,
                self.resampler_algo,
            ));
            core.out_rate = out_rate;
            core.out_ch = out_ch;
            core.decoder = Some(src);
            core.pos_secs = 0.0;
            core.playing = false;
            core.finished = false;
            core.natural_end = false;
            core.prepare_scratch();
            // Fresh dither seed per track: statistically independent noise,
            // reproducible across runs for a given track path.
            core.tpdf.reseed(track_seed(path));
            if core.bit_perfect.load(Ordering::Relaxed)
                && core.resampler.as_ref().map(|r| r.is_enabled()).unwrap_or(false)
            {
                eprintln!(
                    "[audio] WARN: device does not support native rate {src_rate} Hz, \
                     resampling to {out_rate} Hz under Bit-perfect mode"
                );
            }
        }

        let stream = build_stream(&spec, self.core.clone(), None)?;
        if let Err(e) = stream.play() {
            self.last_error = Some(format!("Cannot start stream: {e}"));
        }
        self.stream = Some(stream);
        self.last_error = None;

        Ok(info)
    }

    /// DoP (DSD over PCM) path: the decoder keeps the raw DSD bytes and packs
    /// them into 24-bit DoP words at the container rate (byte rate / 2, i.e.
    /// two bytes per channel per frame). Any device mismatch (rate, channels or
    /// stream build) falls back to a honest PCM (CIC) decoding with a WARN.
    fn open_dop(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let dop = DsdDecoder::open_with_mode(path, DecodeMode::Dop)?;
        // DSD64: byte rate = dsd_rate / 8 = 352800; DoP carries 2 bytes per
        // channel per 32-bit frame -> container rate 176400 (mpv/mpd framing).
        let dop_rate = dop.info().sample_rate * 4;
        let src_ch = dop.info().channels;
        let info = dop.info().clone();

        self.stream = None;
        let mut spec = select_output(dop_rate, src_ch, self.preferred_device.as_deref())?;

        // DoP is bit-exact only when the device opens the exact container slot.
        if spec.config.sample_rate != dop_rate || spec.config.channels as usize != src_ch {
            let offered = format!("{} Hz × {} ch", spec.config.sample_rate, spec.config.channels);
            eprintln!(
                "[audio] WARN: device does not support the DoP slot {dop_rate} Hz × {src_ch} ch \
                 (offers {offered}); falling back to PCM (CIC)"
            );
            return self.open_pcm(path);
        }
        spec.sample_format = SampleFormat::I32;
        spec.is_dop = true;
        self.device_desc = spec.device_name.clone();

        {
            let mut core = match self.core.lock() {
                Ok(c) => c,
                Err(_) => return Err("audio core poisoned; cannot open track".into()),
            };
            // Identity config: source and output rates/channels match, so the
            // resampler degrades to a pure pass-through and the DoP words are
            // delivered verbatim (no volume, no dither — Direct Output).
            core.resampler = Some(Resampler::with_algo(
                dop_rate,
                dop_rate,
                src_ch,
                src_ch,
                self.resampler_algo,
            ));
            core.out_rate = dop_rate;
            core.out_ch = src_ch;
            core.decoder = Some(Box::new(dop));
            core.pos_secs = 0.0;
            core.playing = false;
            core.finished = false;
            core.natural_end = false;
            core.prepare_scratch();
            core.tpdf.reseed(track_seed(path));
        }

        let stream = match build_stream(&spec, self.core.clone(), None) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[audio] WARN: cannot build DoP stream ({e}); falling back to PCM (CIC)");
                return self.open_pcm(path);
            }
        };
        if let Err(e) = stream.play() {
            eprintln!("[audio] WARN: cannot start DoP stream ({e}); falling back to PCM (CIC)");
            return self.open_pcm(path);
        }
        self.stream = Some(stream);
        self.last_error = None;

        Ok(info)
    }

    /// Start/resume playback. A finished track is rewound and replayed.
    pub fn play(&mut self) {
        let Ok(mut core) = self.core.lock() else {
            self.last_error = Some("audio core busy/poisoned".into());
            return;
        };
        if core.decoder.is_none() {
            return;
        }
        if core.finished {
            core.pos_secs = 0.0;
            if let Some(dec) = &mut core.decoder {
                let _ = dec.seek(0.0);
            }
            if let Some(res) = &mut core.resampler {
                res.reset();
            }
            core.finished = false;
        }
        core.natural_end = false;
        core.playing = true;
    }

    /// Returns `true` if a decoder (track) is currently loaded.
    pub fn has_decoder(&self) -> bool {
        self.core.lock().map(|c| c.decoder.is_some()).unwrap_or(false)
    }

    /// Pause/resume the current track (rewinds if it had finished).
    pub fn toggle(&mut self) {
        let Ok(mut core) = self.core.lock() else {
            self.last_error = Some("audio core busy/poisoned".into());
            return;
        };
        if core.decoder.is_none() {
            return;
        }
        if core.playing {
            core.playing = false;
        } else {
            if core.finished {
                core.pos_secs = 0.0;
                if let Some(dec) = &mut core.decoder {
                    let _ = dec.seek(0.0);
                }
                if let Some(res) = &mut core.resampler {
                    res.reset();
                }
                core.finished = false;
            }
            core.natural_end = false;
            core.playing = true;
        }
    }

    /// Stop playback, mark the track as finished and rewind to the start.
    pub fn stop(&mut self) {
        let Ok(mut core) = self.core.lock() else { return };
        core.playing = false;
        core.finished = true;
        core.natural_end = false;
        core.pos_secs = 0.0;
        if let Some(dec) = &mut core.decoder {
            let _ = dec.seek(0.0);
        }
        if let Some(res) = &mut core.resampler {
            res.reset();
        }
    }

    /// Seek to `secs` (clamped to >= 0). Resets the resampler so the new
    /// position is played from the decoder, not computed from stale buffers.
    pub fn seek(&mut self, secs: f64) {
        let Ok(mut core) = self.core.lock() else { return };
        if let Some(dec) = &mut core.decoder {
            if dec.seek(secs).is_ok() {
                if let Some(res) = &mut core.resampler {
                    res.reset();
                }
                core.pos_secs = secs.max(0.0);
                core.finished = false;
                core.natural_end = false;
            }
        }
    }

    /// Set volume, clamped to [0, 1]. Applied inside the audio callback.
    pub fn set_volume(&mut self, v: f32) {
        if let Ok(mut core) = self.core.lock() {
            core.volume = v.clamp(0.0, 1.0);
        }
    }

    /// Current volume in [0, 1] (fallback 0.8 while the core is busy).
    pub fn volume(&self) -> f32 {
        self.core.lock().map(|c| c.volume).unwrap_or(0.8)
    }

    /// Mute/unmute without changing the volume.
    pub fn set_muted(&mut self, m: bool) {
        if let Ok(mut core) = self.core.lock() {
            core.muted = m;
        }
    }

    /// Mute state.
    pub fn muted(&self) -> bool {
        self.core.lock().map(|c| c.muted).unwrap_or(false)
    }

    /// Toggle mute.
    pub fn toggle_mute(&mut self) {
        if let Ok(mut core) = self.core.lock() {
            core.muted = !core.muted;
        }
    }

    /// Enable/disable bit-perfect (Direct Output) mode. When active, the
    /// audio callback bypasses the software volume/mute stage entirely and (at
    /// a native-rate match) delivers the stream untouched. Safe to call from
    /// any thread: the flag is an atomic read inside the audio callback.
    pub fn set_bit_perfect(&mut self, enabled: bool) {
        if let Ok(core) = self.core.lock() {
            core.bit_perfect.store(enabled, Ordering::Relaxed);
        }
    }

    /// Current bit-perfect flag.
    pub fn bit_perfect(&self) -> bool {
        self.core
            .lock()
            .map(|c| c.bit_perfect.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Set the dither mode applied during final quantization (i16/u8).
    pub fn set_dither(&mut self, dither: ResamplerDither) {
        if let Ok(core) = self.core.lock() {
            core.dither.store(dither_index(dither), Ordering::Relaxed);
        }
    }

    /// Whether the core is actively decoding (not paused/stopped).
    pub fn is_playing(&self) -> bool {
        self.core.lock().map(|c| c.playing).unwrap_or(false)
    }

    /// True when the current track played to its natural end (EOF).
    pub fn ended(&self) -> bool {
        self.core.lock().map(|c| c.natural_end).unwrap_or(false)
    }

    /// Clear the natural-end flag (e.g. after the playlist handled it).
    pub fn clear_end(&mut self) {
        if let Ok(mut core) = self.core.lock() {
            core.natural_end = false;
        }
    }

    /// Attach (or detach) the visualizer tap producer. The audio callback
    /// writes post-resampler PCM into it only while [`Self::set_viz_tap_active`]
    /// keeps it enabled.
    pub fn set_viz_tap(&mut self, tap: Option<rtrb::Producer<f32>>) {
        if let Ok(mut core) = self.core.lock() {
            core.viz_tap = tap;
        }
    }

    /// Toggle the tap on/off. When off, the audio callback skips the copy
    /// entirely (zero CPU cost), per ТЗ §13.2.
    pub fn set_viz_tap_active(&mut self, active: bool) {
        if let Ok(mut core) = self.core.lock() {
            core.viz_tap_active = active;
        }
    }

    /// Return a snapshot for the UI: (playing, pos, duration).
    pub fn snapshot(&self) -> (bool, f64, Option<f64>) {
        let core = match self.core.lock() {
            Ok(c) => c,
            Err(_) => return (false, 0.0, None),
        };
        (core.playing, core.pos_secs, core.duration_secs())
    }

    /// Output format of the current audio stream: (sample rate, channels).
    /// Used by the visualizer to drive the FFT (tap is post-resampler PCM).
    pub fn format(&self) -> (u32, usize) {
        let core = match self.core.lock() {
            Ok(c) => c,
            Err(_) => return (44_100, 2),
        };
        (core.out_rate, core.out_ch)
    }
}

fn effective_volume(core: &PlaybackCore) -> f32 {
    if core.muted {
        0.0
    } else {
        core.volume
    }
}

/// Whether dithering must be applied during final quantization (i16/u8).
///
/// Dither is skipped in a true bit-perfect passthrough (native sample rate,
/// resampler inactive) so the samples stay untouched. It is applied in the
/// normal Mixed path and in bit-perfect mode whenever a rate conversion forced
/// a re-quantization anyway — there the noise is the only thing masking the
/// quantization error.
fn dither_enabled(core: &PlaybackCore) -> bool {
    let idx = core.dither.load(Ordering::Relaxed);
    if idx == DITHER_INDEX_OFF {
        return false;
    }
    if core.bit_perfect.load(Ordering::Relaxed) {
        let native = !core.resampler.as_ref().map(|r| r.is_enabled()).unwrap_or(true);
        if native {
            return false;
        }
    }
    true
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

// ---- Audio callback entry points (called from the cpal audio thread). ----
//
// These run on the real-time audio thread and must never block. They use
// `try_lock()`; if the UI thread holds the lock (e.g. during a seek or track
// open), the callback emits silence for that buffer instead of blocking.

/// cpal callback for f32 output: pulls up to `data.len()` frames from the
/// decoder/resampler, applies volume (bypassed in bit-perfect mode),
/// returns silence on lock contention.
pub fn audio_callback_f32(core: &Arc<Mutex<PlaybackCore>>, data: &mut [f32]) {
    let Ok(mut c) = core.try_lock() else {
        data.fill(0.0);
        return;
    };
    if !c.playing {
        data.fill(0.0);
        return;
    }
    // Direct Output: bit-perfect bypasses the software volume stage (mute is
    // ignored too — volume control moves to the external DAC/amp).
    let vol = if c.bit_perfect.load(Ordering::Relaxed) {
        1.0
    } else {
        effective_volume(&c)
    };
    let produced = c.fill(data);
    c.pos_secs += produced as f64 / c.out_rate as f64 / c.out_ch as f64;
    // Visualizer tap: post-resampler PCM, exactly what reaches the DAC
    // (WYSIWYG), before soft volume is applied. Non-blocking: drops on full.
    if c.viz_tap_active {
        if let Some(tap) = c.viz_tap.as_mut() {
            for s in data.iter().take(produced) {
                if tap.push(*s).is_err() {
                    break;
                }
            }
        }
    }
    // Skip the multiply entirely when the effective volume is unity (saves
    // cycles and preserves bits for `volume == 1.0`).
    if vol < 1.0 {
        for s in data.iter_mut().take(produced) {
            *s *= vol;
        }
    }
    if produced < data.len() {
        data[produced..].fill(0.0);
        if c.decoder.as_ref().map(|d| d.eof()).unwrap_or(false) {
            c.playing = false;
            c.finished = true;
            c.natural_end = true;
        }
    }
}

/// cpal callback for i16 output (see [`audio_callback_f32`]).
pub fn audio_callback_i16(core: &Arc<Mutex<PlaybackCore>>, data: &mut [i16]) {
    let Ok(mut c) = core.try_lock() else {
        data.fill(0);
        return;
    };
    if !c.playing {
        data.fill(0);
        return;
    }
    let vol = if c.bit_perfect.load(Ordering::Relaxed) {
        1.0
    } else {
        effective_volume(&c)
    };
    let (apply_dither, dither_amp) = if dither_enabled(&c) {
        (true, dither_amplitude(c.dither.load(Ordering::Relaxed)))
    } else {
        (false, 1.0)
    };
    let out_ch = c.out_ch;
    let mut tmp = c.scratch_f32();
    tmp.resize(data.len(), 0.0);
    let produced = c.fill(&mut tmp);
    c.pos_secs += produced as f64 / c.out_rate as f64 / out_ch as f64;
    if c.viz_tap_active {
        if let Some(tap) = c.viz_tap.as_mut() {
            for s in tmp.iter().take(produced) {
                if tap.push(*s).is_err() {
                    break;
                }
            }
        }
    }
    let mut tpdf = c.tpdf;
    for (dst, &src) in data.iter_mut().zip(tmp.iter()).take(produced) {
        let mut x = src.clamp(-1.0, 1.0) * vol * 32767.0;
        if apply_dither {
            x += tpdf.next_tpdf() * dither_amp;
        }
        *dst = x.round().clamp(-32768.0, 32767.0) as i16;
    }
    c.tpdf = tpdf;
    for s in data.iter_mut().skip(produced) {
        *s = 0;
    }
    if produced < data.len() && c.decoder.as_ref().map(|d| d.eof()).unwrap_or(false) {
        c.playing = false;
        c.finished = true;
        c.natural_end = true;
    }
    c.scratch_release(tmp);
}

/// cpal callback for u8 output (silence = 128, samples centered at 0.5).
pub fn audio_callback_u8(core: &Arc<Mutex<PlaybackCore>>, data: &mut [u8]) {
    let Ok(mut c) = core.try_lock() else {
        data.fill(128);
        return;
    };
    if !c.playing {
        data.fill(128);
        return;
    }
    let vol = if c.bit_perfect.load(Ordering::Relaxed) {
        1.0
    } else {
        effective_volume(&c)
    };
    let (apply_dither, dither_amp) = if dither_enabled(&c) {
        (true, dither_amplitude(c.dither.load(Ordering::Relaxed)))
    } else {
        (false, 1.0)
    };
    let out_ch = c.out_ch;
    let mut tmp = c.scratch_f32();
    tmp.resize(data.len(), 0.0);
    let produced = c.fill(&mut tmp);
    c.pos_secs += produced as f64 / c.out_rate as f64 / out_ch as f64;
    if c.viz_tap_active {
        if let Some(tap) = c.viz_tap.as_mut() {
            for s in tmp.iter().take(produced) {
                if tap.push(*s).is_err() {
                    break;
                }
            }
        }
    }
    let mut tpdf = c.tpdf;
    for (dst, &src) in data.iter_mut().zip(tmp.iter()).take(produced) {
        let mut x = (src.clamp(-1.0, 1.0) * vol * 0.5 + 0.5) * 255.0;
        if apply_dither {
            x += tpdf.next_tpdf() * dither_amp;
        }
        *dst = x.round().clamp(0.0, 255.0) as u8;
    }
    c.tpdf = tpdf;
    for s in data.iter_mut().skip(produced) {
        *s = 128;
    }
    if produced < data.len() && c.decoder.as_ref().map(|d| d.eof()).unwrap_or(false) {
        c.playing = false;
        c.finished = true;
        c.natural_end = true;
    }
    c.scratch_release(tmp);
}

/// cpal callback for i32 output on the DoP (DSD over PCM) path: samples are
/// already framed 24-bit DoP words (marker 0x05/0xFA in bits 23-16, two DSD
/// bytes in bits 15-0), carried verbatim as f32 through the identity resampler.
/// The i32 container left-aligns them: `<< 8` places the marker into bits
/// 31-24 (mpv `marker << 24 | d0 << 16 | d1 << 8`; the low byte stays zero).
/// This is a Direct Output path: no volume, no dither, no resampler, so the
/// raw DSD stream reaches the DAC untouched.
pub fn audio_callback_i32_dop(core: &Arc<Mutex<PlaybackCore>>, data: &mut [i32]) {
    let Ok(mut c) = core.try_lock() else {
        data.fill(0);
        return;
    };
    if !c.playing {
        data.fill(0);
        return;
    }
    let out_ch = c.out_ch;
    let mut tmp = c.scratch_f32();
    tmp.resize(data.len(), 0.0);
    let produced = c.fill(&mut tmp);
    c.pos_secs += produced as f64 / c.out_rate as f64 / out_ch as f64;
    for (dst, &src) in data.iter_mut().zip(tmp.iter()).take(produced) {
        *dst = ((src.clamp(0.0, 16_777_215.0) as u32) << 8) as i32;
    }
    for s in data.iter_mut().skip(produced) {
        *s = 0;
    }
    if produced < data.len() && c.decoder.as_ref().map(|d| d.eof()).unwrap_or(false) {
        c.playing = false;
        c.finished = true;
        c.natural_end = true;
    }
    c.scratch_release(tmp);
}

/// cpal callback for i32 PCM output (a native-I32 hardware node, e.g. the raw
/// `hw:*` ALSA handle of a USB DAC). Same semantics as [`audio_callback_i16`]
/// scaled to 32-bit: volume (bypassed in bit-perfect mode), optional TPDF
/// dither, hard clipping to the i32 range.
pub fn audio_callback_i32_pcm(core: &Arc<Mutex<PlaybackCore>>, data: &mut [i32]) {
    let Ok(mut c) = core.try_lock() else {
        data.fill(0);
        return;
    };
    if !c.playing {
        data.fill(0);
        return;
    }
    let vol = if c.bit_perfect.load(Ordering::Relaxed) {
        1.0
    } else {
        effective_volume(&c)
    };
    let (apply_dither, dither_amp) = if dither_enabled(&c) {
        (true, dither_amplitude(c.dither.load(Ordering::Relaxed)))
    } else {
        (false, 1.0)
    };
    let out_ch = c.out_ch;
    let mut tmp = c.scratch_f32();
    tmp.resize(data.len(), 0.0);
    let produced = c.fill(&mut tmp);
    c.pos_secs += produced as f64 / c.out_rate as f64 / out_ch as f64;
    if c.viz_tap_active {
        if let Some(tap) = c.viz_tap.as_mut() {
            for s in tmp.iter().take(produced) {
                if tap.push(*s).is_err() {
                    break;
                }
            }
        }
    }
    const I32_MAX: f64 = 2_147_483_647.0;
    let mut tpdf = c.tpdf;
    for (dst, &src) in data.iter_mut().zip(tmp.iter()).take(produced) {
        let mut x = src.clamp(-1.0, 1.0) as f64 * vol as f64 * I32_MAX;
        if apply_dither {
            x += tpdf.next_tpdf() as f64 * dither_amp as f64;
        }
        *dst = x.round().clamp(-I32_MAX - 1.0, I32_MAX) as i32;
    }
    c.tpdf = tpdf;
    for s in data.iter_mut().skip(produced) {
        *s = 0;
    }
    if produced < data.len() && c.decoder.as_ref().map(|d| d.eof()).unwrap_or(false) {
        c.playing = false;
        c.finished = true;
        c.natural_end = true;
    }
    c.scratch_release(tmp);
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Player {
    /// Test-only: build a `Player` without touching the audio backend.
    fn test_new() -> Self {
        Self {
            core: Arc::new(Mutex::new(PlaybackCore::new())),
            stream: None,
            device_desc: String::from("test"),
            last_error: None,
            preferred_device: None,
            resampler_algo: ResamplerAlgorithm::SincMedium,
            dsd_mode: DsdMode::Pcm,
        }
    }
}

// Keep stream alive (no-op guard used by the main loop).
#[allow(dead_code)]
/// Keep the stream object alive for its intended lifetime (used by the
/// startup probe, which must hold the stream until the device is checked).
pub fn keep_alive(_s: &cpal::Stream) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decoder::TrackInfo;

    /// Deterministic test source: mono PCM at 44100 Hz, constant amplitude.
    struct MockSource {
        info: TrackInfo,
        data: Vec<f32>,
        offset: usize,
        scratch: Vec<f32>,
        eof: bool,
    }

    impl MockSource {
        fn new(frames: usize) -> Self {
            let rate = 44100;
            Self {
                info: TrackInfo {
                    sample_rate: rate,
                    channels: 1,
                    num_frames: Some(frames as u64),
                    format_name: "mock".into(),
                    bitrate: 0,
                    bits: Some(16),
                    tags: Default::default(),
                },
                data: vec![0.25; frames],
                offset: 0,
                scratch: vec![0.0; 512],
                eof: false,
            }
        }
    }

    impl AudioSource for MockSource {
        fn next_frames(&mut self) -> Option<&[f32]> {
            if self.eof || self.offset >= self.data.len() {
                self.eof = true;
                return None;
            }
            let n = self.scratch.len().min(self.data.len() - self.offset);
            self.scratch[..n].copy_from_slice(&self.data[self.offset..self.offset + n]);
            self.offset += n;
            Some(&self.scratch[..n])
        }
        fn seek(&mut self, _secs: f64) -> Result<(), String> {
            self.offset = 0;
            self.eof = false;
            Ok(())
        }
        fn info(&self) -> &TrackInfo {
            &self.info
        }
        fn eof(&self) -> bool {
            self.eof
        }
    }

    /// Core with a mono 1:1 resampler so callbacks can actually produce
    /// frames from `MockSource`.
    fn core_with_source(frames: usize) -> Arc<Mutex<PlaybackCore>> {
        let core = Arc::new(Mutex::new(PlaybackCore::new()));
        seed_core(&core, frames);
        core
    }

    fn seed_core(core: &Arc<Mutex<PlaybackCore>>, frames: usize) {
        let mut c = core.lock().unwrap();
        c.decoder = Some(Box::new(MockSource::new(frames)));
        c.resampler = Some(Resampler::new(44100, 44100, 1, 1));
        c.out_rate = 44100;
        c.out_ch = 1;
        c.playing = false;
        c.finished = false;
        c.natural_end = false;
        c.pos_secs = 0.0;
    }

    fn player_with_source(frames: usize) -> Player {
        let p = Player::test_new();
        seed_core(&p.core, frames);
        p
    }

    #[test]
    fn play_without_decoder_is_noop() {
        let mut p = Player::test_new();
        p.play();
        assert!(!p.is_playing());
    }

    #[test]
    fn toggle_pauses_and_resumes() {
        let mut p = player_with_source(1000);
        p.toggle();
        assert!(p.is_playing(), "toggle should start playback");
        p.toggle();
        assert!(!p.is_playing(), "toggle should pause");
    }

    #[test]
    fn stop_rewinds_and_confirms_manual_end() {
        let mut p = player_with_source(1000);
        p.play();
        p.stop();
        assert!(!p.is_playing());
        assert_eq!(p.snapshot().1, 0.0, "stop must rewind to start");
        let c = p.core.lock().unwrap();
        assert!(c.finished);
        assert!(!c.natural_end, "manual stop is not a natural end");
    }

    #[test]
    fn play_after_stop_replays_from_start() {
        let mut p = player_with_source(1000);
        p.play();
        p.stop();
        p.play();
        assert!(p.is_playing());
        let c = p.core.lock().unwrap();
        assert!(!c.finished);
        assert_eq!(c.pos_secs, 0.0);
    }

    #[test]
    fn seek_moves_position() {
        let mut p = player_with_source(100_000);
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
        let p = player_with_source(4_410_000);
        let (_, _, dur) = p.snapshot();
        assert_eq!(dur, Some(100.0));
    }

    #[test]
    fn callback_applies_volume() {
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
            c.volume = 0.5;
        }
        let mut buf = vec![0.0; 256];
        audio_callback_f32(&core, &mut buf);
        let filled = buf.len() - buf.iter().rev().take_while(|s| **s == 0.0).count();
        assert!(filled > 0, "expected audio output");
        // Constant 0.25 input scaled by 0.5 volume.
        assert!(
            buf[..filled].iter().all(|s| (*s - 0.125).abs() < 1e-6),
            "volume scaling"
        );
    }

    #[test]
    fn callback_advances_position_while_playing() {
        let core = core_with_source(4_410_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
        }
        let mut buf = vec![0.0; 256];
        audio_callback_f32(&core, &mut buf);
        let pos = core.lock().unwrap().pos_secs;
        assert_eq!(pos, 256.0 / 44100.0);
    }

    #[test]
    fn callback_silences_when_muted() {
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
            c.muted = true;
        }
        let mut buf = vec![0.3; 256];
        audio_callback_f32(&core, &mut buf);
        assert!(buf.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn callback_marks_natural_end_at_eof() {
        let core = core_with_source(1000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
        }
        let mut buf = vec![0.0; 4096];
        audio_callback_f32(&core, &mut buf);

        let ended = core.lock().unwrap();
        assert!(!ended.playing, "EOF must stop playback");
        assert!(ended.finished);
        assert!(ended.natural_end);
    }

    #[test]
    fn callback_emits_silence_when_lock_held() {
        let core = Arc::new(Mutex::new(PlaybackCore::new()));
        // std Mutex is not reentrant: holding the guard here simulates the
        // UI thread owning the lock while the audio thread tries to write.
        let _guard = core.lock().unwrap();
        let mut buf = vec![0.3; 8];
        audio_callback_f32(&core, &mut buf);
        assert!(buf.iter().all(|s| *s == 0.0), "callback must not block");
    }

    #[test]
    fn scratch_capacity_is_pinned() {
        // After `open` (simulated via prepare_scratch) the f32 pool stays
        // pinned to MAX_OUT_SAMPLES: a resize inside the callback reuses the
        // capacity and never reallocates (ТЗ A2.0 §3.1).
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.prepare_scratch();
            c.playing = true;
            c.dither.store(DITHER_INDEX_OFF, Ordering::Relaxed);
        }
        let mut buf = vec![0i16; 256];
        audio_callback_i16(&core, &mut buf);
        {
            let c = core.lock().unwrap();
            assert_eq!(c.scratch.capacity(), MAX_OUT_SAMPLES);
            assert_eq!(c.scratch.len(), 0, "pool must be returned cleared");
        }
        // A buffer at the ceiling must also never grow the pool.
        let mut buf = vec![0i16; MAX_OUT_SAMPLES];
        audio_callback_i16(&core, &mut buf);
        let c = core.lock().unwrap();
        assert_eq!(c.scratch.capacity(), MAX_OUT_SAMPLES);
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
        assert!(0.0 > -1.5, "guard");
    }

    #[test]
    fn bit_perfect_bypasses_software_volume() {
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
            c.volume = 0.5;
            c.bit_perfect.store(true, Ordering::Relaxed);
        }
        let mut buf = vec![0.0; 256];
        audio_callback_f32(&core, &mut buf);
        let filled = buf.len() - buf.iter().rev().take_while(|s| **s == 0.0).count();
        assert!(buf[..filled].iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "bit-perfect must not scale samples by software volume");
    }

    #[test]
    fn bit_perfect_passthrough_keeps_samples_untouched() {
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
            c.volume = 0.3;
            c.muted = true;
            c.bit_perfect.store(true, Ordering::Relaxed);
        }
        let mut buf = vec![0.0; 256];
        audio_callback_f32(&core, &mut buf);
        assert!(buf.iter().any(|s| *s != 0.0), "mute must be ignored in bit-perfect");
        let filled = buf.len() - buf.iter().rev().take_while(|s| **s == 0.0).count();
        assert!(buf[..filled].iter().all(|s| (*s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn i16_clamps_and_scales_volume() {
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
            c.volume = 0.5;
            c.dither.store(DITHER_INDEX_OFF, Ordering::Relaxed);
        }
        let mut buf = vec![0i16; 256];
        audio_callback_i16(&core, &mut buf);
        let produced = buf.len() - buf.iter().rev().take_while(|s| **s == 0).count();
        assert!(produced > 0, "expected i16 output");
        let expected = (0.25f32 * 0.5 * 32767.0).round() as i16;
        assert!(buf[..produced].iter().all(|s| *s == expected), "got {:?}", &buf[..produced]);
    }

    #[test]
    fn i32_pcm_scales_and_clamps_volume() {
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
            c.volume = 0.5;
            c.dither.store(DITHER_INDEX_OFF, Ordering::Relaxed);
        }
        let mut buf = vec![0i32; 256];
        audio_callback_i32_pcm(&core, &mut buf);
        let produced = buf.len() - buf.iter().rev().take_while(|s| **s == 0).count();
        assert!(produced > 0, "expected i32 PCM output");
        let expected = (0.25f32 * 0.5 * 2_147_483_647.0).round() as i32;
        assert!(buf[..produced].iter().all(|s| *s == expected), "got {:?}", &buf[..produced]);
    }

    #[test]
    fn i32_pcm_is_not_dop_packed() {
        // The ADI-2 regression: the old i32 callback was the DoP packer
        // (`(src << 8)`), producing silence for plain PCM. A positive sample
        // must come out as a positive i32 value, not a left-shifted byte.
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
            c.volume = 1.0;
            c.dither.store(DITHER_INDEX_OFF, Ordering::Relaxed);
        }
        let mut buf = vec![0i32; 64];
        audio_callback_i32_pcm(&core, &mut buf);
        let produced = buf.len() - buf.iter().rev().take_while(|s| **s == 0).count();
        assert!(produced > 0);
        let expected = (0.25f32 * 2_147_483_647.0).round() as i32;
        assert_eq!(buf[0], expected);
        // A left-shifted 0.25 would be 0x40000000 (or clamped 0.0 in old code).
        assert!(buf[0] > 0 && buf[0] != 0x4000_0000_i32, "DoP packing leaked into PCM");
    }

    #[test]
    fn tpdf_dither_adds_noise_to_quantization() {
        let core = core_with_source(10_000);
        {
            let mut c = core.lock().unwrap();
            c.playing = true;
            c.volume = 1.0;
            c.dither.store(DITHER_INDEX_TPDF, Ordering::Relaxed);
        }
        let mut buf = vec![0i16; 256];
        audio_callback_i16(&core, &mut buf);
        let mut unique = std::collections::BTreeSet::new();
        for &s in &buf {
            unique.insert(s);
        }
        assert!(unique.len() > 1, "dithering must make quantized samples vary");
        // All output must stay within i16 bounds.
        let (lo, hi) = (i16::MIN, i16::MAX);
        assert!(buf.iter().all(|s| *s >= lo && *s <= hi));
    }
}
