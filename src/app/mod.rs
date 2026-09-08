use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use rfd::FileDialog;
use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem, VecModel};
use slint::language::TableColumn;

use music_player_rs::audio::output::{default_device_name, probe_output};
use music_player_rs::audio::player::Player;
use music_player_rs::cover::{self, CoverDone, CoverJob};
use music_player_rs::playlist::{self, ScanMsg, Track};
use music_player_rs::settings::{ColumnId, RepeatMode, Settings, SettingsStore, Theme};
use music_player_rs::tray::{self, TrayCmd};

pub mod playback_manager;
pub mod playlist_manager;
pub mod ui_manager;

/// Throttle for persisting user-dragged column widths: ticks (100 ms each)
/// with a stable column signature before a write. ~2 s.
const COL_SAVE_DEBOUNCE_TICKS: u32 = 20;

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

/// Parse a hex color `#RRGGBB` or `#AARRGGBB` into a Slint color.
///
/// Returns `None` on any invalid input (wrong length, non-hex characters, or
/// a non-UTF-8 string) instead of panicking, so callers are safe against
/// malformed configuration or other input.
fn hex_color(hex: &str) -> Option<slint::Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let bytes = hex.as_bytes();
    if !bytes.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let val = |i: usize| -> Option<u8> { u8::from_str_radix(&hex[i..i + 2], 16).ok() };
    if hex.len() == 8 {
        let (a, r, g, b) = (val(0)?, val(2)?, val(4)?, val(6)?);
        Some(slint::Color::from_argb_u8(a, r, g, b))
    } else {
        let (r, g, b) = (val(0)?, val(2)?, val(4)?);
        Some(slint::Color::from_rgb_u8(r, g, b))
    }
}

/// Parse a compile-time hex color literal, falling back to black if it is
/// somehow invalid. Only used with hard-coded palette constants.
fn hex_color_lit(hex: &str) -> slint::Color {
    hex_color(hex).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0))
}

fn num_str(v: u32, suffix: &str) -> SharedString {
    if v > 0 { format!("{v}{suffix}").into() } else { "—".into() }
}

pub struct MusicApp {
    ui: AppWindow,
    settings: SettingsStore,
    player: Player,
    tracks: Vec<Track>,
    /// Persistent row model for the playlist table. Mutated incrementally
    /// (push/set_row_data) instead of rebuilding the whole list on every
    /// append, so folder scans stay cheap.
    playlist_rows: Rc<VecModel<ModelRc<StandardListViewItem>>>,
    current: Option<usize>,
    scan_rx: Option<Receiver<ScanMsg>>,
    /// Tracks buffered by `drain_scan` while a background scan runs; committed
    /// to `tracks` atomically when `ScanMsg::Done` arrives.
    scan_pending: Vec<Track>,
    known_paths: HashSet<PathBuf>,
    status: SharedString,
    repeat: RepeatMode,
    shuffle: bool,
    shuffle_order: Vec<usize>,
    shuffle_pos: usize,
    /// True when the output device probe succeeded at startup.
    audio_ready: bool,
    /// Startup probe error (unavailable configured/default device).
    audio_error: Option<String>,
    /// Effective output device name (from the probe or last successful switch).
    active_device: String,
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
    /// In-flight async enumeration of output devices for the Settings dialog.
    audio_devices_rx: Option<Receiver<Vec<SharedString>>>,
    /// Async startup playlist load: yields the persisted track list once it
    /// has been read off disk (avoids blocking UI init on large playlists).
    startup_tracks_rx: Option<Receiver<Vec<Track>>>,
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

        // Load the persisted playlist in the background so a large library
        // doesn't block window construction; `tick()` applies it on arrival.
        let (startup_tx, startup_tracks_rx) = channel::<Vec<Track>>();
        let startup_path = music_player_rs::settings::playlist_path();
        thread::spawn(move || {
            let tracks = playlist::load_track_list(&startup_path);
            let _ = startup_tx.send(tracks);
        });
        let tracks = Vec::new();
        let known_paths: HashSet<PathBuf> = HashSet::new();
        let (tray_rx, tray_up_tx) = tray::start();

        let (cover_tx, cover_job_rx) = channel::<CoverJob>();
        let (cover_done_tx, cover_done_rx) = channel::<CoverDone>();
        cover::start_worker(cover_job_rx, cover_done_tx);

        // Wire the persistent playlist row model to the table once; later
        // mutations flow through it without re-creating ModelRc objects.
        let playlist_rows: Rc<VecModel<ModelRc<StandardListViewItem>>> =
            Rc::new(slint::VecModel::default());
        ui.set_playlist_rows(ModelRc::from(playlist_rows.clone()));

        let repeat = settings.settings.repeat;
        let shuffle = settings.settings.shuffle;

        // Probe the configured (or default) output device so availability is
        // known before the UI is shown.
        let saved_device = settings.settings.audio_device.clone();
        let preferred = if saved_device.is_empty() {
            None
        } else {
            Some(saved_device.as_str())
        };
        eprintln!("[init] probing audio device: {:?}", preferred.unwrap_or("(default)"));
        let probe = probe_output(preferred);
        let (audio_ready, audio_error, active_device) = match probe {
            Ok(name) => (true, None, name),
            Err(e) => {
                eprintln!("[init] audio probe failed: {e}");
                let active = if saved_device.is_empty() {
                    String::from("none")
                } else {
                    saved_device.clone()
                };
                (false, Some(e), active)
            }
        };
        let startup_status = match &audio_error {
            Some(e) => format!(
                "Audio device unavailable \u{2014} playback will not start: {e}"
            ),
            None => format!("Audio: {active_device} ready"),
        };

