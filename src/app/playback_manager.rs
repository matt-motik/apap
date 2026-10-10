//! Playback concerns of [`MusicApp`]: track playback, transport requests,
//! shuffle state, playback-state sync to the UI and cover-art handling.

use super::*;
use music_player_rs::audio::decoder::TrackInfo;
use music_player_rs::persist::state_file::{Origin, StateChange};
use music_player_rs::playlist::compare::CompareKeys;

impl MusicApp {
    pub(super) fn sync_playback_state_to_ui(&mut self) {
        let (playing, pos, dur) = self.player.snapshot();
        let muted = self.player.muted();
        let v = self.player.volume();
        let bit_perfect = self.player.bit_perfect();
        let bp_resample = self.player.bit_perfect_resampled();
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
        let ui = self.try_ui();
        let seeking = ui.as_ref().is_some_and(|u| u.get_seekbar_dragging());
        let cur = &self.last_ui;
        if playing != cur.playing {
            if let Some(ui) = &ui {
                ui.set_playing(playing);
            }
        }
        if muted != cur.muted {
            if let Some(ui) = &ui {
                ui.set_muted(muted);
            }
        }
        if v != cur.volume {
            if let Some(ui) = &ui {
                ui.set_volume(v);
            }
        }
        if bit_perfect != cur.bit_perfect {
            if let Some(ui) = &ui {
                ui.set_bit_perfect(bit_perfect);
                // ТЗ-52, §8 С1: бейдж не следует флагу настроек — только status_badge.
                let (bp_active, bp_text) = super::bp_report::status_badge(bit_perfect);
                ui.set_status_bp_active(bp_active);
                ui.set_status_bp_text(bp_text.into());
            }
        }
        if bp_resample != cur.bp_resample {
            if let Some(ui) = &ui {
                ui.set_status_bp_resample(bp_resample);
            }
        }
        if !seeking && (pos_s != cur.pos || seek_f != cur.seek_fraction) {
            if let Some(ui) = &ui {
                ui.set_pos(pos_s.clone().into());
                ui.set_seek_fraction(seek_f);
            }
        }
        let dur_s = playlist::format_duration(dur_f);
        if !seeking && dur_s != cur.dur {
            if let Some(ui) = &ui {
                ui.set_dur(dur_s.clone().into());
            }
        }
        let status_s = self.status.to_string();
        if status_s != cur.status {
            if let Some(ui) = &ui {
                ui.set_status_text(self.status.clone());
            }
        }
        self.last_ui = UiState {
            playing,
            muted,
            volume: v,
            bit_perfect,
            bp_resample,
            pos: pos_s,
            dur: dur_s,
            seek_fraction: seek_f,
            status: status_s,
        };
    }

