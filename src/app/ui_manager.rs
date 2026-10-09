//! UI-related concerns of [`MusicApp`]: synchronising settings, playlist
//! model and column widths to the Slint window, plus theme application.

use super::*;

use music_player_rs::audio::output::DeviceCategory;
use music_player_rs::persist::state_file::{
    effective_width_pct, Origin, PhysPos, PhysSize, SizeUnits, StateChange,
    WindowGeometry,
};
use music_player_rs::playlist::model::SortDir;
use music_player_rs::theme::StandardPalette;
use music_player_rs::persist::settings_file::SaveInterval;

/// Значения N в порядке пунктов списка диалога (ТЗ-33, И-Т3).
pub(super) const SAVE_INTERVALS: [SaveInterval; 4] =
    [SaveInterval::S10, SaveInterval::S30, SaveInterval::S60, SaveInterval::S120];

impl MusicApp {
    /// `true`, если окно в данный момент работает под нативным Wayland
    /// (ADR-22, §6.17): композитор не сообщает клиенту положение окна на
    /// экране, поэтому `window().position()` всегда возвращает (0,0), а
    /// `set_position()` молча игнорируется. До первого `show()` хэндл ещё
    /// недоступен — в этом случае, как и при любой другой ошибке получения
    /// хэндла, считаем сессию не-Wayland (`false`).
    fn is_wayland_window(&self) -> bool {
        use raw_window_handle::HasWindowHandle;

        let Some(ui) = self.try_ui() else { return false };
        let slint_handle = ui.window().window_handle();
        let Ok(handle) = slint_handle.window_handle() else {
            return false;
        };
        matches!(handle.as_raw(), raw_window_handle::RawWindowHandle::Wayland(_))
    }

    /// Restore the saved window size/position (if any) before the window is shown.
    /// Also re-applied from `main` after `show()` (surface exists) so winit
    /// does not collapse the window to its content minimum (V5.1-B7).
    /// Размер восстанавливается в тех же единицах, в которых он сохранён
    /// (`size_units`): на Wayland — логические px, поскольку масштаб
    /// композитора до первого `configure` ещё не известен и восстановление
    /// физического размера под масштабом 1.0 даёт видимый скачок при
    /// последующем приходе реального масштаба (ADR-22, SP1.0-B4, §6.17).
    /// Ширина дублируется в `initial-width` (предпочтительная ширина окна):
    /// при создании winit-окна Slint 1.17 сбрасывает ширину элемента окна к
    /// предпочтительной, и на Wayland, где окно создаётся уже в цикле
    /// событий, без этого ширина схлопывалась до минимума содержимого.
    pub(crate) fn apply_window_geometry(&self) {
        let Some(ui) = self.try_ui() else { return };
        let win = self.core.state().window();
        if let Some(size) = win.size {
            if (200..=8000).contains(&size.width) && (200..=8000).contains(&size.height) {
                match win.size_units {
                    SizeUnits::Logical => {
                        // Диапазон выше (200..=8000) гарантирует, что оба
                        // значения помещаются в u16 — конверсия в f32 точная.
                        if let (Ok(w), Ok(h)) =
                            (u16::try_from(size.width), u16::try_from(size.height))
                        {
                            ui.set_initial_width(f32::from(w));
                            ui.window().set_size(slint::WindowSize::Logical(
                                slint::LogicalSize::new(f32::from(w), f32::from(h)),
                            ));
                        }
                    }
                    SizeUnits::Physical => {
                        // До показа Slint и так трактует размер как логический
                        // под масштабом 1.0 — та же ширина идёт в `initial-width`.
                        if let Ok(w) = u16::try_from(size.width) {
                            ui.set_initial_width(f32::from(w));
                        }
                        ui.window().set_size(slint::WindowSize::Physical(
                            slint::PhysicalSize::new(size.width, size.height),
                        ));
                    }
                }
            }
        } else {
            // No persisted geometry yet (first run): give the window a sensible
            // logical size. The Window no longer carries a fixed
            // `preferred-width/height`, so it can be resized freely and fast;
            // without this the first-run window would collapse to its content
            // minimum.
            ui.set_initial_width(1200.0);
            ui
                .window()
                .set_size(slint::WindowSize::Logical(slint::LogicalSize::new(1200.0, 760.0)));
        }
        // На Wayland позиция окна недоступна программе (ADR-22, §6.17):
        // `set_position` композитор молча игнорирует, поэтому не вызываем его
        // вовсе — чтобы не создавать ложное впечатление восстановленной
        // геометрии.
        if let Some(pos) = win.position {
            if !self.is_wayland_window() {
                ui.window().set_position(slint::WindowPosition::Physical(
                    slint::PhysicalPosition::new(pos.x, pos.y),
                ));
            }
        }
        // Восстанавливаем состояние окна поверх обычной геометрии: сначала
        // нормальный размер/позиция, затем максимизация и fullscreen.
        if win.maximized {
            ui.window().set_maximized(true);
        }
        if win.fullscreen {
            ui.window().set_fullscreen(true);
        }
    }

