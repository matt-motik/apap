//! Менеджер полнотрековых визуализаций (осциллограмма, ТЗ §16.4).
//!
//! Отдельный поток (`start_worker`) принимает команды `FullCmd`; на каждый
//! `Build` запускается свежий sub-thread с собственным `AtomicBool` (отмена
//! по следующему Build/Cancel без блокировки главного потока). Результаты
//! приходят событиями `FullEvt`, разбираются на UI-тике (`drain_fulltrack`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

use music_player_rs::audio::decoder::{AudioSource, Decoder};
use music_player_rs::audio::dsd::DsdDecoder;
use music_player_rs::audio::fulltrack::{self as ft};
use music_player_rs::audio::spectrogram::{self, Spectrogram};
use music_player_rs::audio::visualizer::{
    ChannelMode, OscilloscopeCfg, VisualizerConfig, VisualizationMode,
};

use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::MusicApp;

/// Debounce перестроения полнотрековых при изменении параметров из диалога
/// настроек (ТЗ §9.2).
const FULLTRACK_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// Запрос на построение полнотрековой визуализации трека.
#[derive(Clone)]
pub struct FullBuild {
    pub id: u64,
    pub path: PathBuf,
    /// Снимок настроек (режим + OscilloscopeCfg/SpectrogramCfg).
    pub cfg: VisualizerConfig,
}

#[derive(Clone)]
pub enum FullCmd {
    Build(Box<FullBuild>),
    Cancel,
}

/// Результаты билда; размер варианта `Ready` большой (RGBA-буфер).
#[allow(clippy::large_enum_variant)]
pub enum FullEvt {
    Progress { id: u64, p: f32 },
    /// Изображение RGBA готово (готово к показу; может прийти из disk-кэша).
    Ready {
        id: u64,
        rgba: Vec<u8>,
        w: usize,
        h: usize,
        key: String,
    },
    Failed { id: u64, reason: String },
}

/// Трек в DSD (DSF/DFF) — влияет на выбор декодера и skip-логику.
pub fn is_dsd(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str());
    matches!(ext.map(|s| s.to_ascii_lowercase()), Some(s) if s == "dsf" || s == "dff")
}

/// Запускает воркер-поток. Канал `out` получает события; `rx` — команды.
pub fn start_worker(rx: Receiver<FullCmd>, out: Sender<FullEvt>) {
    thread::spawn(move || {
        let mut recent: Option<Arc<AtomicBool>> = None;
        while let Ok(cmd) = rx.recv() {
            match cmd {
                FullCmd::Cancel => {
                    if let Some(f) = &recent {
                        f.store(true, Ordering::SeqCst);
                    }
                }
                FullCmd::Build(b) => {
                    if let Some(f) = &recent {
                        f.store(true, Ordering::SeqCst);
                    }
                    let flag = Arc::new(AtomicBool::new(false));
                    let out_c = out.clone();
                    let b2 = b;
                    let f2 = flag.clone();
                    thread::spawn(move || run_build(*b2, f2, out_c));
                    recent = Some(flag);
                }
            }
        }
    });
}

/// Один билд: декод → envelope/спектр → рендер RGBA → (кэш на диск) → Ready.
fn run_build(b: FullBuild, flag: Arc<AtomicBool>, out: Sender<FullEvt>) {
    match b.cfg.mode {
        VisualizationMode::Oscilloscope => run_osc(b, flag, out),
        VisualizationMode::Spectrogram => run_spec(b, flag, out),
        VisualizationMode::Off | VisualizationMode::Spectrum => {}
    }
}

