use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};

use crate::audio::worker::{RtConsumer, RtShared};
use crate::settings::ResamplerAlgorithm;

/// How long the startup device probe holds the stream open (ms). Long enough
/// for the backend to surface early ALSA errors, short enough to not delay UI.
pub const PROBE_OPEN_MS: u64 = 50;

/// Hard cap on the resampler's buffered source frames (in frames).
///
/// The buffer is sized once at construction so `push` never reallocates in the
/// real-time audio callback. It is larger than any single decoder packet
/// (symphonia packets ~ 4096 frames max, DSD groups 512), and the player's
/// `fill` loop only pushes when `buffered_frames` underflows, so the buffered
/// peak stays at `1 + one packet`.
pub const MAX_BUFFERED_FRAMES: usize = 8192;

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
            // Preallocated once so the audio callback performs zero heap
            // allocations (the buffer is reused across `reset`, `drain`, etc.).
            buf: Vec::with_capacity(MAX_BUFFERED_FRAMES * src_ch),
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

    /// Whether this resampler actually transforms the stream (source rate or
    /// channel count differs from the output config). A 1:1 config is a pure
    /// pass-through, used by the player to decide dithering applicability.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn push(&mut self, samples: &[f32]) -> usize {
        let frames = samples.len() / self.src_ch;
        // Zero-allocation guard: never grow `buf` past the construction-time
        // capacity. If the buffer is full, drop the surplus frames and let the
        // caller (Player::fill) pull before pushing again. Frames are kept
        // aligned to `src_ch` so interleaving is never torn mid-frame.
        let room_frames = MAX_BUFFERED_FRAMES.saturating_sub(self.frames_in_buf);
        let take_frames = frames.min(room_frames);
        let take = take_frames * self.src_ch;
        if take > 0 {
            self.buf.extend_from_slice(&samples[..take]);
            self.frames_in_buf += take_frames;
        }
        take_frames
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
    /// Stable backend key of the device (persisted in settings).
    pub device_id: String,
    pub device_name: String,
    /// Whether the stream carries DoP (DSD-over-PCM) words. I32 callbacks must
    /// pack DoP markers only on this path; plain PCM on an I32 node uses the
    /// normal PCM conversion instead.
    pub is_dop: bool,
}

/// Backend-agnostic snapshot of an output device's capabilities, enough for
/// the stream-selection logic to run without a live audio backend (and thus
/// to be unit-tested with a mock).
///
/// `id` is the stable, backend-usable key that settings persist and
/// `select_output` uses to re-open the device (on ALSA it is the pcm id, e.g.
/// `hw:CARD=4,DEV=0`); `name` is the human-readable label shown in the UI.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub sample_format: SampleFormat,
    pub buffer_size: SupportedBufferSize,
    pub supported: Vec<RateRange>,
    /// Категория устройства (ТЗ A3.0 §3.1): фильтр «только железо» и
    /// exclusive-политики опираются на неё.
    pub category: DeviceCategory,
    /// Плоский сортированный список семпловых частот (без дубликатов) для UI
    /// и проверок `supports_rate` / семейств клока.
    pub supported_rates: Vec<u32>,
    /// Поддерживаемые sample-форматы (порядок первого появления, без дублей).
    pub supported_formats: Vec<SampleFormat>,
    /// Может ли устройство (теоретически) открыть монопольный raw-поток:
    /// это raw-нода `hw:*` и она же классифицирована как Hardware.
    pub exclusive_capable: bool,
}

/// Собирает плоский сортированный (без дубликатов) список семпловых частот из
/// набора диапазонов. Дискретные конфиги (`min == max`) дают ровно одну
/// частоту; непрерывные диапазоны отдают границы `min`/`max` — на реальном
/// ALSA-железе перечень приходит дискретными записями, поэтому список точный,
/// а не «пила» из тысяч значений.
fn expand_supported_rates(supported: &[RateRange]) -> Vec<u32> {
    let mut rates = std::collections::BTreeSet::new();
    for r in supported {
        if r.min == r.max {
            rates.insert(r.min);
        } else {
            rates.insert(r.min);
            rates.insert(r.max);
        }
    }
    rates.into_iter().collect()
}

/// Снимает дубликаты sample-форматов, сохраняя порядок первого появления.
fn dedup_formats(formats: &[SampleFormat]) -> Vec<SampleFormat> {
    let mut seen: Vec<SampleFormat> = Vec::new();
    let mut out: Vec<SampleFormat> = Vec::new();
    for &f in formats {
        if !seen.contains(&f) {
            seen.push(f);
            out.push(f);
        }
    }
    out
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
    /// Stable backend key of the chosen device (persisted in settings).
    pub device_id: String,
    pub device_name: String,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
}

