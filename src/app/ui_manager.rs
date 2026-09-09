//! UI-related concerns of [`MusicApp`]: synchronising settings, playlist
//! model and column widths to the Slint window, plus theme application.

use super::*;

impl MusicApp {
    /// Restore the saved window size/position (if any) before the window is shown.
    pub(super) fn apply_window_geometry(&self) {
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
    pub(super) fn save_window_geometry(&mut self) {
        let size = self.ui.window().size();
        let pos = self.ui.window().position();
        let s = &mut self.settings.settings;
        s.win_w = Some(size.width);
        s.win_h = Some(size.height);
        s.win_x = Some(pos.x);
        s.win_y = Some(pos.y);
        self.settings.save();
    }

    /// Build the dialog's column-list model (visibility/order/width display)
    /// from the current settings view (draft while the dialog is open).
    fn dialog_cols_model(&self) -> Vec<ColumnSetting> {
        let s = self.settings_ref();
        let ordered = s.ordered_columns();
        ordered
            .iter()
            .map(|c| {
                let mut cs = ColumnSetting::default();
                cs.index = ordered.iter().position(|x| x == c).unwrap_or(0) as i32;
                cs.label = c.label().into();
                cs.visible = s.column_visible(*c);
                cs.width_pct = s.column_width_pct(*c);
                cs
            })
            .collect()
    }

    /// Full UI sync from the current settings. Used at startup (and harmless
    /// on open, where the draft equals the real settings). Also writes the
    /// live properties `cover-size`/`col-info-w`/`col-gap` — only call this
    /// when those really should change.
    pub(super) fn sync_settings_to_ui(&self) {
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
        self.ui
            .set_settings_cols(ModelRc::from(self.dialog_cols_model().as_slice()));
    }

    /// Refresh only the dialog's Columns list after a draft-only reorder /
    /// visibility toggle. Deliberately does NOT touch the live UI properties
    /// (`cover_size`, `col_info_w`, `col_gap`): those are Edit-Commit and must
    /// change only on Save.
    pub(super) fn sync_dialog_cols(&self) {
        self.ui
            .set_settings_cols(ModelRc::from(self.dialog_cols_model().as_slice()));
    }

    pub(super) fn sync_cover_settings_to_ui(&self) {
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

    /// Kick off an asynchronous enumeration of output devices for the Audio tab.
    ///
    /// The active device / error fields are updated immediately (they do not
    /// depend on the listing); the combo-box model is filled once the worker
    /// thread hands back the device names (see [`drain_audio_devices`]) so
    /// opening the dialog never blocks the UI thread.
    pub(super) fn sync_audio_devices(&mut self) {
        let active = self.active_device.clone();
        self.ui.set_settings_active_device(active.into());
        let err = self.audio_error.clone().unwrap_or_default();
        self.ui.set_settings_active_error(err.into());

        if self.audio_devices_rx.is_some() {
            return;
        }
        let (tx, rx): (std::sync::mpsc::Sender<Vec<SharedString>>, _) = channel();
        self.audio_devices_rx = Some(rx);
        thread::spawn(move || {
            let names: Vec<SharedString> = music_player_rs::audio::output::output_devices()
                .into_iter()
                .map(|(n, _)| n.into())
                .collect();
            let _ = tx.send(names);
        });
        // Show a placeholder on the very first enumeration; on reopen keep the
        // previous list visible until the fresh one lands (no "first item"
        // flash while the active device index is unknown).
        if self.ui.get_settings_devices().row_count() == 0 {
            self.ui
                .set_settings_devices(ModelRc::from([SharedString::from("(loading\u{2026})")].as_slice()));
            self.ui.set_settings_device_idx(-1);
        }
    }

    /// Finish `sync_audio_devices`: apply the device list once the worker
    /// thread has produced it. Polled from the UI tick loop.
    pub(super) fn drain_audio_devices(&mut self) {
        let names = {
            let Some(rx) = &self.audio_devices_rx else { return };
            match rx.try_recv() {
                Ok(names) => names,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.audio_devices_rx = None;
                    return;
                }
            }
        };
        self.audio_devices_rx = None;

        let saved = self.settings_ref().audio_device.clone();

        // Highlight the device actually in use right now; fall back to the
        // configured preference; finally defer to the host default so a fresh
        // install lands on the active device.
        let active = self.active_device.clone();
        let want = if !active.is_empty() {
            Some(active)
        } else if !saved.is_empty() {
            Some(saved.clone())
        } else {
            default_device_name().map(|n| n.into())
        };

        let mut model: Vec<SharedString> = Vec::with_capacity(names.len() + 1);
        let sel: i32 = if let Some(w) = want {
            if let Some(i) = names.iter().position(|n| *n == w) {
                model.extend_from_slice(&names);
                i as i32
            } else {
                model.push("(select device)".into());
                model.extend_from_slice(&names);
                0
            }
        } else {
            model.push("(select device)".into());
            model.extend_from_slice(&names);
            0
        };

        // Order matters: the material ComboBox re-assigns `current-index` on a
        // model change (`changed model => reset-current()`), which breaks the
        // `current-index: root.audio-device-idx` binding. Setting the index
        // *before* the model makes `reset-current` clamp the already-correct
        // value, so the combo ends up highlighting the active device.
        self.ui.set_settings_device_idx(sel);
        self.ui.set_settings_devices(ModelRc::from(model.as_slice()));
    }

    pub(super) fn apply_theme(&self) {
        let c = self.ui.global::<Colors>();
        let dark = self.settings.settings.theme == Theme::Dark;
        self.ui.global::<MaterialPalette>().set_color_scheme(if dark {
            slint::private_unstable_api::re_exports::ColorScheme::Dark
        } else {
            slint::private_unstable_api::re_exports::ColorScheme::Light
        });

        if dark {
            c.set_bg_window(hex_color_lit("#121018"));
            c.set_bg_surface(hex_color_lit("#1a1720"));
            c.set_bg_toolbar(hex_color_lit("#211e28"));
            c.set_bg_elevated(hex_color_lit("#252230"));
            c.set_bg_overlay(hex_color_lit("#00000088"));
            c.set_border_subtle(hex_color_lit("#2d2a38"));
            c.set_border_default(hex_color_lit("#3a3645"));
            c.set_text_primary(hex_color_lit("#e6e1ec"));
            c.set_text_secondary(hex_color_lit("#a9a3b8"));
            c.set_text_tertiary(hex_color_lit("#7c7690"));
            c.set_text_dim(hex_color_lit("#5c5670"));
            c.set_text_on_accent(hex_color_lit("#ffffff"));
            c.set_text_error(hex_color_lit("#f2b8b5"));
            c.set_accent(hex_color_lit("#d0bcff"));
            c.set_accent_container(hex_color_lit("#4f378b"));
            c.set_accent_on(hex_color_lit("#eaddff"));
            c.set_surface_hover(hex_color_lit("#322e3c"));
            c.set_surface_active(hex_color_lit("#3a2f1f"));
            c.set_surface_selected(hex_color_lit("#2d2a38"));
            c.set_viz_1(hex_color_lit("#d35400"));
            c.set_viz_2(hex_color_lit("#f1c40f"));
            c.set_viz_3(hex_color_lit("#e74c3c"));
        } else {
            c.set_bg_window(hex_color_lit("#f8f5fa"));
            c.set_bg_surface(hex_color_lit("#ffffff"));
            c.set_bg_toolbar(hex_color_lit("#f3edf7"));
            c.set_bg_elevated(hex_color_lit("#ffffff"));
            c.set_bg_overlay(hex_color_lit("#00000044"));
            c.set_border_subtle(hex_color_lit("#e4dde8"));
            c.set_border_default(hex_color_lit("#cac4d0"));
            c.set_text_primary(hex_color_lit("#1d1b20"));
            c.set_text_secondary(hex_color_lit("#49454f"));
            c.set_text_tertiary(hex_color_lit("#79747e"));
            c.set_text_dim(hex_color_lit("#938f99"));
            c.set_text_on_accent(hex_color_lit("#ffffff"));
            c.set_text_error(hex_color_lit("#b3261e"));
            c.set_accent(hex_color_lit("#6750a4"));
            c.set_accent_container(hex_color_lit("#eaddff"));
            c.set_accent_on(hex_color_lit("#21005d"));
            c.set_surface_hover(hex_color_lit("#e8e0ec"));
            c.set_surface_active(hex_color_lit("#d0c4db"));
            c.set_surface_selected(hex_color_lit("#e4dde8"));
            c.set_viz_1(hex_color_lit("#b14a00"));
            c.set_viz_2(hex_color_lit("#c4a00a"));
            c.set_viz_3(hex_color_lit("#c0392b"));
        }
    }

    /// Columns currently visible, in display order.
    fn visible_col_ids(&self) -> Vec<ColumnId> {
        self.settings.settings
            .ordered_columns()
            .iter()
            .copied()
            .filter(|c| self.settings.settings.column_visible(*c))
            .collect()
    }

    /// Build a single playlist-table row for track `t` at `index`.
    fn build_row(&self, index: usize, t: &Track) -> ModelRc<StandardListViewItem> {
        let row: Vec<StandardListViewItem> = self
            .visible_col_ids()
            .iter()
            .map(|c| {
                let text = match c {
                    ColumnId::Index => {
                        if self.current == Some(index) {
                            ">".to_string()
                        } else {
                            format!("{}", index + 1)
                        }
                    }
                    ColumnId::TrackNumber => fmt_num(t.track_number, t.track_total).to_string(),
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
                    ColumnId::FileName => t
                        .path
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    ColumnId::FilePath => t.path.to_string_lossy().into_owned(),
                };
                StandardListViewItem::from(text.as_str())
            })
            .collect();
        ModelRc::from(row.as_slice())
    }

    /// Rebuild every row of the playlist model (used on init, sort, deletion,
    /// clear and when the visible column set changes).
    pub(super) fn sync_playlist_to_ui(&mut self) {
        let cols = self.build_table_columns();
        let rows: Vec<ModelRc<StandardListViewItem>> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| self.build_row(i, t))
            .collect();

        self.playlist_rows.set_vec(rows);
        self.playlist_cols.set_vec(cols);
        self.ui.set_current_row(
            self.current.map(|i| i as i32).unwrap_or(-1),
        );
        self.col_model_sig = self.compute_col_sig();
    }

