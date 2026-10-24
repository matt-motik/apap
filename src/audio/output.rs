use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};

use crate::audio::render::dop::DopRender;
use crate::audio::render::gain::GainStage;
use crate::audio::render::out::{F32Le, OutFormat, S16Le, S24Le, S32Le};
use crate::audio::render::pcm::{PcmRender, PcmSample};
use crate::audio::render::tpdf::Tpdf;
use crate::audio::render::RenderCore;
use crate::audio::session::RingPayload;
use crate::settings::{
    ClockFamily, FallbackPolicy, FallbackRatePolicy, ResamplerAlgorithm, ResamplerMode,
};

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
            let rel = left - self.base as i64;
            let enough = rel >= self.behind as i64
                && rel + (self.ahead as i64) < self.frames_in_buf as i64;
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
    /// Whether the sample rate required resampling (source != output rate).
    pub resampled: bool,
    /// Source (track) sample rate in Hz.
    pub source_rate: u32,
    /// Source (track) channel count.
    pub source_channels: usize,
    /// Деградация по пути выбора (ТЗ A3.0 §3.3) — для `StreamDesc` и
    /// bp-report. `None` = каждая ступень политики прошла без потерь.
    pub fallback: Option<FallbackReason>,
}

/// Backend-agnostic snapshot of an output device's capabilities, enough for
/// the stream-selection logic to run without a live audio backend (and thus
/// to be unit-tested with a mock).
///
/// `id` is the stable, backend-usable key that settings persist and
/// `select_output_for` uses to re-open the device (on ALSA it is the pcm id, e.g.
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
    /// Категория устройства (ТЗ A3.0 §3.1): фильтр «только железо» опирается
    /// на неё.
    pub category: DeviceCategory,
    /// Плоский сортированный список семпловых частот (без дубликатов) для UI
    /// и проверок `supports_rate` / семейств клока.
    pub supported_rates: Vec<u32>,
    /// Поддерживаемые sample-форматы (порядок первого появления, без дублей).
    pub supported_formats: Vec<SampleFormat>,
}

/// Короткая метка sample-формата для UI («I32», «F32», …). Верхний регистр —
/// в отличие от cpal's `Display` («i32»), и совпадает с §3.2 спеки.
fn sample_format_label(f: &SampleFormat) -> &'static str {
    match f {
        SampleFormat::I8 => "I8",
        SampleFormat::I16 => "I16",
        SampleFormat::I24 => "I24",
        SampleFormat::I32 => "I32",
        SampleFormat::I64 => "I64",
        SampleFormat::U8 => "U8",
        SampleFormat::U16 => "U16",
        SampleFormat::U24 => "U24",
        SampleFormat::U32 => "U32",
        SampleFormat::U64 => "U64",
        SampleFormat::F32 => "F32",
        SampleFormat::F64 => "F64",
        SampleFormat::DsdU8 => "DSD8",
        SampleFormat::DsdU16 => "DSD16",
        SampleFormat::DsdU32 => "DSD32",
        _ => "?",
    }
}

/// Форматирует частоту в кГц для UI: 48000 → «48», 176400 → «176.4».
fn format_rate_khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{}", rate / 1000)
    } else {
        format!("{:.1}", rate as f64 / 1000.0)
    }
}

impl DeviceInfo {
    pub fn is_stereo(&self) -> bool {
        self.channels == 2
    }

    /// True когда устройство может работать на `rate`.
    ///
    /// Опирается на raw-диапазоны `supported`, а не на плоский
    /// `supported_rates`: непрерывный диапазон (например plughw
    /// 4000..4294967295) покрывает промежуточные частоты, которых нет в
    /// плоском списке границ.
    pub fn supports_rate(&self, rate: u32) -> bool {
        self.supported.iter().any(|r| (r.min..=r.max).contains(&rate))
    }

    pub fn supports_format(&self, f: SampleFormat) -> bool {
        self.supported_formats.contains(&f)
    }

    /// DoP-контейнерная частота для DSD64/128/256 = byte_rate / 2.
    /// Возвращает первую поддерживаемую из {176400, 352800, 705600}, либо
    /// None (устройство не умеет DoP на штатных контейнерных частотах).
    pub fn dop_container_rate(&self) -> Option<u32> {
        [176400u32, 352800, 705600]
            .into_iter()
            .find(|&r| self.supports_rate(r))
    }

    /// Плоский список поддерживаемых частот в кГц через « · » — «44.1 · 48
    /// · 88.2 · 96 · 176.4 · 192 kHz».
    pub fn rates_desc(&self) -> String {
        if self.supported_rates.is_empty() {
            return String::from("—");
        }
        let parts: Vec<String> = self.supported_rates.iter().map(|&r| format_rate_khz(r)).collect();
        format!("{} kHz", parts.join(" · "))
    }

    /// Список поддерживаемых sample-форматов через « · » — «I16 · I24 · I32
    /// · F32».
    pub fn formats_desc(&self) -> String {
        if self.supported_formats.is_empty() {
            return String::from("—");
        }
        let parts: Vec<String> = self
            .supported_formats
            .iter()
            .map(sample_format_label)
            .map(String::from)
            .collect();
        parts.join(" · ")
    }

    /// Семейства клока (ТЗ §3.2) и их полнота — «44k partial (44.1, 176.4) · 48k
    /// full (48, 96, 192)». Семейство полностью выводится в списке только если
    /// устройство поддерживает хотя бы одну из его частот; полнота — наличие
    /// всех трёх {base, 2·base, 4·base}.
    pub fn clock_families_desc(&self) -> String {
        let families: [(&str, [u32; 3]); 2] = [
            ("44k", [44100, 88200, 176400]),
            ("48k", [48000, 96000, 192000]),
        ];
        let mut entries = Vec::new();
        for (label, rates) in families {
            let present: Vec<u32> = rates
                .iter()
                .copied()
                .filter(|&r| self.supports_rate(r))
                .collect();
            if present.is_empty() {
                continue;
            }
            let status = if present.len() == rates.len() { "full" } else { "partial" };
            let list: Vec<String> = present.into_iter().map(format_rate_khz).collect();
            entries.push(format!("{label} {status} ({})", list.join(", ")));
        }
        if entries.is_empty() {
            return String::from("—");
        }
        entries.join(" · ")
    }
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
    /// Выходной rate отличается от native-рейта источника.
    pub resampled: bool,
    /// Параметры источника (для bp-report / валидации §5).
    pub source_rate: u32,
    pub source_channels: usize,
    /// Причина деградации относительно «идеала» (native rate), если
    /// какой-то из шагов §3.4 отклонился.
    pub fallback: Option<FallbackReason>,
}

/// Причина, по которой выбранный поток деградировал относительно «идеала»
/// (native rate) — заполняется в [`choose_output`] (ТЗ A3.0 §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// Запрошенный rate (или fixed_rate) недоступен — выбран другой.
    RateUnsupported {
        requested: u32,
        chosen: u32,
        /// Выбор ушёл в другое частотное семейство (44.1k ↔ 48k).
        cross_family: bool,
    },
    /// Число каналов источника не поддерживается напрямую — downmix.
    ChannelsUnsupported { requested: usize, chosen: u16 },
    /// Ресемплинг выполнен, потому что запрошен режим Fixed.
    ResamplerForced { requested: u32 },
    /// Запрошенное семейство клока пусто на устройстве — взят дефолт.
    ClockFamilyIncomplete { requested: u32, chosen: u32 },
}

/// Параметры выхода, собранные из настроек (ТЗ A3.0 §3.4) — единый value-объект
/// вместо позиционных аргументов. Значения по умолчанию — «деградирующая»
/// конфигурация (shared, Auto/Nearest), эквивалентная поведению до A3.3.
#[derive(Debug, Clone)]
pub struct OutputRequest {
    pub track_rate: u32,
    pub track_channels: usize,
    /// Желаемое устройство: `Player::preferred_device`, которое
    /// `apply_legacy_fields` заполняет из `LegacyAudio` активного режима
    /// (`legacy_audio()`, §8 С4) — либо `None`.
    pub preferred_device: Option<String>,
    /// Политика фолбека по параметрам трека (ТЗ A3.0 §2.2).
    pub fallback: FallbackPolicy,
    /// Режим ресемплера (ТЗ A3.0 §2.2).
    pub resampler: ResamplerMode,
    /// Политика выбора fallback-рейта при недоступности native (ТЗ A3.0 §2.2).
    pub fallback_rate: FallbackRatePolicy,
    /// Предпочитаемое частотное семейство (только для `resampler = Fixed`).
    pub clock_family: ClockFamily,
    /// Жёсткая частота ресемплера (0 = авто по семейству).
    pub fixed_rate: u32,
}