    /// Текущее показание геометрии окна для `AppCore::tick` (ADR-22, §6.17):
    /// флаги fullscreen/maximized и, только в обычном состоянии окна, размер
    /// и позиция. В fullscreen/maximized `size()` — размер во весь экран,
    /// который нельзя восстанавливать как размер окна, поэтому остаётся
    /// последний «нормальный» размер из состояния. `None` — окно скрыто
    /// (в трее), показания нет. На Wayland размер снимается в логических px
    /// (размеры поверхности Wayland логические, а масштаб до первого
    /// `configure` композитора ещё не известен) — иначе в физических
    /// (ADR-22, SP1.0-B4, §6.17).
    pub(super) fn window_geometry(&self) -> Option<WindowGeometry> {
        let ui = self.try_ui()?;
        let w = ui.window();
        if !w.is_visible() {
            return None;
        }
        let mut geom = self.core.state().window();
        geom.fullscreen = w.is_fullscreen();
        geom.maximized = w.is_maximized();
        if !geom.fullscreen && !geom.maximized {
            if self.is_wayland_window() {
                let s = w.size().to_logical(w.scale_factor());
                // Клампинг в диапазон u32 перед конверсией делает `as u32`
                // безопасным (без переполнения/обёртывания).
                geom.size = Some(PhysSize {
                    width: s.width.clamp(1.0, 65535.0).round() as u32,
                    height: s.height.clamp(1.0, 65535.0).round() as u32,
                });
                geom.size_units = SizeUnits::Logical;
            } else {
                let size = w.size();
                geom.size = Some(PhysSize { width: size.width, height: size.height });
                geom.size_units = SizeUnits::Physical;
            }
            // На Wayland `position()` не отражает реальное положение окна
            // (композитор его не сообщает), поэтому позицию не читаем и не
            // перезаписываем персистентное значение — оно остаётся прежним
            // (ADR-22, §6.17).
            if !self.is_wayland_window() {
                let pos = w.position();
                geom.position = Some(PhysPos { x: pos.x, y: pos.y });
            }
        }
        Some(geom)
    }

    /// Build the dialog's column-list model (visibility/order/width display)
    /// from the current settings view (draft while the dialog is open),
    /// reading widths from session state via `effective_width_pct` (§8.1 С3).
    fn dialog_cols_model(&self) -> Vec<ColumnSetting> {
        let cfg = self.cfg();
        let widths = self.cfg_column_widths();
        let ordered = cfg.columns.ordered_columns();
        ordered
            .iter()
            .map(|c| {
                ColumnSetting {
                    index: ordered.iter().position(|x| x == c).unwrap_or(0) as i32,
                    label: cfg.columns.column_title(*c).into(),
                    visible: cfg.columns.column_visible(*c),
                    width_pct: effective_width_pct(&cfg.columns, widths, *c),
                }
            })
            .collect()
    }

    /// Full UI sync from the current settings. Used at startup (and harmless
    /// on open, where the draft equals the real settings). Also writes the
    /// live properties `cover-size`/`col-info-w`/`col-gap` — only call this
    /// when those really should change.
    pub(super) fn sync_settings_to_ui(&self) {
        let s = self.cfg();
        let Some(ui) = self.try_ui() else { return };
        ui.set_theme_palette(if s.theme.as_str() == "dark" { 0 } else { 1 });
        ui
            .set_settings_minimize(s.minimize_to_tray);
        let n_idx = SAVE_INTERVALS.iter().position(|n| *n == s.save_interval).unwrap_or(1);
        ui.set_settings_save_interval_idx(i32::try_from(n_idx).unwrap_or(1));
        // Запрет автозаписи settings.toml действует весь сеанс (ОВС-6 в, И-Р20, §2.13).
        ui.set_settings_save_notice(self.core.settings_save_notice().unwrap_or("").into());
        ui
            .set_cover_size(s.top_panel.cover_size);
        ui
            .set_col_info_w(s.top_panel.col_info_w);
        ui.set_col_gap(s.top_panel.col_gap);
        let labels: Vec<SharedString> = s
            .info_labels_ordered()
            .into_iter()
            .map(SharedString::from)
            .collect();
        ui.set_info_labels(ModelRc::from(labels.as_slice()));
        ui.set_shuffle(self.shuffle);
        ui.set_repeat(self.repeat == RepeatMode::All);
        ui.set_repeat_one(self.repeat == RepeatMode::One);
        ui
            .set_viz_mode(self.cfg_viz_mode().index());
        ui
            .set_settings_cols(ModelRc::from(self.dialog_cols_model().as_slice()));
        self.sync_dsd_settings_to_ui();
    }

