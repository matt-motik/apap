#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use app::MusicApp;
use eframe::egui;

fn main() -> eframe::Result<()> {
    // winit's Wayland backend can neither hide nor restore windows
    // (set_visible is a no-op, unminimize is unsupported), which breaks the
    // tray's show/hide and minimize-to-tray. Prefer the X11 backend when a
    // display is available so those work (under XWayland on Wayland sessions).
    #[cfg(target_os = "linux")]
    let force_x11 = std::env::var_os("DISPLAY").is_some();

    let mut native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(1150.0, 700.0))
            .with_min_inner_size(egui::vec2(800.0, 480.0))
            .with_title("Music Player"),
        ..Default::default()
    };

    #[cfg(target_os = "linux")]
    {
        use winit::platform::x11::EventLoopBuilderExtX11;
        if force_x11 {
            native_options.event_loop_builder = Some(Box::new(|builder| {
                builder.with_x11();
            }));
        }
    }

    eframe::run_native(
        "Music Player",
        native_options,
        Box::new(
            |cc| -> Result<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>> {
                Ok(Box::new(MusicApp::new(cc)))
            },
        ),
    )
}
