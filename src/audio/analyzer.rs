//! Live-анализатор спектра (ТЗ 5.1, этап 6.2).
//!
//! `SpectrumEngine` — чистый DSP (тестируемый без аудио): «лестничный»
//! консьюмер из rtrb-кольца, оконный FFT (rustfft) с 50% overlap, маппинг бинов
//! на полосы (linear/log/mel), сглаживание и peak hold.
//!
//! `LiveWorker` — поток-обёртка: читает настройки атомарно на границе кадра
//! (`RwLock<Arc<VisualizerConfig>>`), публикует свежие полосы в `RwLock<Vec<f32>>`
//! (канал без аллокаций через `mem::swap`), UI забирает их таймером ~33 мс.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::audio::visualizer::{
    ChannelMode, FreqScale, LevelScale, SpectrumCfg, VisualizerConfig, VisualizationMode,
};

/// Размер tap-буфера из каркаса (≈1.3 с стерео @96 кГц). f32 при 262144 = 1 МБ.
pub const TAP_CAPACITY: usize = 262_144;
/// Окно БПФ анализатора (ТЗ §11: 2048 при 32 полосах).
pub const SPECTRUM_FFT_SIZE: usize = 2048;
/// Шаг окна (50% overlap) — даёт ≥30 кадров/с обновления полос на 44.1+ кГц.
const SPECTRUM_HOP: usize = SPECTRUM_FFT_SIZE / 2;
/// Нижняя граница диапазона полос, Гц.
const BAND_MIN_HZ: f32 = 20.0;
/// Диапазон отображения уровней: 0 dBFS … −80 dB.
const LEVEL_RANGE_DB: f32 = 80.0;

/// Idle-sleep потоков воркера (нет данных / не тот режим).
const SLEEP_IDLE_MS: u64 = 2;
const SLEEP_DRAIN_MS: u64 = 50;

/// Периодическое окно Ханна суммы ≈ N/2 (периодическая форма, aliasing-free).
fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
        .collect()
}

fn mel(f: f32) -> f32 {
    2595.0 * (1.0 + f / 700.0).log10()
}
fn mel_inv(m: f32) -> f32 {
    700.0 * (10f32.powf(m / 2595.0) - 1.0)
}

/// Частотная сетка полос на нормированной позиции `t ∈ [0, 1]`.
fn band_freq(scale: FreqScale, t: f32, lo: f32, hi: f32) -> f32 {
    match scale {
        FreqScale::Linear => lo + t * (hi - lo),
        FreqScale::Log => lo * (hi / lo).powf(t),
        FreqScale::Mel => mel_inv(mel(lo) + t * (mel(hi) - mel(lo))),
    }
}

/// Чистый DSP-движок полос анализатора спектра.
///
/// Создаётся один раз под структурную конфигурацию (rate, число каналов в
/// кольце, число полос, режим каналов); сортуальные параметры (smoothing,
/// peak_hold, sensitivity…) читаются на границе кадра через `&SpectrumCfg`.
pub struct SpectrumEngine {
    /// Каналов в tap-кольце (out_ch устройства).
    in_ch: usize,
    /// Обрабатываемых каналов (1..=2, из `cfg.channels`).
    proc_ch: usize,
    fft_size: usize,
    hop: usize,
    bands: usize,
    /// Interleaved-окно последних `fft_size` кадров (`window[c + i*proc_ch]`).
    window: Vec<f32>,
    /// Счётчик кадров, накопленных после последнего сдвига (0..hop).
    hop_fill: usize,
    /// Временный входной кадр (in_ch сэмплов).
    frame_buf: Vec<f32>,
    /// Сколько сэмплов в `frame_buf` уже собрано (0..in_ch]. Частичное чтение
    /// кадра сохраняется между вызовами `pull_frame`, чтобы кадр не смешивал
    /// сэмплы из разных временных интервалов.
    frame_fill: usize,
    window_fn: Vec<f32>,
    ffts: Vec<Arc<dyn Fft<f32>>>,
    work: Vec<Complex<f32>>,
    /// Для каждой полосы — диапазон бинов `[start, end)` (бина 0 нет).
    band_ranges: Vec<(usize, usize)>,
    /// Аккумулятор мощностей по полосам (переиспользуется на кадре).
    band_power: Vec<f32>,
    /// Предыдущий сглаженный кадр (bands*proc_ch).
    smoothed: Vec<f32>,
    /// Состояние peak hold (bands*proc_ch).
    peaks: Vec<f32>,
    /// Множитель спада пика за кадр (из peak_decay_ms и реальной fps).
    decay_per_frame: f32,
}