    /// Refresh a single row in place (metadata updates and the `>` marker of
    /// the current track).
    pub(super) fn refresh_playlist_rows_at(&mut self, index: Option<usize>) {
        if let Some(i) = index {
            if self.playlist_rows.row_count() > i {
                if let Some(t) = self.tracks.get(i) {
                    self.playlist_rows.set_row_data(i, self.build_row(i, t));
                }
            }
        }
    }

    /// Resolve the visible `TableColumn`s with current pixel widths.
    fn build_table_columns(&self) -> Vec<TableColumn> {
        let view_w = self.ui.get_playlist_view_width().max(100.0) as f32;

        let ids = self.visible_col_ids();
        let ratios: Vec<f32> = ids
            .iter()
            .map(|c| self.settings.settings.column_width_pct(*c))
            .collect();
        let widths = music_player_rs::playlist_layout::resolve_widths(view_w, &ids, &ratios);

        ids.iter()
            .zip(widths)
            .map(|(c, w)| {
                let mut tc = TableColumn::default();
                tc.title = c.label().into();
                tc.width = w.into();
                tc
            })
            .collect()
    }

    pub(super) fn update_column_widths(&mut self) {
        let cols = self.build_table_columns();
        // Reflow only changes the pixel widths; keep the same ModelRc and touch
        // each column on the fly so window resizing never re-creates the model
        // (no flicker, and the widths never constrain the window size). If the
        // column set/order/visibility differs, fall back to a full replacement.
        if self.playlist_cols.row_count() == cols.len() {
            for (i, c) in cols.iter().enumerate() {
                if let Some(old) = self.playlist_cols.row_data(i) {
                    if old.width != c.width {
                        self.playlist_cols.set_row_data(i, c.clone());
                    }
                }
            }
        } else {
            self.playlist_cols.set_vec(cols);
        }
        self.col_model_sig = self.compute_col_sig();
    }