    fn sync_track_info_to_ui(&mut self) {
        let ui = self.try_ui();
        if let Some(i) = self.current {
            if let Some(t) = self.track_at(i) {
                if let Some(ui) = &ui {
                    ui.set_info_artist(opt_str(&t.artist));
                    ui.set_info_track(fmt_num(t.track_number, t.track_total));
                    ui.set_info_title(if t.title.is_empty() {
                        "—".into()
                    } else {
                        t.title.as_str().into()
                    });
                    ui.set_info_duration(playlist::get_duration_string(t.duration).into());
                    ui.set_info_year(empty_dash(&t.year));
                    ui.set_info_album(opt_str(&t.album));
                    ui.set_info_disc(fmt_num(t.disc, t.disc_total));
                    ui.set_info_genre(empty_dash(t.genre.as_deref().unwrap_or("")));
                    ui.set_info_format(empty_dash(&t.format));
                    ui.set_info_bitrate(if t.bitrate > 0 {
                        format!("{} kbps", t.bitrate).into()
                    } else {
                        "—".into()
                    });
                    ui.set_info_bit_depth(empty_dash(&t.bit_depth));
                    ui.set_info_sample_rate(num_str(t.sample_rate, " Hz"));
                    ui.set_info_channels(num_str(t.channels, " ch"));

                    let mut parts: Vec<String> = Vec::new();
                    if !t.format.is_empty() {
                        parts.push(t.format.clone());
                    }
                    if !t.bit_depth.is_empty() {
                        parts.push(t.bit_depth.clone());
                    }
                    if t.sample_rate > 0 {
                        parts.push(format!("{} Hz", t.sample_rate));
                    }
                    if t.bitrate > 0 {
                        parts.push(format!("{} kbps", t.bitrate));
                    }
                    if t.channels > 0 {
                        parts.push(format!("{} ch", t.channels));
                    }
                    let track_count = format!("{} tracks", self.track_count());
                    ui.set_track_info(parts.join(" \u{2022} ").into());
                    ui.set_track_count(track_count.into());
                }
                self.request_cover(i);
                return;
            }
        }
        if let Some(ui) = &ui {
            ui.set_info_artist("—".into());
            ui.set_info_track("—".into());
            ui.set_info_title("—".into());
            ui.set_info_duration("—".into());
            ui.set_info_year("—".into());
            ui.set_info_album("—".into());
            ui.set_info_disc("—".into());
            ui.set_info_genre("—".into());
            ui.set_info_format("—".into());
            ui.set_info_bitrate("—".into());
            ui.set_info_bit_depth("—".into());
            ui.set_info_sample_rate("—".into());
            ui.set_info_channels("—".into());
            let track_count = format!("{} tracks", self.track_count());
            ui.set_track_info("".into());
            ui.set_track_count(track_count.into());
        }
        self.reset_cover();
    }

    /// Опрос резервирования устройства на тике 100 мс (§8 С1): получено —
    /// отложенное открытие продолжено плеером; отказ захвата или перехват
    /// (`NameLost`) — стоп и сообщение, отката в Shared нет (ТЗ-119, ТЗ-122, И-Р20).
    pub(super) fn handle_reservation(&mut self) {
        let Some(event) = self.player.poll_reservation() else {
            return;
        };
        match event {
            ReservationEvent::Opened => {
                self.stream_desc = self.player.stream_desc().cloned();
                self.status = self.playing_status().into();
                self.sync_track_info_to_ui();
            }
            ReservationEvent::Failed(msg) | ReservationEvent::Lost(msg) => {
                self.player.stop();
                self.emit(AppEvent::PlaybackStopped);
                // Явное действие (воспроизведение) не выполнено — окно Error,
                // не строка состояния (ТЗ-52, ОВ-7; ТЗ-119, ТЗ-122).
                self.push_message(Message {
                    level: MessageLevel::Error,
                    title: "Монопольный режим".into(),
                    body: format!("Не удалось получить устройство: {msg}").into(),
                    buttons: MessageButtons::Ok,
                });
            }
        }
    }

