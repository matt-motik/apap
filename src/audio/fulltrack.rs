//! Полнотрековая осциллограмма — min/max envelope + рендер RGBA + PNG-кэш.
//!
//! ТЗ §5.2 (настройки), §2.3 (поток), §10.4 (кэширование изображений),
//! §16.4 (задача).

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::audio::visualizer::OscilloscopeCfg;

// ---------------------------------------------------------------------------
// Envelope (минимум/максимум по колонкам)
// ---------------------------------------------------------------------------

/// Минимум и максимум одной колонки осциллограммы (на канал).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvelopeColumn {
    pub min: f32,
    pub max: f32,
}

impl EnvelopeColumn {
    pub fn empty() -> Self {
        Self { min: 0.0, max: 0.0 }
    }
}

/// Строит envelope трека из потока interleaved f32.
///
/// При каждом вызове `feed` данные распределяются по колонкам согласно
/// `columns` и known `total_frames`. Если `total_frames == 0`, используется
/// кумулятивный режим (после `finish` колонки масштабируются).
pub struct Envelope {
    columns: usize,
    channels: usize,
    total_frames: u64,
    ch_cols: Vec<Vec<EnvelopeColumn>>,
    /// Кадры, прочитанные на данный момент.
    frames_done: u64,
}

impl Envelope {
    pub fn new(total_frames: u64, channels: usize, columns: usize) -> Self {
        let columns = columns.max(1);
        let channels = channels.max(1);
        // Сентенел: колонка без данных остаётся с min>max и нормализуется
        // в `finish()` в `(0.0, 0.0)`. Позитивные/негативные волны корректны.
        let ch_cols = vec![
            vec![
                EnvelopeColumn {
                    min: f32::MAX,
                    max: f32::MIN,
                };
                columns
            ];
            channels
        ];
        Envelope {
            columns,
            channels,
            total_frames,
            ch_cols,
            frames_done: 0,
        }
    }

    /// Скормить interleaved PCM-кадры (`samples.len() / src_ch` кадров).
    /// Кадры, в которых больше каналов чем нужно, игнорируются; если
    /// каналов меньше — недостающие считаются тишиной.
    pub fn feed(&mut self, samples: &[f32], src_ch: usize) {
        if self.total_frames == 0 {
            return;
        }
        let frames = samples.len() / src_ch;
        for fr in 0..frames {
            let col = ((self.frames_done + fr as u64) * self.columns as u64 / self.total_frames)
                as usize;
            if col >= self.columns {
                continue;
            }
            for (c, ch) in self.ch_cols.iter_mut().enumerate().take(self.channels) {
                let v = if c < src_ch {
                    samples[fr * src_ch + c]
                } else if src_ch > 0 {
                    samples[fr * src_ch]
                } else {
                    0.0
                };
                let ec = &mut ch[col];
                if v < ec.min {
                    ec.min = v;
                } else if v > ec.max {
                    ec.max = v;
                }
            }
        }
        self.frames_done += frames as u64;
    }

    pub fn progress(&self) -> f32 {
        if self.total_frames == 0 {
            1.0
        } else {
            (self.frames_done as f64 / self.total_frames as f64).min(1.0) as f32
        }
    }