impl SpectrumEngine {
    pub fn new(cfg: &SpectrumCfg, rate_hz: u32, in_ch: usize) -> Self {
        let fft_size = SPECTRUM_FFT_SIZE;
        let hop = SPECTRUM_HOP;
        let bands = cfg.bands.clamp(4, 128) as usize;
        let proc_ch = match cfg.channels {
            ChannelMode::Mono => 1,
            ChannelMode::Stereo => in_ch.clamp(1, 2),
        };
        let band_ranges = band_ranges(cfg.freq_scale, bands, rate_hz, fft_size);
        let n_bands = bands * proc_ch;
        let frames_per_sec = rate_hz.max(1) as f32 / hop as f32;
        let decay_pms = cfg.peak_decay_ms.clamp(50, 2000) as f32;
        let decay_per_frame = (-1000.0 / (decay_pms * frames_per_sec)).exp();
        let mut planner = FftPlanner::<f32>::new();
        let ffts = (0..proc_ch)
            .map(|_| planner.plan_fft_forward(fft_size))
            .collect::<Vec<_>>();
        Self {
            in_ch: in_ch.max(1),
            proc_ch,
            fft_size,
            hop,
            bands,
            window: vec![0.0; fft_size * proc_ch],
            hop_fill: 0,
            frame_buf: vec![0.0; in_ch.max(1)],
            frame_fill: 0,
            window_fn: hann(fft_size),
            ffts,
            work: vec![Complex::default(); fft_size],
            band_ranges,
            band_power: vec![0.0; bands],
            smoothed: vec![0.0; n_bands],
            peaks: vec![0.0; n_bands],
            decay_per_frame,
        }
    }

    /// Полос на канал.
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// Обрабатываемых каналов (1 | 2).
    pub fn channels(&self) -> usize {
        self.proc_ch
    }

    /// Попытаться накопить и обработать кадр. Возвращает `true`, если новый
    /// кадр полос записан в `out` (длина `bands * proc_ch`).
    ///
    /// Потребляет входные кадры (frame = in_ch сэмплов) маленькими порциями;
    /// при нехватке данных (Empty/Disconnected) останавливается с `false`.
    /// `cfg` — «снимок на границе кадра», используется только пока `consume`
    /// заполняет один окно.
    pub fn consume(&mut self, consumer: &mut rtrb::Consumer<f32>, cfg: &SpectrumCfg, out: &mut Vec<f32>) -> bool {
        let mut produced = false;
        loop {
            while self.hop_fill < self.hop {
                if self.pull_frame(consumer).is_none() {
                    return produced;
                }
            }
            self.compute_bands(cfg, out);
            self.shift_window();
            produced = true;
        }
    }

    /// Вытянуть один входной кадр (in_ch сэмплов), дозвудить до proc_ch и
    /// положить в хвост окна. `None`, если кольцо пусто/разорвано (частичное
    /// чтение кадра сохраняется в `frame_fill` и дополняется следующими
    /// вызовами — сэмплы одного кадра не смешиваются с другими интервалами).
    fn pull_frame(&mut self, consumer: &mut rtrb::Consumer<f32>) -> Option<()> {
        while self.frame_fill < self.in_ch {
            match consumer.pop() {
                Ok(s) => {
                    self.frame_buf[self.frame_fill] = s;
                    self.frame_fill += 1;
                }
                Err(_) => return None,
            }
        }
        let base = (self.fft_size - self.hop + self.hop_fill) * self.proc_ch;
        if self.proc_ch == 1 {
            let sum: f32 = self.frame_buf.iter().sum();
            self.window[base] = sum / self.in_ch as f32;
        } else {
            for c in 0..self.proc_ch {
                self.window[base + c] = self.frame_buf[c];
            }
        }
        self.hop_fill += 1;
        self.frame_fill = 0;
        Some(())
    }