impl OutputRequest {
    /// Клонировать запрос и сменить только параметры источника.
    pub fn with_source(&self, track_rate: u32, track_channels: usize) -> Self {
        let mut req = self.clone();
        req.track_rate = track_rate;
        req.track_channels = track_channels;
        req
    }
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
                if let Some(info) = device_info_from_cpal(&dev) {
                    out.push(info);
                }
            }
        }
        // На Linux Совместимый обязан показывать только выходы аудиосервера,
        // а не голые ALSA-узлы (ADR-07, ТЗ-127, матрица AM1.0 ТЗ §5,
        // AM1.0-B32, фаза 1): карта/формат/ресемплер-плагины схлопываются в
        // одну запись «Системное по умолчанию» — старая hw-первая
        // группировка `collapse_same_name` для них больше не нужна. На
        // Windows/macOS (WASAPI/CoreAudio) понятия "голый ALSA-узел" нет —
        // список остаётся как раньше (review).
        #[cfg(target_os = "linux")]
        {
            // Ни одного фиксированного алиаса сервера среди перечисленных PCM
            // (редкий случай: `output_devices()` пропустил "default"/"pipewire"/
            // "pulse", например из-за временного сбоя пробы) — отдельно
            // спрашиваем у cpal его собственное представление о дефолтном
            // устройстве (AM1.0-B32, фаза 2); `collapse_to_system_default`
            // всё равно откажется от голого hw:/plughw: узла.
            let fallback_default = if out.iter().any(|d| is_server_routed_pcm(&d.id)) {
                None
            } else {
                host.default_output_device().as_ref().and_then(device_info_from_cpal)
            };
            collapse_to_system_default(out, &system_default_label(), fallback_default)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // cpal's ALSA backend exposes several handles with the same human name
            // (raw `hw:*`, `plughw:*`, server proxies, format variants). Among them
            // only the raw hardware node can deliver native rates / bit-exact
            // output; keep exactly one entry per name, preferring `hw:*`.
            collapse_same_name(out)
        }
    }

    fn default_name(&self) -> Option<String> {
        let host = cpal::default_host();
        #[cfg(target_os = "linux")]
        {
            // Подпись обязана совпадать байт-в-байт с той, что `devices()`
            // ставит единственной записи «Системное по умолчанию» — иначе
            // `choose_output`/`CpalSharedBackend::enumerate` не находят её по
            // имени при `SharedDeviceChoice::SystemDefault` (ADR-07,
            // AM1.0-B32).
            host.default_output_device().map(|_| system_default_label())
        }
        #[cfg(not(target_os = "linux"))]
        host.default_output_device()
            .and_then(|d| d.description().ok())
            .map(|d| d.name().to_string())
    }
}

/// Probes one cpal output device into [`DeviceInfo`], applying the same
/// "default config unresolvable right now" fallback as the enumeration loop
/// (a device busy with a transient probe must not vanish from the list).
/// `None` when the device exposes neither a readable name nor any supported
/// config at all. Shared between full enumeration and the single-device
/// probe of cpal's own default output device (AM1.0-B32, фаза 2).
fn device_info_from_cpal(dev: &cpal::Device) -> Option<DeviceInfo> {
    let name = dev.description().map(|d| d.name().to_string()).ok()?;
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
    // A device whose default config cannot be resolved right now (e.g. the
    // probe/another stream holds it at that instant) must still show up in
    // the settings list — otherwise a working DAC "disappears" between two
    // Refreshes. Fall back to the first 2-channel supported config;
    // `choose_output` re-picks a precise rate from `supported` anyway.
    let (channels, sample_rate, sample_format, buffer_size) = match &default {
        Some(cfg) => (cfg.channels(), cfg.sample_rate(), cfg.sample_format(), *cfg.buffer_size()),
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
                None => return None, // no supported config at all
            }
        }
    };
    let category = classify_device(&id, &name);
    let rates = expand_supported_rates(&supported);
    let formats = dedup_formats(&supported_formats);
    Some(DeviceInfo {
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
    })
}

/// True when the backend-specific device id names a raw ALSA hardware node
/// (`hw:*`). Such nodes expose discrete native rates and formats without a
/// software-resampling proxy (PipeWire/Pulse/plughw) in between, so they are
/// the only handles that can deliver native-rate and bit-exact (DoP) output.
/// Used by [`collapse_same_name`], which on Linux only remains live in tests
/// (AM1.0-B32, фаза 2 — the Linux call site now goes through
/// `collapse_to_system_default`).
#[cfg(any(not(target_os = "linux"), test))]
fn is_raw_hardware_id(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.starts_with("hw:") || id.starts_with("hw=")
}

/// True when the id names either a raw hardware node (`hw:*`) or its
/// software-plugin twin (`plughw:*`) — both are "голое железо" for
/// AM1.0-B32's Совместимый fallback: the system-default entry must never be
/// backed by either, even when it is cpal's own default output device.
fn is_bare_hardware_or_plughw_id(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.starts_with("hw:") || id.starts_with("hw=") || id.starts_with("plughw:") || id.starts_with("plughw=")
}

/// Категория устройства вывода (ТЗ A3.0 §3.1) — используется для фильтрации
/// списка («только железо») и пометки capabilities.
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
///
/// On Linux the Совместимый list no longer calls this directly (ADR-07,
/// AM1.0-B32, фаза 2 — `collapse_to_system_default` owns that path); it stays
/// live for Windows/macOS and for tests.
#[cfg(any(not(target_os = "linux"), test))]
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

/// True when the backend id names one of the three fixed ALSA aliases that
/// always route through the desktop audio server (`default`/`pipewire`/
/// `pulse`) — never a raw hardware node, a per-format/-rate ALSA plugin
/// (`plughw:`/`front:`/`surround*`/`dmix`/`dsnoop`/`iec958`/`hdmi`), a
/// resampler (`samplerate`/`speexrate`/`lavrate`) or an unrelated backend
/// (`jack`/`oss`/`upmix`/`vdownmix`/`usbstream`). Совместимый режим обязан
/// показывать только выходы аудиосервера (ADR-07, ТЗ-127, матрица AM1.0 ТЗ
/// §5, AM1.0-B32) — все эти варианты, как и любой id с `:CARD=`, не являются
/// фиксированным алиасом сервера и отбрасываются простым несовпадением.
///
/// `pub(crate)`: переиспользуется миграцией сохранённого Совместимого
/// устройства при загрузке настроек (`settings::playback_dto`, AM1.0-B32,
/// фаза 2) — тот же критерий «это выход аудиосервера», без дублирования.
pub(crate) fn is_server_routed_pcm(id: &str) -> bool {
    matches!(id.to_ascii_lowercase().as_str(), "default" | "pipewire" | "pulse")
}

