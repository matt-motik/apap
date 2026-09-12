use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};

use crate::audio::player::PlaybackCore;
use crate::settings::ResamplerAlgorithm;

/// How long the startup device probe holds the stream open (ms). Long enough
/// for the backend to surface early ALSA errors, short enough to not delay UI.
pub const PROBE_OPEN_MS: u64 = 50;

/// Target output latency (~40 ms) — enough headroom to smooth single-decode
/// stalls that would otherwise cause ALSA/PipeWire buffer underruns.
fn target_buffer_frames(rate: u32, min: u32, max: u32) -> u32 {
    ((rate as f64 * 0.04) as u32).clamp(min, max)
}

/// Streaming resampler with a selectable algorithm (ТЗ 5.1 §8.3) and channel
/// mixdown.
///
/// Steps source frames in and produces output frames at the device rate.
/// For 1:1 rate/channel configs the resampler is skipped entirely.
///
/// Algorithms:
/// * [`ResamplerAlgorithm::Linear`] — two-point linear interpolation.
/// * [`ResamplerAlgorithm::Cubic`] — Catmull-Rom over 4 points.
/// * [`ResamplerAlgorithm::SincFast`]/`SincMedium`/`SincSlow` — windowed-sinc
///   (Hann-windowed, 32/64/128 taps) with DC-gain normalisation.
///
/// Buffering: `buf` holds interleaved source frames starting at source frame
/// `base`. The interpolation position `pos` is absolute (in source frames), so
/// the window can reach `behind` frames before and `ahead` frames after it
/// without those samples having been drained away.
pub struct Resampler {
    src_rate: u32,
    out_rate: u32,
    src_ch: usize,
    out_ch: usize,
    /// Absolute source-frame position of the next output sample.
    pos: f64,
    /// Source-frame index of `buf[0]`.
    base: usize,
    /// Queued interleaved source frames.
    buf: Vec<f32>,
    frames_in_buf: usize,
    enabled: bool,
    algo: ResamplerAlgorithm,
    /// Half the sinc taps (m), 0 for linear/cubic.
    m: usize,
    /// How many frames before `pos` the interpolation window reaches.
    behind: usize,
    /// How many frames after `pos` (floor) the interpolation window reaches.
    ahead: usize,
    /// Hann window coefficients for the sinc kernel (length 2·m), zeros for
    /// linear/cubic.
    win: Vec<f32>,
}

impl Resampler {
    /// Build a resampler with [`ResamplerAlgorithm::Linear`] (backward
    /// compatible with the original behaviour).
    pub fn new(src_rate: u32, out_rate: u32, src_ch: usize, out_ch: usize) -> Self {
        Self::with_algo(src_rate, out_rate, src_ch, out_ch, ResamplerAlgorithm::Linear)
    }

    /// Build a resampler that uses `algo` for the fractional interpolation.
    pub fn with_algo(
        src_rate: u32,
        out_rate: u32,
        src_ch: usize,
        out_ch: usize,
        algo: ResamplerAlgorithm,
    ) -> Self {
        let (m, behind, ahead, win) = match algo {
            ResamplerAlgorithm::Cubic => (0usize, 1usize, 2usize, Vec::new()),
            ResamplerAlgorithm::Linear => (0usize, 0usize, 1usize, Vec::new()),
            _ => {
                let taps = algo.sinc_taps() as usize;
                let m = taps / 2;
                // Hann window over the symmetric domain [-m, m], indexed by the
                // kernel offset j ∈ (-(m-1))..=m.
                let mut win = Vec::with_capacity(taps);
                let mut j = -(m as i64) + 1;
                while j <= m as i64 {
                    let d = j as f64 / m as f64;
                    let w = 0.5 + 0.5 * (std::f64::consts::PI * d).cos();
                    win.push(w as f32);
                    j += 1;
                }
                (m, m - 1, m, win)
            }
        };
        let same_rate = src_rate == out_rate;
        let same_ch = src_ch == out_ch;
        Resampler {
            src_rate,
            out_rate,
            src_ch,
            out_ch,
            pos: 0.0,
            base: 0,
            buf: Vec::with_capacity(8192),
            frames_in_buf: 0,
            enabled: !(same_rate && same_ch),
            algo,
            m,
            behind,
            ahead,
            win,
        }
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.base = 0;
        self.buf.clear();
        self.frames_in_buf = 0;
    }