    /// Синхронизация статистики кэша (ТЗ-22, ADR-20, §10.5): RAM
    /// визуализации (счёт в UI-потоке) + дисковые размеры визуализации и
    /// обложек из ответа потока `apap-io` (`CacheSizes`).
    pub(super) fn apply_cache_sizes(&self, disk: &music_player_rs::core::io::CacheSizes) {
        let ram = self.ram_cache_size();
        let Some(ui) = self.try_ui() else { return };
        ui
            .set_settings_cache_ram_size(music_player_rs::audio::fulltrack::fmt_cache_bytes(ram).into());
        ui
            .set_settings_cache_viz_size(music_player_rs::audio::fulltrack::fmt_cache_bytes(disk.viz_disk).into());
        ui
            .set_settings_cache_cover_size(music_player_rs::audio::fulltrack::fmt_cache_bytes(disk.covers).into());
    }

    /// Синхронизация DSD-полей диалога настроек: текущий режим (0=PCM,
    /// 1=Native, 2=DoP) и признак конфликта «DSD→PCM + bit-perfect» (§8.4).
    pub(super) fn sync_dsd_settings_to_ui(&self) {
        let s = self.cfg();
        let Some(ui) = self.try_ui() else { return };
        ui.set_settings_dsd_mode(s.playback.dsd.mode.index());
        ui.set_settings_dsd_bp_warn(s.playback.dsd_pcm_breaks_bit_perfect());
        ui.set_settings_bit_perfect(s.playback.audio.bit_perfect);
    }

    /// ТЗ §7.5: актуализировать индикатор статус-бара «Не bit-perfect
    /// (DSD→PCM)» по текущему треку. Вызывается в `tick()`, поэтому всегда
    /// отражает последний выбранный трек и live-настройки.
    pub(super) fn sync_dsd_status_ui(&self) {
        let warn = self.current_track_is_dsd()
            && self.cfg().playback.dsd_pcm_breaks_bit_perfect();
        if let Some(ui) = self.try_ui() {
            ui.set_status_dsd_not_bp(warn);
        }
    }

    /// True, если текущий трек — DSD (DSF/DFF по расширению в `Track.format`).
    pub(super) fn current_track_is_dsd(&self) -> bool {
        let Some(i) = self.current else {
            return false;
        };
        match self.track_at(i) {
            Some(t) => {
                let fmt = t.format.to_ascii_lowercase();
                fmt == "dsf" || fmt == "dff"
            }
            None => false,
        }
    }

    /// Refresh only the dialog's Columns list after a draft-only reorder /
    /// visibility toggle. Deliberately does NOT touch the live UI properties
    /// (`cover_size`, `col_info_w`, `col_gap`): those are Edit-Commit and must
    /// change only on Save.
    pub(super) fn sync_dialog_cols(&self) {
        if let Some(ui) = self.try_ui() {
            ui.set_settings_cols(ModelRc::from(self.dialog_cols_model().as_slice()));
        }
    }

    pub(super) fn sync_cover_settings_to_ui(&self) {
        let s = self.cfg();
        let covers: Vec<CoverSetting> = s
            .covers
            .priority
            .iter()
            .enumerate()
            .map(|(i, c)| {
                CoverSetting {
                    label: c.label().into(),
                    pos: i as i32,
                }
            })
            .collect();
        let Some(ui) = self.try_ui() else { return };
        ui.set_settings_covers(ModelRc::from(covers.as_slice()));
        let names = s.covers.cover_folder_names_list().join(", ");
        ui.set_settings_cover_names(names.into());
        ui.set_settings_cover_online(s.covers.online);
    }

