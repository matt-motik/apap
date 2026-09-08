//! Playback concerns of [`MusicApp`]: track playback, transport requests,
//! shuffle state, playback-state sync to the UI and cover-art handling.

use super::*;

impl MusicApp {
    pub(super) fn sync_playback_state_to_ui(&mut self) {
        let (playing, pos, dur) = self.player.snapshot();
        let muted = self.player.muted();
        let v = self.player.volume();
        let dur_f = dur.unwrap_or(0.0);
        let seek_f = if dur_f > 0.0 {
            (pos / dur_f).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let pos_s = playlist::format_duration(pos);

        // Delta: only re-write properties that actually changed, so a paused
        // or stopped player does not churn the seekbar/status every tick.
        // While the user drags the seekbar, the position/seekbar are driven by
        // the grab (top_panel.slint); restore only after seek-commit lands.
        let seeking = self.ui.get_seekbar_dragging();
        let cur = &self.last_ui;
        if playing != cur.playing {
            self.ui.set_playing(playing);
        }
        if muted != cur.muted {
            self.ui.set_muted(muted);
        }
        if v != cur.volume {
            self.ui.set_volume(v);
        }
        if !seeking && (pos_s != cur.pos || seek_f != cur.seek_fraction) {
            self.ui.set_pos(pos_s.clone().into());
            self.ui.set_seek_fraction(seek_f);
        }
        let dur_s = playlist::format_duration(dur_f);
        if !seeking && dur_s != cur.dur {
            self.ui.set_dur(dur_s.clone().into());
        }
        let status_s = self.status.to_string();
        if status_s != cur.status {
            self.ui.set_status_text(self.status.clone());
        }
        self.last_ui = UiState {
            playing,
            muted,
            volume: v,
            pos: pos_s,
            dur: dur_s,
            seek_fraction: seek_f,
            status: status_s,
        };
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

    pub(super) fn handle_auto_advance(&mut self) {
        if self.player.ended() && self.current.is_some() {
            match self.repeat {
                RepeatMode::One => {
                    self.player.clear_end();
                    self.player.play();
                    self.emit(AppEvent::PlaybackStarted);
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
                    None => {
                        self.player.clear_end();
                        self.emit(AppEvent::PlaybackStopped);
                    }
                }
                }
            }
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
    pub(super) fn reset_cover(&mut self) {
        self.cover_gen = self.cover_gen.wrapping_add(1);
        self.ui.set_cover_art(slint::Image::default());
    }

    /// Apply cover results from the worker, keeping only the most recent one
    /// (and only if it is newer than the last requested id).
    pub(super) fn drain_cover(&mut self) {
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
        self.emit(AppEvent::CoverChanged);
    }

    pub fn play_track(&mut self, index: usize) {
        if index >= self.tracks.len() {
            return;
        }
        let path = self.tracks[index].path.clone();
        let title = self.tracks[index].title.clone();
        let prev_current = self.current;
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
                // Only the affected rows change: metadata of the new current
                // track and the `>` marker on the old/new current indices.
                self.refresh_playlist_rows_at(prev_current);
                self.refresh_playlist_rows_at(self.current);
                self.sync_track_info_to_ui();
                self.emit(AppEvent::TrackChanged(self.current));
                self.emit(AppEvent::PlaybackStarted);
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

    pub(super) fn rebuild_shuffle(&mut self) {
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
        let Some(cur) = self.current else { return };
        if self.shuffle_order.is_empty() {
            return;
        }
        if let Some(p) = self.shuffle_order.iter().position(|&i| i == cur) {
            self.shuffle_pos = p;
        }
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.next();
        self.settings.settings.repeat = self.repeat;
        // Persisted at exit (save-at-exit).
        self.ui.set_repeat(self.repeat == RepeatMode::All);
        self.ui.set_repeat_one(self.repeat == RepeatMode::One);
    }

    pub(super) fn play_next(&mut self, direction: i32) {
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

    pub(super) fn play_prev(&mut self) {
        let (_playing, pos, _) = self.player.snapshot();
        // If we are a few seconds into the track, "previous" restarts it.
        const RESTART_THRESHOLD_SECS: f64 = 3.0;
        if pos > RESTART_THRESHOLD_SECS {
            self.player.seek(0.0);
            return;
        }
        self.play_next(-1);
    }

    pub(super) fn set_output_device(&mut self, name: String) {
        if name.is_empty() || name == "(select device)" {
            return;
        }
        if name == self.settings.settings.audio_device {
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
                if let Err(e) = self.player.set_device(name.clone(), Some(&path), pos) {
                    self.status = format!("Cannot switch audio device: {e}").into();
                    self.audio_error = Some(e);
                    self.ui.set_settings_active_error(
                        self.audio_error.clone().unwrap_or_default().into(),
                    );
                } else {
                    self.status = format!("Audio device: {name}").into();
                    self.audio_ready = true;
                    self.audio_error = None;
                    self.active_device = name.clone();
                    self.ui.set_settings_active_device(name.into());
                    self.ui.set_settings_active_error(String::new().into());
                }
            }
            None => {
                self.player.set_preferred_device(name.clone());
                self.status = format!("Audio device: {name}").into();
                self.active_device = name.clone();
                self.ui.set_settings_active_device(name.into());
            }
        }
        self.emit(AppEvent::DeviceChanged);
        self.push_tray_now();
    }
}