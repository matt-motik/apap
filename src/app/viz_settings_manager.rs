//! Настройки визуализации в диалоге (ТЗ §9) и переключение типа (§3.2).
//!
//! - Вкладка «Visualization» в параметрах: выпадающий список типа, динамический
//!   блок полей текущего типа, «Сбросить настройки типа», валидация
//!   (`freq_min<freq_max`, `fft_size` степень двойки, `bands` в диапазоне).
//! - Все изменения пишутся в draft, поэтому применяются в реальном времени
//!   (drain_fulltrack/viz_push читают draft, пока диалог открыт); в конфиг
//!   сохраняются кнопкой Save (§9.2). Debounce на перестроение полнотрековых —
//!   в `drain_fulltrack`.
//! - Горячая клавиша `V` циклически переключает тип и сразу сохраняет режим
//!   в конфиг (§3.2).

use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle as _;
use slint::SharedString;

use super::MusicApp;
use music_player_rs::audio::visualizer::{
    ChannelMode, FreqScale, LevelScale, OscilloscopeCfg, Palette, SpectrogramCfg, SpectrumCfg,
    VisualizationMode, WindowType,
};

/// Индекс канала в комбобоксе: 0 mono, 1 stereo.
fn channel_index(c: ChannelMode) -> i32 {
    match c {
        ChannelMode::Mono => 0,
        ChannelMode::Stereo => 1,
    }
}

fn channel_from_index(i: i32) -> ChannelMode {
    if i <= 0 {
        ChannelMode::Mono
    } else {
        ChannelMode::Stereo
    }
}

fn freq_scale_index(fs: FreqScale) -> i32 {
    match fs {
        FreqScale::Linear => 0,
        FreqScale::Log => 1,
        FreqScale::Mel => 2,
    }
}

fn freq_scale_from_index(i: i32) -> FreqScale {
    match i {
        0 => FreqScale::Linear,
        2 => FreqScale::Mel,
        _ => FreqScale::Log,
    }
}

fn level_scale_index(ls: LevelScale) -> i32 {
    match ls {
        LevelScale::Linear => 0,
        LevelScale::Log => 1,
    }
}

fn level_scale_from_index(i: i32) -> LevelScale {
    if i <= 0 {
        LevelScale::Linear
    } else {
        LevelScale::Log
    }
}

fn window_index(w: WindowType) -> i32 {
    match w {
        WindowType::Hann => 0,
        WindowType::Hamming => 1,
        WindowType::Blackman => 2,
    }
}

fn window_from_index(i: i32) -> WindowType {
    match i {
        1 => WindowType::Hamming,
        2 => WindowType::Blackman,
        _ => WindowType::Hann,
    }
}

fn palette_index(p: Palette) -> i32 {
    match p {
        Palette::Solid => 0,
        Palette::Magma => 1,
        Palette::Viridis => 2,
        Palette::Plasma => 3,
        Palette::Inferno => 4,
        Palette::Gray => 5,
        Palette::Thermal => 6,
        Palette::Rainbow => 7,
    }
}

fn palette_from_index(i: i32) -> Palette {
    match i {
        0 => Palette::Solid,
        2 => Palette::Viridis,
        3 => Palette::Plasma,
        4 => Palette::Inferno,
        5 => Palette::Gray,
        6 => Palette::Thermal,
        7 => Palette::Rainbow,
        _ => Palette::Magma,
    }
}

/// Индекс FFT-размера в комбобоксе [512..8192].
fn fft_index(fft: u32) -> i32 {
    match fft {
        512 => 0,
        1024 => 1,
        2048 => 2,
        4096 => 3,
        8192 => 4,
        _ => 2,
    }
}

fn fft_from_index(i: i32) -> u32 {
    512u32 << i.unsigned_abs().clamp(0, 4)
}

impl MusicApp {
    /// Цикл по всем типам (Off→Osc→Spectrogram→Spectrum→Off), ТЗ §3.2.
    /// Немедленно сохраняет режим в конфиг и применяет к UI/воркерам.
    pub(super) fn cycle_viz_mode(&mut self) {
        if self.settings_draft.is_some() {
            return;
        }
        let current = self.settings.settings.visualization.mode;
        let next = VisualizationMode::from_index(current.index() + 1);
        self.settings.settings.visualization.mode = next;
        self.settings.save();
        self.sync_viz_settings_to_ui();
        eprintln!("[viz] cycle: {:?} -> {:?}", current, next);
    }