/// Abstraction over the audio backend (cpal in production, a mock in tests).
pub trait AudioHost {
    /// Usable output devices, each with a resolvable default config (or a
    /// sane synthesized one — a device whose default probe transiently fails
    /// must not silently vanish from the settings list).
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
                let name = match dev.description().map(|d| d.name().to_string()) {
                    Ok(name) => name,
                    Err(_) => continue,
                };
                // Stable backend key used to re-open the device and persisted in
                // settings (ALSA pcm id, e.g. "hw:CARD=4,DEV=0").
                let id = dev.id().map(|d| d.id().to_string()).unwrap_or_else(|_| name.clone());
                // Supported configs and their formats are aligned by index so a
                // fallback default can pick a format the device really supports.
                let mut supported = Vec::new();
                let mut supported_formats: Vec<SampleFormat> = Vec::new();
                if let Ok(configs) = dev.supported_output_configs() {
                    for cfg in configs {
                        supported.push(RateRange {
                            channels: cfg.channels(),
                            min: cfg.min_sample_rate(),
                            max: cfg.max_sample_rate(),
                            buffer_size: *cfg.buffer_size(),
                        });
                        supported_formats.push(cfg.sample_format());
                    }
                }
                let default = dev.default_output_config().ok();
                // A device whose default config cannot be resolved right now
                // (e.g. the probe/another stream holds it at that instant) must
                // still show up in the settings list — otherwise a working DAC
                // "disappears" between two Refreshes. Fall back to the first
                // 2-channel supported config; `choose_output` re-picks a precise
                // rate from `supported` anyway.
                let (channels, sample_rate, sample_format, buffer_size) = match &default {
                    Some(cfg) => (
                        cfg.channels(),
                        cfg.sample_rate(),
                        cfg.sample_format(),
                        *cfg.buffer_size(),
                    ),
                    None => {
                        let pick = supported
                            .iter()
                            .enumerate()
                            .find(|(_, r)| r.channels == 2)
                            .or_else(|| supported.iter().enumerate().next());
                        match pick {
                            Some((i, r)) => (
                                r.channels,
                                r.min,
                                supported_formats.get(i).copied().unwrap_or(SampleFormat::F32),
                                r.buffer_size,
                            ),
                            None => continue, // no supported config at all
                        }
                    }
                };
                let category = classify_device(&id, &name);
                let rates = expand_supported_rates(&supported);
                let formats = dedup_formats(&supported_formats);
                let exclusive_capable =
                    is_raw_hardware_id(&id) && category == DeviceCategory::Hardware;
                out.push(DeviceInfo {
                    id,
                    name,
                    channels,
                    sample_rate,
                    sample_format,
                    buffer_size,
                    supported,
                    category,
                    supported_rates: rates,
                    supported_formats: formats,
                    exclusive_capable,
                });
            }
        }
        // cpal's ALSA backend exposes several handles with the same human name
        // (raw `hw:*`, `plughw:*`, server proxies, format variants). Among them
        // only the raw hardware node can deliver native rates / bit-exact
        // output; keep exactly one entry per name, preferring `hw:*`.
        collapse_same_name(out)
    }

    fn default_name(&self) -> Option<String> {
        let host = cpal::default_host();
        host.default_output_device()
            .and_then(|d| d.description().ok())
            .map(|d| d.name().to_string())
    }
}

/// True when the backend-specific device id names a raw ALSA hardware node
/// (`hw:*`). Such nodes expose discrete native rates and formats without a
/// software-resampling proxy (PipeWire/Pulse/plughw) in between, so they are
/// the only handles that can deliver native-rate and bit-exact (DoP) output.
fn is_raw_hardware_id(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.starts_with("hw:") || id.starts_with("hw=")
}

/// Категория устройства вывода (ТЗ A3.0 §3.1) — используется для фильтрации
/// списка («только железо»), пометки capabilities и решения об exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceCategory {
    /// Реальная нода `hw:*` (raw-железо без прослойки-ресемплера).
    Hardware,
    /// Прокси звукового сервера (PipeWire/Pulse/plughw/default/front/surround).
    ServerProxy,
    /// Виртуальные узлы ALSA (dmix, dsnoop, softvol, null, ffmpeg, loopback).
    Virtual,
    /// Loopback: «Monitor of …» / id loopback.
    Loopback,
    /// Не известа категория — нетривиальный id/name.
    Unknown,
}

