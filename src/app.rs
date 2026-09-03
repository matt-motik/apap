use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use eframe::egui;
use egui::RichText;
use egui_extras::{Column, TableBuilder};

use music_player_rs::audio::player::Player;
use music_player_rs::playlist::{self, ScanMsg, Track};
use music_player_rs::settings::{ColumnId, RepeatMode, SettingsStore, Theme};
use music_player_rs::tray::{self, TrayCmd};

/// Таблица геометрии верхней панели.
///
/// Рассчитывается один раз в кадр из актуальной ширины окна и настроек
/// (размер обложки, ширина колонки инфо), чтобы вся компоновка держалась
/// одной центральной формулой — она повторяет `src/top_panel.md`. Все
/// функции отрисовки верхней панели получают готовые значения отсюда,
/// поэтому при ресайзе или смене настроек всё пересчитывается само собой.
struct TopLayout {
    /// Сторона квадратной обложки = ширине колонки cover (пикс.).
    cover_wh: f32,
    /// Горизонтальный отступ между колонками верхней панели (из настроек).
    /// Влияет только на ширину (см. `col_viz_w`) — не на высоту.
    gap: f32,
    /// Фиксированный вертикальный зазор между блоками внутри колонки
    /// (визуализацией/слайдером, обложкой/кнопками). Не зависит от `gap`.
    v_gap: f32,
    /// Высота строки меток времени под seek-слайдером (стиль Body).
    time_row_h: f32,
    /// Фиксированная ширина колонки информации (пикс., из настроек 200..=400).
    col_info_w: f32,
    /// Ширина колонки громкости = ширине надписи «100%».
    col_volume_w: f32,
    /// Ширина визуализации и seek-слайдера = остаток окна после остальных колонок.
    col_viz_w: f32,
    /// Высота строки меню (входит в состав верхней панели, документация).
    #[allow(dead_code)]
    menu_h: f32,
    /// Высота зоны контента под строкой меню (обложка + вертикальный зазор + кнопки).
    content_h: f32,
    /// Полная высота верхней панели = содержимое + строка меню.
    top_panel_h: f32,
    /// Сторона квадратной кнопки управления = обложка / 6.
    button_side: f32,
    /// Высота панели кнопок = размеру кнопки (документация из `top_panel.md`).
    #[allow(dead_code)]
    control_panel_h: f32,
    /// Ширина seek-слайдера = ширине визуализации.
    seekbar_w: f32,
    /// Высота seek-слайдера = размеру кнопки.
    seekbar_h: f32,
    /// Высота визуализации = высоте блока громкости.
    viz_h: f32,
    /// Высота блока громкости = высоте визуализации.
    volume_h: f32,
    /// «Распорка» под обложкой = вертикальному зазору (прижимает кнопки к низу).
    spacer: f32,
    /// Ширина зоны вертикального разделителя (документация из `top_panel.md`).
    #[allow(dead_code)]
    vsep_w: f32,
}

impl TopLayout {
    /// Вычисляет всю геометрию верхней панели по коду `src/top_panel.md`:
    ///   top_panel_w = window_w
    ///   VSEP_W      = GAP + | + GAP
    ///   col_cover_w = cover_wh
    ///   col_viz_w   = top_panel_w - (col_cover_w + col_info_w + col_volume_w + VSEP_W*3)
    ///   control_button_wh = col_cover_w / 6
    ///   control_panel_h   = control_button_wh
    ///   seekbar_h = control_button_wh,  seekbar_w = col_viz_w
    ///   content_h = cover_wh + V_GAP + control_panel_h   (высота зоны контента)
    ///   viz_h     = content_h - V_GAP - seekbar_h - time_row_h
    ///   volume_h  = viz_h
    ///
    /// Важно: `gap` (настройка) применяется ТОЛЬКО по горизонтали (в `vsep_w` и
    /// `col_viz_w`); вертикальные зазоры фиксированы константой `V_GAP`, поэтому
    /// изменение `gap` влияет лишь на ширину, но не на высоту визуализации.
    fn compute(ui: &egui::Ui, cover_wh: f32, col_info_w: f32, gap: f32) -> Self {
        // Толщина вертикального разделителя — единая для всей панели.
        const SEP: f32 = 1.0;
        // Вертикальный зазор между блоками внутри колонки (фикс., не из настроек).
        const V_GAP: f32 = 4.0;
        // Высота строки меток времени (стиль Body, ровно высота строки без запаса,
        // чтобы под текстом не оставалось пустого места).
        let time_row_h = ui.text_style_height(&egui::TextStyle::Body);

        // Ширина колонки громкости = ширине надписи «100%» в мелком стиле.
        let font_id = ui
            .style()
            .text_styles
            .get(&egui::TextStyle::Small)
            .cloned()
            .unwrap_or_else(|| egui::FontId::proportional(11.0));
        // Измеряем ширину строки «100%» через пейнтер (не требует &mut шрифтов).
        let col_volume_w = ui
            .painter()
            .layout_no_wrap("100%".into(), font_id, egui::Color32::WHITE)
            .size()
            .x;
        // Небольшой запас, чтобы значок и кнопка мьюта помещались.
        let col_volume_w = col_volume_w + 16.0;

        // Зона разделителя по горизонтали: отступ + линия + отступ.
        let vsep_w = gap + SEP + gap;

        // Сторона квадратной кнопки: обложка делится на 6 кнопок.
        let button_side = cover_wh / 6.0;
        let control_panel_h = button_side;

        // Высота зоны контента (без строки меню): обложка + зазор + панель кнопок.
        let content_h = cover_wh + V_GAP + control_panel_h;

        // Высота строки меню: верхний отступ 4 + высота кнопки + нижний отступ 4.
        // Кнопка меню — обычный `Button` (стиль Body): паддинг по вертикали ×2
        // плюс высота строки.
        let button_h =
            ui.spacing().button_padding.y * 2.0 + ui.text_style_height(&egui::TextStyle::Button);
        let menu_h = 4.0 + button_h + 4.0;

        // Полная высота панели = зона контента + строка меню. Плюс нижний
        // отступ 4, который добавляется в конце `top_panel`.
        let top_panel_h = content_h + menu_h + 4.0;

        // Полная ширина окна, отданная под верхнюю панель.
        let top_panel_w = ui.available_width();

        // Ширина визуализации = всё, что осталось после 3 колонок и 3 разделителей.
        let col_viz_w =
            (top_panel_w - (cover_wh + col_info_w + col_volume_w + vsep_w * 3.0)).max(120.0);

        let seekbar_w = col_viz_w;
        let seekbar_h = control_panel_h;

        // Высота визуализации (и громкости): зона контента − зазор − seekbar −
        // строка времени. Завист только от вертикальных (фиксированных) размеров.
        let viz_h = (content_h - V_GAP - seekbar_h - time_row_h).max(40.0);
        let volume_h = viz_h;

        // Распорка под обложкой = вертикальному зазору (прижимает кнопки).
        let spacer = V_GAP;

        Self {
            cover_wh,
            gap,
            v_gap: V_GAP,
            time_row_h,
            col_info_w,
            col_volume_w,
            col_viz_w,
            menu_h,
            content_h,
            top_panel_h,
            button_side,
            control_panel_h,
            seekbar_w,
            seekbar_h,
            viz_h,
            volume_h,
            spacer,
            vsep_w,
        }
    }
}