    /// Выбор пункта меню «Визуализация» (ТЗ §3.2): клик по отмеченному режиму
    /// выключает визуализацию (Off), иначе — включает выбранный. Сразу
    /// сохраняет режим в конфиг и синхронизирует UI (галочки меню).
    pub(super) fn menu_select_viz_mode(&mut self, i: i32) {
        if self.settings_draft.is_some() {
            return;
        }
        let picked = VisualizationMode::from_index(i);
        let cur = self.settings.settings.visualization.mode;
        let next = if picked == cur { VisualizationMode::Off } else { picked };
        self.settings.settings.visualization.mode = next;
        self.settings.save();
        self.sync_viz_settings_to_ui();
        eprintln!("[viz] menu select {i} -> {:?}", next);
    }

    /// Сброс настроек текущего типа к дефолтам (кнопка в диалоге, §9.2).
    pub(super) fn reset_viz_type(&mut self) {
        if self.settings_draft.is_none() {
            return;
        }
        let mode = self.settings_ref().visualization.mode;
        match mode {
            VisualizationMode::Oscilloscope => {
                self.settings_mut().visualization.oscilloscope = OscilloscopeCfg::default();
            }
            VisualizationMode::Spectrogram => {
                self.settings_mut().visualization.spectrogram = SpectrogramCfg::default();
            }
            VisualizationMode::Spectrum => {
                self.settings_mut().visualization.spectrum = SpectrumCfg::default();
            }
            VisualizationMode::Off => {}
        }
        self.sync_viz_settings_to_ui();
        self.viz_apply_validated_texts();
    }

    /// Проброс всех настроек визуализации в диалог (open + reset).
    pub(super) fn sync_viz_settings_to_ui(&self) {
        let s = self.settings_ref();
        let v = &s.visualization;

        self.ui.set_settings_viz_mode(v.mode.index());
        self.ui.set_settings_viz_skip_dsd(v.skip_fulltrack_for_dsd);

        let o = &v.oscilloscope;
        self.ui.set_settings_viz_osc_channels(channel_index(o.channels));
        self.ui.set_settings_viz_osc_sensitivity(o.sensitivity);
        self.ui.set_settings_viz_osc_line_width(o.line_width);
        self.ui.set_settings_viz_osc_center_line(o.draw_center_line);
        self.ui.set_settings_viz_osc_max_columns(o.max_columns as i32);
        self.ui.set_settings_viz_osc_cache_mem(o.cache_in_memory);
        self.ui.set_settings_viz_osc_cache_disk(o.cache_on_disk);

        let sp = &v.spectrogram;
        self.ui.set_settings_viz_spec_channels(channel_index(sp.channels));
        self.ui.set_settings_viz_spec_sensitivity(sp.sensitivity);
        self.ui.set_settings_viz_spec_fft(fft_index(sp.fft_size));
        self.ui.set_settings_viz_spec_window(window_index(sp.window_type));
        self.ui.set_settings_viz_spec_scale(freq_scale_index(sp.freq_scale));
        self.ui
            .set_settings_viz_spec_gain(sp.gain_db);
        self.ui
            .set_settings_viz_spec_range(sp.range_db);
        self.ui
            .set_settings_viz_spec_boost(sp.high_boost_db);
        self.ui
            .set_settings_viz_spec_palette(palette_index(sp.palette));
        self.ui.set_settings_viz_spec_max_frames(sp.max_frames as i32);
        self.ui.set_settings_viz_spec_dsd_comp(sp.dsd_cic_compensation);
        self.ui
            .set_settings_viz_spec_cache_mem(sp.cache_in_memory);
        self.ui
            .set_settings_viz_spec_cache_disk(sp.cache_on_disk);

        let sm = &v.spectrum;
        self.ui.set_settings_viz_sp_channels(channel_index(sm.channels));
        self.ui.set_settings_viz_sp_sensitivity(sm.sensitivity);
        self.ui.set_settings_viz_sp_freq_scale(freq_scale_index(sm.freq_scale));
        self.ui.set_settings_viz_sp_level_scale(level_scale_index(sm.level_scale));
        self.ui.set_settings_viz_sp_smoothing(sm.smoothing);
        self.ui.set_settings_viz_sp_peak_hold(sm.peak_hold);
        self.ui.set_settings_viz_sp_peak_decay(sm.peak_decay_ms as i32);
        self.ui.set_settings_viz_sp_bar_gap(sm.bar_gap as i32);
        self.ui.set_settings_viz_sp_bar_radius(sm.bar_radius as i32);
        self.ui.set_settings_viz_sp_gradient(sm.gradient);
        self.ui.set_settings_viz_sp_dsd_comp(sm.dsd_cic_compensation);

        // Текстовые (валидируемые) поля: фактическое применяемое значение.
        self.ui
            .set_settings_viz_freq_min_text(SharedString::from(format!("{}", sp.freq_min)));
        self.ui
            .set_settings_viz_freq_max_text(SharedString::from(format!("{}", sp.freq_max)));
        self.ui
            .set_settings_viz_bands_text(SharedString::from(format!("{}", sm.bands)));
    }