    pub(super) fn compute_col_sig(&self) -> u64 {
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

    pub(super) fn save_column_widths_from_ui(&mut self) {
        let cols = self.ui.get_playlist_cols();
        let len = cols.row_count();
        if len == 0 {
            return;
        }
        let ordered = self.settings.settings.ordered_columns();
        let visible_cols: Vec<ColumnId> = ordered
            .iter()
            .copied()
            .filter(|c| self.settings.settings.column_visible(*c))
            .collect();
        if visible_cols.len() != len {
            return;
        }
        // Clamp the dragged pixel widths to the per-column bounds before
        // converting them to percentages, so the post-debounce rebuild snaps
        // to the same clamped layout and the column signature stays stable
        // (no save/rebuild ping-pong).
        let view_w = self.ui.get_playlist_view_width().max(100.0);
        let mut px: Vec<f32> = Vec::with_capacity(len);
        for (i, col_id) in visible_cols.iter().enumerate() {
            if let Some(tc) = cols.row_data(i) {
                let lim = music_player_rs::playlist_layout::column_limit(*col_id);
                let max_eff = lim.effective_max(view_w);
                let min_eff = lim.min_px.min(max_eff);
                px.push(tc.width.clamp(min_eff, max_eff));
            }
        }
        let total_px: f32 = px.iter().sum();
        if total_px <= 0.0 {
            return;
        }
        for (col_id, w) in visible_cols.iter().zip(px) {
            let pct = (w / total_px) * 100.0;
            self.settings.settings.column_widths.insert(col_id.key().to_string(), pct);
        }
        // Persisted at exit (save-at-exit).
    }
}