/// Классифицирует устройство по стабильному backend-id и человекочитаемому
/// имени (ТЗ A3.0 §3.1). Чистая функция, никакого бэкенда не трогает.
///
/// Порядок правил (первое совпадение побеждает):
///  1. `hw:` / `hw=` → Hardware
///  2. id `loopback` / name «Monitor of …» → Loopback (раньше Virtual — id
///     loopback попадает в список Virtual в §3.1, но это именно отражение
///     ALSA-loopback-устройства)
///  3. `plughw:` / `default` / `front:` / `surround*` / `sysdefault` /
///     name PipeWire/Pulse → ServerProxy
///  4. id содержит dmix|dsnoop|softvol|null|ffmpeg → Virtual
///  5. иначе → Unknown
pub fn classify_device(id: &str, name: &str) -> DeviceCategory {
    let id_l = id.to_ascii_lowercase();
    let name_l = name.to_ascii_lowercase();

    if id_l.starts_with("hw:") || id_l.starts_with("hw=") {
        return DeviceCategory::Hardware;
    }
    if id_l.contains("loopback") || name_l.contains("monitor of") {
        return DeviceCategory::Loopback;
    }
    if id_l.starts_with("plughw:")
        || id_l == "default"
        || id_l.starts_with("front:")
        || id_l.starts_with("surround")
        || id_l.starts_with("sysdefault")
        || name_l.contains("pipewire")
        || name_l.contains("pulseaudio")
        || name_l.contains("sound server")
    {
        return DeviceCategory::ServerProxy;
    }
    if id_l.contains("dmix")
        || id_l.contains("dsnoop")
        || id_l.contains("softvol")
        || id_l.contains("null")
        || id_l.contains("ffmpeg")
    {
        return DeviceCategory::Virtual;
    }
    DeviceCategory::Unknown
}

/// Best handle for a device name, preferring the raw `hw:*` node.
///
/// cpal's ALSA backend reports several handles with the same human-readable
/// description (raw `hw:*`, `plughw:*` with all software conversions, server
/// proxies, per-format variants). The first one in enumeration order is
/// typically a `plughw:`/`default` proxy whose range covers every rate and
/// buffer size — selecting it silently re-samples everything to the server's
/// rate (48 kHz) and makes DoP impossible. Among equal names the raw `hw:*`
/// node wins; anything else keeps the first handle (existing behaviour).
fn collapse_same_name(infos: Vec<DeviceInfo>) -> Vec<DeviceInfo> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<DeviceInfo> = Vec::with_capacity(infos.len());
    for info in infos {
        if !seen.insert(info.name.clone()) {
            // Same name already included: prefer the raw hardware node when
            // the kept entry is not one yet.
            if is_raw_hardware_id(&info.id) {
                if let Some(kept) = out.iter_mut().find(|k| k.name == info.name) {
                    if !is_raw_hardware_id(&kept.id) {
                        *kept = info;
                    }
                }
            }
            continue;
        }
        out.push(info);
    }
    out
}

impl CpalHost {
    /// Recover the concrete cpal device handle for stream building by its
    /// stable backend id (ALSA pcm id). Falls back to a description-name match
    /// for configs persisted before the id was stored.
    fn device_by_id(&self, id: &str) -> Result<cpal::Device, String> {
        let host = cpal::default_host();
        if let Ok(devices) = host.output_devices() {
            for dev in devices {
                let matches_id = dev.id().map(|d| d.id() == id).unwrap_or(false);
                let matches_name = dev
                    .description()
                    .map(|d| d.name() == id)
                    .unwrap_or(false);
                if matches_id || matches_name {
                    return Ok(dev);
                }
            }
        }
        Err(format!("Audio device '{id}' no longer available"))
    }
}