    /// Перечитать текстовые поля (`freq_min`/`freq_max`/`bands`) из UI,
    /// применить валидные значения в draft и выставить флаги подсветки (§9.3).
    /// Некорректные значения не применяются; поля помечаются красным.
    pub(super) fn viz_apply_validated_texts(&mut self) {
        let fmin_t = self
            .ui
            .get_settings_viz_freq_min_text()
            .trim()
            .to_string();
        let fmax_t = self
            .ui
            .get_settings_viz_freq_max_text()
            .trim()
            .to_string();
        let bands_t = self
            .ui
            .get_settings_viz_bands_text()
            .trim()
            .to_string();

        let fmin = fmin_t.parse::<u32>().ok();
        let fmax = fmax_t.parse::<u32>().ok();
        let bands = bands_t.parse::<u32>().ok();

        let fmin_num_ok = matches!(fmin, Some(v) if (1..=22_050).contains(&v));
        let fmax_num_ok = matches!(fmax, Some(v) if (1..=22_050).contains(&v));
        let bands_ok = matches!(bands, Some(v) if (4..=128).contains(&v));
        let cross_ok = !(fmin_num_ok && fmax_num_ok && fmin.unwrap() >= fmax.unwrap());
        let fmin_invalid = !fmin_num_ok || !cross_ok;
        let fmax_invalid = !fmax_num_ok || !cross_ok;

        let mut err = Vec::new();
        if fmin_invalid {
            err.push("freq_min: целое 1..22050 и строго меньше freq_max");
        }
        if fmax_invalid {
            err.push("freq_max: целое 1..22050 и строго больше freq_min");
        }
        if !bands_ok {
            err.push("bands: целое 4..128");
        }

        // Применяем только валидные значения (запись в draft при открытом диалоге).
        if fmin_num_ok && cross_ok {
            self.settings_mut()
                .visualization
                .spectrogram
                .freq_min = fmin.unwrap();
        }
        if fmax_num_ok && cross_ok {
            self.settings_mut()
                .visualization
                .spectrogram
                .freq_max = fmax.unwrap();
        }
        if bands_ok {
            self.settings_mut()
                .visualization
                .spectrum
                .bands = bands.unwrap();
        }

        self.ui
            .set_settings_viz_freq_min_invalid(fmin_invalid);
        self.ui
            .set_settings_viz_freq_max_invalid(fmax_invalid);
        self.ui
            .set_settings_viz_bands_invalid(!bands_ok);
        self.ui
            .set_settings_viz_error(SharedString::from(err.join("\n")));
    }
}

