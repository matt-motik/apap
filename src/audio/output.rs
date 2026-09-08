use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};

use crate::audio::player::PlaybackCore;

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

/// Pick an output device and a stream config close to the track's native parameters.
pub fn select_output(
    track_rate: u32,
    track_channels: usize,
    preferred_name: Option<&str>,
) -> Result<OutputSpec, String> {
    let host = cpal::default_host();
    let devices: Vec<cpal::Device> = match host.output_devices() {
        Ok(list) => list.filter(|d| d.default_output_config().is_ok()).collect(),
        Err(_) => Vec::new(),
    };

    // Resolve the requested device, falling back to the host default.
    let device: cpal::Device = match preferred_name {
        Some(name) if !name.is_empty() => devices
            .iter()
            .find(|d| {
                d.description()
                    .map(|desc| desc.name().to_string() == name)
                    .unwrap_or(false)
            })
            .cloned(),
        _ => None,
    }
    .or_else(|| {
        devices
            .iter()
            .find(|d| Some(d.id()) == host.default_output_device().map(|d| d.id()))
            .cloned()
    })
    .or_else(|| host.default_output_device())
    .ok_or_else(|| String::from("No audio output device found"))?;

    let device_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "default".to_string());

    let default = device
        .default_output_config()
        .map_err(|e| format!("Cannot query default output config: {e}"))?;

    // Prefer a config matching the source channels when the device supports it,
    // otherwise fall back to the device default (usually 2ch).
    let mut channels = track_channels.min(default.channels() as usize) as u16;
    if channels == 0 {
        channels = 2;
    }

    // Prefer matching the source sample rate when supported.
    let mut supported_rate: Option<u32> = None;
    let mut chosen_buf = match default.buffer_size() {
        SupportedBufferSize::Range { min, max } => {
            BufferSize::Fixed(target_buffer_frames(default.sample_rate(), *min, *max))
        }
        _ => BufferSize::Default,
    };
    if let Ok(configs) = device.supported_output_configs() {
        for cfg in configs {
            if cfg.channels() != channels {
                continue;
            }
            let (lo, hi) = (cfg.min_sample_rate(), cfg.max_sample_rate());
            if supported_rate.is_none() && track_rate >= lo && track_rate <= hi {
                supported_rate = Some(track_rate);
            }
            if let SupportedBufferSize::Range { min, max } = cfg.buffer_size() {
                chosen_buf = BufferSize::Fixed(target_buffer_frames(hi, *min, *max));
            }
        }
    }

    let (out_rate, sample_format) = match supported_rate {
        Some(r) => (r, default.sample_format()),
        None => (default.sample_rate(), default.sample_format()),
    };

    let config = StreamConfig {
        channels,
        sample_rate: out_rate,
        buffer_size: chosen_buf,
    };

    Ok(OutputSpec {
        device,
        config,
        sample_format,
        device_name,
    })
}

/// Enumerate usable output devices as `(name, description)` pairs. The `name`
/// is the stable key persisted in settings and passed back to `select_output`.
pub fn output_devices() -> Vec<(String, String)> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for dev in devices {
            if let Ok(desc) = dev.description() {
                let name = desc.name().to_string();
                out.push((name.clone(), name));
            }
        }
    }
    out
}

/// Name of the host's default output device, if any.
pub fn default_device_name() -> Option<String> {
    let host = cpal::default_host();
    host.default_output_device()
        .and_then(|d| d.description().ok())
        .map(|d| d.name().to_string())
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
    let stream = build_stream(&spec, core)?;
    stream
        .play()
        .map_err(|e| format!("Cannot start audio stream: {e}"))?;
    // Give the backend a moment to actually open the device; starting ALSA
    // usually fails promptly when the slave cannot be opened.
    std::thread::sleep(std::time::Duration::from_millis(50));
    drop(stream);
    Ok(spec.device_name)
}

/// Build the output stream. `sample_format` decides the callback sample type.
pub fn build_stream(
    spec: &OutputSpec,
    core: Arc<Mutex<PlaybackCore>>,
) -> Result<cpal::Stream, String> {
    match spec.sample_format {
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U8 => {}
        other => {
            return Err(format!("Unsupported output sample format: {other:?}"));
        }
    }

    let build =
        |cfg: StreamConfig, core: Arc<Mutex<PlaybackCore>>| -> Result<cpal::Stream, cpal::Error> {
            match spec.sample_format {
                SampleFormat::F32 => {
                    let core = core.clone();
                    spec.device.build_output_stream(
                        cfg,
                        move |data: &mut [f32], _| {
                            crate::audio::player::audio_callback_f32(&core, data);
                        },
                        move |e| {
                            eprintln!("Audio stream error: {e}");
                        },
                        None,
                    )
                }
                SampleFormat::I16 => {
                    let core = core.clone();
                    spec.device.build_output_stream(
                        cfg,
                        move |data: &mut [i16], _| {
                            crate::audio::player::audio_callback_i16(&core, data);
                        },
                        move |e| {
                            eprintln!("Audio stream error: {e}");
                        },
                        None,
                    )
                }
                SampleFormat::U8 => {
                    let core = core.clone();
                    spec.device.build_output_stream(
                        cfg,
                        move |data: &mut [u8], _| {
                            crate::audio::player::audio_callback_u8(&core, data);
                        },
                        move |e| {
                            eprintln!("Audio stream error: {e}");
                        },
                        None,
                    )
                }
                _other => unreachable!("sample format checked above"),
            }
        };

    match build(spec.config.clone(), core.clone()) {
        Ok(stream) => Ok(stream),
        Err(_e) if matches!(spec.config.buffer_size, BufferSize::Fixed(_)) => {
            // Some devices reject an explicit buffer size; retry with the default.
            let mut cfg = spec.config.clone();
            cfg.buffer_size = BufferSize::Default;
            build(cfg, core).map_err(|e| format!("Cannot build output stream: {e}"))
        }
        Err(e) => Err(format!("Cannot build output stream: {e}")),
    }
}