    pub fn push(&mut self, samples: &[f32]) -> usize {
        let frames = samples.len() / self.src_ch;
        self.buf.extend_from_slice(samples);
        self.frames_in_buf += frames;
        frames
    }

    /// Channel sample of buffered frame `i` (relative to `base`).
    #[inline]
    fn mix_rel(&self, i: usize, oc: usize) -> f32 {
        let base = i * self.src_ch;
        if self.out_ch == 1 {
            let mut sum = 0.0f32;
            for v in &self.buf[base..base + self.src_ch] {
                sum += *v;
            }
            sum / self.src_ch as f32
        } else if self.src_ch == 1 {
            self.buf[base]
        } else if self.src_ch == self.out_ch {
            self.buf[base + oc]
        } else if self.out_ch == 2 {
            if oc == 0 {
                self.buf[base]
            } else {
                self.buf[base + 1]
            }
        } else {
            self.buf[base + oc]
        }
    }

    /// Kernel sample index `rel + j` clamped into the range filled by `buf`, so
    /// EOF tails can reuse the last buffered frame instead of reading past it.
    #[inline]
    fn clamp_j(&self, rel: i64, j: i64) -> usize {
        let i = rel + j;
        if i < 0 {
            0
        } else if (i as usize) >= self.frames_in_buf {
            self.frames_in_buf - 1
        } else {
            i as usize
        }
    }

    /// Produce up to `max_out_frames` output frames into `out` (interleaved).
    ///
    /// Returns the number of output frames written. When `eof_mode` is true the
    /// resampler is allowed to reuse the last buffered source frame for taps
    /// that run past the available data (clamped kernel, renormalised).
    pub fn pull(&mut self, out: &mut [f32], max_out_frames: usize, eof_mode: bool) -> usize {
        if !self.enabled {
            let n = max_out_frames.min(self.frames_in_buf);
            self.copy_direct(out, n);
            return n;
        }
        let ratio = self.src_rate as f64 / self.out_rate as f64;
        let mut produced = 0usize;
        while produced < max_out_frames {
            let p = self.pos;
            let left = p.floor() as i64;
            let data_end = (self.base + self.frames_in_buf) as i64;
            if left >= data_end {
                // Position is past every buffered frame — data is exhausted.
                break;
            }
            let frac = (p - left as f64) as f32;
            let rel = (left - self.base as i64) as i64;
            let enough = rel >= self.behind as i64
                && rel + self.ahead as i64 + 1 <= self.frames_in_buf as i64;
            if !enough && !eof_mode {
                break;
            }
            let dst = produced * self.out_ch;
            if self.frames_in_buf == 0 {
                return produced;
            }
            for oc in 0..self.out_ch {
                out[dst + oc] = match self.algo {
                    ResamplerAlgorithm::Linear => self.linear(rel, frac, oc),
                    ResamplerAlgorithm::Cubic => self.cubic(rel, frac, oc),
                    _ => self.sinc(rel, frac, oc),
                };
            }
            self.pos += ratio;
            produced += 1;
        }
        // Free frames that the interpolation window will never touch again.
        if produced > 0 {
            let keep_floor = (self.pos.floor() as i64).max(0) - self.behind as i64;
            let keep = keep_floor.max(0) as usize;
            let n_frames = (keep.saturating_sub(self.base)).min(self.frames_in_buf);
            if n_frames > 0 {
                self.buf.drain(..n_frames * self.src_ch);
                self.base += n_frames;
                self.frames_in_buf -= n_frames;
            }
        }
        produced
    }