/// Осциллограмма (ТЗ §16.4): envelope + `render_rgba`.
fn run_osc(b: FullBuild, flag: Arc<AtomicBool>, out: Sender<FullEvt>) {
    let osc = b.cfg.oscilloscope.clone();
    let md = fs::metadata(&b.path).ok();
    let mtime = md
        .as_ref()
        .and_then(|m| m.modified().ok())
        .unwrap_or(UNIX_EPOCH);
    let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
    let key = ft::cache_key(&b.path, mtime, size, &osc);

    if flag.load(Ordering::SeqCst) {
        return;
    }

    // 1. Disk-кэш (PNG + sidecar).
    if osc.cache_on_disk && ft::cache_meta_valid(&key) {
        if let Some((rgba, w, h)) = ft::load_cached_png(&key) {
            let _ = out.send(FullEvt::Ready {
                id: b.id,
                rgba,
                w,
                h,
                key,
            });
            return;
        }
    }

    // 2. Открыть track отдельным источником (WYSIWYG, независимо от плеера).
    let mut src: Box<dyn AudioSource> = if is_dsd(&b.path) {
        match DsdDecoder::open(&b.path) {
            Ok(d) => Box::new(d),
            Err(e) => return fail(&out, b.id, &e),
        }
    } else {
        match Decoder::open(&b.path) {
            Ok(d) => Box::new(d),
            Err(e) => return fail(&out, b.id, &e),
        }
    };

    if flag.load(Ordering::SeqCst) {
        return;
    }

    let info = src.info().clone();
    let dst_ch = match osc.channels {
        ChannelMode::Mono => 1,
        ChannelMode::Stereo => 2.min(info.channels.max(1)),
    };
    let columns = osc.max_columns as usize;
    let total = info.num_frames.unwrap_or(0);
    let mut env = ft::Envelope::new(total, dst_ch, columns);

    // 3. Полный декод с прогрессом.
    let mut blk = 0u64;
    loop {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        let chunk = match src.next_frames() {
            Some(c) => c,
            None => break,
        };
        env.feed(chunk, info.channels);
        blk += 1;
        // Прогресс не чаще, чем раз в 64 пакета.
        if blk.is_multiple_of(64) {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            let _ = out.send(FullEvt::Progress { id: b.id, p: env.progress() });
        }
    }

    if flag.load(Ordering::SeqCst) {
        return;
    }
    if total == 0 {
        return fail(&out, b.id, "не удалось определить длину трека (num_frames == 0)");
    }

    let cols = env.finish();
    let w = columns;
    let h = 512usize;
    let stereo = dst_ch == 2 && osc.channels == ChannelMode::Stereo;
    let rgba = ft::render_rgba(&cols, stereo, &osc, w, h);

    if flag.load(Ordering::SeqCst) {
        return;
    }

    // 4. Кэш на диск (PNG + sidecar JSON).
    if osc.cache_on_disk {
        let meta = ft::CacheMeta {
            path: b.path.clone(),
            mtime_secs: mtime.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            size,
            mode: "oscilloscope".into(),
            channels: format!("{:?}", osc.channels),
            max_columns: osc.max_columns,
            width: w,
            height: h,
        };
        let _ = ft::save_png(&key, &rgba, w, h, &meta);
    }
    let _ = out.send(FullEvt::Ready {
        id: b.id,
        rgba,
        w,
        h,
        key,
    });
}

/// Спектрограмма (ТЗ §16.5): стриминговое БПФ + адаптивный hop.
fn run_spec(b: FullBuild, flag: Arc<AtomicBool>, out: Sender<FullEvt>) {
    let scfg = b.cfg.spectrogram.clone();
    let dsd = is_dsd(&b.path);
    let md = fs::metadata(&b.path).ok();
    let mtime = md
        .as_ref()
        .and_then(|m| m.modified().ok())
        .unwrap_or(UNIX_EPOCH);
    let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
    let key = ft::cache_key_spectrogram(&b.path, mtime, size, &scfg);

    if flag.load(Ordering::SeqCst) {
        return;
    }

    // 1. Disk-кэш (PNG + sidecar).
    if scfg.cache_on_disk && ft::cache_meta_valid(&key) {
        if let Some((rgba, w, h)) = ft::load_cached_png(&key) {
            let _ = out.send(FullEvt::Ready {
                id: b.id,
                rgba,
                w,
                h,
                key,
            });
            return;
        }
    }

    let mut src: Box<dyn AudioSource> = if dsd {
        match DsdDecoder::open(&b.path) {
            Ok(d) => Box::new(d),
            Err(e) => return fail(&out, b.id, &e),
        }
    } else {
        match Decoder::open(&b.path) {
            Ok(d) => Box::new(d),
            Err(e) => return fail(&out, b.id, &e),
        }
    };

    if flag.load(Ordering::SeqCst) {
        return;
    }

    let info = src.info().clone();
    let total = info.num_frames.unwrap_or(0);
    if total == 0 {
        return fail(&out, b.id, "не удалось определить длину трека (num_frames == 0)");
    }
    let dst_ch = match scfg.channels {
        ChannelMode::Mono => 1,
        ChannelMode::Stereo => 2.min(info.channels.max(1)),
    };
    let mut spec = match Spectrogram::new(
        total,
        info.sample_rate.max(1),
        dst_ch,
        dsd && scfg.dsd_cic_compensation,
        &scfg,
    ) {
        Ok(s) => s,
        Err(e) => return fail(&out, b.id, &e),
    };

    // 2. Полный декод с прогрессом.
    let mut blk = 0u64;
    loop {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        let chunk = match src.next_frames() {
            Some(c) => c,
            None => break,
        };
        spec.feed(chunk, info.channels);
        blk += 1;
        if blk.is_multiple_of(64) {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            let _ = out.send(FullEvt::Progress { id: b.id, p: spec.progress() });
        }
    }

    if flag.load(Ordering::SeqCst) {
        return;
    }

    let w = spec.columns().min(spectrogram::MAX_PIX_W);
    let h = spectrogram::IMG_H;
    let rgba = spec.finish();

    if flag.load(Ordering::SeqCst) {
        return;
    }

    // 3. Кэш на диск (PNG + sidecar JSON).
    if scfg.cache_on_disk {
        let meta = ft::CacheMeta {
            path: b.path.clone(),
            mtime_secs: mtime.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            size,
            mode: "spectrogram".into(),
            channels: format!("{:?}", scfg.channels),
            max_columns: scfg.max_frames,
            width: w,
            height: h,
        };
        let _ = ft::save_png(&key, &rgba, w, h, &meta);
    }
    let _ = out.send(FullEvt::Ready {
        id: b.id,
        rgba,
        w,
        h,
        key,
    });
}

