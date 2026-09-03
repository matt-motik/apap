// Slint-плеер (MVP) — см. ROADMAP_SLINT. Переиспользует ядро (`music_player_rs`):
// player, playlist, meta, tray, settings. Интерфейс описан в `ui/*.slint`.
//
// На этом этапе (S0/S1): окно + верхняя панель 4 колонки + спектральный
// визуализатор (симуляция амплитуд из примера `slintfft.example`).
// Плейлист (S2), настройки (S3), реальные сэмплы для спектра (S5) — позже.

use music_player_rs::audio::player::Player;
use music_player_rs::playlist;
use music_player_rs::playlist::Track;
use music_player_rs::settings::{ColumnId, RepeatMode, SettingsStore};
use music_player_rs::tray;

use slint::language::{StandardListViewItem, TableColumn};
use slint::{ComponentHandle, Global, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::sync::{mpsc::TryRecvError, Arc, Mutex};

slint::include_modules!();

/// Построить модель строк таблицы из треков в порядке отображения и колонок.
fn build_rows(tracks: &[Track], cols: &[ColumnId]) -> ModelRc<ModelRc<StandardListViewItem>> {
    let rows: Vec<ModelRc<StandardListViewItem>> = tracks
        .iter()
        .map(|t| {
            let cells: Vec<StandardListViewItem> = cols
                .iter()
                .map(|c| {
                    let mut item = StandardListViewItem::default();
                    item.text = playlist::sort_rows_text(t, *c).into();
                    item
                })
                .collect();
            ModelRc::new(VecModel::from(cells))
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// Построить модель колонок (заголовки) из списка видимых колонок.
fn build_columns(cols: &[ColumnId]) -> Vec<TableColumn> {
    cols.iter()
        .map(|c| {
            let mut col = TableColumn::default();
            col.title = c.label().into();
            col
        })
        .collect()
}

fn main() -> Result<(), slint::PlatformError> {
    // Slint's winit backend prefers Wayland when WAYLAND_DISPLAY is set, but the
    // software renderer (softbuffer) doesn't support Wayland ("unsupported
    // platform"). When an X11 display is available, force it (mirrors the egui
    // version's `builder.with_x11()`), like the egui main.rs.
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("DISPLAY").is_some() {
            std::env::remove_var("WAYLAND_DISPLAY");
        }
    }

    // ---- Настройки и ядро (общие в Arc<Mutex> для колбэков/таймера) ----
    let settings = Arc::new(Mutex::new(SettingsStore::load()));
    let player = Arc::new(Mutex::new(Player::new()));
    {
        let s = settings.lock().unwrap();
        let mut p = player.lock().unwrap();
        p.set_volume(s.settings.volume);
        p.set_muted(s.settings.muted);
        if !s.settings.audio_device.is_empty() {
            p.set_preferred_device(s.settings.audio_device.clone());
        }
    }

    // ---- Трей (SNI) ----
    let (tray_rx, _tray_up_tx) = tray::start();

    // ---- UI ----
    let ui = AppWindow::new()?;
    let ui_handle = ui.as_weak();

    // Форсируем тёмную схему Material (виджеты соответствуют тёмным поверхностям).
    Palette::get(&ui).set_color_scheme(slint::language::ColorScheme::Dark);

    // Начальные значения из настроек.
    {
        let s = settings.lock().unwrap();
        ui.set_cover_size(s.settings.cover_size as f32);
        ui.set_info_width(s.settings.col_info_w as f32);
        ui.set_col_gap(s.settings.col_gap as f32);
        ui.set_volume(s.settings.volume);
        ui.set_muted(s.settings.muted);
        ui.set_repeat(s.settings.repeat != RepeatMode::Off);
        ui.set_shuffle(s.settings.shuffle);
    }

    // ---- Плейлист (S2): загружаем треки, строим модель таблицы ----
    let tracks = Arc::new(RefCell::new(playlist::load_track_list(
        &music_player_rs::settings::playlist_path(),
    )));
    let current = Arc::new(RefCell::new(None::<usize>));

    let rebuild: Arc<dyn Fn(&SettingsStore, &[Track]) + Send + Sync> = {
        let ui_handle = ui_handle.clone();
        Arc::new(move |settings: &SettingsStore, tracks: &[Track]| {
            let cols: Vec<ColumnId> = settings
                .settings
                .ordered_columns()
                .into_iter()
                .filter(|c| settings.settings.column_visible(*c))
                .collect();
            let ui = match ui_handle.upgrade() {
                Some(u) => u,
                None => return,
            };
            ui.set_playlist_rows(build_rows(tracks, &cols));
            ui.set_playlist_columns(ModelRc::new(VecModel::from(build_columns(&cols))));
        })
    };
    {
        let s = settings.lock().unwrap();
        rebuild(&s, &tracks.borrow());
    }

    // ---- Колбэки от UI к ядру ----
    ui.on_play_pause({
        let player = player.clone();
        move || {
            player.lock().unwrap().toggle();
        }
    });
    ui.on_stop({
        let player = player.clone();
        move || {
            player.lock().unwrap().stop();
        }
    });
    ui.on_seek({
        let player = player.clone();
        move |fraction: f32| {
            let mut p = player.lock().unwrap();
            if let Some(dur) = p.snapshot().2 {
                p.seek(fraction as f64 * dur);
            }
        }
    });
    ui.on_volume_changed({
        let player = player.clone();
        let settings = settings.clone();
        let ui_handle = ui_handle.clone();
        move |v: f32| {
            player.lock().unwrap().set_volume(v);
            settings.lock().unwrap().settings.volume = v;
            settings.lock().unwrap().save();
            ui_handle.upgrade().map(|u| u.set_volume(v));
        }
    });
    ui.on_toggle_mute({
        let player = player.clone();
        let settings = settings.clone();
        let ui_handle = ui_handle.clone();
        move || {
            player.lock().unwrap().toggle_mute();
            let muted = player.lock().unwrap().muted();
            settings.lock().unwrap().settings.muted = muted;
            settings.lock().unwrap().save();
            ui_handle.upgrade().map(|u| u.set_muted(muted));
        }
    });

    // Воспроизведение трека по индексу (видимый порядок таблицы).
    let play_track = {
        let player = player.clone();
        let ui_handle = ui_handle.clone();
        let tracks = tracks.clone();
        let current = current.clone();
        move |index: usize| {
            let tracks = tracks.borrow();
            if index >= tracks.len() {
                return;
            }
            let path = tracks[index].path.clone();
            match player.lock().unwrap().open(&path) {
                Ok(_info) => {
                    *current.borrow_mut() = Some(index);
                    player.lock().unwrap().play();
                    if let Some(u) = ui_handle.upgrade() {
                        u.set_current_row(index as i32);
                    }
                }
                Err(_) => {
                    if let Some(u) = ui_handle.upgrade() {
                        u.set_current_row(-1);
                    }
                }
            }
        }
    };

    ui.on_play_track({
        let play_track = play_track.clone();
        move |row: i32| {
            if row >= 0 {
                play_track(row as usize);
            }
        }
    });

    // Сортировка по колонке заголовка.
    let on_sort = {
        let settings = settings.clone();
        let ui_handle = ui_handle.clone();
        let tracks = tracks.clone();
        let current = current.clone();
        let rebuild = rebuild.clone();
        move |col_idx: i32, desc: bool| {
            let s = settings.lock().unwrap();
            let cols: Vec<ColumnId> = s
                .settings
                .ordered_columns()
                .into_iter()
                .filter(|c| s.settings.column_visible(*c))
                .collect();
            let col = match cols.get(col_idx as usize).copied() {
                Some(c) => c,
                None => return,
            };
            drop(s);
            if col == ColumnId::Index {
                return;
            }
            let mut tr = tracks.borrow_mut();
            let prev_path = (*current.borrow())
                .and_then(|i| tr.get(i))
                .map(|t| t.path.clone());
            if desc {
                tr.sort_by(|a, b| playlist::sort_rows_compare(b, a, col));
            } else {
                tr.sort_by(|a, b| playlist::sort_rows_compare(a, b, col));
            }
            *current.borrow_mut() =
                prev_path.and_then(|p| tr.iter().position(|t| t.path == p));
            drop(tr);
            let s = settings.lock().unwrap();
            rebuild(&s, &tracks.borrow());
            drop(s);
            if let Some(u) = ui_handle.upgrade() {
                u.set_current_row(current.borrow().map(|i| i as i32).unwrap_or(-1));
            }
        }
    };
    ui.on_sort_ascending({
        let on_sort = on_sort.clone();
        move |c: i32| on_sort(c, false)
    });
    ui.on_sort_descending({
        let on_sort = on_sort.clone();
        move |c: i32| on_sort(c, true)
    });

    // prev/next по плейлисту.
    ui.on_prev_track({
        let play_track = play_track.clone();
        let current = current.clone();
        let tracks = tracks.clone();
        move || {
            let n = tracks.borrow().len();
            let idx = playlist::advance_index(*current.borrow(), -1, n, RepeatMode::All);
            if let Some(i) = idx {
                play_track(i);
            }
        }
    });
    ui.on_next_track({
        let play_track = play_track.clone();
        let current = current.clone();
        let tracks = tracks.clone();
        move || {
            let n = tracks.borrow().len();
            let idx = playlist::advance_index(*current.borrow(), 1, n, RepeatMode::All);
            if let Some(i) = idx {
                play_track(i);
            }
        }
    });
    ui.on_toggle_repeat({
        let settings = settings.clone();
        let ui_handle = ui_handle.clone();
        move || {
            let mut s = settings.lock().unwrap();
            s.settings.repeat = if s.settings.repeat == RepeatMode::All {
                RepeatMode::Off
            } else {
                RepeatMode::All
            };
            let on = s.settings.repeat != RepeatMode::Off;
            s.save();
            ui_handle.upgrade().map(|u| u.set_repeat(on));
        }
    });
    ui.on_toggle_shuffle({
        let settings = settings.clone();
        let ui_handle = ui_handle.clone();
        move || {
            let mut s = settings.lock().unwrap();
            s.settings.shuffle = !s.settings.shuffle;
            let sh = s.settings.shuffle;
            s.save();
            ui_handle.upgrade().map(|u| u.set_shuffle(sh));
        }
    });

    // ---- Спектр: FFT + сглаживание (симуляция сигнала из примера) ----
    const FFT_SIZE: usize = 64;
    const SPECTRUM_LEN: usize = FFT_SIZE / 2;
    let mut planner = rustfft::FftPlanner::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let tick = RefCell::new(0.0f32);
    let prev = RefCell::new(vec![0.0f32; SPECTRUM_LEN]);

    // ---- Таймер 60fps: спектр + позиция/время + статус + трей ----
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(16), {
        let player = player.clone();
        move || {
            let ui = match ui_handle.upgrade() {
                Some(u) => u,
                None => return,
            };

            // ШАГ A: симуляция аудиосигнала (до S5 реальные сэмплы не подключены).
            let mut buffer: Vec<num_complex::Complex<f32>> = (0..FFT_SIZE)
                .map(|i| {
                    let t = i as f32 / FFT_SIZE as f32;
                    let f1 = (t * 2.0 * std::f32::consts::PI * 3.0 + *tick.borrow()).sin();
                    let f2 =
                        (t * 2.0 * std::f32::consts::PI * 10.0 + *tick.borrow() * 1.8).sin() * 0.4;
                    let noise = (*tick.borrow() * (i as f32)).cos() * 0.15;
                    num_complex::Complex::new(f1 + f2 + noise, 0.0)
                })
                .collect();

            // ШАГ B: FFT
            fft.process(&mut buffer);

            // ШАГ C: амплитуды со сглаживанием (Attack & Decay).
            let mut p = prev.borrow_mut();
            const DECAY_RATE: f32 = 0.08;
            let smoothed: Vec<f32> = buffer[0..SPECTRUM_LEN]
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let raw = (c.norm() / (FFT_SIZE as f32).sqrt()).clamp(0.0, 1.0);
                    let cur = if raw > p[i] { raw } else { (p[i] - DECAY_RATE).max(0.0) };
                    p[i] = cur;
                    cur
                })
                .collect();
            *tick.borrow_mut() += 0.08;

            // ШАГ D: передать спектр в UI.
            ui.set_spectrum_data(ModelRc::new(VecModel::from(smoothed)));

            // Позиция/время/статус воспроизведения.
            let (playing, pos, dur) = player.lock().unwrap().snapshot();
            ui.set_playing(playing);
            ui.set_pos(playlist::format_duration(pos).into());
            ui.set_dur(playlist::format_duration(dur.unwrap_or(0.0)).into());

            // Трей-команды (show/hide, quit, transport).
            loop {
                match tray_rx.try_recv() {
                    Ok(tray::TrayCmd::TogglePlay) => {
                        player.lock().unwrap().toggle();
                    }
                    Ok(tray::TrayCmd::Stop) => {
                        player.lock().unwrap().stop();
                    }
                    Ok(tray::TrayCmd::Quit) => {
                        let _ = ui.hide();
                        return;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                    _ => {}
                }
            }

            // Статус-бар: короткая строка состояния.
            let status = if playing {
                format!("Playing — {}", playlist::format_duration(pos))
            } else {
                "Stopped".to_string()
            };
            ui.set_status(status.into());
        }
    });

    ui.run()
}