    /// Завершить сбор; вернуть envelope по каналам (channels × columns).
    /// Колонки, которые не получили данных, остаются `(0.0, 0.0)`.
    pub fn finish(self) -> Vec<Vec<EnvelopeColumn>> {
        self.ch_cols
            .into_iter()
            .map(|cols| {
                cols.into_iter()
                    .map(|c| if c.min <= c.max { c } else { EnvelopeColumn::empty() })
                    .collect()
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Рендер RGBA (без зависимости от Slint)
// ---------------------------------------------------------------------------

/// Парсит hex-строку `#rrggbb` в `[r,g,b,a]`.
pub fn parse_color(hex: &str) -> [u8; 4] {
    let h: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let h = h.as_str();
    if h.len() >= 6 {
        let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
        [r, g, b, 255]
    } else {
        [0, 0, 0, 255]
    }
}

/// Отрендерить осциллограмму в RGBA-буфер.
///
/// * `ch_cols` — envelope по каналам: `[L_column, L_column, …]`, `[R…]`.
/// * `stereo` — true: канал 0 в верхней половине, канал 1 — в нижней.
/// * `w` / `h` — размер изображения в пикселях (max 8192 × 512).
pub fn render_rgba(
    ch_cols: &[Vec<EnvelopeColumn>],
    stereo: bool,
    cfg: &OscilloscopeCfg,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let w = w.clamp(1, 8192);
    let h = h.clamp(1, 512);
    let mut buf = vec![0u8; w * h * 4];
    let bg = parse_color(&cfg.bg_color);
    let fg = parse_color(&cfg.fg_color);

    // Фон.
    for pixel in buf.chunks_mut(4) {
        pixel.copy_from_slice(&bg);
    }

    // Целевая высота в пикселях ( half для stereo ).
    let rows = if stereo && ch_cols.len() >= 2 { 2 } else { 1 };
    let half_h = h / rows;

    for (row, ch_idx) in (0..rows).enumerate().map(|(r, _)| (r, r.min(ch_cols.len() - 1))) {
        let cols = &ch_cols[ch_idx];
        if cols.is_empty() {
            continue;
        }
        let y_base = row * half_h;
        let mid = (half_h as f32) * 0.5;
        let col_scale = half_h as f32 * 0.5; // амплитуда → пиксели (±1.0 → half).
        let line_w_px = ((cfg.line_width * 1.5) as usize).max(1);
        for (x, ec) in cols.iter().enumerate() {
            let px = x * w / cols.len();
            let px_end = ((x + 1) * w / cols.len()).min(w);
            // Вертикальная линия: ymin..ymax по центру.
            let amp_max = (ec.max * cfg.sensitivity).clamp(-1.0, 1.0);
            let amp_min = (ec.min * cfg.sensitivity).clamp(-1.0, 1.0);
            let y_max_px = (mid - amp_max * col_scale).round() as i32;
            let y_min_px = (mid - amp_min * col_scale).round() as i32;
            let top = y_min_px.min(y_max_px);
            let bot = y_max_px.max(y_min_px);
            let half_lw = line_w_px / 2;
            for x_px in px..px_end {
                for y_px in (top - half_lw as i32)..=(bot + half_lw as i32) {
                    let y = y_px + y_base as i32;
                    if y < 0 || y >= half_h as i32 {
                        continue;
                    }
                    let off = (y as usize * w + x_px) * 4;
                    buf[off] = fg[0];
                    buf[off + 1] = fg[1];
                    buf[off + 2] = fg[2];
                    buf[off + 3] = fg[3];
                }
            }
            // Центральная линия (точка посередине) — по ТЗ §5.2.
            if cfg.draw_center_line {
                let y = y_base as i32 + mid.round() as i32;
                if y >= 0 && y < half_h as i32 {
                    for x_px in px..px_end {
                        let off = (y as usize * w + x_px) * 4;
                        buf[off] = fg[0] / 2;
                        buf[off + 1] = fg[1] / 2;
                        buf[off + 2] = fg[2] / 2;
                        buf[off + 3] = 255;
                    }
                }
            }
        }
    }
    buf
}

// ---------------------------------------------------------------------------
// Ключ кэша
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMeta {
    pub path: PathBuf,
    pub mtime_secs: u64,
    pub size: u64,
    pub mode: String,
    pub channels: String,
    pub max_columns: u32,
    pub width: usize,
    pub height: usize,
}

/// Строка-ключ кэша (hex-коды через дефис).
pub fn cache_key(
    path: &Path,
    mtime: std::time::SystemTime,
    size: u64,
    cfg: &OscilloscopeCfg,
) -> String {
    let mut h = DefaultHasher::new();
    path.to_string_lossy().hash(&mut h);
    mtime
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .hash(&mut h);
    size.hash(&mut h);
    "oscilloscope".hash(&mut h);
    (cfg.channels as u8).hash(&mut h);
    cfg.line_width.to_bits().hash(&mut h);
    cfg.draw_center_line.hash(&mut h);
    cfg.max_columns.hash(&mut h);
    cfg.sensitivity.to_bits().hash(&mut h);
    cfg.bg_color.hash(&mut h);
    cfg.fg_color.hash(&mut h);
    format!("{:x}", h.finish())
}

/// Ключ кэша полнотрековой спектрограммы (учитывает все визуальные параметры).
pub fn cache_key_spectrogram(
    path: &Path,
    mtime: std::time::SystemTime,
    size: u64,
    cfg: &crate::audio::visualizer::SpectrogramCfg,
) -> String {
    let mut h = DefaultHasher::new();
    path.to_string_lossy().hash(&mut h);
    mtime
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .hash(&mut h);
    size.hash(&mut h);
    "spectrogram".hash(&mut h);
    (cfg.channels as u8).hash(&mut h);
    cfg.fft_size.hash(&mut h);
    (cfg.window_type as u8).hash(&mut h);
    (cfg.freq_scale as u8).hash(&mut h);
    cfg.freq_min.hash(&mut h);
    cfg.freq_max.hash(&mut h);
    cfg.gain_db.to_bits().hash(&mut h);
    cfg.range_db.to_bits().hash(&mut h);
    cfg.high_boost_db.to_bits().hash(&mut h);
    (cfg.palette as u8).hash(&mut h);
    cfg.sensitivity.to_bits().hash(&mut h);
    cfg.max_frames.hash(&mut h);
    cfg.dsd_cic_compensation.hash(&mut h);
    cfg.bg_color.hash(&mut h);
    cfg.fg_color.hash(&mut h);
    format!("{:x}", h.finish())
}

/// Директория кэша осциллограмм: `$XDG_CACHE_HOME/music_player/viz`.
pub fn viz_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("music_player")
        .join("viz")
}

/// Путь к PNG-файлу кэша для данного ключа.
pub fn cache_png_path(key: &str) -> PathBuf {
    viz_cache_dir().join(format!("{key}.png"))
}

/// Путь к JSON sidecar-файлу кэша.
pub fn cache_meta_path(key: &str) -> PathBuf {
    viz_cache_dir().join(format!("{key}.json"))
}

/// Сохранить изображение на диск (PNG) + sidecar JSON.
pub fn save_png(key: &str, rgba: &[u8], w: usize, h: usize, meta: &CacheMeta) -> std::io::Result<()> {
    let dir = viz_cache_dir();
    fs::create_dir_all(&dir)?;
    // PNG
    let img_path = cache_png_path(key);
    let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba.to_vec())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad image dims"))?;
    let file = BufWriter::new(fs::File::create(&img_path)?);
    use image::ImageEncoder;
    image::codecs::png::PngEncoder::new(file)
        .write_image(rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8)
        .map_err(std::io::Error::other)?;
    // sidecar
    let meta_path = cache_meta_path(key);
    let f = BufWriter::new(fs::File::create(meta_path)?);
    serde_json::to_writer(f, meta)?;
    drop(img); // just in case
    Ok(())
}

/// Загрузить PNG из кэша; возвращает (RGBA bytes, width, height) или None.
pub fn load_cached_png(key: &str) -> Option<(Vec<u8>, usize, usize)> {
    let path = cache_png_path(key);
    let data = fs::read(path).ok()?;
    let img = image::load_from_memory(&data).ok()?;
    let rgba = img.to_rgba8().into_raw();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Some((rgba, w, h))
}

/// Проверить, валиден ли sidecar (mtime не новее файла).
pub fn cache_meta_valid(key: &str) -> bool {
    let meta_path = cache_meta_path(key);
    let meta_data = match fs::read_to_string(&meta_path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let meta: CacheMeta = match serde_json::from_str(&meta_data) {
        Ok(m) => m,
        Err(_) => return false,
    };
    // Проверяем, что mtime файла на диске не новее mtime из мета-данных.
    if let Ok(disk_mtime) = fs::metadata(&meta.path).and_then(|m| m.modified()) {
        let disk_secs = disk_mtime
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        disk_secs <= meta.mtime_secs
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_min_max_mono() {
        let mut env = Envelope::new(6, 1, 3); // 3 columns, 6 frames → 2 frames per column
        // col0 (frames 0-1):  -0.5,  0.5
        // col1 (frames 2-3):  -1.0,  0.2
        // col2 (frames 4-5):   0.1,  0.9
        env.feed(&[-0.5, 0.5, -1.0, 0.2, 0.1, 0.9], 1);
        let result = env.finish();
        assert_eq!(result.len(), 1);
        let cols = &result[0];
        assert_eq!(cols[0], EnvelopeColumn { min: -0.5, max: 0.5 });
        assert_eq!(cols[1], EnvelopeColumn { min: -1.0, max: 0.2 });
        assert_eq!(cols[2], EnvelopeColumn { min: 0.1, max: 0.9 });
    }

    #[test]
    fn envelope_progress() {
        let mut env = Envelope::new(200, 1, 4);
        env.feed(&[0.0; 50], 1);
        assert!((env.progress() - 0.25).abs() < 1e-6);
        env.feed(&[0.0; 50], 1);
        env.feed(&[0.0; 100], 1);
        assert!((env.progress() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn render_rgba_size_matches() {
        let cols = vec![vec![EnvelopeColumn { min: -1.0, max: 1.0 }; 4]; 1];
        let cfg = OscilloscopeCfg::default();
        let buf = render_rgba(&cols, false, &cfg, 4, 10);
        assert_eq!(buf.len(), 4 * 10 * 4);
    }

    #[test]
    fn parse_color_hex() {
        assert_eq!(parse_color("#cba6f7"), [203, 166, 247, 255]);
        assert_eq!(parse_color("#1111bb"), [17, 17, 187, 255]);
        assert_eq!(parse_color("invalid"), [0, 0, 0, 255]);
    }

    #[test]
    fn cache_key_deterministic() {
        let cfg = OscilloscopeCfg::default();
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let k1 = cache_key(Path::new("foo.flac"), t, 12345, &cfg);
        let k2 = cache_key(Path::new("foo.flac"), t, 12345, &cfg);
        assert_eq!(k1, k2);
        let k3 = cache_key(Path::new("bar.flac"), t, 12345, &cfg);
        assert_ne!(k1, k3);
    }
}
