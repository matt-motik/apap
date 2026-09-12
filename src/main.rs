#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use app::MusicApp;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;

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

fn main() {
    let ui = app::create_ui().expect("Failed to create Slint UI");
    let app = Rc::new(RefCell::new(MusicApp::new(ui.clone_strong())));
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

    ui.show().unwrap();
    slint::run_event_loop_until_quit().unwrap();
    drop(viz_timer);
    drop(ref_timer);
    drop(timer);
}