/// Схлопывает полный cpal-список до одной записи с заданной подписью, если
/// среди устройств есть хоть один выход аудиосервера ([`is_server_routed_pcm`]).
///
/// Иначе (редкий случай — ни "default", ни "pipewire", ни "pulse" среди
/// перечисленных PCM) список обязан по-прежнему не показывать голые
/// hw:/plughw: узлы (ADR-07, AM1.0-B32, фаза 2): берётся `fallback_default`
/// (отдельный, вне перечисления, вызов cpal для его собственного дефолтного
/// устройства — платформенная проверка и сам вызов живут в вызывающем коде,
/// эта функция остаётся чистой), с тем же ограничением — если он сам
/// hw:/plughw:, он отбрасывается. Нет и такого — список честно пуст
/// (AGENTS.md: честность сигнала важнее удобства; UI уже обрабатывает «нет
/// устройств»).
fn collapse_to_system_default(
    infos: Vec<DeviceInfo>,
    label: &str,
    fallback_default: Option<DeviceInfo>,
) -> Vec<DeviceInfo> {
    // Любой из трёх алиасов ведёт на тот же сервер; порядок — просто
    // предпочтение более информативного имени для представителя записи.
    const PRIORITY: [&str; 3] = ["pipewire", "pulse", "default"];
    let mut filtered: Vec<DeviceInfo> = infos.into_iter().filter(|d| is_server_routed_pcm(&d.id)).collect();
    if filtered.is_empty() {
        return match fallback_default {
            Some(d) if !is_bare_hardware_or_plughw_id(&d.id) => {
                let mut d = d;
                d.name = label.to_owned();
                vec![d]
            }
            _ => Vec::new(),
        };
    }
    filtered.sort_by_key(|d| {
        PRIORITY.iter().position(|p| *p == d.id.to_ascii_lowercase()).unwrap_or(PRIORITY.len())
    });
    let mut representative = filtered.swap_remove(0);
    representative.name = label.to_owned();
    vec![representative]
}

/// «Системное по умолчанию» — единственная запись списка Совместимого на
/// Linux (ADR-07, AM1.0-B32): подпись дополняется определившимся активным
/// аудиосервером ([`active_server_name`](crate::audio::reservation::dbus::active_server_name)).
#[cfg(target_os = "linux")]
fn system_default_label() -> String {
    match crate::audio::reservation::dbus::active_server_name() {
        Some(server) => format!("Системное по умолчанию ({server})"),
        None => "Системное по умолчанию".to_owned(),
    }
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

/// Семейство клока для частоты: 44.1k-семейство (44100, 88200, 176400,
/// 352800) или 48k (48000, 96000, 192000, 384000).
fn clock_family_of(rate: u32) -> ClockFamily {
    if rate.is_multiple_of(44100) {
        ClockFamily::Family44k
    } else {
        ClockFamily::Family48k
    }
}

/// True когда обе частоты принадлежат одному семейству клока.
fn same_family(a: u32, b: u32) -> bool {
    clock_family_of(a) == clock_family_of(b)
}

/// Конкретное семейство для запроса: `Auto` разрешается по источнику.
fn resolve_family(prefer: ClockFamily, track_rate: u32) -> ClockFamily {
    match prefer {
        ClockFamily::Auto => clock_family_of(track_rate),
        other => other,
    }
}

/// Максимальная частота устройства, кратная базе семейства (`44k` → 44100,
/// `48k` → 48000); `None` — в семействе нет ни одной поддерживаемой частоты
/// («пустое семейство», §3.4 шаг 4).
fn nearest_in_family(device: &DeviceInfo, family: ClockFamily) -> Option<u32> {
    let base = match family {
        ClockFamily::Family44k => 44100,
        ClockFamily::Family48k => 48000,
        ClockFamily::Auto => return None,
    };
    device
        .supported_rates
        .iter()
        .copied()
        .filter(|&r| r.is_multiple_of(base))
        .max()
}

/// Минимальный поддерживаемый rate ≥ `requested` (для
/// [`FallbackRatePolicy::NeverDownsample`]) из плоского списка границ.
fn nearest_rate_at_least(device: &DeviceInfo, requested: u32) -> Option<u32> {
    device.supported_rates.iter().copied().filter(|&r| r >= requested).min()
}

/// Decide which device/config to use for the given track parameters.
///
/// Pure selection (ТЗ A3.0 §3.4): no audio backend is touched, so it can be
/// tested with a mock. Follows the policy chain — channels, sample rate
/// (Auto/Native/Fixed + fallback policies), sample format, buffer size — and
/// reports each degradation step in [`ChosenOutput::fallback`].
pub fn choose_output(
    devices: &[DeviceInfo],
    default_name: Option<&str>,
    req: &OutputRequest,
) -> Result<ChosenOutput, String> {
    if devices.is_empty() {
        return Err(String::from("No audio output device found"));
    }

    // Resolve the requested device, falling back to the host default. `preferred`
    // may be a stable id (new settings) or a human name (legacy configs).
    let preferred = req.preferred_device.as_deref().filter(|n| !n.is_empty());
    let device = preferred
        .and_then(|key| devices.iter().find(|d| d.id == key).or_else(|| devices.iter().find(|d| d.name == key)))
        .or_else(|| default_name.and_then(|d| devices.iter().find(|dev| dev.id == d).or_else(|| devices.iter().find(|dev| dev.name == d))))
        .ok_or_else(|| String::from("No audio output device found"))?;

    let track_rate = req.track_rate;
    let track_channels = req.track_channels;
    let mut fallback: Option<FallbackReason> = None;

    // §3.4 шаг 3: каналы. Fail — строгое совпадение (устройство обязано
    // потянуть запрошенное число); иначе clamp в 1..=device.channels.
    let channels = (track_channels.min(device.channels as usize) as u16).max(1);
    if req.fallback == FallbackPolicy::Fail && track_channels as u16 > device.channels {
        return Err(format!("device does not support {track_channels} channels"));
    }
    if channels != track_channels.max(1) as u16 {
        fallback = Some(FallbackReason::ChannelsUnsupported {
            requested: track_channels,
            chosen: channels,
        });
    }

    // §3.4 шаги 4–5: выбор частоты по режиму ресемплера.
    let (out_rate, rate_reason): (u32, Option<FallbackReason>) = match req.resampler {
        ResamplerMode::Native => {
            if !device.supports_rate(track_rate) {
                return Err(format!(
                    "Native mode requires exact rate match; device lacks {track_rate} Hz"
                ));
            }
            (track_rate, None)
        }
        ResamplerMode::Fixed => {
            if req.fixed_rate != 0 {
                if !device.supports_rate(req.fixed_rate) {
                    // «Fixed» — интент, а не контракт: жёсткий Err запрещён
                    // (ревью §14 №20). fallback_rate при Fixed игнорируется
                    // (§2.3), поэтому «ближайший» = nearest_rate.
                    let nearest = nearest_rate(&device.supported, channels, req.fixed_rate)
                        .unwrap_or(device.sample_rate);
                    let cross_family = !same_family(req.fixed_rate, nearest);
                    (
                        nearest,
                        Some(FallbackReason::RateUnsupported {
                            requested: req.fixed_rate,
                            chosen: nearest,
                            cross_family,
                        }),
                    )
                } else {
                    (req.fixed_rate, None)
                }
            } else {
                // 0 = авто по семейству.
                match nearest_in_family(device, resolve_family(req.clock_family, track_rate)) {
                    Some(r) => (r, None),
                    None => (
                        device.sample_rate,
                        Some(FallbackReason::ClockFamilyIncomplete {
                            requested: track_rate,
                            chosen: device.sample_rate,
                        }),
                    ),
                }
            }
        }
        ResamplerMode::Auto => {
            if device.supports_rate(track_rate) {
                (track_rate, None)
            } else {
                match req.fallback {
                    FallbackPolicy::Fail => {
                        return Err(format!("device does not support {track_rate} Hz"));
                    }
                    FallbackPolicy::DeviceDefault => (device.sample_rate, None),
                    FallbackPolicy::Nearest => match req.fallback_rate {
                        FallbackRatePolicy::Nearest => {
                            let nearest = nearest_rate(&device.supported, channels, track_rate)
                                .unwrap_or(device.sample_rate);
                            let cross_family = !same_family(track_rate, nearest);
                            (
                                nearest,
                                Some(FallbackReason::RateUnsupported {
                                    requested: track_rate,
                                    chosen: nearest,
                                    cross_family,
                                }),
                            )
                        }
                        FallbackRatePolicy::SameFamily => {
                            match nearest_in_family(device, clock_family_of(track_rate)) {
                                Some(r) => (
                                    r,
                                    Some(FallbackReason::RateUnsupported {
                                        requested: track_rate,
                                        chosen: r,
                                        cross_family: false,
                                    }),
                                ),
                                None => {
                                    let nearest =
                                        nearest_rate(&device.supported, channels, track_rate)
                                            .unwrap_or(device.sample_rate);
                                    (
                                        nearest,
                                        Some(FallbackReason::RateUnsupported {
                                            requested: track_rate,
                                            chosen: nearest,
                                            cross_family: true,
                                        }),
                                    )
                                }
                            }
                        }
                        FallbackRatePolicy::NeverDownsample => {
                            match nearest_rate_at_least(device, track_rate) {
                                Some(r) => (
                                    r,
                                    Some(FallbackReason::RateUnsupported {
                                        requested: track_rate,
                                        chosen: r,
                                        cross_family: !same_family(track_rate, r),
                                    }),
                                ),
                                None => {
                                    return Err(format!(
                                        "no supported rate at or above {track_rate} Hz"
                                    ));
                                }
                            }
                        }
                    },
                }
            }
        }
    };

    // §3.4 шаг 7: буфер — «лучшая» конфигурация среди диапазонов с нужным
    // числом каналов (буфер по максимальному рейту диапазона).
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
        if let SupportedBufferSize::Range { min, max } = cfg.buffer_size {
            chosen_buf = BufferSize::Fixed(target_buffer_frames(cfg.max, min, max));
        }
    }

    // §3.4 шаг 6: sample-формат. Shared-путь говорит с ОС-микшером, поэтому
    // предпочитается F32; резерв — формат устройства по умолчанию.
    let sample_format = if device.supports_format(SampleFormat::F32) {
        SampleFormat::F32
    } else {
        device.sample_format
    };

    let resampled = out_rate != track_rate;
    // Приоритет причины: специфичная rate-причина > более ранний фолбек
    // (каналы) > форсированный ресемплинг в режиме Fixed.
    let fallback = if let Some(r) = rate_reason {
        Some(r)
    } else if let Some(f) = fallback {
        Some(f)
    } else if req.resampler == ResamplerMode::Fixed && resampled {
        Some(FallbackReason::ResamplerForced { requested: track_rate })
    } else {
        None
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
        resampled,
        source_rate: track_rate,
        source_channels: track_channels,
        fallback,
    })
}