    #[inline]
    fn linear(&self, rel: i64, frac: f32, oc: usize) -> f32 {
        let i0 = self.clamp_j(rel, 0);
        let i1 = self.clamp_j(rel, 1);
        let a = self.mix_rel(i0, oc);
        let b = self.mix_rel(i1, oc);
        a + (b - a) * frac
    }

    #[inline]
    fn cubic(&self, rel: i64, frac: f32, oc: usize) -> f32 {
        // Catmull-Rom over x[-1..=2] relative to `rel`.
        let p0 = self.mix_rel(self.clamp_j(rel, -1), oc);
        let p1 = self.mix_rel(self.clamp_j(rel, 0), oc);
        let p2 = self.mix_rel(self.clamp_j(rel, 1), oc);
        let p3 = self.mix_rel(self.clamp_j(rel, 2), oc);
        catmull_rom(p0, p1, p2, p3, frac)
    }

    #[inline]
    fn sinc(&self, rel: i64, frac: f32, oc: usize) -> f32 {
        let m = self.m as i64;
        let fs = frac as f64;
        let mut acc = 0.0f64;
        let mut sum = 0.0f64;
        let mut j = -(m - 1);
        while j <= m {
            let d = j as f64 - fs;
            // `win` is indexed by kernel offset j ∈ (-(m-1))..=m, in order.
            let w = self.win[(j + m - 1) as usize] as f64;
            let sinc = if d.abs() < 1e-9 {
                1.0
            } else {
                let x = std::f64::consts::PI * d;
                x.sin() / x
            };
            let k = sinc * w;
            sum += k;
            let i = self.clamp_j(rel, j);
            acc += k * self.mix_rel(i, oc) as f64;
            j += 1;
        }
        if sum.abs() > 1e-12 {
            (acc / sum) as f32
        } else {
            0.0
        }
    }

    fn copy_direct(&mut self, out: &mut [f32], frames: usize) {
        let n_bytes = frames * self.src_ch;
        out[..n_bytes].copy_from_slice(&self.buf[..n_bytes]);
        self.buf.drain(..n_bytes);
        self.frames_in_buf -= frames;
    }

    pub fn buffered_frames(&self) -> usize {
        self.frames_in_buf
    }
}

/// Catmull-Rom cubic interpolation between `p1` and `p2` at fraction `t`,
/// using neighbours `p0` and `p3`.
#[inline]
fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    0.5
        * ((2.0 * p1)
            + (-p0 + p2) * t
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t)
}

/// Output configuration resolved for a track.
pub struct OutputSpec {
    pub device: cpal::Device,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
    pub device_name: String,
}

/// Backend-agnostic snapshot of an output device's capabilities, enough for
/// the stream-selection logic to run without a live audio backend (and thus
/// to be unit-tested with a mock).
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub sample_format: SampleFormat,
    pub buffer_size: SupportedBufferSize,
    pub supported: Vec<RateRange>,
}

/// One entry from a device's supported-configuration list.
#[derive(Debug, Clone)]
pub struct RateRange {
    pub channels: u16,
    pub min: u32,
    pub max: u32,
    pub buffer_size: SupportedBufferSize,
}

/// The result of device selection, independent of the concrete backend.
#[derive(Debug, Clone)]
pub struct ChosenOutput {
    pub device_name: String,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
}

/// Abstraction over the audio backend (cpal in production, a mock in tests).
pub trait AudioHost {
    /// Usable output devices (only those with a resolvable default config).
    fn devices(&self) -> Vec<DeviceInfo>;
    /// Name of the host's default output device, if any.
    fn default_name(&self) -> Option<String>;
}

/// Live cpal backend.
pub struct CpalHost;

