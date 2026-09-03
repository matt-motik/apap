// Slint-плеер (MVP) — переиспользует ядро (player, playlist, settings, tray).
// UI описан в `ui/app.slint`.

use music_player_rs::audio::player::Player;
use music_player_rs::playlist;
use music_player_rs::settings::{RepeatMode, SettingsStore};
use music_player_rs::tray;

use slint::{ComponentHandle, Timer, TimerMode};
use std::sync::{mpsc::TryRecvError, Arc, Mutex};

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("DISPLAY").is_some() {
            std::env::remove_var("WAYLAND_DISPLAY");
        }
    }

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

    let (tray_rx, _tray_up_tx) = tray::start();

    let ui = AppWindow::new()?;
    let ui_handle = ui.as_weak();

    {
        let s = settings.lock().unwrap();
        ui.set_volume(s.settings.volume);
        ui.set_muted(s.settings.muted);
        ui.set_repeat(s.settings.repeat != RepeatMode::Off);
        ui.set_shuffle(s.settings.shuffle);
    }

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

    ui.on_prev_track({
        let player = player.clone();
        move || {
            let mut p = player.lock().unwrap();
            let (playing, pos, _dur) = p.snapshot();
            if playing && pos > 3.0 {
                p.seek(0.0);
            }
        }
    });

    ui.on_next_track({
        let _player = player.clone();
        move || {
            // TODO: next track by playlist index
        }
    });

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(16), {
        let player = player.clone();
        move || {
            let ui = match ui_handle.upgrade() {
                Some(u) => u,
                None => return,
            };

            let (playing, pos, dur) = player.lock().unwrap().snapshot();
            ui.set_playing(playing);
            ui.set_pos(playlist::format_duration(pos).into());
            ui.set_dur(playlist::format_duration(dur.unwrap_or(0.0)).into());

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

            let status = if playing {
                format!("Playing: {}", playlist::format_duration(pos))
            } else {
                "Stopped".to_string()
            };
            ui.set_status(status.into());
        }
    });

    ui.run()
}