/// Привязка всех колбэков вкладки «Visualization» и горячей клавиши V.
pub fn bind_viz_settings_callbacks(this: &Rc<RefCell<MusicApp>>) {
    bind_int(this, "viz-mode", |a, i| {
        a.settings_mut().visualization.mode = VisualizationMode::from_index(i);
        // Пересинхронизируем диалог, чтобы переключились блоки полей (§3.2/§9).
        a.sync_viz_settings_to_ui();
        a.viz_apply_validated_texts();
        eprintln!("[gui] settings_viz_mode mode={i}");
    });
    bind_bool(this, "viz-skip-dsd", |a, b| {
        a.settings_mut().visualization.skip_fulltrack_for_dsd = b;
        eprintln!("[gui] settings_viz_skip_dsd on={b}");
    });
    bind_int(this, "viz-osc-channels", |a, i| {
        a.settings_mut().visualization.oscilloscope.channels = channel_from_index(i);
        eprintln!("[gui] settings_viz_osc_channels idx={i}");
    });
    bind_float(this, "viz-osc-sensitivity", |a, v| {
        a.settings_mut().visualization.oscilloscope.sensitivity = v.clamp(0.01, 20.0);
    });
    bind_float(this, "viz-osc-line-width", |a, v| {
        a.settings_mut().visualization.oscilloscope.line_width = v.clamp(0.5, 4.0);
    });
    bind_bool(this, "viz-osc-center-line", |a, b| {
        a.settings_mut().visualization.oscilloscope.draw_center_line = b;
    });
    bind_int(this, "viz-osc-max-columns", |a, i| {
        a.settings_mut().visualization.oscilloscope.max_columns = (i as u32).clamp(512, 8192);
    });
    bind_bool(this, "viz-osc-cache-mem", |a, b| {
        a.settings_mut().visualization.oscilloscope.cache_in_memory = b;
    });
    bind_bool(this, "viz-osc-cache-disk", |a, b| {
        a.settings_mut().visualization.oscilloscope.cache_on_disk = b;
    });

    bind_int(this, "viz-spec-channels", |a, i| {
        a.settings_mut().visualization.spectrogram.channels = channel_from_index(i);
    });
    bind_float(this, "viz-spec-sensitivity", |a, v| {
        a.settings_mut().visualization.spectrogram.sensitivity = v.clamp(0.01, 20.0);
    });
    bind_int(this, "viz-spec-fft", |a, i| {
        a.settings_mut().visualization.spectrogram.fft_size = fft_from_index(i);
        eprintln!("[gui] settings_viz_spec_fft idx={i}");
    });
    bind_int(this, "viz-spec-window", |a, i| {
        a.settings_mut().visualization.spectrogram.window_type = window_from_index(i);
    });
    bind_int(this, "viz-spec-scale", |a, i| {
        a.settings_mut().visualization.spectrogram.freq_scale = freq_scale_from_index(i);
    });
    bind_str(this, "viz-spec-freq-min", |a, t| {
        eprintln!("[gui] settings_viz_spec_freq_min '{t}'");
        a.viz_apply_validated_texts();
    });
    bind_str(this, "viz-spec-freq-max", |a, t| {
        eprintln!("[gui] settings_viz_spec_freq_max '{t}'");
        a.viz_apply_validated_texts();
    });
    bind_float(this, "viz-spec-gain", |a, v| {
        a.settings_mut().visualization.spectrogram.gain_db = v.clamp(-40.0, 100.0);
    });
    bind_float(this, "viz-spec-range", |a, v| {
        a.settings_mut().visualization.spectrogram.range_db = v.clamp(1.0, 200.0);
    });
    bind_float(this, "viz-spec-boost", |a, v| {
        a.settings_mut().visualization.spectrogram.high_boost_db = v.clamp(0.0, 60.0);
    });
    bind_int(this, "viz-spec-palette", |a, i| {
        a.settings_mut().visualization.spectrogram.palette = palette_from_index(i);
    });
    bind_int(this, "viz-spec-max-frames", |a, i| {
        a.settings_mut().visualization.spectrogram.max_frames = (i as u32).clamp(512, 8192);
    });
    bind_bool(this, "viz-spec-dsd-comp", |a, b| {
        a.settings_mut().visualization.spectrogram.dsd_cic_compensation = b;
    });
    bind_bool(this, "viz-spec-cache-mem", |a, b| {
        a.settings_mut().visualization.spectrogram.cache_in_memory = b;
    });
    bind_bool(this, "viz-spec-cache-disk", |a, b| {
        a.settings_mut().visualization.spectrogram.cache_on_disk = b;
    });

    bind_int(this, "viz-sp-channels", |a, i| {
        a.settings_mut().visualization.spectrum.channels = channel_from_index(i);
    });
    bind_float(this, "viz-sp-sensitivity", |a, v| {
        a.settings_mut().visualization.spectrum.sensitivity = v.clamp(0.01, 20.0);
    });
    bind_str(this, "viz-sp-bands", |a, t| {
        eprintln!("[gui] settings_viz_sp_bands '{t}'");
        a.viz_apply_validated_texts();
    });
    bind_int(this, "viz-sp-freq-scale", |a, i| {
        a.settings_mut().visualization.spectrum.freq_scale = freq_scale_from_index(i);
    });
    bind_int(this, "viz-sp-level-scale", |a, i| {
        a.settings_mut().visualization.spectrum.level_scale = level_scale_from_index(i);
    });
    bind_float(this, "viz-sp-smoothing", |a, v| {
        a.settings_mut().visualization.spectrum.smoothing = v.clamp(0.0, 0.99);
    });
    bind_bool(this, "viz-sp-peak-hold", |a, b| {
        a.settings_mut().visualization.spectrum.peak_hold = b;
    });
    bind_int(this, "viz-sp-peak-decay", |a, i| {
        a.settings_mut().visualization.spectrum.peak_decay_ms = (i as u32).clamp(50, 2000);
    });
    bind_int(this, "viz-sp-bar-gap", |a, i| {
        a.settings_mut().visualization.spectrum.bar_gap = (i as u32).clamp(0, 10);
    });
    bind_int(this, "viz-sp-bar-radius", |a, i| {
        a.settings_mut().visualization.spectrum.bar_radius = (i as u32).clamp(0, 12);
    });
    bind_bool(this, "viz-sp-gradient", |a, b| {
        a.settings_mut().visualization.spectrum.gradient = b;
    });
    bind_bool(this, "viz-sp-dsd-comp", |a, b| {
        a.settings_mut().visualization.spectrum.dsd_cic_compensation = b;
    });

    // Кнопка «Сбросить настройки типа».
    let app = this.clone();
    let ui = app.borrow().ui.clone_strong();
    ui.on_settings_set_viz_reset_type(move || {
        eprintln!("[gui] settings_viz_reset_type");
        app.borrow_mut().reset_viz_type();
    });

    // Горячая клавиша V (срабатывает, когда диалог настроек закрыт).
    let app = this.clone();
    let ui = app.borrow().ui.clone_strong();
    ui.on_cycle_viz(move || {
        eprintln!("[gui] cycle_viz");
        app.borrow_mut().cycle_viz_mode();
    });

    // Меню «Визуализация» в MenuBar (ТЗ §3.2): клик по пункту.
    let app = this.clone();
    let ui = app.borrow().ui.clone_strong();
    ui.on_menu_select_viz(move |i| {
        eprintln!("[gui] menu_select_viz {i}");
        app.borrow_mut().menu_select_viz_mode(i);
    });
}

