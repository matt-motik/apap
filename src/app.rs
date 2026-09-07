use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use rfd::FileDialog;
use cpal::traits::{DeviceTrait, HostTrait};
use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem};
use slint::language::TableColumn;

use music_player_rs::audio::player::Player;
use music_player_rs::cover::{self, CoverDone, CoverJob};
use music_player_rs::playlist::{self, ScanMsg, Track};
use music_player_rs::settings::Settings;
use music_player_rs::settings::{ColumnId, RepeatMode, SettingsStore, Theme};
use music_player_rs::tray::{self, TrayCmd};

slint::include_modules!();

pub fn create_ui() -> Result<AppWindow, slint::PlatformError> {
    AppWindow::new()
}

fn fmt_num(num: u32, total: u32) -> SharedString {
    if num > 0 {
        if total > 0 {
            format!("{num} / {total}").into()
        } else {
            format!("{num}").into()
        }
    } else if total > 0 {
        "—".into()
    } else {
        "—".into()
    }
}

fn opt_str(s: &Option<String>) -> SharedString {
    s.as_deref().unwrap_or("—").into()
}

fn empty_dash(s: &str) -> SharedString {
    if s.is_empty() { "—".into() } else { s.into() }
}

fn hex_color(hex: &str) -> slint::Color {
    let hex = hex.trim_start_matches('#');
    let bytes = hex.as_bytes();
    match bytes.len() {
        6 => {
            let r = u8::from_str_radix(&std::str::from_utf8(&bytes[0..2]).unwrap(), 16).unwrap();
            let g = u8::from_str_radix(&std::str::from_utf8(&bytes[2..4]).unwrap(), 16).unwrap();
            let b = u8::from_str_radix(&std::str::from_utf8(&bytes[4..6]).unwrap(), 16).unwrap();
            slint::Color::from_rgb_u8(r, g, b)
        }
        8 => {
            let a = u8::from_str_radix(&std::str::from_utf8(&bytes[0..2]).unwrap(), 16).unwrap();
            let r = u8::from_str_radix(&std::str::from_utf8(&bytes[2..4]).unwrap(), 16).unwrap();
            let g = u8::from_str_radix(&std::str::from_utf8(&bytes[4..6]).unwrap(), 16).unwrap();
            let b = u8::from_str_radix(&std::str::from_utf8(&bytes[6..8]).unwrap(), 16).unwrap();
            slint::Color::from_argb_u8(a, r, g, b)
        }
        _ => panic!("Invalid hex color length"),
    }
}

fn num_str(v: u32, suffix: &str) -> SharedString {
    if v > 0 { format!("{v}{suffix}").into() } else { "—".into() }
}

pub struct MusicApp {
    ui: AppWindow,
    settings: SettingsStore,
    player: Player,
    tracks: Vec<Track>,
    current: Option<usize>,
    scan_rx: Option<Receiver<ScanMsg>>,
    known_paths: HashSet<PathBuf>,
    status: SharedString,
    repeat: RepeatMode,
    shuffle: bool,
    shuffle_order: Vec<usize>,
    shuffle_pos: usize,
    devices: Vec<(String, String)>,
    playlist_dirty: bool,
    tray_rx: Option<std::sync::mpsc::Receiver<TrayCmd>>,
    tray_up_tx: Option<tokio::sync::mpsc::UnboundedSender<tray::TrayState>>,
    last_tray_update: Instant,
    last_view_width: f32,
    col_model_sig: u64,
    col_sig_stable_ticks: u32,
    settings_draft: Option<Settings>,
    cover_tx: Option<std::sync::mpsc::Sender<CoverJob>>,
    cover_rx: Option<Receiver<CoverDone>>,
    cover_gen: u64,
}

impl MusicApp {
    pub fn new(ui: AppWindow) -> Self {
        let settings = SettingsStore::load();
        let mut player = Player::new();
        player.set_volume(settings.settings.volume);
        player.set_muted(settings.settings.muted);
        if !settings.settings.audio_device.is_empty() {
            player.set_preferred_device(settings.settings.audio_device.clone());
        }

        let tracks = playlist::load_track_list(&music_player_rs::settings::playlist_path());
        let known_paths: HashSet<PathBuf> = tracks.iter().map(|t| t.path.clone()).collect();
        let (tray_rx, tray_up_tx) = tray::start();

        let (cover_tx, cover_job_rx) = channel::<CoverJob>();
        let (cover_done_tx, cover_done_rx) = channel::<CoverDone>();
        cover::start_worker(cover_job_rx, cover_done_tx);

        let repeat = settings.settings.repeat;
        let shuffle = settings.settings.shuffle;
        let mut app = Self {
            ui: ui.clone_strong(),
            settings,
            player,
            tracks,
            current: None,
            scan_rx: None,
            known_paths,
            status: String::new().into(),
            repeat,
            shuffle,
            shuffle_order: Vec::new(),
            shuffle_pos: 0,
            devices: Vec::new(),
            playlist_dirty: false,
            tray_rx: Some(tray_rx),
            tray_up_tx: Some(tray_up_tx),
            last_tray_update: Instant::now(),
            last_view_width: 0.0,
            col_model_sig: 0,
            col_sig_stable_ticks: 0,
            settings_draft: None,
            cover_tx: Some(cover_tx),
            cover_rx: Some(cover_done_rx),
            cover_gen: 0,
        };
        app.rebuild_shuffle();
        if let Some(col) = app.settings.settings.sorted_col {
            let desc = app.settings.settings.sort_desc;
            app.apply_sort(col, desc);
        }
        app.apply_window_geometry();
        app
    }

    pub fn init(this: &Rc<RefCell<Self>>) {
        {
            let mut app = this.borrow_mut();
            app.apply_theme();
            app.sync_settings_to_ui();
            app.sync_playlist_to_ui();
        }
        Self::bind_callbacks(this);
    }

    fn settings_ref(&self) -> &Settings {
        self.settings_draft.as_ref().unwrap_or(&self.settings.settings)
    }

    fn settings_mut(&mut self) -> &mut Settings {
        self.settings_draft.as_mut().unwrap_or(&mut self.settings.settings)
    }

    /// Restore the saved window size/position (if any) before the window is shown.
    fn apply_window_geometry(&self) {
        let s = &self.settings.settings;
        if let (Some(w), Some(h)) = (s.win_w, s.win_h) {
            if (200..=8000).contains(&w) && (200..=8000).contains(&h) {
                self.ui.window().set_size(slint::WindowSize::Physical(slint::PhysicalSize::new(w, h)));
            }
        }
        if let (Some(x), Some(y)) = (s.win_x, s.win_y) {
            self.ui
                .window()
                .set_position(slint::WindowPosition::Physical(slint::PhysicalPosition::new(x, y)));
        }
    }