    /// Kick off an asynchronous enumeration of output devices for the Audio tab.
    ///
    /// The active device / error fields are updated immediately (they do not
    /// depend on the listing); the combo-box model is filled once the worker
    /// thread hands back the device names (see [`drain_audio_devices`]) so
    /// opening the dialog never blocks the UI thread.
    pub(super) fn sync_audio_devices(&mut self) {
        let active = self.active_device.clone();
        let ui = self.try_ui();
        if let Some(ui) = &ui {
            ui.set_settings_active_device(active.into());
            let err = self.audio_error.clone().unwrap_or_default();
            ui.set_settings_active_error(err.into());
            // Отразить фильтры из текущего draft-снапшота (ТЗ A3.0 §8.1): тумблеры
            // в диалоге — live-превью, показывают то, что применено к списку.
            ui
                .set_settings_audio_filter_hardware(self.cfg().playback.audio.filter_hardware_only);
            ui
                .set_settings_audio_filter_stereo(self.cfg().playback.audio.filter_stereo_only);
        }

        if self.audio_devices_rx.is_some() {
            return;
        }
        // Воркер отдаёт полные инфосы один раз; из них диалог строит и пары
        // `(id, label)` для ComboBox, и превью capabilities/validation. Тогглы
        // фильтров при этом никогда не ходят к бэкенду заново.
        let (tx, rx): (std::sync::mpsc::Sender<Vec<DeviceInfo>>, _) = channel();
        self.audio_devices_rx = Some(rx);
        thread::spawn(move || {
            let infos = music_player_rs::audio::output::output_device_infos();
            let _ = tx.send(infos);
        });
        // Show a placeholder on the very first enumeration; on reopen keep the
        // previous list visible until the fresh one lands (no "first item"
        // flash while the active device index is unknown).
        if let Some(ui) = &ui {
            if ui.get_settings_devices().row_count() == 0 {
                ui
                    .set_settings_devices(ModelRc::from([SharedString::from("(loading\u{2026})")].as_slice()));
                ui.set_settings_device_idx(-1);
            }
        }
    }

