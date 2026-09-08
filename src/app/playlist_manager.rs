//! Playlist concerns of [`MusicApp`]: adding/removing tracks, folder
//! scanning, persistence of the M3U playlist and column sorting.

use super::*;

impl MusicApp {
    /// Start a background scan/import of files and/or folders ("Add Files",
    /// "Add Folder"). The scanner streamed `ScanMsg` batches from a worker
    /// thread; results are buffered and committed atomically in `drain_scan`.
    pub(super) fn start_scan(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if self.scan_rx.is_some() {
            eprintln!("[app] scan already running; ignoring new request");
            return;
        }
        let (tx, rx): (std::sync::mpsc::Sender<ScanMsg>, Receiver<ScanMsg>) = channel();
        self.scan_rx = Some(rx);
        self.status = format!("Adding tracks\u{2026} {} item(s)", paths.len()).into();
        self.ui.set_busy(true);
        thread::spawn(move || {
            playlist::probe_paths(paths, tx);
        });
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
                                self.scan_pending.push(t);
                                added += 1;
                            }
                        }
                        if added > 0 {
                            // User-visible progress only; the model stays
                            // untouched until the whole batch has been scanned.
                            self.status =
                                format!("Scanning\u{2026} {} tracks", self.scan_pending.len()).into();
                        }
                    }
                    Ok(ScanMsg::Done(total)) => {
                        let n = self.scan_pending.len();
                        if n > 0 {
                            let added: Vec<Track> = std::mem::take(&mut self.scan_pending);
                            self.disk_tracks.extend(added.iter().cloned());
                            self.tracks.extend(added);
                            self.mark_playlist_dirty();
                            self.rebuild_shuffle();
                            self.status = format!("Added {n} tracks").into();
                        } else {
                            self.status = format!("Scan finished: nothing new ({total} found)").into();
                        }
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
            self.ui.set_busy(false);
            // Flush leftovers if the stream ended without a final Done message.
            if !self.scan_pending.is_empty() {
                let n = self.scan_pending.len();
                let added: Vec<Track> = std::mem::take(&mut self.scan_pending);
                self.disk_tracks.extend(added.iter().cloned());
                self.tracks.extend(added);
                self.mark_playlist_dirty();
                self.rebuild_shuffle();
                self.status = format!("Added {n} tracks").into();
                self.emit(AppEvent::QueueChanged);
            }
            // Single atomic UI update for the entire scanned batch.
            self.sync_playlist_to_ui();
        }
    }

    pub(super) fn mark_playlist_dirty(&mut self) {
        self.playlist_dirty = true;
    }

    pub(super) fn save_playlist(&mut self) {
        // The on-disk playlist always reflects `disk_tracks` (original load +
        // scanned additions), never the on-screen sort order.
        if playlist::save_track_list(&music_player_rs::settings::playlist_path(), &self.disk_tracks) {
            self.playlist_dirty = false;
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
        let path = t.path.clone();
        self.known_paths.remove(&path);
        self.tracks.remove(index);
        self.disk_tracks.retain(|d| d.path != path);
        self.rebuild_shuffle();
        // Incremental: drop the row, then refresh the shifted tail (indices and
        // the `>` marker) instead of rebuilding the whole model.
        if self.playlist_rows.row_count() > index {
            self.playlist_rows.remove(index);
            for i in index..self.tracks.len() {
                self.refresh_playlist_rows_at(Some(i));
            }
        }
        self.mark_playlist_dirty();
        self.status = "Track removed".into();
        self.emit(AppEvent::QueueChanged);
    }

    pub(super) fn clear_playlist(&mut self) {
        self.player.stop();
        self.current = None;
        self.reset_cover();
        self.tracks.clear();
        self.disk_tracks.clear();
        self.known_paths.clear();
        self.rebuild_shuffle();
        self.mark_playlist_dirty();
        self.sync_playlist_to_ui();
        self.status = "Playlist cleared".into();
        self.emit(AppEvent::QueueChanged);
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
        self.emit(AppEvent::QueueChanged);
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