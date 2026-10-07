//! Трей (ADR-6, В-1, §2.8): StatusNotifier через ksni — только Linux; на
//! прочих ОС трея нет, `run()` сразу сообщает `ready = false`.

use std::sync::mpsc;

use super::lifecycle::TrayEvent;
use crate::audio::clock::Clock;

/// Minimum interval between tray tooltip pushes from the tick loop (ms).
pub const TRAY_UPDATE_INTERVAL_MS: u128 = 300;

/// Text shown as a transient tray tooltip the moment bit-perfect (Direct
/// Output) is enabled: the software volume stage is bypassed.
pub const BP_NOTICE_TEXT: &str =
    "Bit-perfect: громкость не регулируется программно (регулировка на внешнем предусилителе / ЦАП)";

/// State pushed from the application to the tray (title/tooltip updates).
#[derive(Debug, Clone, Default)]
pub struct TrayState {
    pub now_playing: String,
    pub playing: bool,
    /// Bit-perfect (Direct Output) mode is active: software volume is bypassed.
    pub bit_perfect: bool,
    /// Non-empty when audio is unavailable (e.g. device missing at startup).
    pub error: Option<String>,
    /// Transient tooltip override (e.g. bit-perfect volume notice). When set,
    /// the tray tooltip shows this text instead of the regular status.
    pub notice: Option<String>,
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{mpsc, Clock, TrayEvent, TrayPort, TrayState};
    use crate::platform::lifecycle::TrayScroll;
    use ksni::menu::{MenuItem, StandardItem};

    struct PlayerTray {
        notifier: mpsc::Sender<TrayEvent>,
        /// Метка времени `TrayScroll` для модуля ввода (ADR-8, ТЗ-24).
        clock: Box<dyn Clock>,
        state: TrayState,
    }

    impl PlayerTray {
        fn item(&self, label: &str, icon: &str, cmd: TrayEvent) -> MenuItem<Self> {
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
            let _ = self.notifier.send(TrayEvent::TogglePlay);
        }

        fn secondary_activate(&mut self, _x: i32, _y: i32) {
            let _ = self.notifier.send(TrayEvent::ShowHide);
        }

        fn scroll(&mut self, delta: i32, orientation: ksni::Orientation) {
            let ev = TrayScroll {
                t: self.clock.now(),
                delta,
                vertical: matches!(orientation, ksni::Orientation::Vertical),
            };
            let _ = self.notifier.send(TrayEvent::Scroll(ev));
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
            let description = match &self.state.notice {
                Some(n) if !n.is_empty() => n.clone(),
                _ => match &self.state.error {
                    Some(e) => format!("Playback unavailable: {e}"),
                    None if self.state.bit_perfect => {
                        "Playing \u{2014} Bit-perfect (Direct Output, volume on DAC)".into()
                    }
                    None if self.state.playing => "Playing".into(),
                    None => "Paused".into(),
                },
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
                self.item(
                    "Play / Pause",
                    "media-playback-start",
                    TrayEvent::TogglePlay,
                ),
                self.item("Stop", "media-playback-stop", TrayEvent::Stop),
                self.item("Previous", "media-skip-backward", TrayEvent::Prev),
                self.item("Next", "media-skip-forward", TrayEvent::Next),
                MenuItem::Separator,
                self.item("Show / Hide window", "view-restore", TrayEvent::ShowHide),
                MenuItem::Separator,
                self.item("Quit", "application-exit", TrayEvent::Quit),
            ]
        }
    }

    /// Поток трея StatusNotifier (ADR-6): `clock` ставит метки `TrayScroll`
    /// (ADR-8). После попытки регистрации шлёт в `port.ready` `true` (хост
    /// найден) или `false` (нет D-Bus / хоста StatusNotifier).
    pub(super) fn spawn(port: TrayPort, events: mpsc::Sender<TrayEvent>, clock: Box<dyn Clock>) {
        use ksni::TrayMethods;

        let TrayPort { updates: mut up_rx, ready: ready_tx, rt: rt_slot } = port;
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => {
                    let _ = ready_tx.send(false);
                    return;
                }
            };
            rt.block_on(async move {
                let tray = PlayerTray {
                    notifier: events,
                    clock,
                    state: TrayState::default(),
                };
                let Ok(handle) = tray.spawn().await else {
                    let _ = ready_tx.send(false);
                    return;
                };
                // Рантайм трея — для `LinuxNotifier` (ADR-9), до `ready = true`.
                let _ = rt_slot.set(tokio::runtime::Handle::current());
                let _ = ready_tx.send(true);
                while let Some(state) = up_rx.recv().await {
                    handle.update(|t: &mut PlayerTray| t.state = state).await;
                }
            });
        });
    }
}

/// Концы каналов на стороне трея (ADR-23 шаг 8): UI-концы держит приложение,
/// эти уходят в `Lifecycle::install`.
pub struct TrayPort {
    /// Обновления состояния значка из UI.
    pub updates: tokio::sync::mpsc::UnboundedReceiver<TrayState>,
    /// Одно значение после попытки регистрации (`PlatformCaps.tray`, В-1).
    pub ready: mpsc::Sender<bool>,
    /// Слот рантайма трея для системных уведомлений (ADR-9).
    pub rt: crate::platform::notify::RtSlot,
}

/// Запуск трея (ADR-6, ADR-23 шаг 8): события идут в `events`.
#[cfg(target_os = "linux")]
pub fn run(port: TrayPort, events: mpsc::Sender<TrayEvent>, clock: Box<dyn Clock>) {
    linux::spawn(port, events, clock);
}

/// Трея нет (ADR-6, В-1): сразу `ready = false`.
#[cfg(not(target_os = "linux"))]
pub fn run(port: TrayPort, events: mpsc::Sender<TrayEvent>, clock: Box<dyn Clock>) {
    let _ = (events, clock);
    let _ = port.ready.send(false);
}

/// UI-концы каналов трея (§2.8, ADR-6); пару с `TrayPort` создаёт `main`
/// (ADR-23 шаг 8).
pub struct TrayChannels {
    /// События трея в UI (разбор на тике).
    pub events: mpsc::Receiver<TrayEvent>,
    /// Обновление состояния значка из UI.
    pub updates: tokio::sync::mpsc::UnboundedSender<TrayState>,
    /// Одно значение после попытки регистрации: `true` — хост StatusNotifier
    /// найден, значок показан (`PlatformCaps.tray`, ADR-6, В-1).
    pub ready: mpsc::Receiver<bool>,
}