pub struct MusicApp {
    ctx: egui::Context,
    settings: SettingsStore,
    player: Player,
    tracks: Vec<Track>,
    current: Option<usize>,
    scroll_to_row: Option<usize>,
    scan_rx: Option<Receiver<ScanMsg>>,
    known_paths: HashSet<PathBuf>,
    status: String,
    scrub: Option<f64>,
    repeat: RepeatMode,
    shuffle: bool,
    shuffle_order: Vec<usize>,
    shuffle_pos: usize,
    devices: Vec<(String, String)>,
    show_settings: bool,
    settings_tab: u8,
    last_playlist_save: Instant,
    last_col_save: Instant,
    col_reset: bool,
    playlist_dirty: bool,
    tray_rx: Option<std::sync::mpsc::Receiver<TrayCmd>>,
    tray_up_tx: Option<tokio::sync::mpsc::UnboundedSender<tray::TrayState>>,
    last_tray_update: Instant,
    window_visible: bool,
    force_quit: bool,
}

impl MusicApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = SettingsStore::load();
        let theme = settings.settings.theme;
        apply_theme(&cc.egui_ctx, theme);

        let mut player = Player::new();
        let vol = settings.settings.volume;
        let muted = settings.settings.muted;
        player.set_volume(vol);
        player.set_muted(muted);
        if !settings.settings.audio_device.is_empty() {
            player.set_preferred_device(settings.settings.audio_device.clone());
        }

        let tracks = playlist::load_track_list(&music_player_rs::settings::playlist_path());
        let known_paths: HashSet<PathBuf> = tracks.iter().map(|t| t.path.clone()).collect();

        let (tray_rx, tray_up_tx) = tray::start();

        let repeat = settings.settings.repeat;
        let shuffle = settings.settings.shuffle;
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            settings,
            player,
            tracks,
            current: None,
            scroll_to_row: None,
            scan_rx: None,
            known_paths,
            status: String::new(),
            scrub: None,
            repeat,
            shuffle,
            shuffle_order: Vec::new(),
            shuffle_pos: 0,
            devices: Vec::new(),
            show_settings: false,
            settings_tab: 0,
            last_playlist_save: Instant::now(),
            last_col_save: Instant::now(),
            col_reset: false,
            playlist_dirty: false,
            tray_rx: Some(tray_rx),
            tray_up_tx: Some(tray_up_tx),
            last_tray_update: Instant::now(),
            window_visible: true,
            force_quit: false,
        };
        app.rebuild_shuffle();
        if let Some(col) = app.settings.settings.sorted_col {
            let desc = app.settings.settings.sort_desc;
            app.apply_sort(col, desc);
        }
        app
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
        self.status = format!("Scanning folder: {status_root}");
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
                            self.status = format!("...{added} tracks added");
                        }
                    }
                    Ok(ScanMsg::Done(total)) => {
                        self.status = format!("Folder scan finished: {total} tracks found");
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
        }
    }

    fn play_track(&mut self, index: usize) {
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
                self.scroll_to_row = Some(index);
                self.sync_shuffle_pos();
                let title = &self.tracks[index].title;
                let artist = self.tracks[index].artist.clone();
                let album = self.tracks[index].album.clone();
                self.status = format!(
                    "Playing: {title} — {} Hz, {} ch, {}",
                    info.sample_rate, info.channels, info.format_name
                );
                if let Some(a) = artist {
                    if let Some(al) = album {
                        self.status = format!(
                            "Playing: {title} — {a} [{al}] · {} Hz, {} ch, {}",
                            info.sample_rate, info.channels, info.format_name
                        );
                    } else {
                        self.status = format!(
                            "Playing: {title} — {a} · {} Hz, {} ch, {}",
                            info.sample_rate, info.channels, info.format_name
                        );
                    }
                }
                self.player.play();
            }
            Err(e) => {
                self.status = format!("Cannot play {title}: {e}");
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

    fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.next();
        self.settings.settings.repeat = self.repeat;
        self.settings.save();
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
                    self.status = format!("Cannot switch audio device: {e}");
                }
            }
            None => {
                self.player.set_preferred_device(name);
            }
        }
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
        // Seek to the start when the current track has been playing for a while.
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
        self.status = String::from("Track removed");
    }

    fn clear_playlist(&mut self) {
        self.player.stop();
        self.current = None;
        self.tracks.clear();
        self.known_paths.clear();
        self.rebuild_shuffle();
        self.mark_playlist_dirty();
        self.status = String::from("Playlist cleared");
    }

    fn save_if_dirty(&mut self) {
        // Settings are persisted immediately on every change; only the
        // playlist is written with a short debounce (folder scans can add many
        // tracks per frame).
        if self.playlist_dirty
            && self.last_playlist_save.elapsed() >= std::time::Duration::from_secs(2)
        {
            self.save_playlist();
            self.last_playlist_save = Instant::now();
        }
    }

    /// Строка меню в самой верхней части панели: кнопки «Open Files»,
    /// «Open Folder» (слева) и «Settings» (прижата к правому краю).
    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0); // верхний отступ строки
        ui.horizontal(|ui| {
            // Кнопка «Open Files» — открыть диалог выбора аудиофайлов.
            if ui.button("📂 Open Files").clicked() {
                let dir = self.settings.settings.last_dir.clone(); // последняя папка
                let mut dlg = rfd::FileDialog::new()
                    .add_filter("Audio", playlist::SUPPORTED_EXTENSIONS)
                    .add_filter("All files", &["*"]);
                if !dir.is_empty() {
                    dlg = dlg.set_directory(&dir); // открываем в последней папке
                }
                if let Some(files) = dlg.pick_files() {
                    let added = self.add_paths(files); // добавляем в плейлист
                    self.status = format!("Added {added} track(s)");
                }
            }
            // Кнопка «Open Folder» — выбрать папку и просканировать её.
            if ui.button("🗀 Open Folder").clicked() {
                let dir = self.settings.settings.last_dir.clone();
                let mut dlg = rfd::FileDialog::new();
                if !dir.is_empty() {
                    dlg = dlg.set_directory(&dir);
                }
                if let Some(folder) = dlg.pick_folder() {
                    // Запоминаем выбранную папку как последнюю.
                    if let Some(s) = folder.to_str() {
                        self.settings.settings.last_dir = s.to_string();
                        self.settings.save();
                    }
                    // Запускаем фоновое сканирование папки на аудиофайлы.
                    self.start_folder_scan(folder);
                }
            }
            // Кнопку «Settings» прижимаем к правому краю строки меню.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⚙ Settings").clicked() {
                    // Переключаем видимость окна настроек.
                    self.show_settings = !self.show_settings;
                }
            });
        });
        ui.add_space(4.0); // нижний отступ строки
    }

    /// Верхняя панель: повторяет «верхнюю зону» Python-версии — 4 колонки
    /// [обложка | инфо | визуализация + seek | вертикальная громкость],
    /// разделённые тремя вертикальными линиями (по `src/top_panel.md`).
    /// Все размеры берутся из `TopLayout::compute`, который пересчитывается
    /// каждый кадр под актуальную ширину окна и настройки.
    fn top_panel(&mut self, ui: &mut egui::Ui) {
        // Строка меню (открыть файлы/папку, настройки) рисуется первой и
        // занимает свою вертикальную высоту в начале панели.
        self.menu_bar(ui);

        // Значения размеров обложки, ширины инфо и отступа берём из настроек.
        let cover = self.settings.settings.cover_size; // сторона квадратной обложки
        let col_info_w = self.settings.settings.col_info_w; // ширина колонки инфо
        let gap = self.settings.settings.col_gap; // отступ между колонками

        // Единый расчёт всей геометрии панели (см. `TopLayout::compute`).
        let lay = TopLayout::compute(ui, cover, col_info_w, gap);

        // Вся верхняя зона — один горизонтальный ряд колонок, выровненный по верху.
        // Отключаем неявный item_spacing.x между детьми ряда, чтобы ширины колонок
        // совпадали с расчётом `TopLayout` (иначе egui добавляет ~8px на каждую
        // грань и правый столбец выпадает за край окна).
        ui.horizontal_top(|ui| {
            // Отключаем неявный промежуток между детьми ряда (см. комментарий выше).
            ui.style_mut().spacing.item_spacing.x = 0.0;

            // ---- Col 0: обложка + панель кнопок (левая зона) ----
            ui.vertical(|ui| {
                // Квадратный плейсхолдер обложки размером cover_wh.
                self.cover_placeholder(ui, lay.cover_wh);
                // «Распорка»: остаток высоты — прижимает кнопки к низу колонки.
                ui.add_space(lay.spacer);
                // Панель из 6 квадратных кнопок управления (ширина = обложка).
                self.control_panel(ui, &lay);
            });

            // Разделитель между колонками: отступ + линия + отступ (gap из настроек).
            // Линия строго 1px (`.spacing(1.0)`), чтобы ширина зоны в точности
            // равнялась `lay.vsep_w` и не было переполнения ряда.
            ui.add_space(lay.gap);
            ui.add(egui::Separator::default().vertical().spacing(1.0));
            ui.add_space(lay.gap);

            // ---- Col 1: инфо о треке (фикс. ширина, вертикальный скролл) ----
            // Выделяем контейнер ширины col_info_w и высоты зоны контента (без
            // меню); внутри — вертикальный скролл (полоса скрыта), чтобы при
            // переносе строк содержимое не вылезало за границы панели.
            ui.allocate_ui_with_layout(
                egui::vec2(lay.col_info_w, lay.content_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    egui::ScrollArea::vertical()
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .auto_shrink(false)
                        .id_salt("info_scroll")
                        .show(ui, |ui| {
                            // 13 полей о треке, раскладываются сверху вниз.
                            self.track_info_panel(ui, lay.col_info_w);
                        });
                },
            );

            // Разделитель между колонкой инфо и визуализацией.
            ui.add_space(lay.gap);
            ui.add(egui::Separator::default().vertical().spacing(1.0));
            ui.add_space(lay.gap);

            // ---- Col 2: визуализация + seek + метки времени (центральная зона) ----
            ui.vertical(|ui| {
                // Плейсхолдер визуализации ширины col_viz_w и высоты viz_h.
                self.visualizer_placeholder(ui, lay.col_viz_w, lay.viz_h);
                ui.add_space(lay.v_gap); // вертикальный зазор до слайдера
                                         // Слайдер seek той же ширины, что и визуализация.
                self.seek_bar(ui, &lay);
            });

            // Разделитель между визуализацией и громкостью.
            ui.add_space(lay.gap);
            ui.add(egui::Separator::default().vertical().spacing(1.0));
            ui.add_space(lay.gap);

            // ---- Col 3: вертикальная громкость + мьют (ширина «100%», справа) ----
            // Выделяем контейнер ширины col_volume_w и высоты зоны контента
            // (без меню); содержимое центрируется по горизонтали.
            ui.allocate_ui_with_layout(
                egui::vec2(lay.col_volume_w, lay.content_h),
                egui::Layout::top_down(egui::Align::Center),
                |ui| {
                    // Центрируем слайдер громкости и кнопку мьюта по вертикали.
                    ui.vertical_centered(|ui| {
                        // Вертикальный слайдер громкости высоты volume_h (= viz_h).
                        self.vertical_volume(ui, lay.volume_h, lay.button_side);
                        ui.add_space(lay.v_gap); // зазор до кнопки
                                                 // Плоская кнопка мьюта (только монохромный значок).
                        self.mute_button(ui, &lay);
                    });
                },
            );
        });
        ui.add_space(4.0); // нижний отступ панели
    }

    /// Плейсхолдер обложки альбома (загрузка обложки появится позже).
    /// Рисует тёмный квадрат с подписью «Album Art».
    fn cover_placeholder(&mut self, ui: &mut egui::Ui, size: f32) {
        // Выделяем точный квадрат size×size под плейсхолдер (без реакции на мышь).
        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
        // Заливаем квадрат тёмно-серым цветом со скруглением углов 6px.
        ui.painter()
            .rect_filled(rect, 6.0, egui::Color32::from_gray(30));
        // Рисуем текст «Album Art» по центру квадрата.
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Album\nArt",
            egui::FontId::proportional(16.0),
            egui::Color32::from_gray(110),
        );
    }

    /// Плейсхолдер визуализации (рендеринг появится позже).
    /// Рисует тёмный прямоугольник size×height с подписью «Visualization».
    fn visualizer_placeholder(&mut self, ui: &mut egui::Ui, width: f32, height: f32) {
        // Выделяем точный прямоугольник width×height (ширина приходит из top_panel).
        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        // Заливаем область чуть более тёмным цветом, чем обложка.
        ui.painter()
            .rect_filled(rect, 6.0, egui::Color32::from_gray(26));
        // Рисуем текст «Visualization» по центру прямоугольника.
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Visualization",
            egui::FontId::proportional(18.0),
            egui::Color32::from_gray(100),
        );
    }

    /// Вертикальный слайдер громкости (справа от визуализации).
    /// Поддерживает изменение колёсиком мыши; настройка громкости сохраняется.
    /// Ширина слайдера передаётся явно (`width`) — как и seek-слайдер,
    /// он рисуется через `add_sized`, чтобы размер соответствовал заданному.
    fn vertical_volume(&mut self, ui: &mut egui::Ui, height: f32, width: f32) {
        // Текущая громкость в процентах (0–100) — значение для слайдера.
        let mut vol = (self.player.volume() * 100.0).round() as i32;
        // Вертикальный слайдер без числового значения и без «умного прицеливания».
        let slider = egui::Slider::new(&mut vol, 0..=100)
            .show_value(false)
            .orientation(egui::SliderOrientation::Vertical)
            .smart_aim(false);
        // Размещаем слайдер явно заданных ширины и высоты (= визуализация).
        let resp = ui.add_sized([width, height], slider);
        // При изменении слайдером применяем новую громкость и сохраняем её.
        if resp.changed() {
            let v = vol as f32 / 100.0;
            self.player.set_volume(v);
            self.settings.settings.volume = v;
            self.settings.save();
        }
        // Пока курсор над слайдером — колёсико меняет громкость.
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                // Направление совпадает с колёсиком иконки в трее: вверх — тише.
                let v = (self.player.volume() + scroll.signum() * 0.02).clamp(0.0, 1.0);
                self.player.set_volume(v);
                self.settings.settings.volume = v;
                self.settings.save();
            }
        }
        // Подсказка при наведении.
        let _ = resp.on_hover_text("Volume (wheel)");
    }

    /// Плоская кнопка мьюта под вертикальным слайдером громкости.
    /// Только монохромный значок (🔇/🔊) без рамки и без текста — размер
    /// равен квадратной кнопке управления (см. `TopLayout.button_side`).
    fn mute_button(&mut self, ui: &mut egui::Ui, lay: &TopLayout) {
        let muted = self.player.muted();
        // Значок mute/unmute в зависимости от текущего состояния.
        let icon = if muted { "🔇" } else { "🔊" };
        let side = lay.button_side;
        // По клику переключаем mute и сохраняем новое состояние.
        if ui
            .add_sized(
                [side, side],
                egui::Button::new(RichText::new(icon).heading()).frame(false),
            )
            .on_hover_text("Mute / unmute (M)")
            .clicked()
        {
            self.player.toggle_mute();
            self.settings.settings.muted = self.player.muted();
            self.settings.save();
        }
    }

    /// Панель из 13 полей о текущем треке (повторяет Python `TrackInfoWidget`).
    /// Поля выводятся по одному в строку, чтобы блок читался сверху вниз.
    /// Ширину берём из `width` (задаётся слайдером настроек); высота приходит
    /// из контейнера-скролла в `top_panel`.
    fn track_info_panel(&mut self, ui: &mut egui::Ui, width: f32) {
        // Берём текущий трек из плейлиста (индекс `current`).
        let track = self.current.and_then(|i| self.tracks.get(i));
        // Замыкание: собирает 13 строк-значений полей для трека (неизвестные
        // поля заменяются длинным тире «—»).
        let value = |t: &playlist::Track| {
            [
                t.artist.clone().unwrap_or_else(|| "—".into()), // артист
                num_slash_total(t.track_number, t.track_total), // номер трека/всего в альбоме
                if t.title.is_empty() {
                    "—".into()
                } else {
                    t.title.clone()
                }, // название трека
                playlist::get_duration_string(t.duration),      // длительность ("3:06")
                if t.year.is_empty() {
                    "—".into()
                } else {
                    t.year.clone()
                }, // год
                t.album.clone().unwrap_or_else(|| "—".into()),  // альбом
                num_slash_total(t.disc, t.disc_total),          // номер диска/всего дисков
                if t.genre.as_deref().map_or(true, |g| g.is_empty()) {
                    "—".into()
                } else {
                    t.genre.clone().unwrap_or_default().into()
                }, // жанр
                if t.format.is_empty() {
                    "—".into()
                } else {
                    t.format.clone()
                }, // формат (FLAC, DSF…)
                if t.bitrate > 0 {
                    format!("{} kbps", t.bitrate)
                } else {
                    "—".into()
                }, // битрейт
                if t.bit_depth.is_empty() {
                    "—".into()
                } else {
                    t.bit_depth.clone()
                }, // разрядность ("24 bit", "dsd64")
                if t.sample_rate > 0 {
                    format!("{} Hz", t.sample_rate)
                } else {
                    "—".into()
                }, // частота сэмплирования
                if t.channels > 0 {
                    format!("{} ch", t.channels)
                } else {
                    "—".into()
                }, // число каналов
            ]
        };
        // Вектор значений: для текущего трека — его поля, иначе 13 «—».
        let v: Vec<String> = match track {
            Some(t) => value(t).to_vec(),
            None => vec!["—".to_string(); 13],
        };

        // Пары (подпись, индекс значения в `v`) — 13 строк по одной в ряд.
        let pairs: [(&str, usize); 13] = [
            ("Artist", 0),
            ("Track", 1),
            ("Title", 2),
            ("Duration", 3),
            ("Year", 4),
            ("Album", 5),
            ("Disc", 6),
            ("Genre", 7),
            ("Format", 8),
            ("Bitrate", 9),
            ("Bit depth", 10),
            ("Sample rate", 11),
            ("Channels", 12),
        ];

        let text_style = egui::TextStyle::Small;
        // Ограничиваем ширину контейнера значением `width`; высота остаётся
        // свободной, чтобы вертикальный скролл в `top_panel` управлял ей сам.
        ui.scope(|ui| {
            ui.set_width(width);
            // Принудительно используем мелкий шрифт для всей панели.
            ui.style_mut().override_text_style = Some(text_style.clone());
            // Для каждой из 13 строк: подпись (приглушённая) + значение.
            for (idx, (label, _)) in pairs.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{label}:")).weak());
                    if let Some(val) = v.get(idx) {
                        ui.label(val);
                    }
                });
            }
        });
    }

    /// Кнопки транспорта под обложкой (повторяют Python `ControlsBar`).
    /// Панель управления под обложкой: 6 квадратных плоских кнопок.
    /// Кнопки — только монохромные значки без видимых границ (.frame(false)),
    /// по 6 штук в ряд; ширина панели = ширине обложки, каждая кнопка
    /// = обложка / 6 (см. `TopLayout.button_side`).
    fn control_panel(&mut self, ui: &mut egui::Ui, lay: &TopLayout) {
        // Размер квадратной кнопки из геометрии панели.
        let side = lay.button_side;
        // Цвета подсветки: акцентный (для активного состояния) и приглушённый.
        let accent = ui.visuals().hyperlink_color;
        let weak = ui.visuals().weak_text_color();

        // Горизонтальный ряд из 6 квадратных кнопок вплотную: без промежутков
        // между кнопками (item_spacing.x = 0), чтобы суммарная ширина
        // ровно равнялась 6 * side = ширине обложки.
        ui.style_mut().spacing.item_spacing.x = 0.0;
        ui.horizontal(|ui| {
            // Плоская кнопка | Play/Pause: иконка ⏸/▶ в зависимости от состояния.
            let play_label = if self.player.is_playing() {
                RichText::new("⏸").heading()
            } else {
                RichText::new("▶").heading()
            };
            if ui
                .add_sized([side, side], egui::Button::new(play_label).frame(false))
                .on_hover_text("Play / Pause (Space)")
                .clicked()
            {
                self.player.toggle(); // переключить play/pause
            }
            // | Stop.
            if ui
                .add_sized(
                    [side, side],
                    egui::Button::new(RichText::new("⏹").heading()).frame(false),
                )
                .on_hover_text("Stop (S)")
                .clicked()
            {
                self.player.stop();
            }
            // | Previous (возврат к началу текущего или предыдущий трек).
            if ui
                .add_sized(
                    [side, side],
                    egui::Button::new(RichText::new("⏮").heading()).frame(false),
                )
                .on_hover_text("Previous (Shift+P)")
                .clicked()
            {
                self.play_prev();
            }
            // | Next (следующий трек).
            if ui
                .add_sized(
                    [side, side],
                    egui::Button::new(RichText::new("⏭").heading()).frame(false),
                )
                .on_hover_text("Next (N)")
                .clicked()
            {
                self.play_next(1);
            }

            // | Repeat: разные иконки и цвет в зависимости от режима.
            let (rep_icon, rep_color, rep_hint) = match self.repeat {
                RepeatMode::Off => ("🔁", weak, "Repeat: off"), // выключен — приглушённый
                RepeatMode::All => ("🔁", accent, "Repeat all"), // повторять список — акцент
                RepeatMode::One => ("🔂", accent, "Repeat one"), // повторять трек — акцент
            };
            if ui
                .add_sized(
                    [side, side],
                    egui::Button::new(RichText::new(rep_icon).heading().color(rep_color))
                        .frame(false),
                )
                .on_hover_text(rep_hint)
                .clicked()
            {
                self.cycle_repeat(); // циклически: Off → All → One → Off
            }

            // | Shuffle: иконка 🔀, подсвечивается акцентом, когда режим включён.
            let mut shuffle = self.shuffle;
            let sh_icon = RichText::new("🔀")
                .heading()
                .color(if shuffle { accent } else { weak });
            if ui
                .add_sized([side, side], egui::Button::new(sh_icon).frame(false))
                .on_hover_text("Shuffle")
                .clicked()
            {
                shuffle = !shuffle; // инвертируем режим
                self.shuffle = shuffle; // применяем к состоянию плеера
                self.settings.settings.shuffle = self.shuffle; // сохраняем в настройки
                self.settings.save();
                self.rebuild_shuffle(); // пересобираем перемешанный порядок треков
            }
        });
    }

    /// Панель seek: слайдер позиции воспроизведения + метки времени.
    /// Ширина и высота слайдера равны визуализации и размеру кнопки
    /// соответственно — значения берутся из `TopLayout` (см. seekbar_w/h).
    fn seek_bar(&mut self, ui: &mut egui::Ui, lay: &TopLayout) {
        // Текущая позиция, длительность трека и метка "растягивания" (scrub).
        let (_playing, pos, dur) = self.player.snapshot();
        let duration = dur.unwrap_or(0.0);
        // Пока слайдер тянется, показываем "ручную" позицию (scrub), иначе реальную.
        let shown_pos = self.scrub.unwrap_or(pos).clamp(0.0, duration.max(0.1));

        // Без вертикального промежутка между слайдером и строкой времени —
        // зазор уже задан `v_gap` до вызова (иначе 3px «съели бы» высоту).
        ui.spacing_mut().item_spacing.y = 0.0;

        // Слайдер: заполняет ровно seekbar_w, высота seekbar_h (= размер кнопки).
        let mut p = shown_pos;
        let slider = egui::Slider::new(&mut p, 0.0..=duration.max(0.1))
            .show_value(false)
            .smart_aim(false);
        let resp = ui.add_sized([lay.col_viz_w, lay.seekbar_h], slider);
        // Начали тянуть — фиксируем scrub-позицию.
        if resp.drag_started() {
            self.scrub = Some(p);
        }
        // Тянем — обновляем scrub-позицию.
        if resp.dragged() {
            self.scrub = Some(p);
        }
        // Отпустили — выполняем seek в выбранную позицию и сбрасываем scrub.
        if resp.drag_stopped() {
            self.player.seek(p);
            self.scrub = None;
        }

        // Метки времени: контейнер ровно seekbar_w×time_row_h, чтобы правый текст
        // прижимался к правому краю слайдера/визуализации, а не к краю окна.

        ui.allocate_ui_with_layout(
            egui::vec2(lay.seekbar_w, lay.time_row_h),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                let cur = playlist::format_duration(self.scrub.unwrap_or(pos));
                let tot = playlist::format_duration(duration.max(pos));
                ui.label(cur); // текущее время (или scrub, пока тянем слайдер)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(tot); // общая длительность прижата к правому краю
                });
            },
        );
    }

    fn column_visible_now(&self, id: ColumnId) -> bool {
        self.settings.settings.column_visible(id)
    }

    fn column_width_now(&self, id: ColumnId) -> f32 {
        self.settings.settings.column_width(id)
    }

    fn sort_tracks(&mut self, col: ColumnId) {
        if col == ColumnId::Index {
            return;
        }
        let desc = if self.settings.settings.sorted_col == Some(col) {
            !self.settings.settings.sort_desc
        } else {
            false
        };
        self.apply_sort(col, desc);
        self.settings.save();
    }

    fn apply_sort(&mut self, col: ColumnId, desc: bool) {
        if col == ColumnId::Index {
            return;
        }
        let n = self.tracks.len();
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| {
            let ord = playlist::sort_rows_compare(&self.tracks[a], &self.tracks[b], col);
            if desc {
                ord.reverse()
            } else {
                ord
            }
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
    }

    fn reset_columns(&mut self) {
        self.settings.settings.column_widths.clear();
        self.settings.settings.column_visibility.clear();
        self.col_reset = true;
        self.settings.save();
    }

    fn playlist_table(&mut self, ui: &mut egui::Ui) {
        let current = self.current;
        let n = self.tracks.len();

        let all_ordered = self.settings.settings.ordered_columns();
        let cols: Vec<ColumnId> = all_ordered
            .iter()
            .copied()
            .filter(|c| self.column_visible_now(*c))
            .collect();

        let sorted_col = self.settings.settings.sorted_col;
        let sort_desc = self.settings.settings.sort_desc;
        let mut clicked_sort: Option<ColumnId> = None;
        let mut pending_move: Option<(usize, usize)> = None;

        let mut builder = TableBuilder::new(ui)
            .id_salt("playlist_table")
            .striped(true)
            .resizable(true)
            .min_scrolled_height(0.0);
        for col in &cols {
            let w = self.column_width_now(*col);
            builder = builder.column(Column::initial(w).at_least(30.0));
        }
        if self.col_reset {
            self.col_reset = false;
            builder.reset();
        }
        if let Some(r) = self.scroll_to_row.take() {
            builder = builder.scroll_to_row(r, Some(egui::Align::Center));
        }

        let indicator = |col: ColumnId| -> &'static str {
            if sorted_col == Some(col) {
                if sort_desc {
                    " ↓"
                } else {
                    " ↑"
                }
            } else {
                ""
            }
        };

        let mut live_widths: Vec<f32> = Vec::new();
        builder
            .header(18.0, |mut header| {
                for (vis_idx, col) in cols.iter().enumerate() {
                    let from_order = all_ordered.iter().position(|c| c == col).unwrap_or(vis_idx);
                    let label = format!("{}{}", col.label(), indicator(*col));
                    header.col(|ui| {
                        // The whole header cell is a drop target for reordering.
                        let (_resp, drop) = ui.dnd_drop_zone(egui::Frame::new(), |ui| {
                            // The sort button doubles as the drag handle.
                            ui.dnd_drag_source(
                                ui.id().with(("col_hdr", from_order)),
                                from_order,
                                |ui| {
                                    let resp = ui.button(&label);
                                    if resp.clicked() {
                                        clicked_sort = Some(*col);
                                    }
                                    resp
                                },
                            );
                        });
                        if let Some(from) = drop {
                            pending_move = Some((*from, from_order));
                        }
                    });
                }
            })
            .body(|body| {
                live_widths = body.widths().to_vec();
                body.rows(18.0, n, |mut row| {
                    let idx = row.index();
                    let is_current = current == Some(idx);
                    for col in &cols {
                        row.col(|ui| {
                            if *col == ColumnId::Title {
                                let title = self.tracks[idx].title.clone();
                                if ui.selectable_label(is_current, title).clicked() {
                                    self.play_track(idx);
                                }
                            } else {
                                let text = playlist::sort_rows_text(&self.tracks[idx], *col);
                                if *col == ColumnId::Index && is_current {
                                    ui.label(">").on_hover_text("Now playing");
                                } else if !text.is_empty() {
                                    ui.label(text);
                                }
                            }
                        });
                    }
                });
            });

        if live_widths.len() == cols.len()
            && self.last_col_save.elapsed() >= std::time::Duration::from_secs(2)
        {
            let mut changed = false;
            for (col, w) in cols.iter().zip(live_widths.iter()) {
                let prev = self.column_width_now(*col);
                if (*w - prev).abs() > 0.5 {
                    self.settings
                        .settings
                        .column_widths
                        .insert(col.key().to_string(), *w);
                    changed = true;
                }
            }
            if changed {
                self.settings.save();
            }
            self.last_col_save = Instant::now();
        }

        if let Some(col) = clicked_sort {
            self.sort_tracks(col);
            self.scroll_to_row = self.current;
        }

        // Apply a column reorder drag, if one completed this frame.
        if let Some((from, to)) = pending_move {
            if from != to {
                self.settings.settings.move_column(from, to);
                self.settings.save();
            }
        }
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(&self.status);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(t) = self.current.and_then(|i| self.tracks.get(i)) {
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
                    ui.label(egui::RichText::new(parts.join(" • ")).weak());
                    ui.add_space(16.0);
                }
                ui.label(format!("{} tracks", self.tracks.len()));
            });
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, label) in ["General", "Audio", "Columns", "Playlist"]
                        .iter()
                        .enumerate()
                    {
                        let selected = self.settings_tab == i as u8;
                        if ui.selectable_label(selected, *label).clicked() {
                            self.settings_tab = i as u8;
                        }
                    }
                });
                ui.separator();
                ui.add_space(4.0);

                match self.settings_tab {
                    0 => self.settings_general_tab(ui),
                    1 => self.settings_audio_tab(ui),
                    2 => self.settings_columns_tab(ui),
                    _ => self.settings_playlist_tab(ui),
                }
            });
        self.show_settings = open;
    }

    fn settings_general_tab(&mut self, ui: &mut egui::Ui) {
        egui::ComboBox::from_label("Theme")
            .selected_text(match self.settings.settings.theme {
                Theme::Dark => "Dark",
                Theme::Light => "Light",
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.settings.settings.theme == Theme::Dark, "Dark")
                    .clicked()
                {
                    self.settings.settings.theme = Theme::Dark;
                    apply_theme(&self.ctx, Theme::Dark);
                    self.settings.save();
                }
                if ui
                    .selectable_label(self.settings.settings.theme == Theme::Light, "Light")
                    .clicked()
                {
                    self.settings.settings.theme = Theme::Light;
                    apply_theme(&self.ctx, Theme::Light);
                    self.settings.save();
                }
            });
        ui.add_space(6.0);
        // Размер обложки: квадрат, задаёт высоту панели и размер кнопок.
        if ui
            .add(
                egui::Slider::new(&mut self.settings.settings.cover_size, 100.0..=400.0)
                    .step_by(10.0)
                    .text("Cover size"),
            )
            .changed()
        {
            self.settings.save();
        }
        ui.add_space(4.0);
        // Ширина колонки информации о треке.
        if ui
            .add(
                egui::Slider::new(&mut self.settings.settings.col_info_w, 200.0..=400.0)
                    .step_by(10.0)
                    .text("Info column width"),
            )
            .changed()
        {
            self.settings.save();
        }
        ui.add_space(4.0);
        // Отступ между колонками верхней панели.
        if ui
            .add(
                egui::Slider::new(&mut self.settings.settings.col_gap, 0.0..=20.0)
                    .step_by(1.0)
                    .text("Column gap"),
            )
            .changed()
        {
            self.settings.save();
        }
        ui.add_space(6.0);
        let mut min_tray = self.settings.settings.minimize_to_tray;
        if ui
            .checkbox(&mut min_tray, "Minimize to tray on window close")
            .changed()
        {
            self.settings.settings.minimize_to_tray = min_tray;
            self.settings.save();
        }
    }

    fn settings_audio_tab(&mut self, ui: &mut egui::Ui) {
        if self.devices.is_empty() {
            self.devices = music_player_rs::audio::output::output_devices();
        }
        let selected_name = self.settings.settings.audio_device.clone();
        egui::ComboBox::from_label("Audio device")
            .width(220.0)
            .selected_text(if selected_name.is_empty() {
                "Default".to_string()
            } else {
                selected_name.clone()
            })
            .show_ui(ui, |ui| {
                for (name, label) in self.devices.clone() {
                    let selected = selected_name == name
                        || (selected_name.is_empty() && name == self.player.device_desc);
                    if ui.selectable_label(selected, label).clicked() && !selected {
                        self.set_output_device(name);
                    }
                }
            });
        if ui.button("Refresh devices").clicked() {
            self.devices = music_player_rs::audio::output::output_devices();
        }
        ui.label(format!("Active: {}", self.player.device_desc));
        if let Ok(core) = self.player.core.lock() {
            if let Some(d) = &core.decoder {
                ui.label(format!(
                    "Track: {} Hz, {} ch ({})",
                    d.info().sample_rate,
                    d.info().channels,
                    d.info().format_name
                ));
                ui.label(format!("Output: {} Hz, {} ch", core.out_rate, core.out_ch));
            }
        }
        if let Some(err) = &self.player.last_error {
            ui.colored_label(
                egui::Color32::from_rgb(220, 80, 70),
                format!("Error: {err}"),
            );
        }
    }

    fn settings_columns_tab(&mut self, ui: &mut egui::Ui) {
        ui.strong("Playlist columns");
        let mut vis_changes: Vec<(ColumnId, bool)> = Vec::new();
        for col in ColumnId::ALL {
            let mut vis = self.settings.settings.column_visible(col);
            if ui
                .checkbox(&mut vis, col.label())
                .on_hover_text(format!("Show/hide the {} column", col.label()))
                .changed()
            {
                vis_changes.push((col, vis));
            }
        }
        for (col, vis) in vis_changes {
            self.set_column_visible(col, vis);
        }
        if ui.button("Reset column widths").clicked() {
            self.reset_columns();
        }
    }

    fn settings_playlist_tab(&mut self, ui: &mut egui::Ui) {
        if ui.button("Clear playlist").clicked() {
            self.clear_playlist();
        }
        if ui.button("Remove current track").clicked() {
            if let Some(i) = self.current {
                self.remove_track(i);
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            let path = file.path().to_path_buf();
            if path.is_dir() {
                self.start_folder_scan(path);
            } else {
                self.add_paths(vec![path]);
            }
        }
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        use egui::Key;
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = ev
            {
                match key {
                    Key::Space => self.player.toggle(),
                    Key::P if !modifiers.command => {
                        if modifiers.shift {
                            self.play_prev();
                        } else {
                            self.play_next(1);
                        }
                    }
                    Key::S => self.player.stop(),
                    Key::N => self.play_next(1),
                    Key::M => {
                        self.player.toggle_mute();
                        self.settings.settings.muted = self.player.muted();
                        self.settings.save();
                    }
                    Key::ArrowRight => {
                        let (_, pos, _) = self.player.snapshot();
                        self.player.seek(pos + 5.0);
                    }
                    Key::ArrowLeft => {
                        let (_, pos, _) = self.player.snapshot();
                        self.player.seek(pos - 5.0);
                    }
                    _ => {}
                }
            }
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
                    self.window_visible = !self.window_visible;
                    self.ctx
                        .send_viewport_cmd(egui::ViewportCommand::Visible(self.window_visible));
                }
                TrayCmd::Quit => {
                    self.force_quit = true;
                    self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
                    Some(a) if !a.is_empty() => format!("{} — {}", t.title, a),
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

impl eframe::App for MusicApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keep the loop ticking so tray commands/status work while the
        // window is hidden (when no egui pass runs otherwise).
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        // Minimize-to-tray: intercept the OS close request and hide instead.
        if self.settings.settings.minimize_to_tray && !self.force_quit {
            if ctx.input(|i| i.viewport().close_requested()) {
                self.window_visible = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        self.poll_tray();
        self.push_tray_status();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.handle_dropped_files(&ctx);
        self.drain_scan();

        // Auto-advance only on a NATURAL end-of-track (manual Stop keeps the
        // current position and does not advance). Repeat modes steer what
        // happens at the end of a track / the playlist.
        if self.player.ended() && self.current.is_some() {
            match self.repeat {
                music_player_rs::settings::RepeatMode::One => {
                    // Restart the same track from the beginning.
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

        // Высота панели берётся из той же геометрии, что и сама раскладка:
        // обложка + GAP + панель кнопок (см. `TopLayout::compute`). Считаем
        // заранее по ширине окна, чтобы `Panel::top` не обрезал контент.
        let cover = self.settings.settings.cover_size;
        let col_info_w = self.settings.settings.col_info_w;
        let gap = self.settings.settings.col_gap;
        let lay = TopLayout::compute(ui, cover, col_info_w, gap);
        egui::Panel::top("top")
            .exact_size(lay.top_panel_h)
            .resizable(false)
            .show(ui, |ui| {
                self.top_panel(ui);
            });

        egui::Panel::bottom("status").show(ui, |ui| {
            self.bottom_bar(ui);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            self.playlist_table(ui);
        });

        if self.show_settings {
            self.settings_window(&ctx);
        }

        self.handle_keyboard(&ctx);
        self.save_if_dirty();
    }

    fn on_exit(&mut self) {
        self.settings.save();
        self.save_playlist();
    }
}

pub fn apply_theme(ctx: &egui::Context, theme: Theme) {
    match theme {
        Theme::Dark => ctx.set_visuals(egui::Visuals::dark()),
        Theme::Light => ctx.set_visuals(egui::Visuals::light()),
    }
}

/// Format a track/disc number with an optional total: "3 / 12" or "3".
fn num_slash_total(num: u32, total: u32) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    if num > 0 {
        let _ = write!(s, "{num}");
        if total > 0 {
            let _ = write!(s, " / {total}");
        }
    } else if total > 0 {
        let _ = write!(s, "—");
    } else {
        s.push_str("—");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artist: &str, year: &str, bitrate: u32, dur: Option<f64>) -> Track {
        Track {
            path: PathBuf::from("/tmp/x.flac"),
            title: title.to_string(),
            duration: dur,
            artist: if artist.is_empty() {
                None
            } else {
                Some(artist.to_string())
            },
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
        assert_eq!(
            playlist::sort_rows_compare(&a, &b, ColumnId::Title),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            playlist::sort_rows_compare(&a, &b, ColumnId::Artist),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            playlist::sort_rows_compare(&a, &b, ColumnId::Genre),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            playlist::sort_rows_compare(&a, &b, ColumnId::TrackNumber),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            playlist::sort_rows_compare(&a, &b, ColumnId::Year),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            playlist::sort_rows_compare(&a, &b, ColumnId::Bitrate),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            playlist::sort_rows_compare(&a, &b, ColumnId::Duration),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn num_slash_total_formats_numbers() {
        assert_eq!(num_slash_total(0, 0), "—");
        assert_eq!(num_slash_total(3, 0), "3");
        assert_eq!(num_slash_total(3, 12), "3 / 12");
        assert_eq!(num_slash_total(0, 12), "—");
    }

    #[test]
    fn track_carries_full_metadata_fields() {
        let mut t = track("Title", "Artist", "2001", 1411, Some(186.0));
        t.track_number = 3;
        t.track_total = 12;
        t.disc = 2;
        t.disc_total = 3;
        t.channels = 2;
        assert_eq!(t.track_number, 3);
        assert_eq!(t.track_total, 12);
        assert_eq!(t.disc, 2);
        assert_eq!(t.disc_total, 3);
        assert_eq!(t.channels, 2);
        // Rendering helpers used by the 13-field panel.
        assert_eq!(num_slash_total(t.track_number, t.track_total), "3 / 12");
        assert_eq!(num_slash_total(t.disc, t.disc_total), "2 / 3");
    }
}