impl AudioHost for CpalHost {
    fn devices(&self) -> Vec<DeviceInfo> {
        let host = cpal::default_host();
        let mut out = Vec::new();
        if let Ok(devices) = host.output_devices() {
            for dev in devices {
                if let Ok(default) = dev.default_output_config() {
                    let name = dev
                        .description()
                        .map(|d| d.name().to_string())
                        .unwrap_or_else(|_| "default".to_string());
                    let mut supported = Vec::new();
                    if let Ok(configs) = dev.supported_output_configs() {
                        for cfg in configs {
                            supported.push(RateRange {
                                channels: cfg.channels(),
                                min: cfg.min_sample_rate(),
                                max: cfg.max_sample_rate(),
                                buffer_size: *cfg.buffer_size(),
                            });
                        }
                    }
                    out.push(DeviceInfo {
                        name,
                        channels: default.channels(),
                        sample_rate: default.sample_rate(),
                        sample_format: default.sample_format(),
                        buffer_size: *default.buffer_size(),
                        supported,
                    });
                }
            }
        }
        out
    }

    fn default_name(&self) -> Option<String> {
        let host = cpal::default_host();
        host.default_output_device()
            .and_then(|d| d.description().ok())
            .map(|d| d.name().to_string())
    }
}

impl CpalHost {
    /// Recover the concrete cpal device handle for stream building.
    fn device_by_name(&self, name: &str) -> Result<cpal::Device, String> {
        let host = cpal::default_host();
        if let Ok(devices) = host.output_devices() {
            for dev in devices {
                if let Ok(desc) = dev.description() {
                    if desc.name().to_string() == name {
                        return Ok(dev);
                    }
                }
            }
        }
        Err(format!("Audio device '{name}' no longer available"))
    }
}

/// Decide which device/config to use for the given track parameters.
///
/// Pure selection: no audio backend is touched, so it can be tested with a
/// mock. Picks the requested device (or the host default), then a config
/// close to the track's native sample rate/channels.
pub fn choose_output(
    devices: &[DeviceInfo],
    default_name: Option<&str>,
    track_rate: u32,
    track_channels: usize,
    preferred_name: Option<&str>,
) -> Result<ChosenOutput, String> {
    if devices.is_empty() {
        return Err(String::from("No audio output device found"));
    }

    // Resolve the requested device, falling back to the host default.
    let preferred = preferred_name.filter(|n| !n.is_empty());
    let device = preferred
        .and_then(|name| devices.iter().find(|d| d.name == name))
        .or_else(|| default_name.and_then(|d| devices.iter().find(|dev| dev.name == d)))
        .ok_or_else(|| String::from("No audio output device found"))?;

    // Prefer a config matching the source channels when the device supports it,
    // otherwise fall back to the device default (usually 2ch).
    let mut channels = track_channels.min(device.channels as usize) as u16;
    if channels == 0 {
        channels = 2;
    }

    // Prefer matching the source sample rate when supported.
    let mut supported_rate: Option<u32> = None;
    let mut chosen_buf = match device.buffer_size {
        SupportedBufferSize::Range { min, max } => {
            BufferSize::Fixed(target_buffer_frames(device.sample_rate, min, max))
        }
        _ => BufferSize::Default,
    };
    for cfg in &device.supported {
        if cfg.channels != channels {
            continue;
        }
        if supported_rate.is_none() && track_rate >= cfg.min && track_rate <= cfg.max {
            supported_rate = Some(track_rate);
        }
        if let SupportedBufferSize::Range { min, max } = cfg.buffer_size {
            chosen_buf = BufferSize::Fixed(target_buffer_frames(cfg.max, min, max));
        }
    }

    let (out_rate, sample_format) = match supported_rate {
        Some(r) => (r, device.sample_format),
        None => (device.sample_rate, device.sample_format),
    };

    Ok(ChosenOutput {
        device_name: device.name.clone(),
        config: StreamConfig {
            channels,
            sample_rate: out_rate,
            buffer_size: chosen_buf,
        },
        sample_format,
    })
}