/// ТЗ §8.5: ближайший поддерживаемый ЦАП-рейт для запрошенной частоты.
///
/// Учитываются только диапазоны с нужным числом каналов. Точное попадание
/// (`requested` внутри `[min, max]`) → дистанция 0 и возврат `requested`;
/// иначе берётся ближайшая граница диапазона. При равной дистанции
/// предпочитается меньшая частота (более консервативный коэффициент
/// ресемплера, не ломает семейства 44.1/48k).
pub fn nearest_rate(ranges: &[RateRange], channels: u16, requested: u32) -> Option<u32> {
    let req = u64::from(requested);
    let mut best: Option<(u64, u32)> = None;
    for r in ranges {
        if r.channels != channels {
            continue;
        }
        let (lo, hi) = (u64::from(r.min), u64::from(r.max));
        let (candidate, dist) = if req < lo {
            (r.min, lo - req)
        } else if req > hi {
            (r.max, req - hi)
        } else {
            (requested, 0)
        };
        let better = match best {
            None => true,
            Some((best_dist, best_rate)) => {
                dist < best_dist || (dist == best_dist && candidate < best_rate)
            }
        };
        if better {
            best = Some((dist, candidate));
        }
    }
    best.map(|(_, rate)| rate)
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

    // Resolve the requested device, falling back to the host default. `preferred`
    // may be a stable id (new settings) or a human name (legacy configs).
    let preferred = preferred_name.filter(|n| !n.is_empty());
    let device = preferred
        .and_then(|key| devices.iter().find(|d| d.id == key).or_else(|| devices.iter().find(|d| d.name == key)))
        .or_else(|| default_name.and_then(|d| devices.iter().find(|dev| dev.id == d).or_else(|| devices.iter().find(|dev| dev.name == d))))
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

    // ТЗ §8.5: если точный рейт не поддерживается — ближайший поддерживаемый
    // ЦАП-рейт из capabilities; только при полном отсутствии диапазонов с
    // нужным числом каналов — дефолтная частота устройства (последний резерв).
    let nearest = supported_rate.or_else(|| nearest_rate(&device.supported, channels, track_rate));
    let (out_rate, sample_format) = match nearest {
        Some(r) => (r, device.sample_format),
        None => (device.sample_rate, device.sample_format),
    };

    Ok(ChosenOutput {
        device_id: device.id.clone(),
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
    let device = host.device_by_id(&chosen.device_id)?;
    Ok(OutputSpec {
        device,
        config: chosen.config,
        sample_format: chosen.sample_format,
        device_id: chosen.device_id,
        device_name: chosen.device_name,
        is_dop: false,
    })
}

/// True when `name` is a software sound-server node (PipeWire/Pulse) that cpal
/// surfaces as an ALSA "device". Such nodes proxy the stream through a server
/// with implicit resampling and can never deliver bit-exact (DoP) output, so
/// the settings dialog marks them clearly and lists them after the direct
/// hardware nodes.
pub fn is_server_node(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("pipewire")
        || n.contains("pulseaudio")
        || n.contains("sound server")
        || n.contains("default alsa output")
        || n == "default audio device"
}

/// Suffix appended to software sound-server (PipeWire/Pulse) device labels in
/// the settings dialog, signalling that such nodes resample the stream.
pub const SERVER_NODE_SUFFIX: &str = " — (software, resamples)";

/// Build the settings-dialog device list as `(raw_name, label)` pairs.
///
/// 1. Deduplicates repeated names (cpal's ALSA backend exposes several
///    device handles with the same description — one per format/rate variant),
///    keeping the first (the one `device_by_name` will resolve at open time).
/// 2. Groups direct hardware nodes first, software server nodes last, so a
///    user picking an audiophile output (DoP / bit-perfect) sees the DAC before
///    the PipeWire/Pulse proxies.
/// 3. Server nodes get a [`SERVER_NODE_SUFFIX`] suffix in the label while the
///    raw name is preserved untouched (it is what gets persisted in settings
///    and matched by `choose_output`/`device_by_name`).
pub fn label_device_names(names: &[impl AsRef<str>]) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut hw = Vec::new();
    let mut srv = Vec::new();
    for name in names {
        let name = name.as_ref();
        if !seen.insert(name.to_string()) {
            continue;
        }
        let out = if is_server_node(name) {
            &mut srv
        } else {
            &mut hw
        };
        let label = if is_server_node(name) {
            format!("{name}{SERVER_NODE_SUFFIX}")
        } else {
            name.to_string()
        };
        out.push((name.to_string(), label));
    }
    hw.extend(srv);
    hw
}

/// Enumerate usable output devices as `(id, label)` pairs.
///
/// The first element is the stable, backend-openable key (ALSA pcm id, e.g.
/// `hw:CARD=4,DEV=0`) persisted in settings and passed back to
/// `select_output`; the second is the deduplicated, grouped human-readable
/// label shown in the settings dialog (see [`label_device_names`]).
pub fn output_devices() -> Vec<(String, String)> {
    let infos = CpalHost.devices();
    let ids: std::collections::HashMap<&str, &str> =
        infos.iter().map(|d| (d.name.as_str(), d.id.as_str())).collect();
    let names: Vec<String> = infos.iter().map(|d| d.name.clone()).collect();
    label_device_names(&names)
        .into_iter()
        .map(|(name, label)| {
            let id = ids.get(name.as_str()).copied().unwrap_or(name.as_str()).to_string();
            (id, label)
        })
        .collect()
}

/// Name of the host's default output device, if any.
pub fn default_device_name() -> Option<String> {
    CpalHost.default_name()
}

/// Stable backend id of the host's default output device, if any.
pub fn default_device_id() -> Option<String> {
    let host = cpal::default_host();
    host.default_output_device()
        .and_then(|d| d.id().ok())
        .map(|d| d.id().to_string())
}

/// Probe the configured output device (or the host default when `preferred` is
/// empty/`None`).
///
/// The probe opens a real (silent) output stream for a short moment so that
/// runtime failures — e.g. ALSA `snd_pcm_dmix_open: unable to open slave` —
/// surface at startup instead of on the first Play. `preferred` may be a
/// stable id or a human-readable name (legacy configs). Returns the name of
/// the effective device.
pub fn probe_output(preferred: Option<&str>) -> Result<String, String> {
    let preferred = preferred.filter(|n| !n.is_empty());
    let infos = CpalHost.devices();

    // A configured device that is not enumerable counts as unavailable.
    if let Some(key) = preferred {
        let found = infos.iter().any(|d| d.id == key) || infos.iter().any(|d| d.name == key);
        if !found {
            return Err(format!("Configured audio device '{key}' not found"));
        }
    }
    if infos.is_empty() {
        return Err(String::from("No audio output device found"));
    }

    let mut spec = select_output(44100, 2, preferred)?;
    let error_flag = Arc::new(AtomicBool::new(false));
    let attempt = |cfg: &OutputSpec| -> Result<cpal::Stream, String> {
        let shared = RtShared::new(cfg.config.sample_rate, cfg.config.channels as usize, false);
        let (_producer, ring) = rtrb::RingBuffer::<f32>::new(2048);
        let consumer = RtConsumer::new(ring, shared);
        build_stream_rt(cfg, consumer, Some(error_flag.clone()))
    };
    let stream = match attempt(&spec) {
        Ok(s) => s,
        Err(_) if matches!(spec.config.buffer_size, BufferSize::Fixed(_)) => {
            spec.config.buffer_size = BufferSize::Default;
            attempt(&spec)?
        }
        Err(e) => return Err(e),
    };
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

/// Build the output stream for the Producer/Consumer engine (ТЗ A2.0 §5.3).
/// The cpal real-time callback owns the [`RtConsumer`] and reads samples from
/// the lock-free ring; it never touches a `Mutex`.
///
/// The buffer-size fallback (Fixed → Default) is the caller's responsibility:
/// on failure the consumer has been consumed, so a fresh ring/consumer must be
/// created for the retry.
pub fn build_stream_rt(
    spec: &OutputSpec,
    consumer: RtConsumer,
    error_flag: Option<Arc<AtomicBool>>,
) -> Result<cpal::Stream, String> {
    match spec.sample_format {
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U8 | SampleFormat::I32 => {}
        other => {
            return Err(format!("Unsupported output sample format: {other:?}"));
        }
    }

    let build = |cfg: StreamConfig,
                 mut consumer: RtConsumer,
                 ef: Option<Arc<AtomicBool>>|
     -> Result<cpal::Stream, cpal::Error> {
        match spec.sample_format {
            SampleFormat::F32 => {
                let ef = ef.clone();
                spec.device.build_output_stream(
                    cfg,
                    move |data: &mut [f32], _| {
                        crate::audio::player::audio_callback_f32_rt(&mut consumer, data);
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
                let ef = ef.clone();
                spec.device.build_output_stream(
                    cfg,
                    move |data: &mut [i16], _| {
                        crate::audio::player::audio_callback_i16_rt(&mut consumer, data);
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
                let ef = ef.clone();
                spec.device.build_output_stream(
                    cfg,
                    move |data: &mut [u8], _| {
                        crate::audio::player::audio_callback_u8_rt(&mut consumer, data);
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
            SampleFormat::I32 => {
                let ef = ef.clone();
                let is_dop = spec.is_dop;
                spec.device.build_output_stream(
                    cfg,
                    move |data: &mut [i32], _| {
                        if is_dop {
                            crate::audio::player::audio_callback_i32_dop_rt(&mut consumer, data);
                        } else {
                            crate::audio::player::audio_callback_i32_pcm_rt(&mut consumer, data);
                        }
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

    build(spec.config, consumer, error_flag)
        .map_err(|e| format!("Cannot build output stream: {e}"))
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
        mock_device_id(name, name, channels, sample_rate, supported)
    }

    fn mock_device_id(
        id: &str,
        name: &str,
        channels: u16,
        sample_rate: u32,
        supported: &[(u16, u32, u32)],
    ) -> DeviceInfo {
        let supported: Vec<RateRange> = supported
            .iter()
            .map(|&(c, lo, hi)| RateRange {
                channels: c,
                min: lo,
                max: hi,
                buffer_size: SupportedBufferSize::Range { min: 64, max: 4096 },
            })
            .collect();
        let category = classify_device(id, name);
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            channels,
            sample_rate,
            sample_format: SampleFormat::F32,
            buffer_size: SupportedBufferSize::Range { min: 64, max: 4096 },
            supported_rates: expand_supported_rates(&supported),
            supported_formats: vec![SampleFormat::F32],
            category,
            exclusive_capable: is_raw_hardware_id(id) && category == DeviceCategory::Hardware,
            supported,
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
        assert_eq!(chosen.device_id, "HDMI");
        assert_eq!(chosen.config.sample_rate, 44100);
        assert_eq!(chosen.config.channels, 2);
    }

    #[test]
    fn choose_output_resolves_by_id_over_name() {
        // The persisted key is now the stable id; names only resolve because
        // the mock uses id == name. With a distinct id the id must win.
        let dac = mock_device_id(
            "hw:CARD=4,DEV=0",
            "ADI-2 DAC (56680121), USB Audio",
            2,
            48000,
            &[(2, 44100, 192000)],
        );
        let host = MockHost {
            devices: vec![dac.clone()],
            default_name: Some("ADI-2 DAC (56680121), USB Audio".into()),
        };
        // Preferred id matches even though it differs from the display name.
        let chosen = select_from_host(&host, 96000, 2, Some("hw:CARD=4,DEV=0")).unwrap();
        assert_eq!(chosen.device_id, "hw:CARD=4,DEV=0");
        assert_eq!(chosen.device_name, "ADI-2 DAC (56680121), USB Audio");
        // Unknown id falls back to the name lookup (legacy configs).
        let chosen = select_from_host(&host, 44100, 2, Some("ADI-2 DAC (56680121), USB Audio"))
            .unwrap();
        assert_eq!(chosen.device_id, "hw:CARD=4,DEV=0");
        // Unknown id + unknown name -> default rescue when a default exists;
        // strict error when neither id, name nor default matches.
        let chosen = select_from_host(&host, 44100, 2, Some("nope")).unwrap();
        assert_eq!(chosen.device_id, "hw:CARD=4,DEV=0");
        let host_no_default = MockHost {
            devices: vec![dac],
            default_name: None,
        };
        assert!(select_from_host(&host_no_default, 44100, 2, Some("nope")).is_err());
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
    fn choose_output_picks_nearest_supported_when_unsupported() {
        // ТЗ §8.5: 88200 не поддерживается; ближайший ЦАП-рейт — 48000,
        // а не дефолт устройства 44100.
        let device = mock_device(
            "DAC",
            2,
            44100,
            &[(2, 44100, 48000), (2, 176400, 352800)],
        );
        let chosen = choose_output(&[device], Some("DAC"), 88200, 2, None).unwrap();
        assert_eq!(chosen.config.sample_rate, 48000);
    }

    #[test]
    fn choose_output_exact_rate_wins_over_nearest() {
        // 96000 поддерживается напрямую — точный рейт, а не ближайший.
        let device = mock_device(
            "DAC",
            2,
            44100,
            &[(2, 44100, 48000), (2, 88200, 96000)],
        );
        let chosen = choose_output(&[device], Some("DAC"), 96000, 2, None).unwrap();
        assert_eq!(chosen.config.sample_rate, 96000);
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

    fn rate_range(channels: u16, min: u32, max: u32) -> RateRange {
        RateRange {
            channels,
            min,
            max,
            buffer_size: SupportedBufferSize::Range { min: 64, max: 4096 },
        }
    }

    #[test]
    fn nearest_rate_exact_returns_requested() {
        let ranges = vec![rate_range(2, 44100, 96000)];
        assert_eq!(nearest_rate(&ranges, 2, 48000), Some(48000));
        assert_eq!(nearest_rate(&ranges, 2, 44100), Some(44100));
        assert_eq!(nearest_rate(&ranges, 2, 96000), Some(96000));
    }

    #[test]
    fn nearest_rate_clamps_to_closest_edge() {
        // Запрос выше диапазона → верхняя граница (ближайшая).
        let ranges = vec![rate_range(2, 44100, 88200)];
        assert_eq!(nearest_rate(&ranges, 2, 176400), Some(88200));
    }

    #[test]
    fn nearest_rate_falls_back_to_lowest_edge() {
        // Запрос ниже диапазона → нижняя граница (ближайшая).
        let ranges = vec![rate_range(2, 96000, 192000)];
        assert_eq!(nearest_rate(&ranges, 2, 44100), Some(96000));
    }

    #[test]
    fn nearest_rate_picks_closest_from_44_1_48_families() {
        let ranges = vec![rate_range(2, 44100, 44100), rate_range(2, 192000, 192000)];
        // DSD64 2822400/16 → 176400; к семейству 44.1 ближе 192k, чем к 44.1k.
        assert_eq!(nearest_rate(&ranges, 2, 176400), Some(192000));
        // 88200: к 44.1k дистанция 44100, к 192k — 103800 → 44.1k.
        assert_eq!(nearest_rate(&ranges, 2, 88200), Some(44100));
    }

    #[test]
    fn nearest_rate_picks_nearest_across_many_ranges() {
        // 384000 не поддерживается; ближайшая граница из {44100..48000, 176400..352800}.
        let ranges = vec![rate_range(2, 44100, 48000), rate_range(2, 176400, 352800)];
        assert_eq!(nearest_rate(&ranges, 2, 384000), Some(352800));
        assert_eq!(nearest_rate(&ranges, 2, 88200), Some(48000));
    }

    #[test]
    fn nearest_rate_tie_prefers_smaller_rate() {
        // 46050 равноудалён от 44100 и 48000 → меньшая частота.
        let ranges = vec![rate_range(2, 44100, 44100), rate_range(2, 48000, 48000)];
        assert_eq!(nearest_rate(&ranges, 2, 46050), Some(44100));
    }

    #[test]
    fn nearest_rate_no_matching_channel_returns_none() {
        let ranges = vec![rate_range(2, 44100, 192000)];
        assert_eq!(nearest_rate(&ranges, 1, 44100), None);
    }

    #[test]
    fn nearest_rate_empty_ranges_returns_none() {
        assert_eq!(nearest_rate(&[], 2, 44100), None);
    }

    #[test]
    fn device_labels_dedup_group_and_mark_server_nodes() {
        // cpal exposes several ADI-2 handles with the same description (one per
        // format/rate variant) plus the to-be-avoided PipeWire/Pulse proxies.
        let names: Vec<&str> = vec![
            "ADI-2 DAC (56680121), USB Audio",
            "PipeWire Sound Server",
            "ADI-2 DAC (56680121), USB Audio", // duplicate -> dropped
            "Default Audio Device",
            "ADI-2 DAC (56680121)",
        ];
        let pairs = label_device_names(&names);
        assert_eq!(pairs.len(), 4);
        // Direct hardware nodes first, in original order, label == raw name.
        assert_eq!(
            pairs[0],
            (
                "ADI-2 DAC (56680121), USB Audio".to_string(),
                "ADI-2 DAC (56680121), USB Audio".to_string()
            )
        );
        assert_eq!(pairs[1].0, "ADI-2 DAC (56680121)");
        assert_eq!(pairs[1].1, "ADI-2 DAC (56680121)");
        // Server nodes last, raw name preserved, label carries the hint.
        assert_eq!(pairs[2].0, "PipeWire Sound Server");
        assert!(pairs[2].1.contains("(software, resamples)"), "{}", pairs[2].1);
        assert_eq!(pairs[3].0, "Default Audio Device");
        assert!(pairs[3].1.contains("(software, resamples)"), "{}", pairs[3].1);
    }

    #[test]
    fn device_labels_keep_raw_name_for_lookup() {
        // The persisted/‘active’ key is the raw name; the label is only for the
        // drop-down. Every raw name must be recoverable from its pair.
        let names: Vec<&str> = vec![
            "Default ALSA Output (currently PulseAudio Sound Server)",
            "USB Audio",
            "PulseAudio Sound Server",
        ];
        let pairs = label_device_names(&names);
        let raws: Vec<&str> = pairs.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(raws.len(), 3);
        assert!(raws.contains(&"Default ALSA Output (currently PulseAudio Sound Server)"));
        assert!(raws.contains(&"PulseAudio Sound Server"));
        assert!(is_server_node("Default ALSA Output (currently PulseAudio Sound Server)"));
        assert!(is_server_node("PulseAudio Sound Server"));
        assert!(!is_server_node("USB Audio"));
    }

    #[test]
    fn device_labels_empty_input() {
        assert_eq!(label_device_names(&[] as &[&str]), Vec::<(String, String)>::new());
    }

    #[test]
    fn collapse_same_name_prefers_raw_hw_node() {
        // The observed ADI-2 fingerprint: identical human names, several
        // handles — the *last* one is the raw `hw:` node (I32, discrete rates),
        // the first is a plughw/proxy variant. Collapse must keep the hw node.
        let proxy = mock_device_id(
            "plughw:CARD=4,DEV=0",
            "ADI-2 DAC (56680121), USB Audio",
            2,
            48000,
            &[(2, 4000, 4294967295)],
        );
        let raw = mock_device_id(
            "hw:CARD=4,DEV=0",
            "ADI-2 DAC (56680121), USB Audio",
            2,
            48000,
            &[(2, 44100, 44100), (2, 48000, 48000), (2, 176400, 176400)],
        );
        let mut raws = vec![proxy.clone(), proxy.clone(), raw.clone(), raw.clone()];
        // The hw node is already first — stays.
        let collapsed = collapse_same_name(raws.clone());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].id, "hw:CARD=4,DEV=0");
        // The hw node comes last — collapse still finds and promotes it.
        raws.reverse();
        let collapsed = collapse_same_name(raws);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].id, "hw:CARD=4,DEV=0");
    }

    #[test]
    fn collapse_same_name_keeps_distinct_names() {
        let devs = vec![
            mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]),
            mock_device("Default Audio Device", 2, 48000, &[(2, 44100, 48000)]),
            mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]),
        ];
        let collapsed = collapse_same_name(devs);
        assert_eq!(collapsed.len(), 2);
        assert_eq!(collapsed[0].name, "Speakers");
        assert_eq!(collapsed[1].name, "Default Audio Device");
    }

    #[test]
    fn collapse_same_name_keeps_first_without_hw_variant() {
        // No raw hw handle for this name: the first entry wins (existing
        // behaviour), e.g. the virtual "Default Audio Device" (id `default`).
        let devs = vec![
            mock_device_id(
                "default",
                "Default Audio Device",
                2,
                48000,
                &[(2, 4000, 4294967295)],
            ),
            mock_device_id(
                "front:CARD=4,DEV=0",
                "Default Audio Device",
                2,
                48000,
                &[(2, 4000, 4294967295)],
            ),
        ];
        let collapsed = collapse_same_name(devs);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].id, "default");
    }

    #[test]
    fn raw_hardware_id_detection() {
        assert!(is_raw_hardware_id("hw:CARD=4,DEV=0"));
        assert!(is_raw_hardware_id("hw:CARD=DAC56680121,DEV=0"));
        assert!(!is_raw_hardware_id("plughw:CARD=4,DEV=0"));
        assert!(!is_raw_hardware_id("default"));
        assert!(!is_raw_hardware_id("front:CARD=4,DEV=0"));
        assert!(!is_raw_hardware_id("sysdefault:CARD=4"));
        assert!(!is_raw_hardware_id(""));
    }

    #[test]
    fn classify_device_hw_is_hardware() {
        assert_eq!(
            classify_device("hw:CARD=4,DEV=0", "ADI-2 DAC (56680121), USB Audio"),
            DeviceCategory::Hardware
        );
        assert_eq!(classify_device("hw=CARD=DAC,DEV=0", "DAC"), DeviceCategory::Hardware);
    }

    #[test]
    fn classify_device_plughw_is_server_proxy() {
        assert_eq!(classify_device("plughw:CARD=4,DEV=0", "x"), DeviceCategory::ServerProxy);
        assert_eq!(classify_device("default", "Default Audio Device"), DeviceCategory::ServerProxy);
        assert_eq!(classify_device("front:CARD=4,DEV=0", "x"), DeviceCategory::ServerProxy);
        assert_eq!(classify_device("surround51:CARD=4", "x"), DeviceCategory::ServerProxy);
        assert_eq!(classify_device("sysdefault:CARD=4", "x"), DeviceCategory::ServerProxy);
    }

    #[test]
    fn classify_device_pipewire_and_pulse_are_server_proxy() {
        assert_eq!(
            classify_device("pipewire", "PipeWire Sound Server"),
            DeviceCategory::ServerProxy
        );
        assert_eq!(
            classify_device("pulse", "Default ALSA Output (currently PulseAudio Sound Server)"),
            DeviceCategory::ServerProxy
        );
    }

    #[test]
    fn classify_device_virtual_and_loopback() {
        assert_eq!(classify_device("dmix:CARD=4", "x"), DeviceCategory::Virtual);
        assert_eq!(classify_device("dsnoop:CARD=4", "x"), DeviceCategory::Virtual);
        assert_eq!(classify_device("softvol:4", "x"), DeviceCategory::Virtual);
        assert_eq!(classify_device("null", "x"), DeviceCategory::Virtual);
        assert_eq!(classify_device("ffmpeg", "x"), DeviceCategory::Virtual);
        assert_eq!(classify_device("loopback", "x"), DeviceCategory::Loopback);
        assert_eq!(
            classify_device("plughw:CARD=2,DEV=1", "Monitor of Built-in Audio"),
            DeviceCategory::Loopback
        );
    }

    #[test]
    fn classify_device_unknown_fallback() {
        assert_eq!(classify_device("weird-node-42", "USB Audio"), DeviceCategory::Unknown);
        assert_eq!(classify_device("", ""), DeviceCategory::Unknown);
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