type AppRef = Rc<RefCell<MusicApp>>;

fn bind_int(app: &AppRef, name: &'static str, mut f: impl FnMut(&mut MusicApp, i32) + 'static) {
    let app = app.clone();
    let ui = app.borrow().ui.clone_strong();
    match name {
        "viz-mode" => ui.on_settings_set_viz_mode(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-osc-channels" => ui.on_settings_set_viz_osc_channels(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-osc-max-columns" => ui.on_settings_set_viz_osc_max_columns(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-spec-channels" => ui.on_settings_set_viz_spec_channels(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-spec-fft" => ui.on_settings_set_viz_spec_fft(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-spec-window" => ui.on_settings_set_viz_spec_window(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-spec-scale" => ui.on_settings_set_viz_spec_scale(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-spec-palette" => ui.on_settings_set_viz_spec_palette(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-spec-max-frames" => ui.on_settings_set_viz_spec_max_frames(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-sp-channels" => ui.on_settings_set_viz_sp_channels(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-sp-freq-scale" => ui.on_settings_set_viz_sp_freq_scale(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-sp-level-scale" => ui.on_settings_set_viz_sp_level_scale(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-sp-peak-decay" => ui.on_settings_set_viz_sp_peak_decay(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-sp-bar-gap" => ui.on_settings_set_viz_sp_bar_gap(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        "viz-sp-bar-radius" => ui.on_settings_set_viz_sp_bar_radius(move |i| {
            let mut a = app.borrow_mut();
            f(&mut a, i);
        }),
        _ => {}
    }
}

fn bind_bool(app: &AppRef, name: &'static str, mut f: impl FnMut(&mut MusicApp, bool) + 'static) {
    let app = app.clone();
    let ui = app.borrow().ui.clone_strong();
    match name {
        "viz-skip-dsd" => ui.on_settings_set_viz_skip_dsd(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-osc-center-line" => ui.on_settings_set_viz_osc_center_line(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-osc-cache-mem" => ui.on_settings_set_viz_osc_cache_mem(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-osc-cache-disk" => ui.on_settings_set_viz_osc_cache_disk(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-spec-dsd-comp" => ui.on_settings_set_viz_spec_dsd_comp(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-spec-cache-mem" => ui.on_settings_set_viz_spec_cache_mem(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-spec-cache-disk" => ui.on_settings_set_viz_spec_cache_disk(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-sp-peak-hold" => ui.on_settings_set_viz_sp_peak_hold(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-sp-gradient" => ui.on_settings_set_viz_sp_gradient(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        "viz-sp-dsd-comp" => ui.on_settings_set_viz_sp_dsd_comp(move |b| {
            let mut a = app.borrow_mut();
            f(&mut a, b);
        }),
        _ => {}
    }
}

fn bind_float(app: &AppRef, name: &'static str, mut f: impl FnMut(&mut MusicApp, f32) + 'static) {
    let app = app.clone();
    let ui = app.borrow().ui.clone_strong();
    match name {
        "viz-osc-sensitivity" => ui.on_settings_set_viz_osc_sensitivity(move |v| {
            let mut a = app.borrow_mut();
            f(&mut a, v);
        }),
        "viz-osc-line-width" => ui.on_settings_set_viz_osc_line_width(move |v| {
            let mut a = app.borrow_mut();
            f(&mut a, v);
        }),
        "viz-spec-sensitivity" => ui.on_settings_set_viz_spec_sensitivity(move |v| {
            let mut a = app.borrow_mut();
            f(&mut a, v);
        }),
        "viz-spec-gain" => ui.on_settings_set_viz_spec_gain(move |v| {
            let mut a = app.borrow_mut();
            f(&mut a, v);
        }),
        "viz-spec-range" => ui.on_settings_set_viz_spec_range(move |v| {
            let mut a = app.borrow_mut();
            f(&mut a, v);
        }),
        "viz-spec-boost" => ui.on_settings_set_viz_spec_boost(move |v| {
            let mut a = app.borrow_mut();
            f(&mut a, v);
        }),
        "viz-sp-sensitivity" => ui.on_settings_set_viz_sp_sensitivity(move |v| {
            let mut a = app.borrow_mut();
            f(&mut a, v);
        }),
        "viz-sp-smoothing" => ui.on_settings_set_viz_sp_smoothing(move |v| {
            let mut a = app.borrow_mut();
            f(&mut a, v);
        }),
        _ => {}
    }
}

fn bind_str(app: &AppRef, name: &'static str, f: impl FnMut(&mut MusicApp, &str) + 'static) {
    let app = app.clone();
    let ui = app.borrow().ui.clone_strong();
    let mut f = Box::new(f);
    match name {
        "viz-spec-freq-min" => ui.on_settings_set_viz_spec_freq_min(move |t| {
            let mut a = app.borrow_mut();
            f(&mut a, &t);
        }),
        "viz-spec-freq-max" => ui.on_settings_set_viz_spec_freq_max(move |t| {
            let mut a = app.borrow_mut();
            f(&mut a, &t);
        }),
        "viz-sp-bands" => ui.on_settings_set_viz_sp_bands(move |t| {
            let mut a = app.borrow_mut();
            f(&mut a, &t);
        }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use music_player_rs::audio::visualizer::defaults;

    #[test]
    fn fft_index_roundtrip() {
        for size in [512u32, 1024, 2048, 4096, 8192] {
            assert_eq!(fft_from_index(fft_index(size)), size);
        }
        // неизвестный размер нормализуется к 2048
        assert_eq!(fft_index(3000), 2);
    }

    #[test]
    fn channel_roundtrip() {
        for (idx, ch) in [(0, ChannelMode::Mono), (1, ChannelMode::Stereo)] {
            assert_eq!(channel_index(ch), idx);
            assert_eq!(channel_from_index(idx), ch);
        }
    }

    #[test]
    fn palette_from_index_default() {
        assert_eq!(palette_index(Palette::Magma), 1);
        assert_eq!(palette_from_index(99), Palette::Magma);
        assert_eq!(palette_index(palette_from_index(6)), 6);
    }

    #[test]
    fn defaults_provide_reset_values() {
        assert_eq!(defaults::freq_min(), 20);
        assert_eq!(defaults::freq_max(), 22_000);
        assert_eq!(defaults::bands(), 32);
    }
}