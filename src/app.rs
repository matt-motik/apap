use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use rfd::FileDialog;
use cpal::traits::{DeviceTrait, HostTrait};
use slint::{ComponentHandle, ModelRc, SharedString, StandardListViewItem};
use slint::language::TableColumn;

use music_player_rs::audio::player::Player;
use music_player_rs::playlist::{self, ScanMsg, Track};
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
    last_playlist_save: Instant,
    last_col_save: Instant,
    playlist_dirty: bool,
    tray_rx: Option<std::sync::mpsc::Receiver<TrayCmd>>,
    tray_up_tx: Option<tokio::sync::mpsc::UnboundedSender<tray::TrayState>>,
    last_tray_update: Instant,
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
            last_playlist_save: Instant::now(),
            last_col_save: Instant::now(),
            playlist_dirty: false,
            tray_rx: Some(tray_rx),
            tray_up_tx: Some(tray_up_tx),
            last_tray_update: Instant::now(),
        };
        app.rebuild_shuffle();
        if let Some(col) = app.settings.settings.sorted_col {
            let desc = app.settings.settings.sort_desc;
            app.apply_sort(col, desc);
        }
        app
    }

    pub fn init(this: &Rc<RefCell<Self>>) {
        {
            let mut app = this.borrow_mut();
            app.sync_settings_to_ui();
            app.sync_playlist_to_ui();
        }
        Self::bind_callbacks(this);
    }

    fn sync_settings_to_ui(&self) {
        self.ui.set_settings_open(false);
        self.ui.set_settings_theme(match self.settings.settings.theme {
            Theme::Dark => 0,
            Theme::Light => 1,
        });
        self.ui
            .set_settings_minimize(self.settings.settings.minimize_to_tray);
        self.ui
            .set_cover_size(self.settings.settings.cover_size);
        self.ui
            .set_col_info_w(self.settings.settings.col_info_w);
        self.ui.set_col_gap(self.settings.settings.col_gap);

        let ordered = self.settings.settings.ordered_columns();
        let cols: Vec<ColumnSetting> = ordered
            .iter()
            .map(|c| {
                let mut cs = ColumnSetting::default();
                cs.index = ordered.iter().position(|x| x == c).unwrap_or(0) as i32;
                cs.label = c.label().into();
                cs.visible = self.settings.settings.column_visible(*c);
                cs
            })
            .collect();
        self.ui.set_settings_cols(ModelRc::from(cols.as_slice()));
    }

    fn sync_playlist_to_ui(&mut self) {
        let ordered = self.settings.settings.ordered_columns();
        let visible_cols: Vec<ColumnId> = ordered
            .iter()
            .copied()
            .filter(|c| self.settings.settings.column_visible(*c))
            .collect();

        let table_cols: Vec<TableColumn> = visible_cols
            .iter()
            .map(|c| {
                let mut tc = TableColumn::default();
                tc.title = c.label().into();
                tc.width = (self.settings.settings.column_width(*c) * 1.0).into();
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
    }

    fn bind_callbacks(this: &Rc<RefCell<Self>>) {
        let ui = this.borrow().ui.clone_strong();

        // 1. play-pause
        {
            let app = this.clone();
            ui.on_play_pause(move || {
                app.borrow_mut().player.toggle();
            });
        }

        // 2. stop
        {
            let app = this.clone();
            ui.on_stop(move || {
                app.borrow_mut().player.stop();
            });
        }

        // 3. prev-track
        {
            let app = this.clone();
            ui.on_prev_track(move || {
                app.borrow_mut().play_prev();
            });
        }

        // 4. next-track
        {
            let app = this.clone();
            ui.on_next_track(move || {
                app.borrow_mut().play_next(1);
            });
        }

        // 5. toggle-repeat
        {
            let app = this.clone();
            ui.on_toggle_repeat(move || {
                app.borrow_mut().cycle_repeat();
            });
        }

        // 6. toggle-shuffle
        {
            let app = this.clone();
            ui.on_toggle_shuffle(move || {
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
                let duration = app.borrow().player.snapshot().2.unwrap_or(0.0);
                app.borrow_mut().player.seek(fraction as f64 * duration);
            });
        }

        // 8. volume-changed
        {
            let app = this.clone();
            ui.on_volume_changed(move |volume| {
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
                app.borrow_mut().player.toggle_mute();
            });
        }

        // 10. play-track
        {
            let app = this.clone();
            ui.on_play_track(move |index| {
                app.borrow_mut().play_track(index as usize);
            });
        }

        // 11-12. sort-ascending / sort-descending
        {
            let app = this.clone();
            ui.on_sort_ascending(move |col_idx| {
                if let Some(col) = visible_col_at_index(&app.borrow().settings.settings, col_idx) {
                    app.borrow_mut().sort_tracks(col);
                }
            });
        }
        {
            let app = this.clone();
            ui.on_sort_descending(move |col_idx| {
                if let Some(col) = visible_col_at_index(&app.borrow().settings.settings, col_idx) {
                    app.borrow_mut().sort_tracks(col);
                }
            });
        }

        // 13. open-settings
        {
            let app = this.clone();
            ui.on_open_settings(move || {
                app.borrow_mut().ui.set_settings_open(true);
            });
        }

        // 14. add-files
        {
            let app = this.clone();
            ui.on_add_files(move || {
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
                if let Some(folder) = FileDialog::new().pick_folder() {
                    app.borrow_mut().start_folder_scan(folder);
                }
            });
        }

        // 16. save-playlist
        {
            let app = this.clone();
            ui.on_save_playlist(move || {
                app.borrow_mut().save_playlist();
            });
        }

        // 17. load-playlist
        {
            let app = this.clone();
            ui.on_load_playlist(move || {
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

        // 18. settings-close
        {
            let app = this.clone();
            ui.on_settings_close(move || {
                app.borrow_mut().ui.set_settings_open(false);
            });
        }

        // 19. settings-theme-changed
        {
            let app = this.clone();
            ui.on_settings_theme_changed(move |value| {
                let mut a = app.borrow_mut();
                a.settings.settings.theme = if value == 1 {
                    Theme::Light
                } else {
                    Theme::Dark
                };
                a.settings.save();
                a.sync_settings_to_ui();
            });
        }

        // 20. settings-cover-size
        {
            let app = this.clone();
            ui.on_settings_cover_size(move |size| {
                let mut a = app.borrow_mut();
                a.settings.settings.cover_size = size;
                a.settings.save();
                a.ui.set_cover_size(size);
            });
        }

        // 21. settings-col-info-w
        {
            let app = this.clone();
            ui.on_settings_col_info_w(move |width| {
                let mut a = app.borrow_mut();
                a.settings.settings.col_info_w = width;
                a.settings.save();
                a.ui.set_col_info_w(width);
            });
        }

        // 22. settings-col-gap
        {
            let app = this.clone();
            ui.on_settings_col_gap(move |gap| {
                let mut a = app.borrow_mut();
                a.settings.settings.col_gap = gap;
                a.settings.save();
                a.ui.set_col_gap(gap);
            });
        }

        // 23. settings-toggle-minimize
        {
            let app = this.clone();
            ui.on_settings_toggle_minimize(move |enabled| {
                let mut a = app.borrow_mut();
                a.settings.settings.minimize_to_tray = enabled;
                a.settings.save();
            });
        }

        // 24. settings-device
        {
            let app = this.clone();
            ui.on_settings_device(move |name| {
                app.borrow_mut().set_output_device(name.to_string());
            });
        }

        // 25. settings-refresh-devices
        {
            let app = this.clone();
            ui.on_settings_refresh_devices(move || {
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
                let idx = idx as usize;
                let ordered = app.borrow().settings.settings.ordered_columns();
                if let Some(&col) = ordered.get(idx) {
                    let visible = app.borrow().settings.settings.column_visible(col);
                    app.borrow_mut()
                        .set_column_visible(col, !visible);
                }
            });
        }

        // 27. settings-reset-cols
        {
            let app = this.clone();
            ui.on_settings_reset_cols(move || {
                app.borrow_mut().reset_columns();
            });
        }

        // 28. settings-move-col-up
        {
            let app = this.clone();
            ui.on_settings_move_col_up(move |idx| {
                let idx = idx as usize;
                let mut a = app.borrow_mut();
                a.settings.settings.move_column(idx, idx.saturating_sub(1));
                a.settings.save();
                a.sync_settings_to_ui();
            });
        }

        // 29. settings-move-col-down
        {
            let app = this.clone();
            ui.on_settings_move_col_down(move |idx| {
                let idx = idx as usize;
                let n = app.borrow().settings.settings.ordered_columns().len();
                if idx + 1 < n {
                    let mut a = app.borrow_mut();
                    a.settings.settings.move_column(idx, idx + 1);
                    a.settings.save();
                    a.sync_settings_to_ui();
                }
            });
        }

        // 30. settings-clear-playlist
        {
            let app = this.clone();
            ui.on_settings_clear_playlist(move || {
                app.borrow_mut().clear_playlist();
            });
        }

        // 31. settings-remove-current
        {
            let app = this.clone();
            ui.on_settings_remove_current(move || {
                let current = app.borrow().current;
                if let Some(idx) = current {
                    app.borrow_mut().remove_track(idx);
                }
            });
        }

        // 32. show-about (stub)
        {
            ui.on_show_about(move || {});
        }
    }

    pub fn tick(&mut self) {
        self.poll_tray();
        self.drain_scan();
        self.handle_auto_advance();
        self.sync_player_state_to_ui();
        self.push_tray_status();
        self.save_if_dirty();
    }

    fn sync_player_state_to_ui(&mut self) {
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
        }
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
        self.status = "Track removed".into();
    }

    fn clear_playlist(&mut self) {
        self.player.stop();
        self.current = None;
        self.tracks.clear();
        self.known_paths.clear();
        self.rebuild_shuffle();
        self.mark_playlist_dirty();
        self.sync_playlist_to_ui();
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

    fn set_column_visible(&mut self, id: ColumnId, visible: bool) {
        self.settings
            .settings
            .column_visibility
            .insert(id.key().to_string(), visible);
        self.settings.save();
        self.sync_settings_to_ui();
        self.sync_playlist_to_ui();
    }

    fn reset_columns(&mut self) {
        self.settings.settings.column_widths.clear();
        self.settings.settings.column_visibility.clear();
        self.settings.save();
        self.sync_settings_to_ui();
        self.sync_playlist_to_ui();
    }

    fn save_if_dirty(&mut self) {
        if self.playlist_dirty
            && self.last_playlist_save.elapsed() >= std::time::Duration::from_secs(2)
        {
            self.save_playlist();
            self.last_playlist_save = Instant::now();
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
                    if visible {
                        self.ui.hide().unwrap();
                    } else {
                        self.ui.show().unwrap();
                    }
                }
                TrayCmd::Quit => {
                    self.settings.save();
                    self.save_playlist();
                    self.ui.hide().unwrap();
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