/// Полный выбор устройства/конфигурации по запросу политик (ТЗ A3.0 §3.4),
/// против живого cpal-бэкенда. Используется плеером Совместимого режима
/// (`FallbackPolicy`, `ResamplerMode` реально применяются); монопольный вывод
/// ведёт `AlsaExclusiveBackend` (§8 С6, ADR-24).
pub fn select_output_for(req: &OutputRequest) -> Result<OutputSpec, String> {
    let host = CpalHost;
    let default = host.default_name();
    let chosen = choose_output(&host.devices(), default.as_deref(), req)?;
    let device = host.device_by_id(&chosen.device_id)?;
    Ok(OutputSpec {
        device,
        config: chosen.config,
        sample_format: chosen.sample_format,
        device_id: chosen.device_id,
        device_name: chosen.device_name,
        is_dop: false,
        resampled: chosen.resampled,
        source_rate: chosen.source_rate,
        source_channels: chosen.source_channels,
        fallback: chosen.fallback,
    })
}

/// True when `name` is a software sound-server node (PipeWire/Pulse) that cpal
/// surfaces as an ALSA "device". Such nodes proxy the stream through a server
/// with implicit resampling and can never deliver bit-exact (DoP) output, so
/// the settings dialog marks them clearly and lists them after the direct
/// hardware nodes.
pub fn is_server_node(name: &str) -> bool {
    // «Системное по умолчанию» (ADR-07, AM1.0-B32) уже само обозначает себя
    // как единственный выход аудиосервера, вплоть до подписи активного
    // сервера в скобках; старый суффикс
    // [`crate::i18n::audio::SERVER_NODE_SUFFIX`] здесь не нужен и не должен
    // задваиваться с этой подписью.
    if name.starts_with("Системное по умолчанию") {
        return false;
    }
    let n = name.to_ascii_lowercase();
    n.contains("pipewire")
        || n.contains("pulseaudio")
        || n.contains("sound server")
        || n.contains("default alsa output")
        || n == "default audio device"
}

/// Временный мост T1.0-B6 (убирается шагом 5 этой задачи): строка переехала
/// в [`crate::i18n::audio::SERVER_NODE_SUFFIX`], но внешние вызывающие
/// (`app::ui_manager`, тесты `app::mod`) ещё ссылаются на старый путь
/// `audio::output::SERVER_NODE_SUFFIX` — реэкспорт держит крейт собираемым
/// на промежуточных шагах миграции.
pub use crate::i18n::audio::SERVER_NODE_SUFFIX;

/// Build the settings-dialog device list as `(raw_name, label)` pairs.
///
/// 1. Deduplicates repeated names (cpal's ALSA backend exposes several
///    device handles with the same description — one per format/rate variant),
///    keeping the first (the one `device_by_name` will resolve at open time).
/// 2. Groups direct hardware nodes first, software server nodes last, so a
///    user picking an audiophile output (DoP / bit-perfect) sees the DAC before
///    the PipeWire/Pulse proxies.
/// 3. Server nodes get a [`crate::i18n::audio::SERVER_NODE_SUFFIX`] suffix
///    (T1.0-B6, русский текст) in the label while the raw name is preserved
///    untouched (it is what gets persisted in settings and matched by
///    `choose_output`/`device_by_name`).
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
            format!("{name}{}", crate::i18n::audio::SERVER_NODE_SUFFIX)
        } else {
            name.to_string()
        };
        out.push((name.to_string(), label));
    }
    hw.extend(srv);
    hw
}

/// Enumerate all output devices as full [`DeviceInfo`] structs.
///
/// One entry per human-readable name (cpal's ALSA backend exposes several
/// handles with the same description; `collapse_same_name` keeps the raw
/// `hw:*` node). Consumed by the settings dialog to build capabilities and
/// to preview the validation table (ТЗ A3.0 §8.1).
pub fn output_device_infos() -> Vec<DeviceInfo> {
    CpalHost.devices()
}

/// Build the settings-dialog device list `(id, label)` from full infos
/// (see the doc on [`output_devices`] for the semantics of the two elements).
pub fn device_pairs_from_infos(infos: &[DeviceInfo]) -> Vec<(String, String)> {
    let mut ids: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for d in infos {
        // Keep the *first* id per name: cpal's ALSA backend exposes several
        // handles with the same description, and the first one (the raw `hw:*`
        // node, which `collapse_same_name` also prefers) is the one
        // `device_by_name` resolves at open time.
        if let std::collections::hash_map::Entry::Vacant(e) = ids.entry(d.name.as_str()) {
            e.insert(d.id.as_str());
        }
    }
    let names: Vec<String> = infos.iter().map(|d| d.name.clone()).collect();
    label_device_names(&names)
        .into_iter()
        .map(|(name, label)| {
            let id = ids.get(name.as_str()).copied().unwrap_or(name.as_str()).to_string();
            (id, label)
        })
        .collect()
}

/// Enumerate usable output devices as `(id, label)` pairs.
///
/// The first element is the stable, backend-openable key (ALSA pcm id, e.g.
/// `hw:CARD=4,DEV=0`) persisted in settings and passed back to
/// [`select_output_for`]; the second is the deduplicated, grouped human-readable
/// label shown in the settings dialog (see [`label_device_names`]).
pub fn output_devices() -> Vec<(String, String)> {
    device_pairs_from_infos(&output_device_infos())
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

/// Причина отказа открытия PCM (§6.6 OPEN): `EBUSY` повторяется, прочие — без
/// повтора (ТЗ-122).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PcmOpenError {
    /// `EBUSY`: прежний владелец ещё закрывает PCM.
    Busy,
    /// `ENOENT` / `ENODEV`.
    Disconnected,
    Refused { errno: i32 },
}

/// Отказ сборки потока: класс ошибки открытия PCM (§6.6 OPEN: `EBUSY`
/// повторяется, прочие — без повтора; ТЗ-122) и текст.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StreamBuildError {
    pub open: PcmOpenError,
    pub message: String,
}

impl StreamBuildError {
    /// Отказ без errno (формат, тестовый шов): не повторяется.
    pub fn refused(message: String) -> StreamBuildError {
        StreamBuildError { open: PcmOpenError::Refused { errno: 0 }, message }
    }
}