    /// FFT по каждому каналу → мощности по полосам → уровни (0..1) →
    /// сглаживание → peak hold. Пишет `bands * proc_ch` значений в `out`.
    fn compute_bands(&mut self, cfg: &SpectrumCfg, out: &mut Vec<f32>) {
        out.clear();
        out.reserve(self.bands * self.proc_ch);
        let n = self.fft_size;
        for c in 0..self.proc_ch {
            for i in 0..n {
                let v = self.window[c + i * self.proc_ch] * self.window_fn[i];
                self.work[i] = Complex::new(v, 0.0);
            }
            self.ffts[c].process(&mut self.work);
            self.band_power.fill(0.0);
            for b in 0..self.bands {
                let (s, e) = self.band_ranges[b];
                let mut sum = 0.0f32;
                for k in s..e {
                    let z = self.work[k];
                    sum += z.re * z.re + z.im * z.im;
                }
                self.band_power[b] = sum;
            }
            let count_ref = n as f32 * 0.25;
            for b in 0..self.bands {
                let (s, e) = self.band_ranges[b];
                let count = (e - s).max(1) as f32;
                let mag = (self.band_power[b] / count).sqrt();
                let mag_norm = mag / count_ref;
                let level = match cfg.level_scale {
                    LevelScale::Log => {
                        let db = 20.0 * mag_norm.max(1e-7).log10();
                        (db + LEVEL_RANGE_DB) / LEVEL_RANGE_DB
                    }
                    LevelScale::Linear => mag_norm,
                };
                let val = (level * cfg.sensitivity).clamp(0.0, 1.0);
                let idx = c * self.bands + b;
                let cur = val * (1.0 - cfg.smoothing.clamp(0.0, 0.99)) + self.smoothed[idx] * cfg.smoothing.clamp(0.0, 0.99);
                if cfg.peak_hold {
                    let pk = (self.peaks[idx] * self.decay_per_frame).max(cur);
                    self.peaks[idx] = pk;
                    self.smoothed[idx] = pk;
                    out.push(pk.clamp(0.0, 1.0));
                } else {
                    self.smoothed[idx] = cur;
                    out.push(cur);
                }
            }
        }
    }

    /// Сдвинуть окно на `hop` кадров (после обработки): `fft_size - hop`
    /// старых остаются, освободившееся место под новые.
    fn shift_window(&mut self) {
        let keep = self.fft_size - self.hop;
        for i in 0..keep {
            for c in 0..self.proc_ch {
                self.window[c + i * self.proc_ch] = self.window[c + (i + self.hop) * self.proc_ch];
            }
        }
        self.hop_fill = 0;
    }
}

/// Разбить бины 1..half на полосы по частотной сетке `scale`.
fn band_ranges(scale: FreqScale, bands: usize, rate_hz: u32, fft_size: usize) -> Vec<(usize, usize)> {
    let half = fft_size / 2;
    let nyq = rate_hz.max(1) as f32 / 2.0;
    let lo = BAND_MIN_HZ;
    let hi = nyq.min(22_000.0);
    let hi = if hi > lo { hi } else { lo * 1.5 };
    let mut edges = Vec::with_capacity(bands + 1);
    for b in 0..=bands {
        edges.push(band_freq(scale, b as f32 / bands as f32, lo, hi));
    }
    let mut ranges = vec![(0usize, 0usize); bands];
    for i in 1..half {
        let f = i as f32 * rate_hz.max(1) as f32 / fft_size as f32;
        let base = edges.partition_point(|&e| e <= f);
        let b = if base == 0 { 0 } else { (base - 1).min(bands - 1) };
        let e = &mut ranges[b];
        if e.0 == e.1 {
            e.0 = i;
        }
        e.1 = i + 1;
    }
    ranges
}

