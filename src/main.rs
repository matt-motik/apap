#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use app::MusicApp;
use music_player_rs::journal::{FileJournal, Journal};
use music_player_rs::persist::ConfigPaths;
use music_player_rs::platform::fs::os_fs;
use music_player_rs::theme::create_default_themes;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// UI update period: pulls tray commands, scan/cover results and playback
/// state into the window. Also bounds responsiveness of transport controls.
const TICK_INTERVAL_MS: u64 = 100;

/// High-frequency column reflow period. Reads the playlist width and updates
/// column `.width`s nearly at display rate so the table tracks the window
/// during a resize instead of lagging behind on the slow 100 ms tick.
const REF_INTERVAL_MS: u64 = 16;

/// Live spectrum push period (~30 FPS; ТЗ §11.1). Independent of the 100 ms
/// tick — the bar data needs an animation-rate refresh.
const VIZ_PUSH_INTERVAL_MS: u64 = app::visualizer_manager::VIZ_PUSH_INTERVAL_MS;

/// Срок дозаписи журнала при выходе; полный путь выхода — С4 (ADR-7, ADR-21).
const JOURNAL_FLUSH_BUDGET: Duration = Duration::from_secs(1);

fn main() {
    // Каталог настроек пользователя ищется только здесь (ТЗ-49, ADR-19):
    // остальной код получает пути из `ConfigPaths`.
    let paths = ConfigPaths::in_dir(music_player_rs::settings::config_dir());
    // T1.0 §1/§8.2: гарантировать `themes/` + `dark.toml`/`light.toml`.
    // Существующие файлы не перезаписываются (воссоздаются только
    // отсутствующие, в т.ч. после ручного удаления).
    if let Err(e) = create_default_themes(&paths.dir.join("themes")) {
        eprintln!("[theme] create_default_themes failed: {e}");
    }
    // Журнал и модуль ФС собираются только здесь (ADR-19, ADR-21).
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::start(paths.journal.clone()));
    let (_reader, work_fs, _engine_fs) = os_fs(journal.clone());
    let ui = match app::create_ui() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("не удалось создать окно Slint: {e}");
            journal.flush(JOURNAL_FLUSH_BUDGET);
            return;
        }
    };
    let app = Rc::new(RefCell::new(MusicApp::new(ui.clone_strong(), paths, work_fs, journal.clone())));
    MusicApp::init(&app);

    let weak = ui.as_weak();
    let app_for_tick = app.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(TICK_INTERVAL_MS),
        move || {
            if weak.upgrade().is_none() {
                return;
            }
            app_for_tick.borrow_mut().tick();
        },
    );

    let weak = ui.as_weak();
    let app_for_ref = app.clone();
    let ref_timer = slint::Timer::default();
    ref_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(REF_INTERVAL_MS),
        move || {
            if weak.upgrade().is_none() {
                return;
            }
            app_for_ref.borrow_mut().reflow();
        },
    );

    let weak = ui.as_weak();
    let app_for_viz = app.clone();
    let viz_timer = slint::Timer::default();
    viz_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(VIZ_PUSH_INTERVAL_MS),
        move || {
            if weak.upgrade().is_none() {
                return;
            }
            app_for_viz.borrow_mut().viz_push();
        },
    );

    if let Err(e) = ui.show() {
        eprintln!("не удалось показать окно: {e}");
        journal.flush(JOURNAL_FLUSH_BUDGET);
        return;
    }
    // Размер/позиция окна применимы только после создания поверхности:
    // winit игнорирует `set_size`/`set_position` до `show()`, и окно могло бы
    // схлопнуться до минимального размера контента. Повторно применяем
    // сохранённую геометрию (V5.1-B7) из видимого состояния.
    app.borrow().apply_window_geometry();
    if let Err(e) = slint::run_event_loop_until_quit() {
        eprintln!("цикл событий завершился с ошибкой: {e}");
    }
    drop(viz_timer);
    drop(ref_timer);
    drop(timer);
    // Дописать строки журнала до выхода процесса (ADR-21).
    journal.flush(JOURNAL_FLUSH_BUDGET);
}