impl std::fmt::Display for StreamBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Класс ошибки cpal → [`PcmOpenError`]. cpal сводит `EBUSY`/`EAGAIN` к
/// `DeviceBusy`, `ENODEV`/`ENOENT` — к `DeviceNotAvailable` (alsa `From<alsa::Error>`);
/// errno прочих восстанавливается по классу.
pub fn classify_pcm_error(kind: cpal::ErrorKind) -> PcmOpenError {
    const EACCES: i32 = 13;
    const EINVAL: i32 = 22;
    const ENOSYS: i32 = 38;
    match kind {
        cpal::ErrorKind::DeviceBusy => PcmOpenError::Busy,
        cpal::ErrorKind::DeviceNotAvailable => PcmOpenError::Disconnected,
        cpal::ErrorKind::PermissionDenied => PcmOpenError::Refused { errno: EACCES },
        cpal::ErrorKind::UnsupportedConfig | cpal::ErrorKind::InvalidInput => PcmOpenError::Refused { errno: EINVAL },
        cpal::ErrorKind::UnsupportedOperation => PcmOpenError::Refused { errno: ENOSYS },
        _ => PcmOpenError::Refused { errno: 0 },
    }
}

// ---------------------------------------------------------------------------
// Вывод нового тракта (§6.14, §6.15, ADR-03): колбэк пишет байты формата
// устройства через `build_output_stream_raw` + `Data::bytes_mut`.
// ---------------------------------------------------------------------------

/// Рендер периода в байты выходного формата: `PcmRender` или `DopRender`.
pub trait RawRender: Send + 'static {
    fn render(&mut self, out: &mut [u8]);
}

impl<P: PcmSample, O: OutFormat, G: GainStage> RawRender for PcmRender<P, O, G> {
    #[inline]
    fn render(&mut self, out: &mut [u8]) {
        PcmRender::render(self, out);
    }
}

impl<O: OutFormat> RawRender for DopRender<O> {
    #[inline]
    fn render(&mut self, out: &mut [u8]) {
        DopRender::render(self, out);
    }
}

impl RawRender for Box<dyn RawRender> {
    #[inline]
    fn render(&mut self, out: &mut [u8]) {
        (**self).render(out);
    }
}

/// Формат выхода колбэка по формату cpal (ALSA): `I16→S16_LE`, `I24→S24_LE`
/// (4 байта, ОВ-32), `I32→S32_LE`, `F32→FLOAT_LE`. Прочие форматы не открываются.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFormat {
    S16,
    S24,
    S32,
    F32,
}

impl RawFormat {
    pub fn from_sample_format(f: SampleFormat) -> Option<RawFormat> {
        match f {
            SampleFormat::I16 => Some(RawFormat::S16),
            SampleFormat::I24 => Some(RawFormat::S24),
            SampleFormat::I32 => Some(RawFormat::S32),
            SampleFormat::F32 => Some(RawFormat::F32),
            _ => None,
        }
    }

    /// Байт на сэмпл в буфере cpal.
    pub fn bytes(self) -> usize {
        match self {
            RawFormat::S16 => S16Le::BYTES,
            RawFormat::S24 => S24Le::BYTES,
            RawFormat::S32 => S32Le::BYTES,
            RawFormat::F32 => F32Le::BYTES,
        }
    }
}

/// `PcmRender` под формат устройства: мономорфизация по `O` выбирается один раз
/// при сборке потока (§6.14). `None` — формат не поддерживается.
pub fn pcm_render_for<P: PcmSample, G: GainStage>(
    format: SampleFormat,
    core: RenderCore<P>,
    payload: RingPayload,
    tpdf: Tpdf,
) -> Option<Box<dyn RawRender>> {
    Some(match RawFormat::from_sample_format(format)? {
        RawFormat::S16 => Box::new(PcmRender::<P, S16Le, G>::new(core, payload, tpdf)),
        RawFormat::S24 => Box::new(PcmRender::<P, S24Le, G>::new(core, payload, tpdf)),
        RawFormat::S32 => Box::new(PcmRender::<P, S32Le, G>::new(core, payload, tpdf)),
        RawFormat::F32 => Box::new(PcmRender::<P, F32Le, G>::new(core, payload, tpdf)),
    })
}

/// `DopRender` под формат устройства (§6.15): DoP несут только 24/32-битные
/// целые контейнеры. `None` для `S16`/`F32` и прочих.
pub fn dop_render_for(format: SampleFormat, core: RenderCore<i32>) -> Option<Box<dyn RawRender>> {
    match RawFormat::from_sample_format(format)? {
        RawFormat::S24 => Some(Box::new(DopRender::<S24Le>::new(core))),
        RawFormat::S32 => Some(Box::new(DopRender::<S32Le>::new(core))),
        RawFormat::S16 | RawFormat::F32 => None,
    }
}