fn fail(out: &Sender<FullEvt>, id: u64, reason: &str) {
    let _ = out.send(FullEvt::Failed {
        id,
        reason: reason.to_string(),
    });
}

impl MusicApp {
    /// Создаёт канал + стартует воркер; вызывается из `MusicApp::new`.
    pub(super) fn setup_fulltrack(&mut self) {
        let (tx, job_rx) = channel::<FullCmd>();
        let (done_tx, done_rx) = channel::<FullEvt>();
        start_worker(job_rx, done_tx);
        self.fulltrack_tx = Some(tx);
        self.fulltrack_rx = Some(done_rx);
    }

    /// Авто-драйв: следит за `viz-mode == oscilloscope/spectrogram` и текущим
    /// треком, строит/отменяет/показывает картинку. Вызывается каждый UI-тик.
    pub(super) fn drain_fulltrack(&mut self) {
        let cfg = VisualizerConfig::from_settings(self.settings_ref());
        let mode = cfg.mode;
        let osc = cfg.oscilloscope.clone();

        let current = self
            .current
            .and_then(|i| self.tracks.get(i).map(|t| t.path.clone()));

        let skip_dsd = cfg.skip_fulltrack_for_dsd
            && current.as_deref().map(is_dsd).unwrap_or(false);

        let want = matches!(mode, VisualizationMode::Oscilloscope | VisualizationMode::Spectrogram)
            && current.is_some()
            && !(mode == VisualizationMode::Oscilloscope && skip_dsd);

        // Спец-плейсхолдер §6.3 (native DSD / DoP и skip_fulltrack_for_dsd).
        let want_disabled = mode == VisualizationMode::Oscilloscope && skip_dsd;
        let disabled_text = if want_disabled {
            format!(
                "Осциллограмма недоступна для DSD: включён skip_fulltrack_for_dsd\n({})",
                current
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )
        } else {
            String::new()
        };
        if self.ui.get_disabled_text() != disabled_text {
            self.ui.set_disabled_text(disabled_text.into());
        }

        // Целевой ключ: path + ключ кэша построения.
        let target = if want {
            current.map(|p| {
                let md = fs::metadata(&p).ok();
                let mtime = md.as_ref().and_then(|m| m.modified().ok());
                let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
                let key = match (mode, mtime) {
                    (VisualizationMode::Spectrogram, Some(t)) => {
                        ft::cache_key_spectrogram(&p, t, size, &cfg.spectrogram)
                    }
                    (_, Some(t)) => ft::cache_key(&p, t, size, &osc),
                    (_, None) => String::new(),
                };
                (p, key)
            })
        } else {
            None
        };

        // Debounce (§9.2): правки параметров из диалога (draft) перезапускают
        // полнотрековый билд только после 500 мс стабильности; смена режима
        // применяется сразу (V / комбобокс типа).
        let in_dialog = self.settings_draft.is_some();
        let mode_changed = self.fulltrack_mode != Some(mode);
        if !in_dialog {
            self.viz_debounce = None;
        }

        let changed = target != self.fulltrack_target || mode_changed;
        if changed {
            if in_dialog && !mode_changed {
                let rearm = match &self.viz_debounce {
                    Some((_, pending)) => pending.as_ref() != target.as_ref(),
                    None => true,
                };
                if rearm {
                    self.viz_debounce = Some((Instant::now(), target.clone()));
                }
            } else {
                self.viz_debounce = None;
                self.apply_fulltrack(mode, target, &cfg, &osc);
            }
        } else if let Some((t0, pending)) = &self.viz_debounce {
            if pending.as_ref() != target.as_ref() {
                // Вернулись к прежним значениям — сбрасываем отложенный билд.
                self.viz_debounce = None;
            } else if t0.elapsed() >= FULLTRACK_DEBOUNCE {
                self.viz_debounce = None;
                self.apply_fulltrack(mode, target, &cfg, &osc);
            }
        }

        // События воркера.
        self.drain_fulltrack_events(&cfg, &osc);
    }

