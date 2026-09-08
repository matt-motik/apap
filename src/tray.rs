use std::sync::mpsc;

use ksni::menu::{MenuItem, StandardItem};

/// Minimum interval between tray tooltip pushes from the tick loop (ms).
pub const TRAY_UPDATE_INTERVAL_MS: u128 = 300;

/// Commands sent by the tray menu to the application.
#[derive(Debug, Clone, Copy)]
pub enum TrayCmd {
    TogglePlay,
    Stop,
    Prev,
    Next,
    ShowHide,
    Quit,
    Wheel(i32),
}

/// State pushed from the application to the tray (title/tooltip updates).
#[derive(Debug, Clone, Default)]
pub struct TrayState {
    pub now_playing: String,
    pub playing: bool,
    /// Non-empty when audio is unavailable (e.g. device missing at startup).
    pub error: Option<String>,
}

#[derive(Debug)]
struct PlayerTray {
    notifier: mpsc::Sender<TrayCmd>,
    state: TrayState,
}

impl PlayerTray {
    fn item(&self, label: &str, icon: &str, cmd: TrayCmd) -> MenuItem<Self> {
        let tx = self.notifier.clone();
        StandardItem {
            label: label.into(),
            icon_name: icon.into(),
            activate: Box::new(move |_| {
                let _ = tx.send(cmd);
            }),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for PlayerTray {
    fn id(&self) -> String {
        "music-player".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.notifier.send(TrayCmd::TogglePlay);
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        let _ = self.notifier.send(TrayCmd::ShowHide);
    }

    fn scroll(&mut self, delta: i32, _orientation: ksni::Orientation) {
        let _ = self.notifier.send(TrayCmd::Wheel(delta));
    }

    fn icon_name(&self) -> String {
        "multimedia-player".into()
    }

    fn title(&self) -> String {
        if self.state.now_playing.is_empty() {
            "Music Player".into()
        } else {
            self.state.now_playing.clone()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match &self.state.error {
            Some(e) => format!("Playback unavailable: {e}"),
            None if self.state.playing => "Playing".into(),
            None => "Paused".into(),
        };
        ksni::ToolTip {
            icon_name: "multimedia-player".into(),
            icon_pixmap: Vec::new(),
            title: if self.state.now_playing.is_empty() {
                "Music Player".into()
            } else {
                self.state.now_playing.clone()
            },
            description,
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            self.item("Play / Pause", "media-playback-start", TrayCmd::TogglePlay),
            self.item("Stop", "media-playback-stop", TrayCmd::Stop),
            self.item("Previous", "media-skip-backward", TrayCmd::Prev),
            self.item("Next", "media-skip-forward", TrayCmd::Next),
            MenuItem::Separator,
            self.item("Show / Hide window", "view-restore", TrayCmd::ShowHide),
            MenuItem::Separator,
            self.item("Quit", "application-exit", TrayCmd::Quit),
        ]
    }
}

/// Spawns the StatusNotifierItem tray service on a background thread.
///
/// Returns a receiver for tray commands and a sender for updating the tray
/// state. Fails silently when no StatusNotifier host / D-Bus is available,
/// in which case the returned receiver simply stays empty.
pub fn start() -> (
    mpsc::Receiver<TrayCmd>,
    tokio::sync::mpsc::UnboundedSender<TrayState>,
) {
    use ksni::TrayMethods;

    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (up_tx, mut up_rx) = tokio::sync::mpsc::unbounded_channel::<TrayState>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };
        rt.block_on(async move {
            let tray = PlayerTray {
                notifier: cmd_tx,
                state: TrayState::default(),
            };
            let Ok(handle) = tray.spawn().await else {
                return;
            };
            while let Some(state) = up_rx.recv().await {
                handle.update(|t: &mut PlayerTray| t.state = state).await;
            }
        });
    });

    (cmd_rx, up_tx)
}