/// Поток вывода нового тракта: колбэк отдаёт `bytes_mut()` рендеру (§6.14).
/// В колбэке нет блокировок, логирования и аллокаций (ТЗ-98, ТЗ-102); ошибка
/// потока только поднимает `error_flag`.
pub fn build_output_stream_raw<R: RawRender>(
    spec: &OutputSpec,
    mut render: R,
    error_flag: Option<Arc<AtomicBool>>,
) -> Result<cpal::Stream, StreamBuildError> {
    if RawFormat::from_sample_format(spec.sample_format).is_none() {
        return Err(StreamBuildError::refused(format!(
            "Unsupported output sample format: {:?}",
            spec.sample_format
        )));
    }
    spec.device
        .build_output_stream_raw(
            spec.config,
            spec.sample_format,
            move |data: &mut cpal::Data, _| render.render(data.bytes_mut()),
            move |e| {
                eprintln!("Audio stream error: {e}");
                if let Some(f) = error_flag.as_ref() {
                    f.store(true, Ordering::Relaxed);
                }
            },
            None,
        )
        .map_err(|e| StreamBuildError {
            open: classify_pcm_error(e.kind()),
            message: format!("Cannot build output stream: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::render::gain::NoGain;
    use crate::audio::session::SessionShared;

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

    /// Запрос с конфигурацией «по умолчанию» (shared, Auto/Nearest) —
    /// эквивалент выбора до A3.3, используется старыми тестами.
    fn req_defaults(
        track_rate: u32,
        track_channels: usize,
        preferred: Option<&str>,
    ) -> OutputRequest {
        OutputRequest {
            track_rate,
            track_channels,
            preferred_device: preferred.map(String::from),
            fallback: FallbackPolicy::Nearest,
            resampler: ResamplerMode::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            clock_family: ClockFamily::Auto,
            fixed_rate: 0,
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
            &req_defaults(track_rate, track_channels, preferred),
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
            supported,
        }
    }

    #[test]
    fn choose_output_no_devices_errors() {
        let r = choose_output(&[], None, &req_defaults(44100, 2, None));
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

    /// AM1.0-B32 (фаза 2, шаг 3): Совместимый `SharedDeviceChoice::SystemDefault`
    /// маппится в `OutputRequest.preferred_device = None`
    /// (`ModeSettings::device`, §2.4) — значит `choose_output` на каждом
    /// открытии (новый трек — новый вызов `select_output_for` с живым
    /// `host.default_name()`) берёт ТЕКУЩИЙ default хоста, а не «запоминает»
    /// устройство прошлого трека. Смена системного sink между треками
    /// переразрешается на новое устройство.
    #[test]
    fn choose_output_system_default_reresolves_when_host_default_changes_between_tracks() {
        let dac = mock_device("DAC", 2, 44100, &[(2, 44100, 192000)]);
        let speakers = mock_device("Speakers", 2, 44100, &[(2, 44100, 192000)]);
        let devices = [dac, speakers];
        // SystemDefault → preferred_device = None на обоих треках.
        let req = req_defaults(44100, 2, None);

        let first_track = choose_output(&devices, Some("DAC"), &req).expect("первый трек");
        assert_eq!(first_track.device_id, "DAC");

        // Пользователь сменил системный микшерный sink между треками.
        let second_track = choose_output(&devices, Some("Speakers"), &req).expect("второй трек");
        assert_eq!(second_track.device_id, "Speakers");
    }

    #[test]
    fn choose_output_unknown_preferred_errors() {
        let device = mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]);
        let r = choose_output(&[device], None, &req_defaults(44100, 2, Some("Missing")));
        assert!(r.is_err());
    }

    #[test]
    fn choose_output_preferred_drops_to_default_when_missing() {
        let device = mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]);
        let chosen =
            choose_output(&[device], Some("Speakers"), &req_defaults(48000, 2, Some("Missing"))).unwrap();
        assert_eq!(chosen.device_name, "Speakers");
    }

    #[test]
    fn choose_output_matches_track_rate_when_supported() {
        let device = mock_device("DAC", 2, 44100, &[(2, 44100, 48000), (2, 88200, 192000)]);
        let chosen = choose_output(&[device], Some("DAC"), &req_defaults(96000, 2, None)).unwrap();
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
        let chosen = choose_output(&[device], Some("DAC"), &req_defaults(384000, 1, None)).unwrap();
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
        let chosen = choose_output(&[device], Some("DAC"), &req_defaults(88200, 2, None)).unwrap();
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
        let chosen = choose_output(&[device], Some("DAC"), &req_defaults(96000, 2, None)).unwrap();
        assert_eq!(chosen.config.sample_rate, 96000);
    }

    #[test]
    fn choose_output_clamps_channels_to_device() {
        let mono = mock_device("Mono", 1, 44100, &[(1, 44100, 44100)]);
        let chosen = choose_output(&[mono], Some("Mono"), &req_defaults(44100, 6, None)).unwrap();
        assert_eq!(chosen.config.channels, 1);
        assert_eq!(chosen.config.sample_rate, 44100);
    }

    #[test]
    fn choose_output_unknown_default_errors() {
        let device = mock_device("Speakers", 2, 48000, &[(2, 44100, 48000)]);
        let r = choose_output(&[device], Some("Ghost"), &req_defaults(48000, 2, None));
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
        assert!(pairs[2].1.contains(crate::i18n::audio::SERVER_NODE_SUFFIX), "{}", pairs[2].1);
        assert_eq!(pairs[3].0, "Default Audio Device");
        assert!(pairs[3].1.contains(crate::i18n::audio::SERVER_NODE_SUFFIX), "{}", pairs[3].1);
    }

    #[test]
    fn device_pairs_from_infos_dedupe_group_and_map_stable_ids() {
        let infos = vec![
            mock_device_id("hw:CARD=4,DEV=0", "ADI-2 DAC (56680121), USB Audio", 2, 48000, &[(2, 44100, 192000)]),
            mock_device_id("hw:CARD=4,DEV=1", "ADI-2 DAC (56680121), USB Audio", 2, 48000, &[(2, 44100, 192000)]),
            mock_device_id("pulse", "PipeWire Sound Server", 2, 48000, &[(2, 4000, 4294967295)]),
        ];
        let pairs = device_pairs_from_infos(&infos);
        // Two ALSA handles with the same description collapse into one pair;
        // the *first* id per name wins (the raw `hw:*` node).
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "hw:CARD=4,DEV=0");
        assert_eq!(pairs[0].1, "ADI-2 DAC (56680121), USB Audio");
        // Server nodes last: raw id preserved, label carries the hint.
        assert_eq!(pairs[1].0, "pulse");
        assert!(pairs[1].1.contains(crate::i18n::audio::SERVER_NODE_SUFFIX), "{}", pairs[1].1);
    }

    #[test]
    fn device_pairs_from_infos_maps_id_identical_to_name() {
        // Server proxies resolve with id == name; the stable key must survive.
        let infos = vec![mock_device_id(
            "default",
            "Default Audio Device",
            2,
            48000,
            &[(2, 4000, 4294967295)],
        )];
        let mut pairs = device_pairs_from_infos(&infos);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "default");
        let name = pairs.pop().unwrap().1;
        assert!(name.contains(crate::i18n::audio::SERVER_NODE_SUFFIX), "{name}");
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
    fn is_server_node_excludes_system_default_label() {
        // Единственная запись Совместимого (ADR-07, AM1.0-B32) уже сама
        // называет сервер в скобках — старый суффикс не должен задваиваться.
        assert!(!is_server_node("Системное по умолчанию"));
        assert!(!is_server_node("Системное по умолчанию (PipeWire)"));
        assert!(!is_server_node("Системное по умолчанию (PulseAudio)"));
    }

    #[test]
    fn is_server_routed_pcm_allows_only_fixed_server_aliases() {
        for allowed in ["default", "pipewire", "pulse", "DEFAULT", "PipeWire"] {
            assert!(is_server_routed_pcm(allowed), "{allowed} must be allowed");
        }
        for denied in [
            "hw:CARD=4,DEV=0",
            "plughw:CARD=4,DEV=0",
            "front:CARD=4,DEV=0",
            "surround40:CARD=4,DEV=0",
            "dmix",
            "dmix:CARD=4,DEV=0",
            "dsnoop:CARD=4,DEV=0",
            "iec958:CARD=4,DEV=0",
            "hdmi:CARD=4,DEV=0",
            "samplerate",
            "speexrate",
            "lavrate",
            "jack",
            "oss",
            "upmix:CARD=4,DEV=0",
            "vdownmix:CARD=4,DEV=0",
            "usbstream:CARD=4,DEV=0",
            // Сам сервер, но с привязкой к карте — уже не фиксированный алиас.
            "pipewire:CARD=4,DEV=0",
            "",
        ] {
            assert!(!is_server_routed_pcm(denied), "{denied} must be denied");
        }
    }

    #[test]
    fn collapse_to_system_default_collapses_server_aliases_and_drops_hardware() {
        let devs = vec![
            mock_device_id("hw:CARD=4,DEV=0", "ADI-2 DAC, USB Audio", 2, 48000, &[(2, 48000, 48000)]),
            mock_device_id("default", "Default Audio Device", 2, 48000, &[(2, 44100, 48000)]),
            mock_device_id("pulse", "PulseAudio Sound Server", 2, 48000, &[(2, 44100, 48000)]),
            mock_device_id("pipewire", "PipeWire Sound Server", 2, 48000, &[(2, 44100, 48000)]),
        ];
        let collapsed = collapse_to_system_default(devs, "Системное по умолчанию (PipeWire)", None);
        assert_eq!(collapsed.len(), 1);
        // pipewire — высший приоритет среди трёх алиасов сервера.
        assert_eq!(collapsed[0].id, "pipewire");
        assert_eq!(collapsed[0].name, "Системное по умолчанию (PipeWire)");
    }

    #[test]
    fn collapse_to_system_default_falls_back_to_empty_without_server_alias_or_default() {
        // Нет ни одного фиксированного алиаса сервера, и cpal не называет
        // отдельного дефолтного устройства — список обязан остаться честно
        // пустым, а не подсовывать голые hw:/plughw: узлы (AGENTS.md:
        // честность сигнала; AM1.0-B32, фаза 2). UI уже обрабатывает «нет
        // устройств».
        let devs = vec![
            mock_device_id("hw:CARD=4,DEV=0", "ADI-2 DAC, USB Audio", 2, 48000, &[(2, 48000, 48000)]),
            mock_device_id("plughw:CARD=4,DEV=0", "ADI-2 DAC, USB Audio", 2, 48000, &[(2, 4000, 4294967295)]),
        ];
        let collapsed = collapse_to_system_default(devs, "Системное по умолчанию", None);
        assert!(collapsed.is_empty());
    }

    #[test]
    fn collapse_to_system_default_falls_back_to_cpals_own_default_device() {
        // cpal называет дефолтное устройство отдельно от перечисления (id не
        // входит ни в один из трёх алиасов, но и не голое hw:/plughw:) — оно
        // становится представителем «Системного по умолчанию».
        let devs = vec![mock_device_id(
            "hw:CARD=4,DEV=0",
            "ADI-2 DAC, USB Audio",
            2,
            48000,
            &[(2, 48000, 48000)],
        )];
        let fallback = mock_device_id(
            "sysdefault:CARD=4",
            "ADI-2 DAC, USB Audio",
            2,
            48000,
            &[(2, 44100, 48000)],
        );
        let collapsed = collapse_to_system_default(devs, "Системное по умолчанию", Some(fallback.clone()));
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].id, fallback.id);
        assert_eq!(collapsed[0].name, "Системное по умолчанию");
    }

    #[test]
    fn collapse_to_system_default_never_uses_hw_or_plughw_as_fallback_default() {
        // Даже если cpal само называет дефолтным устройством голый hw:/plughw:
        // узел, он не становится представителем — список честно пуст
        // (ADR-07, AM1.0-B32, фаза 2: "никогда hw:/plughw: записи").
        let devs = vec![mock_device_id(
            "hw:CARD=4,DEV=0",
            "ADI-2 DAC, USB Audio",
            2,
            48000,
            &[(2, 48000, 48000)],
        )];
        let fallback = mock_device_id(
            "plughw:CARD=4,DEV=0",
            "ADI-2 DAC, USB Audio",
            2,
            48000,
            &[(2, 4000, 4294967295)],
        );
        let collapsed = collapse_to_system_default(devs, "Системное по умолчанию", Some(fallback));
        assert!(collapsed.is_empty());
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

    #[test]
    fn supported_rates_sorted_unique() {
        // Дискретные записи + непрерывные диапазоны (добавляют границы):
        // дубликаты схлопываются, порядок строго возрастающий.
        let ranges = vec![
            rate_range(2, 48000, 48000),
            rate_range(2, 44100, 88200),
            rate_range(2, 96000, 192000),
            rate_range(2, 48000, 48000),
        ];
        let rates = expand_supported_rates(&ranges);
        assert_eq!(rates, vec![44100, 48000, 88200, 96000, 192000]);
        for w in rates.windows(2) {
            assert!(w[0] < w[1], "not strictly sorted: {w:?}");
        }
    }

    #[test]
    fn rates_desc_formats_khz_list() {
        let dev = mock_device_id(
            "hw:CARD=1,DEV=0",
            "ADI-2",
            2,
            48000,
            &[
                (2, 44100, 44100),
                (2, 48000, 48000),
                (2, 88200, 88200),
                (2, 96000, 96000),
                (2, 176400, 176400),
                (2, 192000, 192000),
            ],
        );
        assert_eq!(dev.rates_desc(), "44.1 · 48 · 88.2 · 96 · 176.4 · 192 kHz");
    }

    #[test]
    fn formats_desc_joins_uppercase_labels() {
        let mut dev = mock_device("USB DAC", 2, 48000, &[(2, 44100, 192000)]);
        dev.supported_formats = vec![
            SampleFormat::I16,
            SampleFormat::I24,
            SampleFormat::I32,
            SampleFormat::F32,
        ];
        assert_eq!(dev.formats_desc(), "I16 · I24 · I32 · F32");
    }

    #[test]
    fn formats_dedup_keeps_first_order() {
        let formats = [
            SampleFormat::F32,
            SampleFormat::I32,
            SampleFormat::F32,
            SampleFormat::I24,
            SampleFormat::I32,
        ];
        assert_eq!(
            dedup_formats(&formats),
            vec![SampleFormat::F32, SampleFormat::I32, SampleFormat::I24]
        );
    }

    #[test]
    fn device_info_capability_flags() {
        let stereo = mock_device("USB DAC", 2, 48000, &[(2, 44100, 44100), (2, 88200, 88200)]);
        assert!(stereo.is_stereo());
        assert!(stereo.supports_rate(44100));
        assert!(stereo.supports_rate(88200));
        assert!(!stereo.supports_rate(96000));
        assert!(stereo.supports_format(SampleFormat::F32));
        assert!(!stereo.supports_format(SampleFormat::I32));

        let mono = mock_device("Mono", 1, 44100, &[(1, 44100, 44100)]);
        assert!(!mono.is_stereo());

        // Непрерывный plughw-диапазон покрывает промежуточные частоты,
        // которых нет в плоском списке.
        let plughw =
            mock_device_id("plughw:CARD=2,DEV=0", "PipeWire", 2, 48000, &[(2, 4000, 4294967295)]);
        assert!(plughw.supports_rate(44100));
        assert!(plughw.supports_rate(88200));
        assert!(plughw.supports_rate(192000));
    }

    #[test]
    fn clock_families_desc_partial_and_full() {
        let dev = mock_device_id(
            "hw:CARD=1,DEV=0",
            "ADI-2",
            2,
            48000,
            &[
                (2, 44100, 44100),
                (2, 88200, 88200),
                (2, 176400, 176400),
                (2, 48000, 48000),
                (2, 96000, 96000),
            ],
        );
        assert_eq!(
            dev.clock_families_desc(),
            "44k full (44.1, 88.2, 176.4) · 48k partial (48, 96)"
        );
    }

    #[test]
    fn clock_families_desc_omits_empty_family() {
        // 44k-семейства нет вовсе — в списке остаётся только 48k.
        let dev = mock_device("DAC", 2, 48000, &[(2, 48000, 48000), (2, 192000, 192000)]);
        assert_eq!(dev.clock_families_desc(), "48k partial (48, 192)");
        // Без семейных частот вообще.
        let dev = mock_device("PCM-only", 2, 48000, &[(2, 200000, 200000)]);
        assert_eq!(dev.clock_families_desc(), "—");
    }

    #[test]
    fn dop_container_rate_picks_first_supported() {
        let dev = mock_device("DAC", 2, 48000, &[(2, 176400, 176400)]);
        assert_eq!(dev.dop_container_rate(), Some(176400));
        // Предпочтение отдаётся младшей из штатных контейнерных частот.
        let dev = mock_device("DAC", 2, 48000, &[(2, 352800, 352800), (2, 176400, 176400)]);
        assert_eq!(dev.dop_container_rate(), Some(176400));
        let dev = mock_device("DAC", 2, 48000, &[(2, 705600, 705600)]);
        assert_eq!(dev.dop_container_rate(), Some(705600));
        // Устройство без DoP-частот.
        let dev = mock_device("DAC", 2, 48000, &[(2, 44100, 44100), (2, 96000, 96000)]);
        assert_eq!(dev.dop_container_rate(), None);
    }

    #[test]
    fn desc_methods_empty_device_use_em_dash() {
        let mut dev = mock_device("DAC", 2, 48000, &[]);
        dev.supported_formats = vec![];
        assert_eq!(dev.rates_desc(), "—");
        assert_eq!(dev.formats_desc(), "—");
        assert_eq!(dev.clock_families_desc(), "—");
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

    // --- A3.0 §11.2: choose_output policies ---

    fn req_off(track_rate: u32, track_channels: usize) -> OutputRequest {
        OutputRequest {
            track_rate,
            track_channels,
            preferred_device: None,
            fallback: FallbackPolicy::Nearest,
            resampler: ResamplerMode::Auto,
            fallback_rate: FallbackRatePolicy::Nearest,
            clock_family: ClockFamily::Auto,
            fixed_rate: 0,
        }
    }

    #[test]
    fn choose_output_fallback_fail_returns_error() {
        // ТЗ §11.2: FallbackPolicy::Fail + несовпадение rate → Err.
        let device = mock_device("DAC", 2, 44100, &[(2, 44100, 44100)]);
        let req = OutputRequest {
            track_rate: 192000,
            fallback: FallbackPolicy::Fail,
            ..req_off(192000, 2)
        };
        let r = choose_output(&[device], Some("DAC"), &req);
        assert!(r.unwrap_err().contains("192000"));
    }

    #[test]
    fn choose_output_device_default_ignores_track_rate() {
        // ТЗ §11.2: DeviceDefault → дефолт устройства вне зависимости от трека.
        let device = mock_device("DAC", 2, 44100, &[(2, 44100, 48000)]);
        let req = OutputRequest {
            fallback: FallbackPolicy::DeviceDefault,
            ..req_off(192000, 2)
        };
        let chosen = choose_output(&[device], Some("DAC"), &req).unwrap();
        assert_eq!(chosen.config.sample_rate, 44100);
        assert!(chosen.resampled);
    }

    #[test]
    fn choose_output_native_mode_requires_exact() {
        // ТЗ §11.2: ResamplerMode::Native + недоступный rate → HARD FAIL.
        let device = mock_device("DAC", 2, 44100, &[(2, 44100, 48000)]);
        let req = OutputRequest {
            resampler: ResamplerMode::Native,
            ..req_off(96000, 2)
        };
        let err = choose_output(&[device], Some("DAC"), &req).unwrap_err();
        assert!(err.contains("Native mode requires exact rate match"));
        assert!(err.contains("96000"));
    }

    #[test]
    fn choose_output_fixed_mode_uses_fixed_rate() {
        // ТЗ §11.2: Fixed + поддерживаемый fixed_rate → ровно он, без фолбека.
        let device = mock_device(
            "DAC",
            2,
            44100,
            &[(2, 44100, 48000), (2, 88200, 88200), (2, 96000, 192000)],
        );
        let req = OutputRequest {
            resampler: ResamplerMode::Fixed,
            fixed_rate: 88200,
            ..req_off(88200, 2)
        };
        let chosen = choose_output(&[device], Some("DAC"), &req).unwrap();
        assert_eq!(chosen.config.sample_rate, 88200);
        assert!(!chosen.resampled);
        assert_eq!(chosen.fallback, None);
    }

    #[test]
    fn choose_output_fixed_mode_uses_family_if_zero() {
        // ТЗ §11.2: Fixed + fixed_rate=0 → максимум предпочитаемого семейства
        // (176400 — максимальная кратная 44100). Ресемплинг форсированный.
        let device = mock_device(
            "DAC",
            2,
            44100,
            &[(2, 44100, 44100), (2, 88200, 88200), (2, 176400, 176400)],
        );
        let req = OutputRequest {
            resampler: ResamplerMode::Fixed,
            fixed_rate: 0,
            clock_family: ClockFamily::Family44k,
            ..req_off(44100, 2)
        };
        let chosen = choose_output(&[device], Some("DAC"), &req).unwrap();
        assert_eq!(chosen.config.sample_rate, 176400);
        assert!(chosen.resampled);
        assert_eq!(
            chosen.fallback,
            Some(FallbackReason::ResamplerForced { requested: 44100 })
        );
    }

    #[test]
    fn choose_output_fixed_mode_unsupported_rate_falls_to_nearest() {
        // ТЗ §11.2 (ревью §14 №20): Fixed + недоступный fixed_rate — интент,
        // а не контракт: целевой = nearest, причина RateUnsupported{cross_family}.
        let device = mock_device(
            "DAC",
            2,
            44100,
            &[(2, 44100, 44100), (2, 48000, 48000), (2, 96000, 96000)],
        );
        let req = OutputRequest {
            resampler: ResamplerMode::Fixed,
            fixed_rate: 88200,
            ..req_off(44100, 2)
        };
        let chosen = choose_output(&[device], Some("DAC"), &req).unwrap();
        assert_eq!(chosen.config.sample_rate, 96000);
        assert_eq!(
            chosen.fallback,
            Some(FallbackReason::RateUnsupported {
                requested: 88200,
                chosen: 96000,
                cross_family: true,
            })
        );
    }

    #[test]
    fn choose_output_fixed_mode_auto_empty_family_falls_to_device_default() {
        // ТЗ §11.2 (ревью §14 №2): Fixed + fixed_rate=0 + пустое семейство →
        // DeviceDefault + явная причина ClockFamilyIncomplete.
        let device = mock_device(
            "DAC",
            2,
            48000,
            &[(2, 48000, 48000), (2, 96000, 96000)],
        );
        let req = OutputRequest {
            resampler: ResamplerMode::Fixed,
            fixed_rate: 0,
            clock_family: ClockFamily::Family44k,
            ..req_off(88200, 2)
        };
        let chosen = choose_output(&[device], Some("DAC"), &req).unwrap();
        assert_eq!(chosen.config.sample_rate, 48000);
        assert_eq!(
            chosen.fallback,
            Some(FallbackReason::ClockFamilyIncomplete {
                requested: 88200,
                chosen: 48000,
            })
        );
    }

    #[test]
    fn choose_output_fallback_rate_same_family() {
        // ТЗ §11.2: 88.2k на {44.1, 48, 96, 192} + SameFamily → 44.1 (своё
        // семейство, cross_family = false), а не 96k.
        let device = mock_device(
            "DAC",
            2,
            44100,
            &[(2, 44100, 44100), (2, 48000, 48000), (2, 96000, 96000), (2, 192000, 192000)],
        );
        let req = OutputRequest {
            fallback_rate: FallbackRatePolicy::SameFamily,
            ..req_off(88200, 2)
        };
        let chosen = choose_output(&[device], Some("DAC"), &req).unwrap();
        assert_eq!(chosen.config.sample_rate, 44100);
        assert_eq!(
            chosen.fallback,
            Some(FallbackReason::RateUnsupported {
                requested: 88200,
                chosen: 44100,
                cross_family: false,
            })
        );
    }

    #[test]
    fn choose_output_fallback_rate_never_downsample() {
        // ТЗ §11.2: 192k на {44.1, 48, 96} + NeverDownsample → нет rate ≥ 192k → Err.
        let device = mock_device(
            "DAC",
            2,
            44100,
            &[(2, 44100, 44100), (2, 48000, 48000), (2, 96000, 96000)],
        );
        let req = OutputRequest {
            fallback_rate: FallbackRatePolicy::NeverDownsample,
            ..req_off(192000, 2)
        };
        let r = choose_output(&[device], Some("DAC"), &req);
        assert!(r.unwrap_err().contains("192000"));
    }

    fn core_i32(channels: usize) -> (rtrb::Producer<i32>, RenderCore<i32>) {
        let (p, c) = rtrb::RingBuffer::<i32>::new(64);
        (p, RenderCore::new(c, Arc::new(SessionShared::new()), channels, 0))
    }

    #[test]
    fn raw_format_maps_alsa_formats() {
        assert_eq!(RawFormat::from_sample_format(SampleFormat::I16), Some(RawFormat::S16));
        assert_eq!(RawFormat::from_sample_format(SampleFormat::I24), Some(RawFormat::S24));
        assert_eq!(RawFormat::from_sample_format(SampleFormat::I32), Some(RawFormat::S32));
        assert_eq!(RawFormat::from_sample_format(SampleFormat::F32), Some(RawFormat::F32));
        assert_eq!(RawFormat::from_sample_format(SampleFormat::U8), None);
        // I24 у cpal — 4 байта (S24_LE), как у `S24Le` (ОВ-32).
        assert_eq!(RawFormat::S24.bytes(), SampleFormat::I24.sample_size());
        assert_eq!(RawFormat::S16.bytes(), SampleFormat::I16.sample_size());
    }

    #[test]
    fn render_selection_by_format() {
        let payload = RingPayload::ExactI32 { valid_bits: crate::audio::format::BitDepth::new(24).unwrap() };
        for f in [SampleFormat::I16, SampleFormat::I24, SampleFormat::I32, SampleFormat::F32] {
            let (_p, c) = core_i32(2);
            assert!(pcm_render_for::<i32, NoGain>(f, c, payload, Tpdf::off()).is_some());
        }
        let (_p, c) = core_i32(2);
        assert!(pcm_render_for::<i32, NoGain>(SampleFormat::U8, c, payload, Tpdf::off()).is_none());
        for (f, ok) in [
            (SampleFormat::I24, true),
            (SampleFormat::I32, true),
            (SampleFormat::I16, false),
            (SampleFormat::F32, false),
        ] {
            let (_p, c) = core_i32(2);
            assert_eq!(dop_render_for(f, c).is_some(), ok, "{f:?}");
        }
    }

    #[test]
    fn boxed_raw_render_writes_silence_when_paused() {
        let (mut p, c) = core_i32(2);
        for _ in 0..8 {
            p.push(0x1234_5600).unwrap();
        }
        let payload = RingPayload::ExactI32 { valid_bits: crate::audio::format::BitDepth::new(24).unwrap() };
        let mut r = pcm_render_for::<i32, NoGain>(SampleFormat::I32, c, payload, Tpdf::off()).unwrap();
        let mut buf = [0xAAu8; 32];
        RawRender::render(&mut r, &mut buf);
        assert!(buf.iter().all(|&b| b == 0));
    }
}