        let mut app = Self {
            ui: ui.clone_strong(),
            settings,
            player,
            tracks,
            playlist_rows,
            current: None,
            scan_rx: None,
            scan_pending: Vec::new(),
            known_paths,
            status: startup_status.into(),
            repeat,
            shuffle,
            shuffle_order: Vec::new(),
            shuffle_pos: 0,
            audio_ready,
            audio_error,
            active_device,
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
            audio_devices_rx: None,
            startup_tracks_rx: Some(startup_tracks_rx),
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
                a.sync_audio_devices();
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
                    app.borrow_mut().start_scan(paths);
                }
            });
        }

        // 15. add-folder
        {
            let app = this.clone();
            ui.on_add_folder(move || {
                eprintln!("[gui] add_folder");
                if let Some(folder) = FileDialog::new().pick_folder() {
                    app.borrow_mut().start_scan(vec![folder]);
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
                // Reset the Audio tab to the real (unchanged) state.
                a.sync_audio_devices();
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
                // Edit-Commit: store the choice in the dialog draft; the real
                // device switch (stream restart, probe, save) happens only when
                // the draft is applied on "Save".
                let mut a = app.borrow_mut();
                a.settings_mut().audio_device = name.to_string();
            });
        }

        // 25. settings-refresh-devices
        {
            let app = this.clone();
            ui.on_settings_refresh_devices(move || {
                eprintln!("[gui] settings_refresh_devices");
                app.borrow_mut().sync_audio_devices();
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

                let device_changed =
                    draft.audio_device != a.settings.settings.audio_device;
                if device_changed {
                    // Apply the device first: its early-return guard compares
                    // against the *live* setting, which still has the old value.
                    a.set_output_device(draft.audio_device.clone());
                }

                a.settings.settings = draft;
                a.settings.save();
                a.apply_theme();
                a.ui.set_cover_size(a.settings.settings.cover_size);
                a.ui.set_col_info_w(a.settings.settings.col_info_w);
                a.ui.set_col_gap(a.settings.settings.col_gap);
                a.sync_cover_settings_to_ui();
                a.sync_playlist_to_ui();
                a.sync_audio_devices();
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
        self.drain_startup_tracks();
        self.drain_scan();
        self.drain_cover();
        self.drain_audio_devices();
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
            // Column layout changed from the UI (user dragging a border): adopt
            // it as the new baseline and start a debounce timer. Nothing is
            // written to disk until the layout has been stable.
            self.col_model_sig = sig;
            self.col_sig_stable_ticks = 0;
        } else if self.col_sig_stable_ticks < COL_SAVE_DEBOUNCE_TICKS {
            self.col_sig_stable_ticks = self.col_sig_stable_ticks.saturating_add(1);
            if self.col_sig_stable_ticks == COL_SAVE_DEBOUNCE_TICKS {
                self.save_column_widths_from_ui();
                self.update_column_widths();
            }
        }
    }


    /// Apply the async-loaded startup playlist once it arrives from the
    /// background thread. No-op while the load is still in flight.
    fn drain_startup_tracks(&mut self) {
        let Some(rx) = self.startup_tracks_rx.take() else {
            return;
        };
        let tracks = match rx.try_recv() {
            Ok(loaded) => loaded,
            Err(_) => {
                self.startup_tracks_rx = Some(rx);
                return;
            }
        };
        let n = tracks.len();
        self.tracks = tracks;
        self.known_paths = self.tracks.iter().map(|t| t.path.clone()).collect();
        self.rebuild_shuffle();
        if let Some(col) = self.settings.settings.sorted_col {
            let desc = self.settings.settings.sort_desc;
            self.apply_sort(col, desc);
        }
        self.sync_playlist_to_ui();
        if n > 0 {
            let audio = if self.audio_ready {
                format!("Audio: {} ready; ", self.active_device)
            } else {
                String::new()
            };
            self.status = format!("{audio}Loaded {n} tracks").into();
        }
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
                    const WHEEL_VOLUME_STEP: f32 = 0.02;
                    let v = (self.player.volume() - delta as f32 * WHEEL_VOLUME_STEP).clamp(0.0, 1.0);
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
        let interval = tray::TRAY_UPDATE_INTERVAL_MS;
        if self.tray_up_tx.is_some() && self.last_tray_update.elapsed().as_millis() >= interval {
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
            error: self.audio_error.clone(),
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

    #[test]
    fn hex_color_parses_valid_inputs() {
        assert!(hex_color("#121018").is_some());
        assert!(hex_color("112233").is_some());
        assert!(hex_color("#00000088").is_some());
        assert!(hex_color("aaff0000").is_some());
    }

    #[test]
    fn hex_color_rejects_invalid_inputs() {
        assert!(hex_color("").is_none());
        assert!(hex_color("#12345").is_none()); // wrong length
        assert!(hex_color("#1234567").is_none()); // wrong length
        assert!(hex_color("#gggggg").is_none()); // non-hex
        assert!(hex_color("#12345g").is_none()); // non-hex tail
        assert!(hex_color("ФфФФФФ").is_none()); // non-utf8/ascii hex
    }
}