    /// Применить целевой (path, key): отменить текущий билд, проверить RAM-кэш,
    /// при промахе — запустить новый. Обновляет `fulltrack_mode`/target/key.
    fn apply_fulltrack(
        &mut self,
        mode: VisualizationMode,
        target: Option<(PathBuf, String)>,
        cfg: &VisualizerConfig,
        osc: &OscilloscopeCfg,
    ) {
        self.fulltrack_mode = Some(mode);
        if let Some(tx) = self.fulltrack_tx.clone() {
            let _ = tx.send(FullCmd::Cancel);
        }
        self.fulltrack_target = target.clone();
        self.fulltrack_key = target.as_ref().map(|(_, k)| k.clone());
        self.ui.set_build_progress(0.0);

        if let Some((path, key)) = &target {
            self.fulltrack_id = self.fulltrack_id.wrapping_add(1);
            let id = self.fulltrack_id;
            // RAM-кэш: мгновенный показ без декода.
            let cache_in_mem = match mode {
                VisualizationMode::Spectrogram => cfg.spectrogram.cache_in_memory,
                _ => osc.cache_in_memory,
            };
            if cache_in_mem {
                if let Some(img) = self.fulltrack_cache.get(key).cloned() {
                    self.ui.set_osc_image(img);
                    self.ui.set_osc_ready(true);
                    self.fulltrack_key = Some(key.clone());
                }
            }
            if !self.fulltrack_cache.contains_key(key) {
                if let Some(tx) = self.fulltrack_tx.clone() {
                    let _ = tx.send(FullCmd::Build(Box::new(FullBuild {
                        id,
                        path: path.clone(),
                        cfg: cfg.clone(),
                    })));
                }
            }
        } else {
            self.ui.set_osc_ready(false);
        }
    }

    /// Разбор событий воркера (прогресс / Ready / Failed).
    fn drain_fulltrack_events(&mut self, cfg: &VisualizerConfig, osc: &OscilloscopeCfg) {
        let Some(rx) = &self.fulltrack_rx else {
            return;
        };
        let mode = cfg.mode;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                FullEvt::Progress { id, p } => {
                    if id == self.fulltrack_id {
                        self.ui.set_build_progress(p);
                    }
                }
                FullEvt::Ready { id, rgba, w, h, key } => {
                    if self.fulltrack_key.as_deref() == Some(key.as_str()) && id == self.fulltrack_id {
                        let img = slint::Image::from_rgba8(
                            SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                &rgba,
                                w as u32,
                                h as u32,
                            ),
                        );
                        let cache_in_mem = match mode {
                            VisualizationMode::Spectrogram => cfg.spectrogram.cache_in_memory,
                            _ => osc.cache_in_memory,
                        };
                        if cache_in_mem {
                            self.fulltrack_cache.insert(key.clone(), img.clone());
                        }
                        self.ui.set_osc_image(img);
                        self.ui.set_osc_ready(true);
                        self.ui.set_build_progress(0.0);
                    }
                }
                FullEvt::Failed { id, reason } => {
                    if id == self.fulltrack_id {
                        eprintln!("[fulltrack] build failed for {}: {reason}", self
                            .fulltrack_target
                            .as_ref()
                            .map(|(p, _)| p.display().to_string())
                            .unwrap_or_default());
                        self.ui.set_build_progress(0.0);
                    }
                }
            }
        }
    }
}