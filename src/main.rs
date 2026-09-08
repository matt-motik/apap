#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use app::MusicApp;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;

/// UI update period: pulls tray commands, scan/cover results and playback
/// state into the window. Also bounds responsiveness of transport controls.
const TICK_INTERVAL_MS: u64 = 100;

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

    ui.show().unwrap();
    slint::run_event_loop_until_quit().unwrap();
    drop(timer);
}
