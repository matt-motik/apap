//! Полнотрековая спектрограмма — частота × время через БПФ (ТЗ §5.3, §16.5).
//!
//! Стриминг: трек кармится кусками, окна БПФ продвигаются с адаптивным hop
//! (столбцов ≤ `max_frames`), каждая колонка сразу разрисовывается в RGBA
//! (`columns × 512`). Маппинг пикселей по частоте — линейный/логарифмический/
//! мель; уровень дБ → палитра; для DSD опциональная компенсация спада CIC.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::audio::palettes;
use crate::audio::visualizer::{FreqScale, Palette, SpectrogramCfg, WindowType};

/// Высота изображения в пикселях (как у осциллограммы).
pub const IMG_H: usize = 512;
/// Предел ширины изображения (колонок).
pub const MAX_PIX_W: usize = 8192;
/// Предел компенсации CIC, дБ (чтобы не усиливать шум на ВЧ).
const CIC_COMP_MAX_DB: f32 = 36.0;

// ---------------------------------------------------------------------------
// Окна БПФ
// ---------------------------------------------------------------------------

/// Коэффициенты оконного фильтра (нормируются к сумме когерентности 0.5/0.54…).
pub fn window_coeffs(n: usize, t: WindowType) -> Vec<f32> {
    let n = n.max(1);
    (0..n)
        .map(|i| {
            let a = 2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32;
            match t {
                WindowType::Hann => 0.5 * (1.0 - a.cos()),
                WindowType::Hamming => 0.54 - 0.46 * a.cos(),
                WindowType::Blackman => 0.42 - 0.5 * a.cos() + 0.08 * (2.0 * a).cos(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Адаптивный hop
// ---------------------------------------------------------------------------

/// Выбирает `(hop, columns)` для всего трека.
///
/// `hop` — шаг окна в кадрах: не меньше `fft_size` (без перекрытия), при этом
/// такой, чтобы столбцов было ≤ `max_frames`. `columns` = ceil(total/hop).
pub fn adaptive_plan(total_frames: u64, fft_size: u32, max_frames: u32) -> (usize, usize) {
    let fft = fft_size.max(1) as usize;
    let cap = max_frames.clamp(512, MAX_PIX_W as u32) as u64;
    if total_frames == 0 {
        return (fft, 0);
    }
    if total_frames <= fft as u64 {
        return (fft, 1);
    }
    let hop = total_frames.div_ceil(cap).max(fft as u64) as usize;
    let columns = total_frames.div_ceil(hop as u64) as usize;
    (hop, columns.max(1))
}

// ---------------------------------------------------------------------------
// Маппинг пиксельного ряда → частота
// ---------------------------------------------------------------------------

/// Строка в канале: 0 = верх (максимум частоты), rows-1 = низ (минимум).
/// Возвращает частоту в Гц для данной строки.
pub fn freq_for_row(y_local: usize, rows: usize, cfg: &SpectrogramCfg, sample_rate: u32) -> f32 {
    if rows <= 1 {
        return cfg.freq_min.max(1) as f32;
    }
    let t = (rows - 1 - y_local.min(rows - 1)) as f32 / (rows - 1) as f32;
    let fmin = cfg.freq_min.max(1) as f32;
    let fmax = cfg.freq_max.max(cfg.freq_min.max(1)) as f32;
    match cfg.freq_scale {
        FreqScale::Linear => fmin + (fmax - fmin) * t,
        FreqScale::Log => fmin * (fmax / fmin).powf(t),
        FreqScale::Mel => {
            let mel = |f: f32| 2595.0 * (1.0 + f / 700.0).log10();
            let m0 = mel(fmin);
            let m1 = mel(fmax);
            let m = m0 + (m1 - m0) * t;
            700.0 * (10f32.powf(m / 2595.0) - 1.0)
        }
    }
    .min(sample_rate as f32 * 0.499)
}

// ---------------------------------------------------------------------------
// Уровень (дБ → 0..1) и CIC
// ---------------------------------------------------------------------------

/// Компенсация спада CIC для DSD (дБ, ≥ 0).
///
/// DsdDecoder (§6.3) — две каскадные 4-й порядок CIC, децимация ×8 каждая
/// (вход dsd_rate, выход `sample_rate` = dsd_rate/64). Нормированная к DC
/// АЧХ (на частоте f, отсчёт от выхода):
///   H1 = ( sin(πf/8Fs) / (8 sin(πf/64Fs)) )^4
///   H2 = ( sin(πf/Fs) / (8 sin(πf/8Fs)) )^4
pub fn cic_compensation_db(f: f32, sample_rate: f32) -> f32 {
    let fs = sample_rate.max(1.0);
    let f = f.clamp(1.0, fs * 0.5);
    let p = std::f32::consts::PI * f;
    let h1 = (p / (8.0 * fs)).sin() / (8.0 * (p / (64.0 * fs)).sin());
    let h2 = (p / fs).sin() / (8.0 * (p / (8.0 * fs)).sin());
    let droop_db = (h1.powf(4.0) * h2.powf(4.0)).max(1e-9).log10() * 20.0;
    (-droop_db).clamp(0.0, CIC_COMP_MAX_DB)
}

/// Уровень энергии bin: magnitude → 0..1 (0 — тишина, 1 — максимум шкалы).
///
/// Заменён в `compute_column` на предвычисленный `RowPlan` (CIC-усиление и
/// high-boost считаются на строку, а не на пиксель); оставлен как публичный
/// помощник для тестов/внешних вычислений.
pub fn level_of(mag_lin: f32, f: f32, cfg: &SpectrogramCfg, cic_db: f32) -> f32 {
    let mut m = mag_lin.max(1e-9) * cfg.sensitivity;
    if cic_db > 0.0 {
        m *= 10f32.powf(cic_db / 20.0);
    }
    let mut db = 20.0 * (m + 1e-12).log10() + cfg.gain_db;
    if f > 0.0 {
        let fmin = (cfg.freq_min.max(1)) as f32;
        if f > fmin {
            db += cfg.high_boost_db * (f / fmin).log10();
        }
    }
    ((db + cfg.range_db) / cfg.range_db).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Палитры
// ---------------------------------------------------------------------------

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let tt = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, tt, p),
        1 => (q, v, p),
        2 => (p, v, tt),
        3 => (p, q, v),
        4 => (tt, p, v),
        _ => (v, p, q),
    };
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}

/// Цвет по уровню `t` ∈ [0,1] для конкретной палитры.
pub fn palette_rgb(t: f32, p: Palette, solid_rgb: [u8; 3]) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    match p {
        Palette::Solid => solid_rgb,
        Palette::Gray => {
            let v = (t * 255.0) as u8;
            [v, v, v]
        }
        Palette::Thermal => {
            // "hot": чёрный → красный → жёлтый → белый.
            let f = (t * 3.0).clamp(0.0, 3.0);
            let r = (255.0 * (f.clamp(0.0, 1.0) + (f - 1.0).clamp(0.0, 1.0))) as u8;
            let g = (255.0 * (f - 1.0).clamp(0.0, 1.0)) as u8;
            let b = (255.0 * (f - 2.0).clamp(0.0, 1.0)) as u8;
            [r, g, b]
        }
        Palette::Rainbow => {
            // Спектр: холодные (синий) на низком уровне, тёплые (красный) — на высоком.
            hsv_to_rgb(0.67 * (1.0 - t), 0.9, 0.3 + 0.7 * t)
        }
        _ => {
            let hex = palettes::hex_for(p);
            let lut = palettes::parse_hex(hex);
            let idx = ((t * 255.0).round() as usize).min(255);
            lut[idx]
        }
    }
}

// ---------------------------------------------------------------------------
// Стриминговый рендер
// ---------------------------------------------------------------------------

/// Предвычисленные константы строки спектрограммы, инвариантные к колонке:
/// интерполяция magnitude по бинам, усиление компенсации CIC и high-boost.
#[derive(Clone, Copy)]
struct RowPlan {
    b0: usize,
    frac: f32,
    b1: usize,
    cic_gain: f32,
    boost_db: f32,
}

/// Построитель спектрограммы: получает PCM пачками, рисует колонки на лету.
pub struct Spectrogram {
    cfg: SpectrogramCfg,
    dst_ch: usize,
    hop: usize,
    columns: usize,
    fft_size: usize,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    /// Буферы на канал (после `trim_base`).
    bufs: Vec<Vec<f32>>,
    /// Абсолютный кадровый индекс начала буфера (`bufs[c][0]`).
    trim_base: u64,
    frames_done: u64,
    total: u64,
    emitted: usize,
    img: Vec<u8>,
    /// Строк на канал.
    rows_per_ch: usize,
    /// Предвычисленные на строку константы (bin-интерполяция, CIC, boost).
    row_plan: Vec<RowPlan>,
    /// LUT палитры 0..=255 для палитр с hex-LUT (Magma/Viridis/Plasma/Inferno);
    /// `None` для формульных (Solid/Gray/Thermal/Rainbow) — там нет парсинга.
    lut: Option<Vec<[u8; 3]>>,
    /// Фон/цвет полосы (парсится один раз, не на каждую колонку).
    bg_rgb: [u8; 3],
    solid_rgb: [u8; 3],
    /// Переиспользуемые буферы (не аллоцируются на каждую колонку×канал).
    wbuf: Vec<Complex<f32>>,
    mags: Vec<f32>,
}

impl Spectrogram {
    pub fn new(
        total_frames: u64,
        sample_rate: u32,
        dst_ch: usize,
        is_dsd: bool,
        cfg: &SpectrogramCfg,
    ) -> Result<Self, String> {
        let fraw = cfg.fft_size.clamp(16, 8192);
        let fft_size = fraw.next_power_of_two().clamp(512, 8192) as usize;
        let (hop, columns) = adaptive_plan(total_frames, cfg.fft_size, cfg.max_frames);
        if columns == 0 {
            return Err("пустой трек (0 кадров)".into());
        }
        let dst_ch = dst_ch.max(1);
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);
        let window = window_coeffs(fft_size, cfg.window_type);
        let rows_per_ch = IMG_H / if dst_ch >= 2 { 2 } else { 1 };
        let img = vec![0u8; columns.min(MAX_PIX_W) * IMG_H * 4];
        let bg = crate::audio::fulltrack::parse_color(&cfg.bg_color);
        let fg = crate::audio::fulltrack::parse_color(&cfg.fg_color);
        let solid_rgb = [fg[0], fg[1], fg[2]];
        let bg_rgb = [bg[0], bg[1], bg[2]];
        // RowPlan: всё, что зависит только от строки (bin-интерполяция, CIC,
        // high-boost), вычисляем один раз на весь трек, а не на пиксель.
        let half = fft_size / 2;
        let fmin = cfg.freq_min.max(1) as f32;
        let cic_on = is_dsd && cfg.dsd_cic_compensation;
        let row_plan = (0..rows_per_ch)
            .map(|y| {
                let f = freq_for_row(y, rows_per_ch, cfg, sample_rate);
                let pin = f * fft_size as f32 / sample_rate.max(1) as f32;
                let bin = pin.min(half as f32 - 1e-3);
                let b0 = bin.floor() as usize;
                let frac = bin - b0 as f32;
                let b1 = (b0 + 1).min(half);
                let cic_db = if cic_on {
                    cic_compensation_db(f, sample_rate as f32)
                } else {
                    0.0
                };
                let cic_gain = if cic_db > 0.0 {
                    10f32.powf(cic_db / 20.0)
                } else {
                    1.0
                };
                let boost_db = if f > fmin {
                    cfg.high_boost_db * (f / fmin).log10()
                } else {
                    0.0
                };
                RowPlan { b0, frac, b1, cic_gain, boost_db }
            })
            .collect();
        // LUT только для hex-палитр (magma/viridis/plasma/inferno): парсинг
        // 1536-байтной строки на пиксель заменяем O(1)-индексом.
        let lut = match cfg.palette {
            Palette::Solid | Palette::Gray | Palette::Thermal | Palette::Rainbow => None,
            _ => Some(
                (0..=255)
                    .map(|i| palette_rgb(i as f32 / 255.0, cfg.palette, solid_rgb))
                    .collect::<Vec<_>>(),
            ),
        };
        Ok(Spectrogram {
            cfg: cfg.clone(),
            dst_ch,
            hop,
            columns,
            fft_size,
            window,
            fft,
            bufs: vec![Vec::with_capacity(fft_size + hop); dst_ch],
            trim_base: 0,
            frames_done: 0,
            total: total_frames,
            emitted: 0,
            img,
            rows_per_ch,
            row_plan,
            lut,
            bg_rgb,
            solid_rgb,
            wbuf: vec![Complex::default(); fft_size],
            mags: vec![0.0; fft_size / 2 + 1],
        })
    }

    /// Количество колонок (ширина изображения).
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Скормить interleaved PCM (`samples.len() / src_ch` кадров).
    pub fn feed(&mut self, samples: &[f32], src_ch: usize) {
        let frames = samples.len() / src_ch.max(1);
        for c in 0..self.dst_ch {
            let buf = &mut self.bufs[c];
            if c < src_ch {
                buf.extend((0..frames).map(|f| samples[f * src_ch + c]));
            } else if src_ch > 0 {
                buf.extend((0..frames).map(|f| samples[f * src_ch]));
            } else {
                buf.extend(std::iter::repeat_n(0.0, frames));
            }
        }
        self.frames_done += frames as u64;
        self.emit_ready();
    }

    pub fn progress(&self) -> f32 {
        if self.total == 0 {
            1.0
        } else {
            (self.frames_done as f64 / self.total as f64).min(1.0) as f32
        }
    }

    /// Эмитирует все колонки, для которых уже есть данные.
    fn emit_ready(&mut self) {
        while self.emitted < self.columns {
            let col = self.emitted;
            let window_end = col as u64 * self.hop as u64 + self.fft_size as u64;
            if (self.trim_base + self.bufs[0].len() as u64) < window_end {
                break;
            }
            self.compute_column(col);
            self.trim_after(col);
            self.emitted += 1;
        }
    }

    /// Завершить рендер всех оставшихся колонок (хвост дополняется нулями).
    pub fn finish(mut self) -> Vec<u8> {
        while self.emitted < self.columns {
            self.compute_column(self.emitted);
            self.emitted += 1;
        }
        self.img
    }

    fn trim_after(&mut self, col: usize) {
        let keep_from = (col as u64 + 1) * self.hop as u64;
        if keep_from <= self.trim_base {
            return;
        }
        let amount = (keep_from - self.trim_base) as usize;
        for b in &mut self.bufs {
            if amount < b.len() {
                b.drain(0..amount);
            } else {
                b.clear();
            }
        }
        self.trim_base = keep_from;
    }

    /// Рисует одну колонку во всех каналах.
    fn compute_column(&mut self, col: usize) {
        let w = self.columns.min(MAX_PIX_W);
        let half = self.fft_size / 2;

        for c in 0..self.dst_ch {
            for k in 0..self.fft_size {
                let abs = col as u64 * self.hop as u64 + k as u64;
                let rel = (abs - self.trim_base) as usize;
                let s = if rel < self.bufs[c].len() {
                    self.bufs[c][rel]
                } else {
                    0.0
                };
                self.wbuf[k] = Complex {
                    re: s * self.window[k],
                    im: 0.0,
                };
            }
            self.fft.process(&mut self.wbuf);

            for i in 0..=half {
                let re = self.wbuf[i].re;
                let im = self.wbuf[i].im;
                self.mags[i] = (re * re + im * im).sqrt();
            }

            let y_base = c * self.rows_per_ch;
            for y in 0..self.rows_per_ch {
                let rp = self.row_plan[y];
                let mag = self.mags[rp.b0] * (1.0 - rp.frac) + self.mags[rp.b1] * rp.frac;
                let mut m = (mag / self.fft_size as f32).max(1e-9) * self.cfg.sensitivity;
                m *= rp.cic_gain;
                let db = 20.0 * (m + 1e-12).log10() + self.cfg.gain_db + rp.boost_db;
                let level = ((db + self.cfg.range_db) / self.cfg.range_db).clamp(0.0, 1.0);
                let rgb = if level < 0.001 {
                    self.bg_rgb
                } else {
                    match &self.lut {
                        Some(lut) => lut[((level * 255.0).round() as usize).min(255)],
                        None => palette_rgb(level, self.cfg.palette, self.solid_rgb),
                    }
                };
                if col < w {
                    let off = ((y_base + y) * w + col) * 4;
                    self.img[off..off + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SpectrogramCfg {
        SpectrogramCfg {
            fft_size: 512,
            max_frames: 1000,
            channels: crate::audio::visualizer::ChannelMode::Mono,
            ..SpectrogramCfg::default()
        }
    }

    fn tone(freq_hz: f32, fs: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| 0.8 * (2.0 * std::f32::consts::PI * freq_hz * i as f32 / fs as f32).sin())
            .collect()
    }

    #[test]
    fn adaptive_plan_respects_caps() {
        let (hop, cols) = adaptive_plan(100_000, 512, 1000);
        assert!(cols <= 1000);
        assert!(hop >= 512);
        assert_eq!(100_000_u64.div_ceil(hop as u64), cols as u64);
        let (hop2, cols2) = adaptive_plan(2000, 512, 1000);
        assert_eq!(hop2, 512);
        assert_eq!(cols2, 4);
        assert_eq!(adaptive_plan(0, 512, 1000), (512, 0));
        let (_h, c1) = adaptive_plan(1, 512, 1000);
        assert_eq!(c1, 1);
    }

    #[test]
    fn window_coeffs_sane() {
        let h = window_coeffs(64, WindowType::Hann);
        assert!(h.iter().all(|&v| (0.0..=1.0).contains(&v)));
        let sum: f32 = h.iter().sum();
        assert!((sum - 32.0).abs() < 1.0, "hann sum {sum}");
        assert_eq!(window_coeffs(4, WindowType::Hann)[0], 0.0);
        let bl = window_coeffs(32, WindowType::Blackman);
        assert!(bl[0].abs() < 1e-4);
    }

    #[test]
    fn freq_row_monotonic_edges() {
        let mut c = cfg();
        c.freq_scale = FreqScale::Linear;
        c.freq_min = 20;
        c.freq_max = 2000;
        let rows = 256;
        assert!((freq_for_row(rows - 1, rows, &c, 8000) - 20.0).abs() < 0.01);
        assert!((freq_for_row(0, rows, &c, 8000) - 2000.0).abs() < 0.01);
        let mut prev = 0.0f32;
        for y in (0..rows).rev() {
            let f = freq_for_row(y, rows, &c, 8000);
            assert!(f >= prev - 1e-3);
            prev = f;
        }
        // Лог-шкала: верхние ряды уже, нижние реже.
        c.freq_scale = FreqScale::Log;
        let f_lo = freq_for_row(rows - 20, rows, &c, 8000);
        let f_hi = freq_for_row(rows - 21, rows, &c, 8000);
        assert!(f_lo < f_hi);
        // Мел — просто валидное значение между границами.
        c.freq_scale = FreqScale::Mel;
        let fm = freq_for_row(rows / 2, rows, &c, 8000);
        assert!((20.0..=2000.0).contains(&fm));
    }

    #[test]
    fn cic_compensation_behavior() {
        let fs = 44100.0;
        assert!(cic_compensation_db(1.0, fs).abs() < 1.0);
        let at_nyq = cic_compensation_db(fs * 0.5, fs);
        assert!(at_nyq > 5.0, "droop at nyquist {at_nyq}");
        assert!(at_nyq <= CIC_COMP_MAX_DB + 0.01);
        let mid = cic_compensation_db(fs * 0.25, fs);
        assert!(mid > cic_compensation_db(fs * 0.05, fs));
    }

    #[test]
    fn palette_lut_data_loaded() {
        let lut = palettes::parse_hex(palettes::hex_for(Palette::Magma));
        assert_eq!(lut.len(), 256);
        assert!(lut[0] == [0, 0, 4]);
        assert!(lut[255][0] > 200);
        // Solid не зависит от уровня.
        assert_eq!(palette_rgb(0.5, Palette::Solid, [1, 2, 3]), [1, 2, 3]);
        // Gray монотонно растёт.
        let (a, b) = (palette_rgb(0.2, Palette::Gray, [0; 3]), palette_rgb(0.8, Palette::Gray, [0; 3]));
        assert!(b[0] > a[0]);
    }

    #[test]
    fn spectrogram_sine_row_energy() {
        let fs = 8000u32;
        let mut c = cfg();
        c.fft_size = 512;
        c.freq_min = 50;
        c.freq_max = 3900;
        c.freq_scale = FreqScale::Log;
        c.palette = Palette::Gray;
        c.gain_db = 30.0;
        c.range_db = 60.0;
        c.max_frames = 400;
        let frames = fs as usize * 2; // 2 с
        let sg_total = frames as u64;
        let mut sg = Spectrogram::new(sg_total, fs, 1, false, &c).unwrap();
        sg.feed(&tone(1000.0, fs, frames), 1);
        let cols = sg.columns().min(MAX_PIX_W);
        let img = sg.finish();
        assert_eq!(img.len(), cols * IMG_H * 4);

        let rows = IMG_H;
        let get = |x: usize, y: usize| img[(y * cols + x) * 4]; // серый канал

        // Ряд для 1000 Гц (log-шкала) — яркий.
        let t1000 = (1000.0f32 / 50.0).log(2.0) / (3900.0f32 / 50.0).log(2.0);
        let y1000 = rows - 1 - (t1000 * (rows - 1) as f32).round() as usize;
        let t2k = (2000.0f32 / 50.0).log(2.0) / (3900.0f32 / 50.0).log(2.0);
        let y2k = rows - 1 - (t2k * (rows - 1) as f32).round() as usize;
        let energy = |y: usize| -> u32 {
            (0..cols).map(|x| get(x, y) as u32 * 4).sum::<u32>() / cols as u32
        };
        let e1 = energy(y1000);
        let e2 = energy(y2k);
        assert!(e1 > 40, "row@1k energy {e1}");
        assert!(e1 > e2 * 3, "{e1} vs {e2}");
    }

    #[test]
    fn spectrogram_stereo_separates() {
        let fs = 8000u32;
        let mut c = cfg();
        c.fft_size = 512;
        c.freq_min = 50;
        c.freq_max = 3900;
        c.freq_scale = FreqScale::Log;
        c.palette = Palette::Gray;
        c.gain_db = 30.0;
        c.max_frames = 200;
        let frames = fs as usize; // 1 с
        let l = tone(1000.0, fs, frames);
        let r = tone(2000.0, fs, frames);
        let mut inter: Vec<f32> = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            inter.push(l[i]);
            inter.push(r[i]);
        }
let mut sg = Spectrogram::new(frames as u64, fs, 2, false, &c).unwrap();
        sg.feed(&inter, 2);
        let cols = sg.columns().min(MAX_PIX_W);
        let img = sg.finish();
        let rows = IMG_H / 2;

        let t1000 = (1000.0f32 / 50.0).log(2.0) / (3900.0f32 / 50.0).log(2.0);
        let y1000 = rows - 1 - (t1000 * (rows - 1) as f32).round() as usize;
        let t2k = (2000.0f32 / 50.0).log(2.0) / (3900.0f32 / 50.0).log(2.0);
        let y2k = rows - 1 - (t2k * (rows - 1) as f32).round() as usize;
        let get = |x: usize, y: usize| img[(y * cols + x) * 4];
        let energy = |y0: usize, y1: usize| -> u32 {
            let sum: u32 = (y0..y1).map(|y| (0..cols).map(|x| get(x, y) as u32).sum::<u32>()).sum();
            sum / ((y1 - y0) as u32)
        };
        // Верхняя половина (L=1000 Гц): ряд 1000 ярче ряда 2000.
        let e_l1 = energy(y1000, y1000 + 1);
        let e_l2 = energy(y2k, y2k + 1);
        assert!(e_l1 > e_l2 * 3, "L 1k {e_l1} vs 2k {e_l2}");
        // Нижняя половина (R=2000 Гц): ряд 2000 ярче.
        let e_r1 = energy(rows + y1000, rows + y1000 + 1);
        let e_r2 = energy(rows + y2k, rows + y2k + 1);
        assert!(e_r2 > e_r1 * 3, "R 2k {e_r2} vs 1k {e_r1}");
    }
}