    /// Строка состояния для текущего трека.
    fn playing_status(&self) -> String {
        let Some(track) = self.current.and_then(|i| self.track_at(i)) else {
            return String::new();
        };
        let artist = track.artist.as_deref().unwrap_or("");
        format!("Playing: {artist}\u{2014}{}", track.title)
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
                    let next = self.next_track_index(self.repeat);
                    match next {
                        Some(idx) => self.play_track(idx),
                        None => {
                            self.player.clear_end();
                            // End of playlist: free an exclusive raw-`hw:` node
                            // so the device returns to the system mixer
                            // (V5.1-B5).
                            self.player.release_if_exclusive();
                            self.emit(AppEvent::PlaybackStopped);
                        }
                    }
                }
            }
        }
    }

    /// Индекс следующего трека с учётом shuffle и `repeat`. При shuffle —
    /// через `AppCore`: `shuffle_first` (первый несыгранный трек прохода,
    /// §3.4, ТЗ-46) и `shuffle_started` (переводит его в текущий прохода,
    /// ТЗ-45), индекс — через `index_of` по видимому порядку. `repeat`
    /// передаётся отдельно от `self.repeat`, чтобы вызывающий код (пропуск
    /// повреждённого трека, ТЗ-86/ТЗ-87) мог подставить `RepeatMode::All`
    /// вместо `One` для последовательного порядка — иначе пропуск
    /// зациклился бы на одном и том же треке; для shuffle это
    /// переопределение не действует, так как `shuffle_first` использует
    /// режим повтора сессии в `AppCore` (ограничение временного моста,
    /// §6.13). Вынесено из `handle_auto_advance`, используется также из
    /// `dispatch_applied` (мост С3).
    pub(super) fn next_track_index(&mut self, repeat: RepeatMode) -> Option<usize> {
        if self.shuffle {
            let id = self.core.shuffle_first()?;
            self.core.shuffle_started(id);
            self.core.playlist().index_of(id)
        } else {
            playlist::advance_index(self.current, 1, self.track_count(), repeat)
        }
    }

    /// Request a cover for the track; bumps `cover_gen` so stale results are
    /// discarded when `drain_cover` applies them.
    fn request_cover(&mut self, index: usize) {
        let Some(track) = self.track_at(index).cloned() else {
            return;
        };
        self.cover_gen = self.cover_gen.wrapping_add(1);
        if let Some(tx) = &self.cover_tx {
            let cfg = cover::CoverConfig::from_settings(&self.core.settings().covers);
            let _ = tx.send(CoverJob {
                id: self.cover_gen,
                track,
                cfg,
            });
        }
    }

    /// Clear the displayed cover and invalidate any in-flight request.
    pub(super) fn reset_cover(&mut self) {
        self.cover_gen = self.cover_gen.wrapping_add(1);
        if let Some(ui) = self.try_ui() {
            ui.set_cover_art(slint::Image::default());
        }
    }

    /// Apply cover results from the worker, keeping only the most recent one
    /// (and only if it is newer than the last requested id).
    pub(super) fn drain_cover(&mut self) {
        if self.cover_rx.is_none() {
            return;
        }
        let mut latest: Option<Option<std::path::PathBuf>> = None;
        let mut latest_id = 0u64;
        while let Some(rx) = &self.cover_rx {
            let msg = rx.try_recv();
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
        if let Some(ui) = self.try_ui() {
            ui.set_cover_art(img);
        }
        self.emit(AppEvent::CoverChanged);
    }

    pub fn play_track(&mut self, index: usize) {
        let Some(track) = self.track_at(index) else {
            return;
        };
        let path = track.path.clone();
        let prev_current = self.current;
        match self.player.open(&path) {
            // Открытие асинхронное: успех здесь — команда отправлена
            // движку, применяется по событию Opened (ADR-01, ТЗ-103).
            Ok(()) => self.pending_open = Some((index, prev_current)),
            Err(e) => self.on_open_failed(index, &e),
        }
    }

    /// Применить результат успешного открытия трека: метаданные, текущий
    /// индекс, запуск, синхронизация UI. Вынесено для асинхронного open
    /// (ADR-01, ТЗ-103).
    pub(super) fn on_track_opened(
        &mut self,
        index: usize,
        prev_current: Option<usize>,
        info: &TrackInfo,
    ) {
        let Some(&id) = self.core.playlist().visible().get(index) else {
            return;
        };
        let Some(track) = self.core.playlist().get(id) else {
            return;
        };
        let mut track = track.clone();
        track.duration = info.num_frames.map(|n| n as f64 / info.sample_rate as f64);
        if let Some(t) = &info.tags.title {
            if !t.trim().is_empty() {
                track.title = t.clone();
            }
        }
        track.artist = info.tags.artist.clone();
        track.album = info.tags.album.clone();
        track.genre = info.tags.genre.clone();
        track.year = info.tags.year.clone().unwrap_or_default();
        self.stream_desc = self.player.stream_desc().cloned();
        if info.tags.track_number > 0 || track.track_number == 0 {
            track.track_number = info.tags.track_number;
        }
        if info.tags.track_total > 0 {
            track.track_total = info.tags.track_total;
        }
        if info.tags.disc_number > 0 {
            track.disc = info.tags.disc_number;
        }
        if info.tags.disc_total > 0 {
            track.disc_total = info.tags.disc_total;
        }
        track.channels = info.channels as u32;
        track.bitrate = info.bitrate;
        if info.format_name.starts_with("DSD") {
            track.bit_depth = info.format_name.to_lowercase();
        } else {
            track.sample_rate = info.sample_rate;
            track.bit_depth = match info.bits {
                Some(b) if b > 0 => format!("{b} bit"),
                _ => track.bit_depth.clone(),
            };
        }
        // МОСТ (§6.13, ТЗ-42, ТЗ-45): тэги трека, уточнённые движком при
        // открытии, пишутся через модель плейлиста по `TrackId`, а не
        // напрямую в зеркало `tracks` — обновление может сдвинуть строку
        // под активной сортировкой, поэтому индекс пересчитывается заново.
        let keys = CompareKeys::from_track(&track);
        self.playlist_op(move |core| core.playlist_update_tags(id, track, keys));
        self.current = self.core.playlist().index_of(id);
        self.status = if self.player.reservation_pending() {
            // ТЗ-119: ожидание резервирования не блокирует UI.
            "Захват устройства…".into()
        } else {
            self.playing_status().into()
        };
        self.player.play();
        // Only the affected rows change: metadata of the new current
        // track and the `>` marker on the old/new current indices.
        self.refresh_playlist_rows_at(prev_current);
        self.refresh_playlist_rows_at(self.current);
        self.sync_track_info_to_ui();
        if let Some(ui) = self.try_ui() {
            if let Some(cur) = self.current {
                ui.set_current_row(cur as i32);
                if self.core.settings().scroll_to_playing {
                    ui.invoke_scroll_to_row(cur as i32);
                }
            }
        }
        self.emit(AppEvent::TrackChanged(self.current));
        self.emit(AppEvent::PlaybackStarted);
    }

    /// Открытие трека не удалось: остановка и окно Error (ТЗ-52, ОВ-7).
    /// Вынесено для асинхронного open (ADR-01, ТЗ-103).
    pub(super) fn on_open_failed(&mut self, index: usize, err: &str) {
        let title = self
            .track_at(index)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        self.player.stop();
        // Явное действие (воспроизведение трека) не выполнено — окно
        // Error, не строка состояния (ТЗ-52, ОВ-7).
        self.push_message(Message {
            level: MessageLevel::Error,
            title: "Не удалось воспроизвести трек".into(),
            body: format!("{title}: {err}").into(),
            buttons: MessageButtons::Ok,
        });
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.next();
        // Изменение состояния — через `change_state` (И-Т7, §8.1 С3).
        self.core.change_state(Origin::User, StateChange::Repeat(self.repeat));
        // Persisted at exit (save-at-exit).
        if let Some(ui) = self.try_ui() {
            ui.set_repeat(self.repeat == RepeatMode::All);
            ui.set_repeat_one(self.repeat == RepeatMode::One);
        }
    }

    /// Переход на соседний трек: `direction` `1` — «Далее», `-1` — «Назад».
    /// При shuffle — через `AppCore`: вперёд `shuffle_first` (первый
    /// несыгранный трек прохода, §3.4, ТЗ-46), переводится в текущий прохода
    /// через `shuffle_started` (ТЗ-45); назад `shuffle_back` уже переводит
    /// текущий прохода сам (§3.4, ТЗ-45), повторный `shuffle_started` не
    /// нужен. Индекс — через `index_of` по видимому порядку.
    pub(super) fn play_next(&mut self, direction: i32) {
        if self.track_count() == 0 {
            return;
        }
        let next = if self.shuffle {
            let id = if direction >= 0 {
                let id = self.core.shuffle_first();
                if let Some(id) = id {
                    self.core.shuffle_started(id);
                }
                id
            } else {
                self.core.shuffle_back()
            };
            id.and_then(|id| self.core.playlist().index_of(id))
        } else {
            playlist::advance_index(self.current, direction, self.track_count(), self.repeat)
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

    /// Play Now после загрузки плейлиста командой «Загрузить плейлист»
    /// (ADR-16, §6.12, ТЗ-13 б): прежний трек останавливается и больше не
    /// действует (отложенное открытие сбрасывается — отвечающий на него
    /// `Opened` прежнего плейлиста будет отброшен), затем запускается
    /// `first` — первый по ключу сортировки трек нового плейлиста, либо
    /// shuffle-пик при включённом Shuffle (AppCore уже начал новый проход).
    /// Пустой плейлист (`first: None`) — стоп без следующего трека (ТЗ-48).
    pub(super) fn apply_play_now(&mut self, first: Option<TrackId>) {
        self.player.stop();
        self.pending_open = None;
        self.current = None;
        self.reset_cover();
        let Some(id) = first else {
            if let Some(ui) = self.try_ui() {
                ui.set_current_row(-1);
            }
            self.emit(AppEvent::PlaybackStopped);
            return;
        };
        if self.shuffle {
            self.core.shuffle_started(id);
        }
        if let Some(index) = self.core.playlist().index_of(id) {
            self.play_track(index);
        }
    }

    pub(super) fn set_output_device(&mut self, name: String) {
        if name.is_empty() || name == "(select device)" {
            return;
        }
        if name == self.core.settings().playback.audio_device {
            return;
        }
        // Устройство — настройка: замена целиком через `set_settings` (И-Т7).
        // Вызывается только из «Сохранить» диалога, который сам пишет
        // `settings.toml` через `save_settings_now` (ТЗ-10, ТЗ-28, §8.1 С4).
        let mut s = self.core.settings().clone();
        s.playback.audio_device = name.clone();
        self.core.set_settings(s);

        let path = self
            .current
            .and_then(|i| self.track_at(i))
            .map(|t| t.path.clone());
        let (_playing, pos, _) = self.player.snapshot();
        match path {
            Some(path) => {
                if let Err(e) = self.player.set_device(name.clone(), Some(&path), pos) {
                    self.audio_error = Some(e.clone());
                    if let Some(ui) = self.try_ui() {
                        ui.set_settings_active_error(e.as_str().into());
                    }
                    // Явное действие (переключение устройства) не выполнено —
                    // окно Error, не строка состояния (ТЗ-52, ОВ-7).
                    self.push_message(Message {
                        level: MessageLevel::Error,
                        title: "Не удалось переключить аудиоустройство".into(),
                        body: e.into(),
                        buttons: MessageButtons::Ok,
                    });
                } else {
                    // The configuration stores the stable device id; present it
                    // to the user by its human-readable name of the opened node.
                    // Успешное переключение — результат виден в диалоге
                    // настроек (settings_active_device), сообщение не нужно
                    // (ТЗ-52, ОВ-7).
                    let human = self.player.device_desc.clone();
                    self.audio_ready = true;
                    self.audio_error = None;
                    self.active_device = human.clone();
                    if let Some(ui) = self.try_ui() {
                        ui.set_settings_active_device(human.into());
                        ui.set_settings_active_error(String::new().into());
                    }
                }
            }
            None => {
                self.player.set_preferred_device(name.clone());
                let human = self
                    .device_display_name(&name)
                    .unwrap_or_else(|| self.player.device_desc.clone());
                self.active_device = human.clone();
                if let Some(ui) = self.try_ui() {
                    ui.set_settings_active_device(human.into());
                }
            }
        }
        self.emit(AppEvent::DeviceChanged);
        self.push_tray_now();
    }
}