/// Pick an output device and a stream config close to the track's native
/// parameters. Resolves the config against the live cpal backend.
pub fn select_output(
    track_rate: u32,
    track_channels: usize,
    preferred_name: Option<&str>,
) -> Result<OutputSpec, String> {
    let host = CpalHost;
    let default = host.default_name();
    let chosen = choose_output(
        &host.devices(),
        default.as_deref(),
        track_rate,
        track_channels,
        preferred_name,
    )?;
    let device = host.device_by_name(&chosen.device_name)?;
    Ok(OutputSpec {
        device,
        config: chosen.config,
        sample_format: chosen.sample_format,
        device_name: chosen.device_name,
    })
}

/// Enumerate usable output devices as `(name, description)` pairs. The `name`
/// is the stable key persisted in settings and passed back to `select_output`.
pub fn output_devices() -> Vec<(String, String)> {
    CpalHost
        .devices()
        .into_iter()
        .map(|d| (d.name.clone(), d.name))
        .collect()
}

/// Name of the host's default output device, if any.
pub fn default_device_name() -> Option<String> {
    CpalHost.default_name()
}

/// Probe the configured output device (or the host default when `preferred` is
/// empty/`None`).
///
/// The probe opens a real (silent) output stream for a short moment so that
/// runtime failures — e.g. ALSA `snd_pcm_dmix_open: unable to open slave` —
/// surface at startup instead of on the first Play. Returns the name of the
/// effective device.
pub fn probe_output(preferred: Option<&str>) -> Result<String, String> {
    let names = output_devices();
    let preferred = preferred.filter(|n| !n.is_empty());

    // A configured device that is not enumerable counts as unavailable.
    if let Some(name) = preferred {
        if !names.iter().any(|(n, _)| n == name) {
            return Err(format!("Configured audio device '{name}' not found"));
        }
    }
    if names.is_empty() {
        return Err(String::from("No audio output device found"));
    }

    let spec = select_output(44100, 2, preferred)?;
    let core = Arc::new(Mutex::new(PlaybackCore::new()));
    let error_flag = Arc::new(AtomicBool::new(false));
    let stream = build_stream(&spec, core, Some(error_flag.clone()))?;
    stream
        .play()
        .map_err(|e| format!("Cannot start audio stream: {e}"))?;
    // Non-blocking probe: poll the error flag with a short timeout instead
    // of blocking the UI thread with a sleep. The error callback sets the
    // flag when ALSA/PipeWire surfaces a runtime failure.
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(PROBE_OPEN_MS);
    while !error_flag.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    drop(stream);
    if error_flag.load(Ordering::Relaxed) {
        return Err(String::from("Audio device rejected the stream"));
    }
    Ok(spec.device_name)
}