/// Поток-обёртка над `SpectrumEngine`: читает tap-кольцо, считает FFT и
/// публикует последний кадр полос. UI забирает полосы через [`Self::bars`].
pub struct LiveWorker {
    running: Arc<AtomicBool>,
    cfg: Arc<RwLock<Arc<VisualizerConfig>>>,
    rate: Arc<AtomicU32>,
    in_ch: Arc<AtomicUsize>,
    bars: Arc<RwLock<Vec<f32>>>,
    handle: Option<JoinHandle<()>>,
}

impl LiveWorker {
    /// Запустить воркер в отдельном потоке над уже созданным consumer'ом
    /// (producer'а привязывает к `Player` отдельно через `set_viz_tap`).
    pub fn start(consumer: rtrb::Consumer<f32>, cfg: Arc<VisualizerConfig>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let cfg_rc = Arc::new(RwLock::new(cfg));
        let rate = Arc::new(AtomicU32::new(44_100));
        let in_ch = Arc::new(AtomicUsize::new(2));
        let bars = Arc::new(RwLock::new(Vec::new()));

        let run = running.clone();
        let cfg_by = cfg_rc.clone();
        let rate_by = rate.clone();
        let in_ch_by = in_ch.clone();
        let bars_by = bars.clone();

        let handle = thread::spawn(move || {
            let mut consumer = consumer;
            let mut scratch: Vec<f32> = Vec::new();
            let mut engine: Option<SpectrumEngine> = None;
            let mut last_key: Option<(u32, usize, u32, ChannelMode, FreqScale, u32)> = None;
            while run.load(Ordering::Relaxed) {
                let cfg = cfg_by
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_else(|p| p.into_inner().clone());
                let rate_now = rate_by.load(Ordering::Relaxed).max(1);
                let ch_now = in_ch_by.load(Ordering::Relaxed).max(1);
                if cfg.mode != VisualizationMode::Spectrum {
                    // Неанализ: слить остатки (только в этом потоке) и не считаем.
                    while consumer.pop().is_ok() {}
                    thread::sleep(std::time::Duration::from_millis(SLEEP_DRAIN_MS));
                    continue;
                }
                let sp = &cfg.spectrum;
                let key = (rate_now, ch_now, sp.bands, sp.channels, sp.freq_scale, sp.peak_decay_ms);
                if last_key != Some(key) {
                    engine = Some(SpectrumEngine::new(sp, rate_now, ch_now));
                    last_key = Some(key);
                }
                let produced = engine
                    .as_mut()
                    .map(|eng| eng.consume(&mut consumer, sp, &mut scratch))
                    .unwrap_or(false);
                if produced {
                    // Обмен без аллокаций: в `bars` — свежий кадр, в scratch — старый.
                    if let Ok(mut guard) = bars_by.write() {
                        std::mem::swap(&mut *guard, &mut scratch);
                    }
                    continue;
                }
                thread::sleep(std::time::Duration::from_millis(SLEEP_IDLE_MS));
            }
        });

        Self {
            running,
            cfg: cfg_rc,
            rate,
            in_ch,
            bars,
            handle: Some(handle),
        }
    }

    /// Обновить снимок настроек (атомарно для воркера).
    pub fn set_cfg(&self, cfg: Arc<VisualizerConfig>) {
        if let Ok(mut g) = self.cfg.write() {
            *g = cfg;
        }
    }

    /// Обновить формат устройства (out_rate / out_ch).
    pub fn set_format(&self, rate: u32, ch: usize) {
        self.rate.store(rate.max(1), Ordering::Relaxed);
        self.in_ch.store(ch.max(1), Ordering::Relaxed);
    }

