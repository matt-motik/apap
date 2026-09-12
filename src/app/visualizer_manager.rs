//! Visualization runtime concerns of [`MusicApp`]: live spectrum worker,
//! tap wiring and fast per-frame sync of bar data to the Slint window.
//!
//! The 100 ms `tick()` is far too coarse for an analyser (ТЗ §11.1 ≥30 FPS),
//! so this manager is driven by its own ~33 ms timer in `main.rs`
//! (`viz_push()`), mirroring the separate 16 ms reflow timer. All FFT work
//! stays in the worker thread (`audio::analyzer::LiveWorker`); this module
//! only publishes bar values.

use std::sync::Arc;

use super::*;
use music_player_rs::audio::analyzer::LiveWorker;
use music_player_rs::audio::visualizer::{
    ChannelMode, VisualizationMode, VisualizerConfig,
};

/// Период быстрого таймера визуализации (ms), обновляет полосы ~30 FPS.
pub const VIZ_PUSH_INTERVAL_MS: u64 = 33;

/// Распад полос на паузе/стопе за один push (короче не обновляется всё равно).
const VIZ_PAUSE_DECAY: f32 = 0.80;
/// Порог, ниже которого распавшиеся полосы считаются нулём и очищаются.
const VIZ_ZERO_THRESHOLD: f32 = 0.004;

impl MusicApp {
    /// Создать tap-кольцо, воркер и привязать producer к плееру. Вызывается
    /// один раз из `MusicApp::new()` до показа окна.
    pub(super) fn setup_visualizer(
        &mut self,
        cfg: Arc<VisualizerConfig>,
        tap_prod: rtrb::Producer<f32>,
        tap_cons: rtrb::Consumer<f32>,
    ) {
        let worker = LiveWorker::start(tap_cons, cfg);
        self.player.set_viz_tap(Some(tap_prod));
        self.viz = Some(worker);
        self.viz_sig = None;
        self.viz_bars = Vec::new();
        self.viz_tap_active = false;
        let (rate, ch) = self.player.format();
        if let Some(v) = &self.viz {
            v.set_format(rate, ch);
        }
    }

    /// Быстрый push визуализации (~33 мс). Применяет конфиг при изменении
    /// (mode/bands/каналы/параметры полос), тумблер tap и публикует полосы;
    /// на паузе/стопе полосы распадаются.
    pub(crate) fn viz_push(&mut self) {
        let Some(viz) = &self.viz else { return };
        let (rate, ch) = self.player.format();
        viz.set_format(rate, ch);

        let settings = self.settings.settings.visualization.clone();
        let mode = settings.mode;
        let sp = &settings.spectrum;
        let bands = sp.bands.clamp(4, 128) as usize;
        let channels = if sp.channels == ChannelMode::Mono { 1 } else { 2 };
        let style = (sp.bar_gap, sp.bar_radius, sp.gradient);

        let sig = Some((mode.index(), bands, channels, style.0, style.1, style.2));
        if sig != self.viz_sig {
            self.viz_sig = sig;
            viz.set_cfg(Arc::new(VisualizerConfig::from_settings(&self.settings.settings)));
            let active = mode == VisualizationMode::Spectrum;
            if active != self.viz_tap_active {
                self.viz_tap_active = active;
                self.player.set_viz_tap_active(active);
            }
            self.ui.set_viz_mode(mode.index());
            self.ui.set_viz_channels(channels as i32);
            self.ui.set_bar_gap(style.0 as f32);
            self.ui.set_bar_radius(style.1 as f32);
            self.ui.set_gradient(style.2);
        }

        if mode != VisualizationMode::Spectrum {
            if !self.viz_bars.is_empty() {
                self.push_spectrum(Vec::new());
            }
            return;
        }

        let (playing, _, _) = self.player.snapshot();
        if playing {
            self.push_spectrum(viz.bars());
        } else if !self.viz_bars.is_empty() {
            let decayed: Vec<f32> = self.viz_bars.iter().map(|v| v * VIZ_PAUSE_DECAY).collect();
            let zero = decayed.iter().all(|v| *v < VIZ_ZERO_THRESHOLD);
            self.push_spectrum(if zero { Vec::new() } else { decayed });
        }
    }

    /// Разбить кадр полос на L/R и запушить модельки спектра (с дельтой).
    /// Stored frame (`viz_bars`) используется и для распада на паузе.
    fn push_spectrum(&mut self, bars: Vec<f32>) {
        if bars == self.viz_bars {
            return;
        }
        self.viz_bars = bars;
        let bands = match self.viz_sig {
            Some(sig) => sig.1,
            None => 0,
        };
        if bands == 0 {
            return;
        }
        if self.viz_bars.len() >= bands * 2 {
            let l = self.viz_bars[0..bands].to_vec();
            let r = self.viz_bars[bands..bands * 2].to_vec();
            self.ui.set_spectrum_l(ModelRc::from(l.as_slice()));
            self.ui.set_spectrum_r(ModelRc::from(r.as_slice()));
        } else if self.viz_bars.len() == bands {
            self.ui.set_spectrum_l(ModelRc::from(self.viz_bars.as_slice()));
            self.ui.set_spectrum_r(ModelRc::from(Vec::<f32>::new().as_slice()));
        } else {
            self.ui.set_spectrum_l(ModelRc::from(self.viz_bars.as_slice()));
            self.ui.set_spectrum_r(ModelRc::from(self.viz_bars.as_slice()));
        }
    }
}