    /// Finish `sync_audio_devices`: apply the device list once the worker
    /// thread has produced it. Polled from the UI tick loop.
    pub(super) fn drain_audio_devices(&mut self) {
        let infos = {
            let Some(rx) = &self.audio_devices_rx else { return };
            match rx.try_recv() {
                Ok(infos) => infos,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.audio_devices_rx = None;
                    return;
                }
            }
        };
        self.audio_devices_rx = None;
        // Keep the previously-known listing when a transient enumeration comes
        // back empty (a DAC briefly busy with a probe/another stream must not
        // "disappear" between two Refreshes).
        if infos.is_empty() && !self.audio_device_infos.is_empty() {
            eprintln!("[audio] enumeration returned no devices; keeping previous list");
        } else {
            self.audio_device_infos = infos;
            self.audio_devices_pairs =
                music_player_rs::audio::output::device_pairs_from_infos(&self.audio_device_infos);
        }
        self.apply_audio_device_listing();
    }

    /// Re-project the *already fetched* device list through the current draft
    /// filters and refresh the ComboBox model/highlight plus the capabilities
    /// and validation preview (§8.1 С3). Synchronous — called when an
    /// enumeration lands and on every filter toggle, so flipping a filter never
    /// needs a backend requery.
    fn apply_audio_device_listing(&mut self) {
        let s = self.cfg();
        let filtered: Vec<&DeviceInfo> = self
            .audio_device_infos
            .iter()
            .filter(|d| {
                audio_filter_matches(d, s.playback.audio.filter_hardware_only, s.playback.audio.filter_stereo_only)
            })
            .collect();

        // Пустой результат после фильтров: вместо молчаливого пустого списка
        // показываем плейсхолдер и обнуляем превью, чтобы диалог не держал
        // устаревшие данные скрытого устройства (ТЗ A3.0 §8.1).
        if filtered.is_empty() {
            if let Some(ui) = self.try_ui() {
                ui.set_settings_devices(
                    ModelRc::from(
                        [SharedString::from("(нет устройств, удовлетворяющих фильтру)")].as_slice(),
                    ),
                );
                ui.set_settings_device_idx(-1);
                ui.set_settings_audio_caps(ModelRc::from(&[][..]));
                ui.set_settings_audio_validation(ModelRc::from(&[][..]));
            }
            return;
        }

        // Локальная проекция отфильтрованных имён в пары `(id, label)` — те же
        // правила, что у `device_pairs_from_infos` (первый id на имя,
        // SERVER_NODE_SUFFIX). Ярлыки — подмножество полного списка, поэтому
        // `resolve_device_label` продолжает корректно мапить их на id.
        let names: Vec<&str> = filtered.iter().map(|d| d.name.as_str()).collect();
        let pairs = music_player_rs::audio::output::label_device_names(&names)
            .into_iter()
            .map(|(name, label)| {
                let id = filtered
                    .iter()
                    .find(|d| d.name == name)
                    .map(|d| d.id.as_str())
                    .unwrap_or(name.as_str());
                (id.to_string(), label)
            })
            .collect::<Vec<_>>();

        let saved = self.cfg().playback.audio_device.clone();
        let active = self.active_device.clone();
        let want = if !active.is_empty() {
            Some(active)
        } else if !saved.is_empty() {
            Some(saved.clone())
        } else {
            default_device_name()
        };

        // The ComboBox model holds *labels*; selection is matched back to the
        // stable device id on the Rust side (`resolve_device_label`).
        let mut model: Vec<SharedString> = Vec::with_capacity(pairs.len() + 1);
        let sel: i32 = if pairs.is_empty() {
            model.push("(no devices \u{2014} retry Refresh)".into());
            0
        } else if let Some(w) = want {
            if let Some(i) = find_device_index_in(&pairs, &w) {
                model.extend(pairs.iter().map(|(_, label)| SharedString::from(label.as_str())));
                i as i32
            } else {
                model.push("(select device)".into());
                model.extend(pairs.iter().map(|(_, label)| SharedString::from(label.as_str())));
                0
            }
        } else {
            model.push("(select device)".into());
            model.extend(pairs.iter().map(|(_, label)| SharedString::from(label.as_str())));
            0
        };

        // Order matters: the material ComboBox re-assigns `current-index` on a
        // model change (`changed model => reset-current()`), which breaks the
        // `current-index: root.audio-device-idx` binding. Setting the index
        // *before* the model makes `reset-current` clamp the already-correct
        // value, so the combo ends up highlighting the active device.
        if let Some(ui) = self.try_ui() {
            ui.set_settings_device_idx(sel);
            ui.set_settings_devices(ModelRc::from(model.as_slice()));
        }
        self.sync_capabilities_and_validation();
    }

    /// Translate a ComboBox *label* (the deduplicated/grouped display string,
    /// possibly with a server-node suffix) back to the stable device id that
    /// the settings and the audio backend use. Empty when the label is unknown
    /// (e.g. the "(select device)" placeholder).
    pub(super) fn resolve_device_label(&self, label: &str) -> String {
        self.audio_devices_pairs
            .iter()
            .find(|(_, l)| l == label)
            .map(|(raw, _)| raw.clone())
            .unwrap_or_default()
    }

    /// Human-readable display label for a stable device id (the grid label
    /// without the server-node suffix). `None` when the id is unknown.
    pub(super) fn device_display_name(&self, id: &str) -> Option<String> {
        self.audio_devices_pairs
            .iter()
            .find(|(raw, _)| raw == id)
            .map(|(_, label)| {
                label
                    .strip_suffix(music_player_rs::audio::output::SERVER_NODE_SUFFIX)
                    .unwrap_or(label)
                    .to_string()
            })
    }

    pub(super) fn apply_theme(&self, theme: &ThemeData) {
        if validate_colors(&theme.colors).is_err() {
            eprintln!("[theme] apply_theme: невалидные HEX-цвета, применение пропущено");
            return;
        }
        let Some(ui) = self.try_ui() else { return };
        let c = ui.global::<Colors>();
        ui.global::<FluentPalette>().set_color_scheme(match theme.standard_palette {
            StandardPalette::Dark => slint::private_unstable_api::re_exports::ColorScheme::Dark,
            StandardPalette::Light => slint::private_unstable_api::re_exports::ColorScheme::Light,
        });

        let col = &theme.colors;
        c.set_bg_window(hex_color(&col.bg_window).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_bg_surface(hex_color(&col.bg_surface).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_bg_toolbar(hex_color(&col.bg_toolbar).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_bg_elevated(hex_color(&col.bg_elevated).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_bg_overlay(hex_color(&col.bg_overlay).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_border_subtle(hex_color(&col.border_subtle).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_border_default(hex_color(&col.border_default).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_text_primary(hex_color(&col.text_primary).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_text_secondary(hex_color(&col.text_secondary).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_text_tertiary(hex_color(&col.text_tertiary).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_text_dim(hex_color(&col.text_dim).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_text_on_accent(hex_color(&col.text_on_accent).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_text_error(hex_color(&col.text_error).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_accent(hex_color(&col.accent).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_accent_container(hex_color(&col.accent_container).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_accent_on(hex_color(&col.accent_on).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_surface_hover(hex_color(&col.surface_hover).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_surface_active(hex_color(&col.surface_active).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_surface_selected(hex_color(&col.surface_selected).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_viz_1(hex_color(&col.viz_1).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_viz_2(hex_color(&col.viz_2).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));
        c.set_viz_3(hex_color(&col.viz_3).unwrap_or(slint::Color::from_rgb_u8(0, 0, 0)));

        // T1.0 §7: иконки и палитра виджетов следуют за standard_palette.
        ui.set_theme_palette(match theme.standard_palette {
            StandardPalette::Dark => 0,
            StandardPalette::Light => 1,
        });
    }

    /// Columns currently visible, in display order.
    fn visible_col_ids(&self) -> Vec<ColumnId> {
        self.core.settings().columns.visible_columns()
    }

    /// Build a single playlist-table row for track `t` at `index`.
    fn build_row(&self, index: usize, t: &Track) -> ModelRc<StandardListViewItem> {
        let row: Vec<StandardListViewItem> = self
            .visible_col_ids()
            .iter()
            .map(|c| {
                let text = match c {
                    ColumnId::NowPlaying => {
                        if self.current == Some(index) {
                            "\u{25B6}".to_string()
                        } else {
                            String::new()
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
                            t.bitrate.to_string()
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
        let rows: Vec<ModelRc<StandardListViewItem>> = (0..self.track_count())
            .filter_map(|i| self.track_at(i).map(|t| self.build_row(i, t)))
            .collect();

        self.playlist_rows.set_vec(rows);
        self.playlist_cols.set_vec(cols);
        if let Some(ui) = self.try_ui() {
            ui.set_current_row(
                self.current.map(|i| i as i32).unwrap_or(-1),
            );
        }
        self.col_model_sig = self.compute_col_sig();
    }

    /// Refresh a single row in place (metadata updates and the `>` marker of
    /// the current track).
    pub(super) fn refresh_playlist_rows_at(&mut self, index: Option<usize>) {
        if let Some(i) = index {
            if self.playlist_rows.row_count() > i {
                if let Some(t) = self.track_at(i) {
                    let row = self.build_row(i, t);
                    self.playlist_rows.set_row_data(i, row);
                }
            }
        }
    }

    /// Resolve the visible `TableColumn`s with current pixel widths, reading
    /// ratios/sort from `AppCore` session state (§8.1 С3).
    fn build_table_columns(&self) -> Vec<TableColumn> {
        let view_w = self.try_ui().map(|ui| ui.get_playlist_view_width()).unwrap_or(100.0).max(100.0);

        let ids = self.visible_col_ids();
        let cols_cfg = &self.core.settings().columns;
        let widths = self.core.state().column_widths();
        let ratios: Vec<f32> = ids
            .iter()
            .map(|c| effective_width_pct(cols_cfg, widths, *c))
            .collect();
        let limits: Vec<music_player_rs::playlist_layout::ColumnLimit> = ids
            .iter()
            .filter_map(|c| {
                cols_cfg.column_def(*c).map(|def| music_player_rs::playlist_layout::ColumnLimit {
                    min_px: def.min_width,
                    max_px: def.max_width,
                    max_pct: def.max_width_percent,
                })
            })
            .collect();
        let resolved = music_player_rs::playlist_layout::resolve_widths(view_w, &ids, &ratios, &limits);

        let sort = self.core.state().sort();
        ids.iter()
            .zip(resolved)
            .map(|(c, w)| {
                let mut tc = TableColumn::default();
                tc.title = cols_cfg.column_title(*c).into();
                tc.width = w;
                tc.sort_order = match sort {
                    Some(k) if k.column.column() == *c => match k.dir {
                        SortDir::Desc => SortOrder::Descending,
                        SortDir::Asc => SortOrder::Ascending,
                    },
                    _ => SortOrder::Unsorted,
                };
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
        let Some(ui) = self.try_ui() else { return 0 };
        let cols = ui.get_playlist_cols();
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

    /// Переносит живые пиксельные ширины UI в состояние сессии `AppCore`
    /// процентами (§2.5, И-Т7). Файл здесь не пишется: срок отложенной записи
    /// взводит только `Origin::User`; программный пересчёт и неизменённые
    /// ширины срок не запускают (ОВС-5 а, §6.17).
    pub(super) fn save_column_widths_from_ui(&mut self, origin: Origin) {
        let Some(ui) = self.try_ui() else { return };
        let cols = ui.get_playlist_cols();
        let len = cols.row_count();
        if len == 0 {
            return;
        }
        let visible_cols = self.visible_col_ids();
        if visible_cols.len() != len {
            return;
        }
        let view_w = ui.get_playlist_view_width().max(100.0);
        let mut px: Vec<f32> = Vec::with_capacity(len);
        {
            let cols_cfg = &self.core.settings().columns;
            for (i, col_id) in visible_cols.iter().enumerate() {
                if let Some(tc) = cols.row_data(i) {
                    let lim = cols_cfg
                        .column_def(*col_id)
                        .map(|def| music_player_rs::playlist_layout::ColumnLimit {
                            min_px: def.min_width,
                            max_px: def.max_width,
                            max_pct: def.max_width_percent,
                        })
                        .unwrap_or(music_player_rs::playlist_layout::ColumnLimit {
                            min_px: 0.0,
                            max_px: None,
                            max_pct: None,
                        });
                    let max_eff = lim.effective_max(view_w);
                    let min_eff = lim.min_px.min(max_eff);
                    px.push(tc.width.clamp(min_eff, max_eff));
                }
            }
        }
        let total_px: f32 = px.iter().sum();
        if total_px <= 0.0 {
            return;
        }
        // Ширины колонок — состояние сессии (§2.5, И-Т7, §8.1 С3).
        let mut new_widths = self.core.state().column_widths().clone();
        for (col_id, w) in visible_cols.iter().zip(px) {
            let pct = (w / total_px) * 100.0;
            match WidthPct::new(pct) {
                Some(wp) => new_widths.insert(*col_id, wp),
                None => new_widths.remove(col_id),
            };
        }
        if &new_widths == self.core.state().column_widths() {
            return;
        }
        self.core.change_state(origin, StateChange::ColumnWidths(new_widths));
    }

    /// Re-apply the current filters from the draft synchronously. A filter
    /// toggle needs no backend requery — the listing is already in memory
    /// (`audio_device_infos`), so the dialog just re-projects it (ТЗ A3.0 §8.1).
    pub(super) fn apply_audio_filter(&mut self) {
        if !self.audio_device_infos.is_empty() {
            self.apply_audio_device_listing();
        }
    }

    /// Проецирует выбор диалога на конкретное [`DeviceInfo`] для превью
    /// capabilities/validation (§8.1 С3): приоритет — draft-превью
    /// (`playback.audio_device`), затем реально активное устройство, затем
    /// первое из списка как дефолт.
    fn current_audio_device_info(&self) -> Option<&DeviceInfo> {
        let want = {
            let s = self.cfg();
            if !s.playback.audio_device.is_empty() {
                Some(s.playback.audio_device.clone())
            } else if !self.active_device.is_empty() {
                Some(self.active_device.clone())
            } else {
                None
            }
        };
        match want {
            Some(w) => self
                .audio_device_infos
                .iter()
                .find(|d| d.id == w || d.name == w)
                .or_else(|| self.audio_device_infos.first()),
            None => self.audio_device_infos.first(),
        }
    }

    /// Push the device-capabilities and validation rows to the dialog. Either
    /// the whole device list is empty (preview blocks cleared) or the preview
    /// is built for exactly one device: the draft/active one (§8.1 С3).
    pub(super) fn sync_capabilities_and_validation(&mut self) {
        let Some(device) = self.current_audio_device_info() else {
            if let Some(ui) = self.try_ui() {
                ui.set_settings_audio_caps(ModelRc::from(&[][..]));
                ui.set_settings_audio_validation(ModelRc::from(&[][..]));
            }
            return;
        };

        let caps = build_capabilities(device);
        let Some(ui) = self.try_ui() else { return };
        ui.set_settings_audio_caps(ModelRc::from(caps.as_slice()));

        let rows = music_player_rs::audio::output::validate_audio_settings(
            device,
            &self.cfg().playback,
        );
        let model: Vec<ValidationRow> = rows
            .into_iter()
            .map(|r| ValidationRow {
                source: r.source.into(),
                outcome: match r.outcome {
                    music_player_rs::audio::output::Outcome::BitPerfect => 0,
                    music_player_rs::audio::output::Outcome::Degraded => 1,
                    music_player_rs::audio::output::Outcome::Unsupported => 2,
                },
                detail: r.detail.into(),
            })
            .collect();
        ui.set_settings_audio_validation(ModelRc::from(model.as_slice()));
    }

    /// Цепочка DSD по выбранному режиму (§8.1 С3) — строка под combo.
    pub(super) fn sync_dsd_chain_desc(&self) {
        let chain = match self.cfg().playback.dsd.mode {
            DsdMode::Native => "Native → DoP → PCM",
            DsdMode::DoP => "DoP → PCM",
            DsdMode::Pcm => "PCM only",
        };
        if let Some(ui) = self.try_ui() {
            ui.set_settings_audio_dsd_chain_desc(chain.into());
        }
    }

    /// Синхронизация Advanced-панели из текущих настроек (draft): индексы
    /// ComboBox, фиксированная частота, глубина ring-буфера (§8.1 С3).
    pub(super) fn sync_audio_advanced(&self) {
        let s = self.cfg();
        let Some(ui) = self.try_ui() else { return };
        ui.set_settings_audio_exclusive_idx(s.playback.audio.exclusive.index());
        ui.set_settings_audio_fallback_idx(s.playback.audio.fallback.index());
        ui
            .set_settings_audio_resampler_mode_idx(s.playback.audio.resampler.mode.index());
        ui
            .set_settings_audio_fixed_rate_idx(
                super::FIXED_RATES
                    .iter()
                    .position(|&r| r == s.playback.audio.resampler.fixed_rate)
                    .unwrap_or(0) as i32,
            );
        ui
            .set_settings_audio_clock_family_idx(s.playback.audio.resampler.prefer_family.index());
        ui
            .set_settings_audio_fallback_rate_idx(s.playback.audio.resampler.fallback_rate.index());
        ui
            .set_settings_audio_ring_buffer_ms(s.playback.audio.ring_buffer_ms as i32);
    }
}

/// Проекция устройства через draft-фильтры вкладки Audio: hardware-only и
/// stereo-only (ТЗ A3.0 §8.1). Чистая функция — тестируется без UI.
pub(super) fn audio_filter_matches(
    device: &DeviceInfo,
    hardware_only: bool,
    stereo_only: bool,
) -> bool {
    (if hardware_only {
        device.category == DeviceCategory::Hardware
    } else {
        true
    }) && (if stereo_only { device.is_stereo() } else { true })
}

/// Index of the pair matching `want` within an *arbitrary* `(id, label)` list.
/// `want` is either a stable device id or a human-readable name; for server
/// nodes the name matches the label with the "software, resamples" suffix
/// stripped.
pub(super) fn find_device_index_in(pairs: &[(String, String)], want: &str) -> Option<usize> {
    pairs
        .iter()
        .position(|(raw, _)| raw == want)
        .or_else(|| pairs.iter().position(|(_, label)| label == want))
        .or_else(|| {
            pairs.iter().position(|(_, label)| {
                label
                    .strip_suffix(music_player_rs::audio::output::SERVER_NODE_SUFFIX)
                    .is_some_and(|stripped| stripped == want)
            })
        })
}

/// Частота в кГц для UI: 48000 → «48», 176400 → «176.4».
fn khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{}", rate / 1000)
    } else {
        format!("{:.1}", rate as f64 / 1000.0)
    }
}

/// Таблица «Возможности устройства» для панели предпросмотра: тип, каналы,
/// частоты, форматы, exclusive и DoP-слот (ТЗ A3.0 §4.1/§8.1). Чистая
/// функция; `ok` управляет подсветкой строки (текст основной vs приглушённый).
pub(super) fn build_capabilities(
    device: &music_player_rs::audio::output::DeviceInfo,
) -> Vec<DeviceCapability> {
    let kind = match device.category {
        DeviceCategory::Hardware => "Аппаратное (hw:*)",
        DeviceCategory::ServerProxy => "Программное (сервер звука)",
        DeviceCategory::Virtual => "Виртуальное (dmix и т.п.)",
        DeviceCategory::Loopback => "Loopback",
        DeviceCategory::Unknown => "Неизвестное",
    };
    let channels = if device.channels >= 2 && device.channels <= 8 {
        let label = match device.channels {
            2 => "стерео",
            6 => "5.1",
            8 => "7.1",
            _ => "каналов",
        };
        format!("{} ({label})", device.channels)
    } else if device.channels == 1 {
        "1 (моно)".into()
    } else {
        device.channels.to_string()
    };
    let dop = device.dop_container_rate();
    let dop_value = dop.map_or_else(|| "—".into(), |r| format!("{} kHz", khz(r)));

    let rows = vec![
        DeviceCapability {
            label: "Тип устройства".into(),
            value: kind.into(),
            ok: true,
        },
        DeviceCapability {
            label: "Каналы".into(),
            value: channels.into(),
            ok: true,
        },
        DeviceCapability {
            label: "Частоты".into(),
            value: device.rates_desc().into(),
            ok: !device.supported_rates.is_empty(),
        },
        DeviceCapability {
            label: "Форматы".into(),
            value: device.formats_desc().into(),
            ok: !device.supported_formats.is_empty(),
        },
        DeviceCapability {
            label: "Exclusive".into(),
            value: if device.exclusive_capable {
                "поддерживается".into()
            } else {
                "не поддерживается".into()
            },
            ok: device.exclusive_capable,
        },
        DeviceCapability {
            label: "DSD (DoP)".into(),
            value: dop_value.into(),
            ok: dop.is_some(),
        },
    ];
    rows
}