    /// Последний кадр полос (`bands * proc_ch`, значение каждой ∈ 0..=1).
    pub fn bars(&self) -> Vec<f32> {
        self.bars
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for LiveWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::visualizer::SpectrumCfg;

    fn cfg_of() -> SpectrumCfg {
        SpectrumCfg::default()
    }

    /// Залить `samples` в кольцо и прогнать `n_frames` кадров.
    fn run_engine(
        samples: &[f32],
        in_ch: usize,
        cfg: &SpectrumCfg,
        rate: u32,
    ) -> Vec<f32> {
        let cap = samples.len().next_power_of_two().max(16);
        let (mut prod, mut cons) = rtrb::RingBuffer::new(cap);
        for &s in samples {
            let _ = prod.push(s);
        }
        let mut out = Vec::new();
        let mut eng = SpectrumEngine::new(cfg, rate, in_ch);
        for _ in 0..4 {
            eng.consume(&mut cons, cfg, &mut out);
        }
        out
    }

    fn sine(rate: u32, freq: f32, frames: usize, phase: f32) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                (2.0 * std::f32::consts::PI * freq / rate as f32 * i as f32 + phase).sin()
            })
            .collect()
    }

    #[test]
    fn silence_produces_zero_bars() {
        let cfg = cfg_of();
        let out = run_engine(&vec![0.0; 8192], 1, &cfg, 44_100);
        assert_eq!(out.len(), cfg.bands as usize);
        assert!(out.iter().all(|v| *v <= 1e-3));
    }

    #[test]
    fn sine_peaks_expected_band_region() {
        // 1 кГц моно: максимум должен быть в нижней-средней части полос
        // (log-шкала 20 Гц…22 кГц: 1 кГц ≈ 43% слева → полоса ~13-15 из 32).
        let cfg = cfg_of();
        let rate = 44_100u32;
        let samples = sine(rate, 1000.0, 16 * SPECTRUM_HOP, 0.0);
        let out = run_engine(&samples, 1, &cfg, rate);
        let mut max_i = 0;
        for (i, &v) in out.iter().enumerate() {
            if v > out[max_i] {
                max_i = i;
            }
        }
        assert!(out[max_i] > 0.7, "peak level {}", out[max_i]);
        assert!((8..20).contains(&max_i), "peak band {max_i}");
    }

    #[test]
    fn stereo_rows_split_l_and_r() {
        let mut cfg = cfg_of();
        cfg.channels = ChannelMode::Stereo;
        let rate = 44_100u32;
        let hop = SPECTRUM_HOP;
        let n = 16 * hop;
        // L — низкий тон (200 Гц), R — высокий (8 кГц). Interleaved.
        let l = sine(rate, 200.0, n, 0.0);
        let r = sine(rate, 8000.0, n, 0.3);
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            samples.push(l[i]);
            samples.push(r[i]);
        }
        cfg.smoothing = 0.0;
        let out = run_engine(&samples, 2, &cfg, rate);
        assert_eq!(out.len(), cfg.bands as usize * 2);
        let bands = cfg.bands as usize;
        let (lrow, rrow) = out.split_at(bands);
        let argmax = |row: &[f32]| row.iter().copied().enumerate().max_by(|a, b| a.1.total_cmp(&b.1)).map(|(i, _)| i).unwrap();
        let il = argmax(lrow);
        let ir = argmax(rrow);
        assert!(out[il] > 0.5);
        assert!(out[bands + ir] > 0.5);
        assert!(il < ir, "L band {il} should be below R band {ir}");
    }

    #[test]
    fn mono_averages_stereo_channels() {
        let mut cfg = cfg_of();
        cfg.channels = ChannelMode::Mono;
        let rate = 44_100u32;
        let hop = SPECTRUM_HOP;
        let n = 16 * hop;
        let a = sine(rate, 440.0, n, 0.0);
        let mut samples = Vec::with_capacity(n * 2);
        // L и R одинаковые → моно-усреднение (разницы нет); просто проверяем длину.
        for i in 0..n {
            samples.push(a[i]);
            samples.push(a[i]);
        }
        let out = run_engine(&samples, 2, &cfg, rate);
        assert_eq!(out.len(), cfg.bands as usize);
    }

    #[test]
    fn smoothing_reduces_first_frame() {
        let rate = 44_100u32;
        let samples = sine(rate, 1000.0, 16 * SPECTRUM_HOP, 0.0);
        let mut raw = cfg_of();
        raw.smoothing = 0.0;
        let mut sm = cfg_of();
        sm.smoothing = 0.9;
        let out_raw = run_engine(&samples, 1, &raw, rate);
        let out_sm = run_engine(&samples, 1, &sm, rate);
        // Первый кадр после старта: prev=0 → smoothed = val*(1-sm) (несколько кадров ещё мало);
        // сумма по всем полосам у сглаженного кадра меньше суммы у сырого.
        let s_raw: f32 = out_raw.iter().sum();
        let s_sm: f32 = out_sm.iter().sum();
        assert!(s_sm < s_raw);
    }

    #[test]
    fn peak_hold_holds_peaks() {
        let rate = 44_100u32;
        let samples = sine(rate, 1000.0, 16 * SPECTRUM_HOP, 0.0);
        let mut cfg = cfg_of();
        cfg.peak_hold = true;
        cfg.peak_decay_ms = 2000; // медленный спад — пики остаются высокими
        cfg.smoothing = 0.0;
        let out = run_engine(&samples, 1, &cfg, rate);
        let max = out.iter().copied().fold(0.0f32, f32::max);
        assert!(max > 0.7);
    }

    #[test]
    fn band_ranges_cover_all_bins_without_gaps() {
        let ranges = band_ranges(FreqScale::Log, 32, 44_100, SPECTRUM_FFT_SIZE);
        let mut prev_end = 1; // бина 0 нет
        let mut bins = 0;
        for (s, e) in &ranges {
            if *e > 0 {
                assert_eq!(*s, prev_end, "no gaps/overlaps");
                prev_end = *e;
                bins += e - s;
            }
        }
        assert_eq!(bins, SPECTRUM_FFT_SIZE / 2 - 1, "all half-bins covered");
        assert_eq!(prev_end, SPECTRUM_FFT_SIZE / 2, "all half-bins covered");
    }

    #[test]
    fn linear_scale_edges_are_monotonic() {
        let n = 16;
        let lo = 20.0f32;
        let hi = 22000.0f32;
        for scale in [FreqScale::Log, FreqScale::Linear, FreqScale::Mel] {
            let edges: Vec<f32> = (0..=n).map(|b| band_freq(scale, b as f32 / n as f32, lo, hi)).collect();
            assert!(edges.windows(2).all(|w| w[0] < w[1]), "{scale:?} monotonic");
        }
    }

    #[test]
    fn worker_drains_and_swaps_bars() {
        // Интеграционный дым-тест: воркер читает заполненное кольцо и публикует ненулевые полосы.
        let cfg = Arc::new(VisualizerConfig::from_settings(&crate::settings::Settings::default()));
        let mut vcfg = (*cfg).clone();
        vcfg.mode = VisualizationMode::Spectrum;
        vcfg.spectrum.channels = ChannelMode::Mono;
        let cfg = Arc::new(vcfg);
        let (mut prod, cons) = rtrb::RingBuffer::new(TAP_CAPACITY);
        let worker = LiveWorker::start(cons, cfg.clone());
        let rate = 44_100u32;
        let samples = sine(rate, 1000.0, 4 * SPECTRUM_HOP + 64, 0.0);
        for &s in samples.iter() {
            let _ = prod.push(s);
        }
        worker.set_format(rate, 1);
        let mut bars = Vec::new();
        for _ in 0..200 {
            bars = worker.bars();
            if !bars.is_empty() && bars.iter().any(|v| *v > 0.1) {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(worker);
        drop(prod);
        assert!(!bars.is_empty());
        assert_eq!(bars.len(), cfg.spectrum.bands as usize);
        assert!(bars.iter().any(|v| *v > 0.1));
    }
}