    /// Persist the current window size/position for the next run.
    fn save_window_geometry(&mut self) {
        let size = self.ui.window().size();
        let pos = self.ui.window().position();
        let s = &mut self.settings.settings;
        s.win_w = Some(size.width);
        s.win_h = Some(size.height);
        s.win_x = Some(pos.x);
        s.win_y = Some(pos.y);
        self.settings.save();
    }

    fn sync_settings_to_ui(&self) {
        let s = self.settings_ref();
        self.ui.set_settings_theme(match s.theme {
            Theme::Dark => 0,
            Theme::Light => 1,
        });
        self.ui
            .set_settings_minimize(s.minimize_to_tray);
        self.ui
            .set_cover_size(s.cover_size);
        self.ui
            .set_col_info_w(s.col_info_w);
        self.ui.set_col_gap(s.col_gap);

        let ordered = s.ordered_columns();
        let cols: Vec<ColumnSetting> = ordered
            .iter()
            .map(|c| {
                let mut cs = ColumnSetting::default();
                cs.index = ordered.iter().position(|x| x == c).unwrap_or(0) as i32;
                cs.label = c.label().into();
                cs.visible = s.column_visible(*c);
                cs.width_pct = s.column_width_pct(*c);
                cs
            })
            .collect();
        self.ui.set_settings_cols(ModelRc::from(cols.as_slice()));
    }

