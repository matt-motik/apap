use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};

use crate::audio::player::PlaybackCore;

/// How long the startup device probe holds the stream open (ms). Long enough
/// for the backend to surface early ALSA errors, short enough to not delay UI.
pub const PROBE_OPEN_MS: u64 = 50;

/// Target output latency (~40 ms) — enough headroom to smooth single-decode
/// stalls that would otherwise cause ALSA/PipeWire buffer underruns.
fn target_buffer_frames(rate: u32, min: u32, max: u32) -> u32 {
    ((rate as f64 * 0.04) as u32).clamp(min, max)
}

/// Linear-interpolation streaming resampler with channel mixdown.
///
/// Steps source frames in and produces output frames at the device rate.
/// For 1:1 rate/channel configs the resampler is skipped entirely.
pub struct Resampler {
    src_rate: u32,
    out_rate: u32,
    src_ch: usize,
    out_ch: usize,
    /// Fractional source-frame offset of the next output sample within `buf`.
    phase: f64,
    /// Queued interleaved source frames.
    buf: Vec<f32>,
    frames_in_buf: usize,
    enabled: bool,
}

impl Resampler {
    pub fn new(src_rate: u32, out_rate: u32, src_ch: usize, out_ch: usize) -> Self {
        let same_rate = src_rate == out_rate;
        let same_ch = src_ch == out_ch;
        Resampler {
            src_rate,
            out_rate,
            src_ch,
            out_ch,
            phase: 0.0,
            buf: Vec::with_capacity(8192),
            frames_in_buf: 0,
            enabled: !(same_rate && same_ch),
        }
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.buf.clear();
        self.frames_in_buf = 0;
    }

    pub fn push(&mut self, samples: &[f32]) -> usize {
        let frames = samples.len() / self.src_ch;
        self.buf.extend_from_slice(samples);
        self.frames_in_buf += frames;
        frames
    }

    fn mix(&self, idx: usize, oc: usize) -> f32 {
        let base = idx * self.src_ch;
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

    /// Produce up to `max_out_frames` output frames into `out` (interleaved).
    ///
    /// Returns the number of output frames written. When `eof_mode` is true the
    /// resampler is allowed to reuse the last buffered source frame for the final
    /// partial interpolation step.
    pub fn pull(&mut self, out: &mut [f32], max_out_frames: usize, eof_mode: bool) -> usize {
        if !self.enabled {
            let n = max_out_frames.min(self.frames_in_buf);
            self.copy_direct(out, n);
            return n;
        }
        let ratio = self.out_rate as f64 / self.src_rate as f64;
        let total = self.frames_in_buf;
        let mut produced = 0usize;
        while produced < max_out_frames {
            let p = self.phase;
            let i0 = p.floor() as usize;
            let frac = (p - i0 as f64) as f32;
            if i0 + 1 >= total {
                if i0 >= total || !eof_mode {
                    break;
                }
            }
            let i1 = (i0 + 1).min(total - 1);
            let dst = produced * self.out_ch;
            for oc in 0..self.out_ch {
                let a = self.mix(i0, oc);
                let b = self.mix(i1, oc);
                out[dst + oc] = a + (b - a) * frac;
            }
            self.phase += ratio;
            produced += 1;
        }
        let consumed = self.phase.floor() as usize;
        if consumed > 0 {
            let n = consumed * self.src_ch;
            self.buf.drain(..n);
            self.frames_in_buf -= consumed;
            self.phase -= consumed as f64;
        }
        produced
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
}