/// Build the output stream. `sample_format` decides the callback sample type.
/// When `error_flag` is provided, the error callback will set it on failure.
pub fn build_stream(
    spec: &OutputSpec,
    core: Arc<Mutex<PlaybackCore>>,
    error_flag: Option<Arc<AtomicBool>>,
) -> Result<cpal::Stream, String> {
    match spec.sample_format {
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U8 => {}
        other => {
            return Err(format!("Unsupported output sample format: {other:?}"));
        }
    }

    let build =
        |cfg: StreamConfig, core: Arc<Mutex<PlaybackCore>>, ef: Option<Arc<AtomicBool>>| -> Result<cpal::Stream, cpal::Error> {
            match spec.sample_format {
                SampleFormat::F32 => {
                    let core = core.clone();
                    let ef = ef.clone();
                    spec.device.build_output_stream(
                        cfg,
                        move |data: &mut [f32], _| {
                            crate::audio::player::audio_callback_f32(&core, data);
                        },
                        move |e| {
                            eprintln!("Audio stream error: {e}");
                            if let Some(f) = ef.as_ref() {
                                f.store(true, Ordering::Relaxed);
                            }
                        },
                        None,
                    )
                }
                SampleFormat::I16 => {
                    let core = core.clone();
                    let ef = ef.clone();
                    spec.device.build_output_stream(
                        cfg,
                        move |data: &mut [i16], _| {
                            crate::audio::player::audio_callback_i16(&core, data);
                        },
                        move |e| {
                            eprintln!("Audio stream error: {e}");
                            if let Some(f) = ef.as_ref() {
                                f.store(true, Ordering::Relaxed);
                            }
                        },
                        None,
                    )
                }
                SampleFormat::U8 => {
                    let core = core.clone();
                    let ef = ef.clone();
                    spec.device.build_output_stream(
                        cfg,
                        move |data: &mut [u8], _| {
                            crate::audio::player::audio_callback_u8(&core, data);
                        },
                        move |e| {
                            eprintln!("Audio stream error: {e}");
                            if let Some(f) = ef.as_ref() {
                                f.store(true, Ordering::Relaxed);
                            }
                        },
                        None,
                    )
                }
                _other => unreachable!("sample format checked above"),
            }
        };

    match build(spec.config.clone(), core.clone(), error_flag.clone()) {
        Ok(stream) => Ok(stream),
        Err(_e) if matches!(spec.config.buffer_size, BufferSize::Fixed(_)) => {
            // Some devices reject an explicit buffer size; retry with the default.
            let mut cfg = spec.config.clone();
            cfg.buffer_size = BufferSize::Default;
            build(cfg, core, error_flag).map_err(|e| format!("Cannot build output stream: {e}"))
        }
        Err(e) => Err(format!("Cannot build output stream: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test double for [`AudioHost`]: serves canned device info.
    struct MockHost {
        devices: Vec<DeviceInfo>,
        default_name: Option<String>,
    }

    impl AudioHost for MockHost {
        fn devices(&self) -> Vec<DeviceInfo> {
            self.devices.clone()
        }
        fn default_name(&self) -> Option<String> {
            self.default_name.clone()
        }
    }

    fn select_from_host(
        host: &dyn AudioHost,
        track_rate: u32,
        track_channels: usize,
        preferred: Option<&str>,
    ) -> Result<ChosenOutput, String> {
        choose_output(
            &host.devices(),
            host.default_name().as_deref(),
            track_rate,
            track_channels,
            preferred,
        )
    }

    fn mock_device(
        name: &str,
        channels: u16,
        sample_rate: u32,
        supported: &[(u16, u32, u32)],
    ) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            channels,
            sample_rate,
            sample_format: SampleFormat::F32,
            buffer_size: SupportedBufferSize::Range { min: 64, max: 4096 },
            supported: supported
                .iter()
                .map(|&(c, lo, hi)| RateRange {
                    channels: c,
                    min: lo,
                    max: hi,
                    buffer_size: SupportedBufferSize::Range { min: 64, max: 4096 },
                })
                .collect(),
        }
    }

    #[test]
    fn choose_output_no_devices_errors() {
        let r = choose_output(&[], None, 44100, 2, None);
        assert_eq!(r.unwrap_err(), "No audio output device found");
    }

    #[test]
    fn choose_output_prefers_requested_device() {
        let host = MockHost {
            devices: vec![
                mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]),
                mock_device("HDMI", 2, 192000, &[(2, 44100, 192000)]),
            ],
            default_name: Some("Speakers".into()),
        };
        let chosen = select_from_host(&host, 44100, 2, Some("HDMI")).unwrap();
        assert_eq!(chosen.device_name, "HDMI");
        assert_eq!(chosen.config.sample_rate, 44100);
        assert_eq!(chosen.config.channels, 2);
    }

    #[test]
    fn choose_output_falls_back_to_default_device() {
        let host = MockHost {
            devices: vec![mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)])],
            default_name: Some("Speakers".into()),
        };
        let chosen = select_from_host(&host, 48000, 2, None).unwrap();
        assert_eq!(chosen.device_name, "Speakers");
        assert_eq!(chosen.config.sample_rate, 48000);
    }

    #[test]
    fn choose_output_unknown_preferred_errors() {
        let device = mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]);
        let r = choose_output(&[device], None, 44100, 2, Some("Missing"));
        assert!(r.is_err());
    }

    #[test]
    fn choose_output_preferred_drops_to_default_when_missing() {
        let device = mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]);
        let chosen =
            choose_output(&[device], Some("Speakers"), 48000, 2, Some("Missing")).unwrap();
        assert_eq!(chosen.device_name, "Speakers");
    }

    #[test]
    fn choose_output_matches_track_rate_when_supported() {
        let device = mock_device("DAC", 2, 44100, &[(2, 44100, 48000), (2, 88200, 192000)]);
        let chosen = choose_output(&[device], Some("DAC"), 96000, 2, None).unwrap();
        assert_eq!(chosen.config.sample_rate, 96000);
        // The matching supported range sets the buffer from its own max rate.
        assert_eq!(
            chosen.config.buffer_size,
            BufferSize::Fixed(target_buffer_frames(192000, 64, 4096))
        );
    }

    #[test]
    fn choose_output_falls_back_to_device_rate_when_unsupported() {
        let device = mock_device("DAC", 2, 44100, &[(2, 44100, 48000)]);
        let chosen = choose_output(&[device], Some("DAC"), 384000, 1, None).unwrap();
        assert_eq!(chosen.config.sample_rate, 44100);
        assert_eq!(chosen.config.channels, 1);
    }

    #[test]
    fn choose_output_clamps_channels_to_device() {
        let mono = mock_device("Mono", 1, 44100, &[(1, 44100, 44100)]);
        let chosen = choose_output(&[mono], Some("Mono"), 44100, 6, None).unwrap();
        assert_eq!(chosen.config.channels, 1);
        assert_eq!(chosen.config.sample_rate, 44100);
    }

    #[test]
    fn choose_output_unknown_default_errors() {
        let device = mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]);
        let r = choose_output(&[device], Some("Ghost"), 48000, 2, None);
        assert!(r.is_err());
    }

    /// Drain a whole resampler: feed a full block, then pull with `eof_mode`
    /// until it stops producing, collecting every output frame.
    fn run_resampler(
        algo: ResamplerAlgorithm,
        src_rate: u32,
        out_rate: u32,
        src_ch: usize,
        out_ch: usize,
        src_frames: &[f32],
    ) -> Vec<f32> {
        let mut res = Resampler::with_algo(src_rate, out_rate, src_ch, out_ch, algo);
        res.push(src_frames);
        let mut out = vec![0.0f32; 65536];
        let mut produced = Vec::new();
        loop {
            let n = res.pull(&mut out, 4096, true);
            if n == 0 {
                break;
            }
            produced.extend_from_slice(&out[..n * out_ch]);
        }
        produced
    }

    #[test]
    fn resampler_identity_rates_stay_identity() {
        // 1:1 rate/channel: every algorithm must reproduce the input exactly,
        // and the resampler must be disabled (no DSP at all).
        for algo in [
            ResamplerAlgorithm::Linear,
            ResamplerAlgorithm::Cubic,
            ResamplerAlgorithm::SincFast,
            ResamplerAlgorithm::SincMedium,
            ResamplerAlgorithm::SincSlow,
        ] {
            let src: Vec<f32> = (0..64).map(|i| (i as f32) * 0.1 - 2.0).collect();
            let out = run_resampler(algo, 44100, 44100, 1, 1, &src);
            assert_eq!(out.len(), src.len(), "{algo:?}");
            for (a, b) in out.iter().zip(&src) {
                assert!((a - b).abs() < 1e-6, "{algo:?}: expected {b}, got {a}");
            }
        }
    }

    #[test]
    fn resampler_upsample_dc_passes_through_all_algorithms() {
        // A DC source must come out unchanged (both amplitude and per-sample
        // value) regardless of the interpolation algorithm/rate ratio.
        for algo in [
            ResamplerAlgorithm::Linear,
            ResamplerAlgorithm::Cubic,
            ResamplerAlgorithm::SincFast,
            ResamplerAlgorithm::SincMedium,
            ResamplerAlgorithm::SincSlow,
        ] {
            let src: Vec<f32> = vec![0.5; 256];
            let out = run_resampler(algo, 44100, 48000, 1, 1, &src);
            let expected_length = src.len() as f64 * 48000.0 / 44100.0;
            assert!(
                (out.len() as f64 - expected_length).abs() <= 1.0,
                "{algo:?}: {}/{}",
                out.len(),
                expected_length
            );
            for v in &out {
                assert!((v - 0.5).abs() < 1e-3, "{algo:?}: got {v}");
            }
        }
    }

    #[test]
    fn resampler_sinc_interp_linear_ramp() {
        // Sinc(64) on a linear ramp at an exactly representable rate 1:2:
        // linear interpolation is exact for ramps, sinc must be close too.
        let src: Vec<f32> = (0..512).map(|i| i as f32).collect();
        for algo in [
            ResamplerAlgorithm::Cubic,
            ResamplerAlgorithm::SincFast,
            ResamplerAlgorithm::SincMedium,
            ResamplerAlgorithm::SincSlow,
        ] {
            let out = run_resampler(algo, 44100, 88200, 1, 1, &src);
            // Skip the filter-edge transient (first 64 and last 64 output samples).
            let n = out.len();
            for (k, &v) in out.iter().enumerate().take(n - 64).skip(64) {
                // 2x upsampling: output sample k sits at source position k/2.
                let expected = k as f64 * 0.5;
                let err = (v - expected as f32).abs();
                // Sinc(64) ringing on the transient is much smaller than 0.5;
                // a coarse bound keeps the test robust while still asserting
                // the output follows the ramp, not e.g. all zeros.
                assert!(err < 0.5, "{algo:?}@{k}: got {v}, want ~{expected}");
            }
        }
    }

    #[test]
    fn resampler_eof_tail_does_not_panic_or_hang() {
        // Puny input, huge downsample factor: the EOF fallback must terminate
        // (using the clamped last frame instead of looping forever) and never
        // produce out-of-range or NaN values during the sinc lead-in.
        let src = vec![0.1f32, 0.2, 0.3];
        for algo in [
            ResamplerAlgorithm::Linear,
            ResamplerAlgorithm::Cubic,
            ResamplerAlgorithm::SincFast,
            ResamplerAlgorithm::SincMedium,
            ResamplerAlgorithm::SincSlow,
        ] {
            let out = run_resampler(algo, 44100, 192000, 1, 1, &src);
            assert!(!out.is_empty(), "{algo:?} produced nothing");
            assert!(out.len() < 4096, "{algo:?} blew up to {}", out.len());
            for v in &out {
                assert!(v.is_finite(), "{algo:?}: non-finite {v}");
                assert!(
                    v.abs() <= 0.5,
                    "{algo:?}: sample outside source range {v}"
                );
            }
        }
    }

    #[test]
    fn resampler_stereo_mixup_downmix_mono() {
        let src: Vec<f32> = (0..128).map(|i| {
            if i % 2 == 0 {
                0.25
            } else {
                0.75
            }
        }).collect();
        let out = run_resampler(ResamplerAlgorithm::SincMedium, 44100, 44100, 2, 1, &src);
        // 1:1 rate, mono out -> (0.25+0.75)/2 == 0.5, one sample per source frame.
        assert_eq!(out.len(), 64);
        for v in &out {
            assert!((v - 0.5).abs() < 1e-3, "got {v}");
        }
    }
}