    fn sync_cover_settings_to_ui(&self) {
        let s = self.settings_ref();
        let covers: Vec<CoverSetting> = s
            .cover_priority_ordered()
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut cs = CoverSetting::default();
                cs.label = c.label().into();
                cs.pos = i as i32;
                cs
            })
            .collect();
        self.ui.set_settings_covers(ModelRc::from(covers.as_slice()));
        let names = s.cover_folder_names_list().join(", ");
        self.ui.set_settings_cover_names(names.into());
        self.ui.set_settings_cover_online(s.cover_online);
    }

    fn apply_theme(&self) {
        let c = self.ui.global::<Colors>();
        let dark = self.settings.settings.theme == Theme::Dark;
        self.ui.global::<MaterialPalette>().set_color_scheme(if dark {
            slint::private_unstable_api::re_exports::ColorScheme::Dark
        } else {
            slint::private_unstable_api::re_exports::ColorScheme::Light
        });

        if dark {
            c.set_bg_window(hex_color("#121018"));
            c.set_bg_surface(hex_color("#1a1720"));
            c.set_bg_toolbar(hex_color("#211e28"));
            c.set_bg_elevated(hex_color("#252230"));
            c.set_bg_overlay(hex_color("#00000088"));
            c.set_border_subtle(hex_color("#2d2a38"));
            c.set_border_default(hex_color("#3a3645"));
            c.set_text_primary(hex_color("#e6e1ec"));
            c.set_text_secondary(hex_color("#a9a3b8"));
            c.set_text_tertiary(hex_color("#7c7690"));
            c.set_text_dim(hex_color("#5c5670"));
            c.set_text_on_accent(hex_color("#ffffff"));
            c.set_text_error(hex_color("#f2b8b5"));
            c.set_accent(hex_color("#d0bcff"));
            c.set_accent_container(hex_color("#4f378b"));
            c.set_accent_on(hex_color("#eaddff"));
            c.set_surface_hover(hex_color("#322e3c"));
            c.set_surface_active(hex_color("#3a2f1f"));
            c.set_surface_selected(hex_color("#2d2a38"));
            c.set_viz_1(hex_color("#d35400"));
            c.set_viz_2(hex_color("#f1c40f"));
            c.set_viz_3(hex_color("#e74c3c"));
        } else {
            c.set_bg_window(hex_color("#f8f5fa"));
            c.set_bg_surface(hex_color("#ffffff"));
            c.set_bg_toolbar(hex_color("#f3edf7"));
            c.set_bg_elevated(hex_color("#ffffff"));
            c.set_bg_overlay(hex_color("#00000044"));
            c.set_border_subtle(hex_color("#e4dde8"));
            c.set_border_default(hex_color("#cac4d0"));
            c.set_text_primary(hex_color("#1d1b20"));
            c.set_text_secondary(hex_color("#49454f"));
            c.set_text_tertiary(hex_color("#79747e"));
            c.set_text_dim(hex_color("#938f99"));
            c.set_text_on_accent(hex_color("#ffffff"));
            c.set_text_error(hex_color("#b3261e"));
            c.set_accent(hex_color("#6750a4"));
            c.set_accent_container(hex_color("#eaddff"));
            c.set_accent_on(hex_color("#21005d"));
            c.set_surface_hover(hex_color("#e8e0ec"));
            c.set_surface_active(hex_color("#d0c4db"));
            c.set_surface_selected(hex_color("#e4dde8"));
            c.set_viz_1(hex_color("#b14a00"));
            c.set_viz_2(hex_color("#c4a00a"));
            c.set_viz_3(hex_color("#c0392b"));
        }
    }

    fn sync_playlist_to_ui(&mut self) {
        let ordered = self.settings.settings.ordered_columns();
        let visible_cols: Vec<ColumnId> = ordered
            .iter()
            .copied()
            .filter(|c| self.settings.settings.column_visible(*c))
            .collect();

        let view_w = self.ui.get_playlist_view_width().max(100.0) as f32;

        let mut widths: Vec<f32> = visible_cols
            .iter()
            .map(|c| self.settings.settings.column_width_pct(*c) / 100.0 * view_w)
            .collect();
        let sum: f32 = widths.iter().sum();
        if let Some(last) = widths.last_mut() {
            *last += view_w - sum;
        }

        let table_cols: Vec<TableColumn> = visible_cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut tc = TableColumn::default();
                tc.title = c.label().into();
                tc.width = widths[i].into();
                tc
            })
            .collect();

        let rows: Vec<Vec<StandardListViewItem>> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                visible_cols
                    .iter()
                    .map(|c| {
                        let text = match c {
                            ColumnId::Index => {
                                if self.current == Some(i) {
                                    ">".to_string()
                                } else {
                                    format!("{}", i + 1)
                                }
                            }
                            ColumnId::TrackNumber => {
                                fmt_num(t.track_number, t.track_total).to_string()
                            }
                            ColumnId::Title => t.title.clone(),
                            ColumnId::Artist => opt_str(&t.artist).to_string(),
                            ColumnId::Album => opt_str(&t.album).to_string(),
                            ColumnId::Genre => empty_dash(t.genre.as_deref().unwrap_or("")).to_string(),
                            ColumnId::Year => empty_dash(&t.year).to_string(),
                            ColumnId::Format => empty_dash(&t.format).to_string(),
                            ColumnId::Bitrate => {
                                if t.bitrate > 0 {
                                    format!("{} kbps", t.bitrate)
                                } else {
                                    "—".to_string()
                                }
                            }
                            ColumnId::BitDepth => empty_dash(&t.bit_depth).to_string(),
                            ColumnId::SampleRate => num_str(t.sample_rate, " Hz").to_string(),
                            ColumnId::Duration => playlist::get_duration_string(t.duration),
                            ColumnId::FileName => {
                                t.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
                            }
                            ColumnId::FilePath => t.path.to_string_lossy().into_owned(),
                        };
                        StandardListViewItem::from(text.as_str())
                    })
                    .collect()
            })
            .collect();

        let rows_rc: ModelRc<ModelRc<StandardListViewItem>> = ModelRc::from(
            rows.into_iter()
                .map(|r| ModelRc::from(r.as_slice()))
                .collect::<Vec<_>>()
                .as_slice(),
        );
        let cols_rc = ModelRc::from(table_cols.as_slice());

        self.ui.set_playlist_rows(rows_rc);
        self.ui.set_playlist_cols(cols_rc);
        self.ui.set_current_row(
            self.current.map(|i| i as i32).unwrap_or(-1),
        );
        self.col_model_sig = self.compute_col_sig();
    }

    fn update_column_widths(&mut self) {
        let ordered = self.settings.settings.ordered_columns();
        let visible_cols: Vec<ColumnId> = ordered
            .iter()
            .copied()
            .filter(|c| self.settings.settings.column_visible(*c))
            .collect();

        let view_w = self.ui.get_playlist_view_width().max(100.0) as f32;

        let mut widths: Vec<f32> = visible_cols
            .iter()
            .map(|c| self.settings.settings.column_width_pct(*c) / 100.0 * view_w)
            .collect();
        let sum: f32 = widths.iter().sum();
        if let Some(last) = widths.last_mut() {
            *last += view_w - sum;
        }

        let table_cols: Vec<TableColumn> = visible_cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut tc = TableColumn::default();
                tc.title = c.label().into();
                tc.width = widths[i].into();
                tc
            })
            .collect();

        let cols_rc = ModelRc::from(table_cols.as_slice());
        self.ui.set_playlist_cols(cols_rc);
        self.col_model_sig = self.compute_col_sig();
    }

    fn compute_col_sig(&self) -> u64 {
        let cols = self.ui.get_playlist_cols();
        let mut sig: u64 = 0;
        let len = cols.row_count();
        for i in 0..len {
            if let Some(tc) = cols.row_data(i) {
                let bits: u32 = tc.width.to_bits();
                sig = sig.wrapping_mul(31).wrapping_add(bits as u64);
            }
        }
        sig
    }

    fn save_column_widths_from_ui(&mut self) {
        let cols = self.ui.get_playlist_cols();
        let len = cols.row_count();
        if len == 0 {
            return;
        }
        let total_px: f32 = (0..len)
            .filter_map(|i| cols.row_data(i).map(|tc| tc.width))
            .sum();
        if total_px <= 0.0 {
            return;
        }
        let ordered = self.settings.settings.ordered_columns();
        let visible_cols: Vec<ColumnId> = ordered
            .iter()
            .copied()
            .filter(|c| self.settings.settings.column_visible(*c))
            .collect();
        for (i, col_id) in visible_cols.iter().enumerate() {
            if let Some(tc) = cols.row_data(i) {
                let pct = (tc.width / total_px) * 100.0;
                self.settings.settings.column_widths.insert(col_id.key().to_string(), pct);
            }
        }
        self.settings.save();
    }

    fn bind_callbacks(this: &Rc<RefCell<Self>>) {
        let ui = this.borrow().ui.clone_strong();

        // 1. play-pause
        {
            let app = this.clone();
            ui.on_play_pause(move || {
                eprintln!("[gui] play_pause");
                app.borrow_mut().player.toggle();
            });
        }

        // 2. stop
        {
            let app = this.clone();
            ui.on_stop(move || {
                eprintln!("[gui] stop");
                app.borrow_mut().player.stop();
            });
        }

        // 3. prev-track
        {
            let app = this.clone();
            ui.on_prev_track(move || {
                eprintln!("[gui] prev_track");
                app.borrow_mut().play_prev();
            });
        }

        // 4. next-track
        {
            let app = this.clone();
            ui.on_next_track(move || {
                eprintln!("[gui] next_track");
                app.borrow_mut().play_next(1);
            });
        }

        // 5. toggle-repeat
        {
            let app = this.clone();
            ui.on_toggle_repeat(move || {
                eprintln!("[gui] toggle_repeat");
                app.borrow_mut().cycle_repeat();
            });
        }

        // 6. toggle-shuffle
        {
            let app = this.clone();
            ui.on_toggle_shuffle(move || {
                eprintln!("[gui] toggle_shuffle");
                let mut a = app.borrow_mut();
                a.shuffle = !a.shuffle;
                a.settings.settings.shuffle = a.shuffle;
                a.settings.save();
                a.rebuild_shuffle();
                a.ui.set_shuffle(a.shuffle);
            });
        }

        // 7. seek
        {
            let app = this.clone();
            ui.on_seek(move |fraction| {
                eprintln!("[gui] seek fraction={fraction:.3}");
                let duration = app.borrow().player.snapshot().2.unwrap_or(0.0);
                app.borrow_mut().player.seek(fraction as f64 * duration);
            });
        }

        // 8. volume-changed
        {
            let app = this.clone();
            ui.on_volume_changed(move |volume| {
                eprintln!("[gui] volume_changed volume={volume:.3}");
                let mut a = app.borrow_mut();
                a.player.set_volume(volume);
                a.settings.settings.volume = volume;
                a.settings.save();
            });
        }

        // 9. toggle-mute
        {
            let app = this.clone();
            ui.on_toggle_mute(move || {
                eprintln!("[gui] toggle_mute");
                app.borrow_mut().player.toggle_mute();
            });
        }

        // 10. play-track
        {
            let app = this.clone();
            ui.on_play_track(move |index| {
                eprintln!("[gui] play_track index={index}");
                app.borrow_mut().play_track(index as usize);
            });
        }

        // 11-12. sort-ascending / sort-descending
        {
            let app = this.clone();
            ui.on_sort_ascending(move |col_idx| {
                eprintln!("[gui] sort_ascending col={col_idx}");
                let col = {
                    let a = app.borrow();
                    visible_col_at_index(&a.settings.settings, col_idx)
                };
                if let Some(col) = col {
                    app.borrow_mut().sort_tracks(col);
                }
            });
        }
        {
            let app = this.clone();
            ui.on_sort_descending(move |col_idx| {
                eprintln!("[gui] sort_descending col={col_idx}");
                let col = {
                    let a = app.borrow();
                    visible_col_at_index(&a.settings.settings, col_idx)
                };
                if let Some(col) = col {
                    app.borrow_mut().sort_tracks(col);
                }
            });
        }

        // 13. open-settings
        {
            let app = this.clone();
            ui.on_open_settings(move || {
                eprintln!("[gui] open_settings");
                let mut a = app.borrow_mut();
                a.settings_draft = Some(a.settings.settings.clone());
                a.sync_cover_settings_to_ui();
                a.ui.set_settings_open(true);
            });
        }

        // 14. add-files
        {
            let app = this.clone();
            ui.on_add_files(move || {
                eprintln!("[gui] add_files");
                let dialog = FileDialog::new()
                    .add_filter(
                        "Audio",
                        &["flac", "mp3", "ogg", "wav", "aac", "m4a", "dsf", "aiff"],
                    )
                    .pick_files();
                if let Some(paths) = dialog {
                    app.borrow_mut().add_paths(paths);
                }
            });
        }

        // 15. add-folder
        {
            let app = this.clone();
            ui.on_add_folder(move || {
                eprintln!("[gui] add_folder");
                if let Some(folder) = FileDialog::new().pick_folder() {
                    app.borrow_mut().start_folder_scan(folder);
                }
            });
        }

        // 16. save-playlist
        {
            let app = this.clone();
            ui.on_save_playlist(move || {
                eprintln!("[gui] save_playlist");
                app.borrow_mut().save_playlist();
            });
        }

        // 17. load-playlist
        {
            let app = this.clone();
            ui.on_load_playlist(move || {
                eprintln!("[gui] load_playlist");
                if let Some(path) = FileDialog::new()
                    .add_filter("Playlist", &["m3u", "m3u8"])
                    .pick_file()
                {
                    let tracks = playlist::load_track_list(&path);
                    let mut a = app.borrow_mut();
                    a.tracks = tracks;
                    a.known_paths = a.tracks.iter().map(|t| t.path.clone()).collect();
                    a.current = None;
                    a.rebuild_shuffle();
                    a.mark_playlist_dirty();
                    a.sync_playlist_to_ui();
                }
            });
        }

        // 18. settings-close (cancel: discard draft, restore original)
        {
            let app = this.clone();
            ui.on_settings_close(move || {
                eprintln!("[gui] settings_close (Cancel)");
                let mut a = app.borrow_mut();
                a.settings_draft = None;
                a.sync_settings_to_ui();
                a.ui.set_settings_open(false);
            });
        }

        // 19. settings-theme-changed
        {
            let app = this.clone();
            ui.on_settings_theme_changed(move |value| {
                eprintln!("[gui] settings_theme_changed value={value}");
                let mut a = app.borrow_mut();
                a.settings_mut().theme = if value == 1 {
                    Theme::Light
                } else {
                    Theme::Dark
                };
                a.sync_settings_to_ui();
            });
        }

        // 20. settings-cover-size
        {
            let app = this.clone();
            ui.on_settings_cover_size(move |size| {
                eprintln!("[gui] settings_cover_size size={size:.1}");
                let mut a = app.borrow_mut();
                a.settings_mut().cover_size = size;
            });
        }

        // 21. settings-col-info-w
        {
            let app = this.clone();
            ui.on_settings_col_info_w(move |width| {
                eprintln!("[gui] settings_col_info_w width={width:.1}");
                let mut a = app.borrow_mut();
                a.settings_mut().col_info_w = width;
            });
        }

        // 22. settings-col-gap
        {
            let app = this.clone();
            ui.on_settings_col_gap(move |gap| {
                eprintln!("[gui] settings_col_gap gap={gap:.1}");
                let mut a = app.borrow_mut();
                a.settings_mut().col_gap = gap;
            });
        }

        // 23. settings-toggle-minimize
        {
            let app = this.clone();
            ui.on_settings_toggle_minimize(move |enabled| {
                eprintln!("[gui] settings_toggle_minimize enabled={enabled}");
                let mut a = app.borrow_mut();
                a.settings_mut().minimize_to_tray = enabled;
            });
        }

        // 24. settings-device
        {
            let app = this.clone();
            ui.on_settings_device(move |name| {
                eprintln!("[gui] settings_device name={name:?}");
                app.borrow_mut().set_output_device(name.to_string());
            });
        }

        // 25. settings-refresh-devices
        {
            let app = this.clone();
            ui.on_settings_refresh_devices(move || {
                eprintln!("[gui] settings_refresh_devices");
                let host = cpal::default_host();
                let devices: Vec<SharedString> = host
                    .output_devices()
                    .map(|ds| {
                        ds.filter_map(|d| {
                            d.description()
                                .ok()
                                .map(|desc| SharedString::from(desc.name()))
                        })
                        .collect()
                    })
                    .unwrap_or_default();
                let model: ModelRc<SharedString> = ModelRc::from(devices.as_slice());
                app.borrow_mut().ui.set_settings_devices(model);
            });
        }

        // 26. settings-toggle-col
        {
            let app = this.clone();
            ui.on_settings_toggle_col(move |idx| {
                eprintln!("[gui] settings_toggle_col idx={idx}");
                let idx = idx as usize;
                let (col, visible) = {
                    let a = app.borrow();
                    let ordered = a.settings_ref().ordered_columns();
                    let Some(&col) = ordered.get(idx) else { return };
                    let visible = a.settings_ref().column_visible(col);
                    (col, visible)
                };
                let mut a = app.borrow_mut();
                if visible {
                    a.settings_mut().disable_column(col);
                } else {
                    a.settings_mut().enable_column(col);
                }
                a.sync_settings_to_ui();
            });
        }

        // 27. settings-reset-cols
        {
            let app = this.clone();
            ui.on_settings_reset_cols(move || {
                eprintln!("[gui] settings_reset_cols");
                let mut a = app.borrow_mut();
                let s = a.settings_mut();
                s.column_widths.clear();
                s.column_visibility.clear();
                s.normalize_visible_pct();
                a.sync_settings_to_ui();
            });
        }

        // 28. settings-move-col-up
        {
            let app = this.clone();
            ui.on_settings_move_col_up(move |idx| {
                eprintln!("[gui] settings_move_col_up idx={idx}");
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                let ordered = a.settings_ref().ordered_columns();
                if idx == 0 || idx >= ordered.len() {
                    return;
                }
                let col_id = ordered[idx];
                a.settings_mut().move_column(idx, idx - 1);
                a.sync_settings_to_ui();
                let new_ordered = a.settings_ref().ordered_columns();
                let new_idx = new_ordered.iter().position(|&c| c == col_id).unwrap_or(idx - 1);
                a.ui.set_settings_selected_col(new_idx as i32);
            });
        }

        // 29. settings-move-col-down
        {
            let app = this.clone();
            ui.on_settings_move_col_down(move |idx| {
                eprintln!("[gui] settings_move_col_down idx={idx}");
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                let ordered = a.settings_ref().ordered_columns();
                if idx + 1 >= ordered.len() {
                    return;
                }
                let col_id = ordered[idx];
                a.settings_mut().move_column(idx, idx + 1);
                a.sync_settings_to_ui();
                let new_ordered = a.settings_ref().ordered_columns();
                let new_idx = new_ordered.iter().position(|&c| c == col_id).unwrap_or(idx + 1);
                a.ui.set_settings_selected_col(new_idx as i32);
            });
        }

        // 29b. settings-save (apply draft)
        {
            let app = this.clone();
            ui.on_settings_save(move || {
                eprintln!("[gui] settings_save (Save) draft_present={}", app.borrow().settings_draft.is_some());
                let mut a = app.borrow_mut();
                let Some(draft) = a.settings_draft.take() else { return };
                a.settings.settings = draft;
                a.settings.save();
                a.apply_theme();
                a.ui.set_cover_size(a.settings.settings.cover_size);
                a.ui.set_col_info_w(a.settings.settings.col_info_w);
                a.ui.set_col_gap(a.settings.settings.col_gap);
                a.sync_cover_settings_to_ui();
                a.sync_playlist_to_ui();
                a.ui.set_settings_open(false);
                eprintln!("[gui] settings_save: applied and closed");
            });
        }

        // 30. settings-clear-playlist
        {
            let app = this.clone();
            ui.on_settings_clear_playlist(move || {
                eprintln!("[gui] settings_clear_playlist");
                app.borrow_mut().clear_playlist();
            });
        }

        // 31. settings-remove-current
        {
            let app = this.clone();
            ui.on_settings_remove_current(move || {
                eprintln!("[gui] settings_remove_current");
                let current = app.borrow().current;
                if let Some(idx) = current {
                    app.borrow_mut().remove_track(idx);
                }
            });
        }

        // 31b. settings-cover-move-up
        {
            let app = this.clone();
            ui.on_settings_cover_move_up(move |idx| {
                eprintln!("[gui] settings_cover_move_up idx={idx}");
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                let ordered = a.settings_ref().cover_priority_ordered();
                if idx == 0 || idx >= ordered.len() {
                    return;
                }
                a.settings_mut().move_cover(idx, idx - 1);
                a.sync_cover_settings_to_ui();
            });
        }

        // 31c. settings-cover-move-down
        {
            let app = this.clone();
            ui.on_settings_cover_move_down(move |idx| {
                eprintln!("[gui] settings_cover_move_down idx={idx}");
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                let ordered = a.settings_ref().cover_priority_ordered();
                if idx + 1 >= ordered.len() {
                    return;
                }
                a.settings_mut().move_cover(idx, idx + 1);
                a.sync_cover_settings_to_ui();
            });
        }

        // 31d. settings-cover-names-edited
        {
            let app = this.clone();
            ui.on_settings_cover_names_edited(move |text| {
                eprintln!("[gui] settings_cover_names_edited text='{text}'");
                let names: Vec<String> = text
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                app.borrow_mut().settings_mut().cover_folder_names = names;
            });
        }

        // 31e. settings-toggle-cover-online
        {
            let app = this.clone();
            ui.on_settings_toggle_cover_online(move |on| {
                eprintln!("[gui] settings_toggle_cover_online on={on}");
                app.borrow_mut().settings_mut().cover_online = on;
            });
        }

        // 32. show-about (stub)
        {
            ui.on_show_about(move || {
                eprintln!("[gui] show_about");
            });
        }

        // 33. window close -> minimize to tray (if enabled), otherwise quit
        {
            let app = this.clone();
            ui.window().on_close_requested(move || {
                eprintln!("[gui] close_requested minimize={}", app.borrow().settings.settings.minimize_to_tray);
                let minimize = app.borrow().settings.settings.minimize_to_tray;
                if minimize {
                    let _ = app.borrow_mut().ui.hide();
                    slint::CloseRequestResponse::KeepWindowShown
                } else {
                    app.borrow_mut().save_window_geometry();
                    let _ = slint::quit_event_loop();
                    slint::CloseRequestResponse::KeepWindowShown
                }
            });
        }
    }

    pub fn tick(&mut self) {
        self.poll_tray();
        self.drain_scan();
        self.drain_cover();
        self.handle_auto_advance();
        self.sync_playback_state_to_ui();
        self.push_tray_status();

        let w = self.ui.get_playlist_view_width() as f32;
        if (w - self.last_view_width).abs() > 1.0 && w > 100.0 {
            self.update_column_widths();
            self.last_view_width = w;
        }

        let sig = self.compute_col_sig();
        if sig != 0 && sig != self.col_model_sig {
            self.save_column_widths_from_ui();
            self.col_model_sig = sig;
            self.col_sig_stable_ticks = 0;
        } else {
            self.col_sig_stable_ticks = self.col_sig_stable_ticks.saturating_add(1);
        }
        if self.col_sig_stable_ticks == 3 {
            self.save_column_widths_from_ui();
            self.update_column_widths();
        }
    }

    fn sync_playback_state_to_ui(&mut self) {
        let (playing, pos, dur) = self.player.snapshot();
        self.ui.set_playing(playing);
        self.ui.set_muted(self.player.muted());
        self.ui.set_volume(self.player.volume());
        self.ui.set_pos(playlist::format_duration(pos).into());

        let dur_f = dur.unwrap_or(0.0);
        self.ui.set_dur(playlist::format_duration(dur_f).into());
        self.ui.set_seek_fraction(if dur_f > 0.0 {
            (pos / dur_f).clamp(0.0, 1.0) as f32
        } else {
            0.0
        });

        self.ui.set_status_text(self.status.clone());
    }

    fn sync_track_info_to_ui(&mut self) {
        if let Some(i) = self.current {
            if let Some(t) = self.tracks.get(i) {
                self.ui.set_info_artist(opt_str(&t.artist));
                self.ui.set_info_track(fmt_num(t.track_number, t.track_total));
                self.ui.set_info_title(if t.title.is_empty() { "—".into() } else { t.title.as_str().into() });
                self.ui.set_info_duration(playlist::get_duration_string(t.duration).into());
                self.ui.set_info_year(empty_dash(&t.year));
                self.ui.set_info_album(opt_str(&t.album));
                self.ui.set_info_disc(fmt_num(t.disc, t.disc_total));
                self.ui.set_info_genre(empty_dash(t.genre.as_deref().unwrap_or("")));
                self.ui.set_info_format(empty_dash(&t.format));
                self.ui.set_info_bitrate(if t.bitrate > 0 { format!("{} kbps", t.bitrate).into() } else { "—".into() });
                self.ui.set_info_bit_depth(empty_dash(&t.bit_depth));
                self.ui.set_info_sample_rate(num_str(t.sample_rate, " Hz"));
                self.ui.set_info_channels(num_str(t.channels, " ch"));

                let mut parts: Vec<String> = Vec::new();
                if !t.format.is_empty() { parts.push(t.format.clone()); }
                if !t.bit_depth.is_empty() { parts.push(t.bit_depth.clone()); }
                if t.sample_rate > 0 { parts.push(format!("{} Hz", t.sample_rate)); }
                if t.channels > 0 { parts.push(format!("{} ch", t.channels)); }
                let track_count = format!("{} tracks", self.tracks.len());
                self.ui.set_track_info(parts.join(" \u{2022} ").into());
                self.ui.set_track_count(track_count.into());
                self.request_cover(i);
                return;
            }
        }
        self.ui.set_info_artist("—".into());
        self.ui.set_info_track("—".into());
        self.ui.set_info_title("—".into());
        self.ui.set_info_duration("—".into());
        self.ui.set_info_year("—".into());
        self.ui.set_info_album("—".into());
        self.ui.set_info_disc("—".into());
        self.ui.set_info_genre("—".into());
        self.ui.set_info_format("—".into());
        self.ui.set_info_bitrate("—".into());
        self.ui.set_info_bit_depth("—".into());
        self.ui.set_info_sample_rate("—".into());
        self.ui.set_info_channels("—".into());
        let track_count = format!("{} tracks", self.tracks.len());
        self.ui.set_track_info("".into());
        self.ui.set_track_count(track_count.into());
        self.reset_cover();
    }

    fn handle_auto_advance(&mut self) {
        if self.player.ended() && self.current.is_some() {
            match self.repeat {
                RepeatMode::One => {
                    self.player.clear_end();
                    self.player.play();
                }
                _ => {
                    let next = if self.shuffle && !self.shuffle_order.is_empty() {
                        match playlist::advance_shuffle(
                            &self.shuffle_order,
                            self.shuffle_pos,
                            1,
                            self.repeat,
                        ) {
                            Some((idx, new_pos)) => {
                                self.shuffle_pos = new_pos;
                                Some(idx)
                            }
                            None => None,
                        }
                    } else {
                        playlist::advance_index(self.current, 1, self.tracks.len(), self.repeat)
                    };
                    match next {
                        Some(idx) => self.play_track(idx),
                        None => self.player.clear_end(),
                    }
                }
            }
        }
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) -> usize {
        let mut added = 0;
        for p in paths {
            if playlist::is_supported_audio(&p) {
                if self.known_paths.insert(p.clone()) {
                    self.tracks.push(playlist::track_for_path(&p));
                    added += 1;
                }
            }
        }
        if added > 0 {
            self.rebuild_shuffle();
            self.mark_playlist_dirty();
            self.sync_playlist_to_ui();
            self.save_playlist();
        }
        added
    }

    fn mark_playlist_dirty(&mut self) {
        self.playlist_dirty = true;
    }

    fn save_playlist(&mut self) {
        if playlist::save_track_list(&music_player_rs::settings::playlist_path(), &self.tracks) {
            self.playlist_dirty = false;
        }
    }

    fn start_folder_scan(&mut self, root: PathBuf) {
        let (tx, rx): (std::sync::mpsc::Sender<ScanMsg>, Receiver<ScanMsg>) = channel();
        self.scan_rx = Some(rx);
        let status_root = root.display().to_string();
        thread::spawn(move || {
            playlist::scan_audio_dir(&root, tx);
        });
        self.status = format!("Scanning folder: {status_root}").into();
    }

    fn drain_scan(&mut self) {
        let mut finished = false;
        if self.scan_rx.is_some() {
            loop {
                let msg = match &self.scan_rx {
                    Some(rx) => rx.try_recv(),
                    None => break,
                };
                match msg {
                    Ok(ScanMsg::Batch(tracks)) => {
                        let mut added = 0;
                        for t in tracks {
                            if self.known_paths.insert(t.path.clone()) {
                                self.tracks.push(t);
                                added += 1;
                            }
                        }
                        if added > 0 {
                            self.mark_playlist_dirty();
                            self.status = format!("...{added} tracks added").into();
                        }
                    }
                    Ok(ScanMsg::Done(total)) => {
                        self.status = format!("Folder scan finished: {total} tracks found").into();
                        finished = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            self.scan_rx = None;
            self.sync_playlist_to_ui();
            self.save_playlist();
        }
    }

    /// Request a cover for the track; bumps `cover_gen` so stale results are
    /// discarded when `drain_cover` applies them.
    fn request_cover(&mut self, index: usize) {
        let Some(track) = self.tracks.get(index) else { return };
        self.cover_gen = self.cover_gen.wrapping_add(1);
        if let Some(tx) = &self.cover_tx {
            let cfg = cover::CoverConfig::from_settings(&self.settings.settings);
            let _ = tx.send(CoverJob {
                id: self.cover_gen,
                track: track.clone(),
                cfg,
            });
        }
    }

    /// Clear the displayed cover and invalidate any in-flight request.
    fn reset_cover(&mut self) {
        self.cover_gen = self.cover_gen.wrapping_add(1);
        self.ui.set_cover_art(slint::Image::default());
    }

    /// Apply cover results from the worker, keeping only the most recent one
    /// (and only if it is newer than the last requested id).
    fn drain_cover(&mut self) {
        if self.cover_rx.is_none() {
            return;
        }
        let mut latest: Option<Option<std::path::PathBuf>> = None;
        let mut latest_id = 0u64;
        loop {
            let msg = match &self.cover_rx {
                Some(rx) => rx.try_recv(),
                None => break,
            };
            match msg {
                Ok(done) => {
                    latest_id = done.id;
                    latest = Some(done.image);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        let Some(image) = latest else { return };
        if latest_id != self.cover_gen {
            return;
        }
        let img = match &image {
            Some(p) => slint::Image::load_from_path(p).unwrap_or_default(),
            None => slint::Image::default(),
        };
        self.ui.set_cover_art(img);
    }

    pub fn play_track(&mut self, index: usize) {
        if index >= self.tracks.len() {
            return;
        }
        let path = self.tracks[index].path.clone();
        let title = self.tracks[index].title.clone();
        match self.player.open(&path) {
            Ok(info) => {
                self.tracks[index].duration =
                    info.num_frames.map(|n| n as f64 / info.sample_rate as f64);
                if let Some(t) = &info.tags.title {
                    if !t.trim().is_empty() {
                        self.tracks[index].title = t.clone();
                    }
                }
                self.tracks[index].artist = info.tags.artist.clone();
                self.tracks[index].album = info.tags.album.clone();
                self.tracks[index].genre = info.tags.genre.clone();
                self.tracks[index].year = info.tags.year.clone().unwrap_or_default();
                if info.tags.track_number > 0 || self.tracks[index].track_number == 0 {
                    self.tracks[index].track_number = info.tags.track_number;
                }
                if info.tags.track_total > 0 {
                    self.tracks[index].track_total = info.tags.track_total;
                }
                if info.tags.disc_number > 0 {
                    self.tracks[index].disc = info.tags.disc_number;
                }
                if info.tags.disc_total > 0 {
                    self.tracks[index].disc_total = info.tags.disc_total;
                }
                self.tracks[index].channels = info.channels as u32;
                self.tracks[index].bitrate = info.bitrate;
                if info.format_name.starts_with("DSD") {
                    self.tracks[index].bit_depth = info.format_name.to_lowercase();
                } else {
                    self.tracks[index].sample_rate = info.sample_rate;
                    self.tracks[index].bit_depth = match info.bits {
                        Some(b) if b > 0 => format!("{b} bit"),
                        _ => self.tracks[index].bit_depth.clone(),
                    };
                }
                self.current = Some(index);
                self.sync_shuffle_pos();
                let title = &self.tracks[index].title;
                self.status = format!(
                    "Playing: {title} \u{2014} {} Hz, {} ch, {}",
                    info.sample_rate, info.channels, info.format_name
                ).into();
                self.player.play();
                self.sync_playlist_to_ui();
                self.sync_track_info_to_ui();
            }
            Err(e) => {
                self.status = format!("Cannot play {title}: {e}").into();
                if let Ok(mut core) = self.player.core.lock() {
                    core.playing = false;
                    core.finished = true;
                    core.natural_end = false;
                }
            }
        }
    }

    fn rebuild_shuffle(&mut self) {
        let n = self.tracks.len();
        if !self.shuffle || n == 0 {
            self.shuffle_order.clear();
            self.shuffle_pos = 0;
            return;
        }
        let mut order: Vec<usize> = (0..n).collect();
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();
        order.shuffle(&mut rng);
        self.shuffle_order = order;
        self.sync_shuffle_pos();
    }

    fn sync_shuffle_pos(&mut self) {
        if self.shuffle_order.is_empty() || self.current.is_none() {
            return;
        }
        let cur = self.current.unwrap();
        if let Some(p) = self.shuffle_order.iter().position(|&i| i == cur) {
            self.shuffle_pos = p;
        }
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.next();
        self.settings.settings.repeat = self.repeat;
        self.settings.save();
        self.ui.set_repeat(self.repeat == RepeatMode::All);
        self.ui.set_repeat_one(self.repeat == RepeatMode::One);
    }

    fn play_next(&mut self, direction: i32) {
        if self.tracks.is_empty() {
            return;
        }
        let next = if self.shuffle && !self.shuffle_order.is_empty() {
            match playlist::advance_shuffle(
                &self.shuffle_order,
                self.shuffle_pos,
                direction,
                self.repeat,
            ) {
                Some((idx, new_pos)) => {
                    self.shuffle_pos = new_pos;
                    Some(idx)
                }
                None => None,
            }
        } else {
            playlist::advance_index(self.current, direction, self.tracks.len(), self.repeat)
        };
        if let Some(idx) = next {
            self.play_track(idx);
        }
    }

    fn play_prev(&mut self) {
        let (_playing, pos, _) = self.player.snapshot();
        if pos > 3.0 {
            self.player.seek(0.0);
            return;
        }
        self.play_next(-1);
    }

    fn remove_track(&mut self, index: usize) {
        if index >= self.tracks.len() {
            return;
        }
        if let Some(cur) = self.current {
            if cur == index {
                self.player.stop();
                self.current = None;
                self.reset_cover();
            } else if cur > index {
                self.current = Some(cur - 1);
            }
        }
        let t = &self.tracks[index];
        self.known_paths.remove(&t.path);
        self.tracks.remove(index);
        self.rebuild_shuffle();
        self.mark_playlist_dirty();
        self.sync_playlist_to_ui();
        self.save_playlist();
        self.status = "Track removed".into();
    }

    fn clear_playlist(&mut self) {
        self.player.stop();
        self.current = None;
        self.reset_cover();
        self.tracks.clear();
        self.known_paths.clear();
        self.rebuild_shuffle();
        self.mark_playlist_dirty();
        self.sync_playlist_to_ui();
        self.save_playlist();
        self.status = "Playlist cleared".into();
    }

    fn set_output_device(&mut self, name: String) {
        if name == self.settings.settings.audio_device && !name.is_empty() {
            return;
        }
        self.settings.settings.audio_device = name.clone();
        self.settings.save();

        let path = self
            .current
            .and_then(|i| self.tracks.get(i))
            .map(|t| t.path.clone());
        let (_playing, pos, _) = self.player.snapshot();
        match path {
            Some(path) => {
                if let Err(e) = self.player.set_device(name, Some(&path), pos) {
                    self.status = format!("Cannot switch audio device: {e}").into();
                }
            }
            None => {
                self.player.set_preferred_device(name);
            }
        }
    }

    fn sort_tracks(&mut self, col: ColumnId) {
        let desc = if self.settings.settings.sorted_col == Some(col) {
            !self.settings.settings.sort_desc
        } else {
            false
        };
        self.apply_sort(col, desc);
        self.settings.save();
        self.sync_playlist_to_ui();
        self.save_playlist();
    }

    fn apply_sort(&mut self, col: ColumnId, desc: bool) {
        if col == ColumnId::Index {
            return;
        }
        let n = self.tracks.len();
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| {
            let ord = playlist::sort_rows_compare(&self.tracks[a], &self.tracks[b], col);
            if desc { ord.reverse() } else { ord }
        });
        let new_tracks: Vec<Track> = idx.iter().map(|&i| self.tracks[i].clone()).collect();
        let mut new_pos = vec![0usize; n];
        for (new_i, &old_i) in idx.iter().enumerate() {
            new_pos[old_i] = new_i;
        }
        if let Some(cur) = self.current {
            self.current = Some(new_pos[cur]);
        }
        self.shuffle_order = self.shuffle_order.iter().map(|&i| new_pos[i]).collect();
        self.tracks = new_tracks;
        self.settings.settings.sorted_col = Some(col);
        self.settings.settings.sort_desc = desc;
    }

    fn poll_tray(&mut self) {
        let Some(rx) = self.tray_rx.take() else {
            return;
        };
        loop {
            let cmd = match rx.try_recv() {
                Ok(cmd) => cmd,
                Err(_) => break,
            };
            eprintln!("[tray] cmd={cmd:?}");
            match cmd {
                TrayCmd::TogglePlay => {
                    self.player.toggle();
                    self.push_tray_now();
                }
                TrayCmd::Stop => {
                    self.player.stop();
                    self.push_tray_now();
                }
                TrayCmd::Prev => self.play_prev(),
                TrayCmd::Next => self.play_next(1),
                TrayCmd::ShowHide => {
                    let visible = self.ui.window().is_visible();
                    let res = if visible {
                        self.ui.hide()
                    } else {
                        self.ui.show()
                    };
                    if let Err(e) = res {
                        eprintln!("tray show/hide failed: {e}");
                    }
                }
                TrayCmd::Quit => {
                    self.save_window_geometry();
                    self.save_playlist();
                    let _ = slint::quit_event_loop();
                }
                TrayCmd::Wheel(delta) => {
                    let v = (self.player.volume() - delta as f32 * 0.02).clamp(0.0, 1.0);
                    self.player.set_volume(v);
                    self.settings.settings.volume = v;
                    self.settings.save();
                }
            }
        }
        self.tray_rx = Some(rx);
    }

    fn push_tray_now(&mut self) {
        if let Some(tx) = &self.tray_up_tx {
            let state = self.tray_state();
            let _ = tx.send(state);
        }
    }

    fn push_tray_status(&mut self) {
        if self.tray_up_tx.is_some() && self.last_tray_update.elapsed().as_millis() >= 300 {
            self.last_tray_update = Instant::now();
            self.push_tray_now();
        }
    }

    fn tray_state(&self) -> tray::TrayState {
        let now_playing = match self.current {
            Some(i) => {
                let t = &self.tracks[i];
                match &t.artist {
                    Some(a) if !a.is_empty() => format!("{} \u{2014} {}", t.title, a),
                    _ => t.title.clone(),
                }
            }
            None => String::new(),
        };
        tray::TrayState {
            now_playing,
            playing: self.player.is_playing(),
        }
    }
}

fn visible_col_at_index(settings: &music_player_rs::settings::Settings, visible_index: i32) -> Option<ColumnId> {
    let ordered = settings.ordered_columns();
    let visible_cols: Vec<ColumnId> = ordered
        .iter()
        .copied()
        .filter(|c| settings.column_visible(*c))
        .collect();
    visible_cols.get(visible_index as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artist: &str, year: &str, bitrate: u32, dur: Option<f64>) -> Track {
        Track {
            path: PathBuf::from("/tmp/x.flac"),
            title: title.to_string(),
            duration: dur,
            artist: if artist.is_empty() { None } else { Some(artist.to_string()) },
            album: None,
            genre: None,
            track_number: 0,
            track_total: 0,
            disc: 0,
            disc_total: 0,
            channels: 2,
            year: year.to_string(),
            format: "FLAC".to_string(),
            bitrate,
            bit_depth: "24 bit".to_string(),
            sample_rate: 96000,
        }
    }

    #[test]
    fn sort_rows_text_formats_columns() {
        let mut t = track("Title", "Artist", "2001", 1411, Some(186.0));
        t.genre = Some("Rock".to_string());
        t.track_number = 3;
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Title), "Title");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Artist), "Artist");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Genre), "Rock");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::TrackNumber), "3");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Year), "2001");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Format), "FLAC");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Bitrate), "1411 kbps");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::BitDepth), "24 bit");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::SampleRate), "96000 Hz");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Duration), "3:06");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::FileName), "x.flac");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::FilePath), "/tmp/x.flac");
    }

    #[test]
    fn sort_rows_empty_fields_render_blank() {
        let mut t = track("Title", "", "2001", 0, None);
        t.sample_rate = 0;
        t.bit_depth = String::new();
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Artist), "");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Bitrate), "");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::SampleRate), "");
        assert_eq!(playlist::sort_rows_text(&t, ColumnId::Duration), "--:--");
    }

    #[test]
    fn sort_rows_compare_orders_by_column() {
        let mut a = track("Bee", "z", "1999", 100, Some(100.0));
        let mut b = track("Alfa", "a", "2000", 500, Some(50.0));
        a.genre = Some("Metal".to_string());
        b.genre = Some("Blues".to_string());
        a.track_number = 2;
        b.track_number = 1;
        assert_eq!(playlist::sort_rows_compare(&a, &b, ColumnId::Title), std::cmp::Ordering::Greater);
        assert_eq!(playlist::sort_rows_compare(&a, &b, ColumnId::Artist), std::cmp::Ordering::Greater);
        assert_eq!(playlist::sort_rows_compare(&a, &b, ColumnId::Genre), std::cmp::Ordering::Greater);
        assert_eq!(playlist::sort_rows_compare(&a, &b, ColumnId::TrackNumber), std::cmp::Ordering::Greater);
        assert_eq!(playlist::sort_rows_compare(&a, &b, ColumnId::Year), std::cmp::Ordering::Less);
        assert_eq!(playlist::sort_rows_compare(&a, &b, ColumnId::Bitrate), std::cmp::Ordering::Less);
        assert_eq!(playlist::sort_rows_compare(&a, &b, ColumnId::Duration), std::cmp::Ordering::Greater);
    }

    #[test]
    fn num_slash_total_formats_numbers() {
        assert_eq!(fmt_num(0, 0).as_str(), "\u{2014}");
        assert_eq!(fmt_num(3, 0).as_str(), "3");
        assert_eq!(fmt_num(3, 12).as_str(), "3 / 12");
        assert_eq!(fmt_num(0, 12).as_str(), "\u{2014}");
    }
}
