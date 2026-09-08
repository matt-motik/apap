//! Playlist concerns of [`MusicApp`]: adding/removing tracks, folder
//! scanning, persistence of the M3U playlist and column sorting.

use super::*;

impl MusicApp {
    pub(super) fn add_paths(&mut self, paths: Vec<PathBuf>) -> usize {
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
            self.append_playlist_rows(added);
            self.save_playlist();
        }
        added
    }

    pub(super) fn mark_playlist_dirty(&mut self) {
        self.playlist_dirty = true;
    }

    pub(super) fn save_playlist(&mut self) {
        if playlist::save_track_list(&music_player_rs::settings::playlist_path(), &self.tracks) {
            self.playlist_dirty = false;
        }
    }

    pub(super) fn start_folder_scan(&mut self, root: PathBuf) {
        let (tx, rx): (std::sync::mpsc::Sender<ScanMsg>, Receiver<ScanMsg>) = channel();
        self.scan_rx = Some(rx);
        let status_root = root.display().to_string();
        thread::spawn(move || {
            playlist::scan_audio_dir(&root, tx);
        });
        self.status = format!("Scanning folder: {status_root}").into();
    }

    pub(super) fn drain_scan(&mut self) {
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
                            // Rows appear progressively, batch by batch, while
                            // the scan thread keeps producing tracks.
                            self.append_playlist_rows(added);
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
            self.save_playlist();
        }
    }

    pub(super) fn remove_track(&mut self, index: usize) {
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

    pub(super) fn clear_playlist(&mut self) {
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

    pub(super) fn sort_tracks(&mut self, col: ColumnId) {
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

    pub(super) fn apply_sort(&mut self, col: ColumnId, desc: bool) {
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
}