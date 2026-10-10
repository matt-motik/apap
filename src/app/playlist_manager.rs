//! Playlist concerns of [`MusicApp`]: adding/removing tracks, folder
//! scanning, persistence of the M3U playlist and column sorting.

use super::*;
use music_player_rs::playlist::compare::CompareKeys;

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
        if let Some(ui) = self.try_ui() {
            ui.set_busy(true);
        }
        // Список недоступен для ТЗ-48 команд до конца фонового сканирования.
        self.gate.set_loading(Some(LoadKind::Command));
        thread::spawn(move || {
            playlist::probe_paths(paths, tx);
        });
    }

    pub(super) fn drain_scan(&mut self) {
        let mut finished = false;
        if self.scan_rx.is_some() {
            while let Some(msg) = self.scan_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                match msg {
                    ScanMsg::Batch(tracks) => {
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
                    ScanMsg::Done(total) => {
                        let n = self.scan_pending.len();
                        if n > 0 {
                            let added: Vec<Track> = std::mem::take(&mut self.scan_pending);
                            // МОСТ (§6.13, ТЗ-42, ТЗ-45): добавление через
                            // модель плейлиста; флаг «изменён» взводит сам
                            // эффект `playlist_add`.
                            let rows: Vec<(Track, CompareKeys)> = added
                                .into_iter()
                                .map(|t| {
                                    let keys = CompareKeys::from_track(&t);
                                    (t, keys)
                                })
                                .collect();
                            self.playlist_op(|core| {
                                core.playlist_add(rows);
                            });
                            // Успешное добавление — результат виден в таблице
                            // плейлиста, сообщение не требуется (ТЗ-52, ОВ-7).
                        } else {
                            self.status = format!("Scan finished: nothing new ({total} found)").into();
                        }
                        finished = true;
                    }
                }
            }
            if self
                .scan_rx
                .as_ref()
                .is_some_and(|rx| matches!(rx.try_recv(), Err(TryRecvError::Disconnected)))
            {
                finished = true;
            }
        }
        if finished {
            self.scan_rx = None;
            if let Some(ui) = self.try_ui() {
                ui.set_busy(false);
            }
            self.gate.set_loading(None);
            // Flush leftovers if the stream ended without a final Done message.
            if !self.scan_pending.is_empty() {
                let added: Vec<Track> = std::mem::take(&mut self.scan_pending);
                // МОСТ (§6.13, ТЗ-42, ТЗ-45): добавление через модель
                // плейлиста; флаг «изменён» взводит сам эффект `playlist_add`.
                let rows: Vec<(Track, CompareKeys)> = added
                    .into_iter()
                    .map(|t| {
                        let keys = CompareKeys::from_track(&t);
                        (t, keys)
                    })
                    .collect();
                self.playlist_op(|core| {
                    core.playlist_add(rows);
                });
                // Успешное добавление — результат виден в таблице плейлиста,
                // сообщение не требуется (ТЗ-52, ОВ-7).
                self.emit(AppEvent::QueueChanged);
            }
            // Single atomic UI update for the entire scanned batch.
            self.sync_playlist_to_ui();
        }
    }

    /// Немедленная запись `playlist.m3u` снимком через писатель `apap-persist`
    /// (ТЗ-22, §6.8). На диске — всегда исходный (файловый) порядок модели
    /// плейлиста `AppCore` (§4.2, ТЗ-12), а не порядок сортировки на экране.
    /// Результат приходит ответом писателя в `tick` и обновляет окно ошибок
    /// записи (ТЗ-20).
    pub(super) fn remove_track(&mut self, index: usize) {
        if index >= self.track_count() {
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
        // МОСТ (§6.13, ТЗ-42, ТЗ-45): удаление через модель плейлиста по
        // `TrackId`; флаг «изменён» взводит сам эффект `playlist_remove`.
        let id = self.core.playlist().visible()[index];
        self.playlist_op(|core| core.playlist_remove(&[id]));
        // Incremental: drop the row, then refresh the shifted tail (indices and
        // the `>` marker) instead of rebuilding the whole model.
        if self.playlist_rows.row_count() > index {
            self.playlist_rows.remove(index);
            for i in index..self.track_count() {
                self.refresh_playlist_rows_at(Some(i));
            }
        }
        // Успешное удаление — строка уже исчезла из таблицы плейлиста,
        // сообщение не требуется (ТЗ-52, ОВ-7).
        self.emit(AppEvent::QueueChanged);
    }

    pub(super) fn clear_playlist(&mut self) {
        self.player.stop();
        self.current = None;
        self.reset_cover();
        // МОСТ (§6.13, ТЗ-42, ТЗ-45): очистка через модель плейлиста; флаг
        // «изменён» взводит сам эффект `playlist_clear`.
        self.playlist_op(|core| core.playlist_clear());
        self.sync_playlist_to_ui();
        // Успешная очистка — таблица плейлиста уже пуста, сообщение не
        // требуется (ТЗ-52, ОВ-7).
        self.emit(AppEvent::QueueChanged);
    }

    pub(super) fn sort_tracks(&mut self, col: ColumnId) {
        // МОСТ (§6.13, ТЗ-42, ТЗ-45): щелчок по заголовку через модель
        // плейлиста — цикл Asc → Desc → «нет ключа» и ключ сессии ведёт
        // сама модель (`playlist_header_click`/`apply_playlist_effect`).
        self.playlist_op(|core| core.playlist_header_click(col));
        self.sync_playlist_to_ui();
        self.emit(AppEvent::QueueChanged);
    }
}