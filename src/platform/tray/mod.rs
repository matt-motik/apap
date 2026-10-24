//! Трей (ADR-6, В-1, §2.8): StatusNotifier через ksni — только Linux; на
//! прочих ОС трея нет, `run()` сразу сообщает `ready = false`.

use std::sync::mpsc;

use super::lifecycle::TrayEvent;
use crate::audio::clock::Clock;

/// Minimum interval between tray tooltip pushes from the tick loop (ms).
pub const TRAY_UPDATE_INTERVAL_MS: u128 = 300;

/// State pushed from the application to the tray (title/tooltip updates).
#[derive(Debug, Clone, Default)]
pub struct TrayState {
    pub now_playing: String,
    pub playing: bool,
    /// Bit-perfect (Direct Output) mode is active: software volume is bypassed.
    pub bit_perfect: bool,
    /// Non-empty when audio is unavailable (e.g. device missing at startup).
    pub error: Option<String>,
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{mpsc, Clock, TrayEvent, TrayPort, TrayState};
    use crate::journal::HostSource;
    use crate::platform::input::{TrayHost, TrayScroll};
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
                crate::i18n::APP_NAME.into()
            } else {
                self.state.now_playing.clone()
            }
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            use crate::i18n::{
                tray_tooltip_unavailable, TRAY_TOOLTIP_BIT_PERFECT, TRAY_TOOLTIP_PAUSED,
                TRAY_TOOLTIP_PLAYING,
            };
            // Сообщения идут через `Notifier`, не через тултип (ADR-9, §8.1).
            let description = match &self.state.error {
                Some(e) => tray_tooltip_unavailable(e),
                None if self.state.bit_perfect => TRAY_TOOLTIP_BIT_PERFECT.into(),
                None if self.state.playing => TRAY_TOOLTIP_PLAYING.into(),
                None => TRAY_TOOLTIP_PAUSED.into(),
            };
            ksni::ToolTip {
                icon_name: "multimedia-player".into(),
                icon_pixmap: Vec::new(),
                title: if self.state.now_playing.is_empty() {
                    crate::i18n::APP_NAME.into()
                } else {
                    self.state.now_playing.clone()
                },
                description,
            }
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            use crate::i18n::{
                TRAY_MENU_NEXT, TRAY_MENU_PLAY_PAUSE, TRAY_MENU_PREVIOUS, TRAY_MENU_QUIT,
                TRAY_MENU_SHOW_HIDE, TRAY_MENU_STOP,
            };
            vec![
                self.item(
                    TRAY_MENU_PLAY_PAUSE,
                    "media-playback-start",
                    TrayEvent::TogglePlay,
                ),
                self.item(TRAY_MENU_STOP, "media-playback-stop", TrayEvent::Stop),
                self.item(TRAY_MENU_PREVIOUS, "media-skip-backward", TrayEvent::Prev),
                self.item(TRAY_MENU_NEXT, "media-skip-forward", TrayEvent::Next),
                MenuItem::Separator,
                self.item(TRAY_MENU_SHOW_HIDE, "view-restore", TrayEvent::ShowHide),
                MenuItem::Separator,
                self.item(TRAY_MENU_QUIT, "application-exit", TrayEvent::Quit),
            ]
        }
    }

    /// Имя службы-наблюдателя StatusNotifier: его владелец — процесс хоста
    /// трея (ADR-8, §6.11 «Хост трея»).
    const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";

    /// Разбор хоста трея без ввода-вывода (ADR-8, §6.11 «Хост трея», В-5,
    /// §2.9): сначала `comm` владельца имени наблюдателя (`plasmashell` /
    /// `gnome-shell` — на GNOME с расширением AppIndicator именем владеет сам
    /// `gnome-shell`), иначе элементы `XDG_CURRENT_DESKTOP` (список через `:`,
    /// без учёта регистра: `KDE` / `GNOME`, первый подошедший), иначе
    /// `Unknown`. Второй элемент пары — способ определения для журнала.
    fn classify_host(
        comm: Option<&str>,
        xdg_current_desktop: Option<&str>,
    ) -> (TrayHost, HostSource) {
        match comm.map(str::trim) {
            Some("plasmashell") => return (TrayHost::KdePlasma, HostSource::DbusOwner),
            Some("gnome-shell") => return (TrayHost::GnomeAppIndicator, HostSource::DbusOwner),
            _ => {}
        }
        for desktop in xdg_current_desktop.unwrap_or_default().split(':') {
            let desktop = desktop.trim();
            if desktop.eq_ignore_ascii_case("KDE") {
                return (TrayHost::KdePlasma, HostSource::XdgCurrentDesktop);
            }
            if desktop.eq_ignore_ascii_case("GNOME") {
                return (TrayHost::GnomeAppIndicator, HostSource::XdgCurrentDesktop);
            }
        }
        (TrayHost::Unknown, HostSource::None)
    }

    /// `comm` процесса-владельца имени наблюдателя (ADR-8, §6.11 «Хост
    /// трея», В-5): `GetNameOwner` → `GetConnectionUnixProcessID` по
    /// сессионной шине → `/proc/<pid>/comm`. Любая ошибка (нет шины, имя без
    /// владельца, нет `/proc`) → `None`: дальше — запасной путь по
    /// `XDG_CURRENT_DESKTOP`.
    async fn watcher_owner_comm() -> Option<String> {
        let conn = zbus::Connection::session().await.ok()?;
        let bus = zbus::fdo::DBusProxy::new(&conn).await.ok()?;
        let watcher = zbus::names::BusName::try_from(WATCHER_NAME).ok()?;
        let owner = bus.get_name_owner(watcher).await.ok()?;
        let pid = bus
            .get_connection_unix_process_id(zbus::names::BusName::from(owner))
            .await
            .ok()?;
        std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()
    }

    /// Отправка хоста трея в UI — один раз за запуск (ADR-8, §6.11 «Хост
    /// трея», §2.9); запись журнала `TrayHost` делает приёмник события.
    fn send_host(events: &mpsc::Sender<TrayEvent>, comm: Option<&str>) {
        let xdg = std::env::var("XDG_CURRENT_DESKTOP").ok();
        let (host, via) = classify_host(comm, xdg.as_deref());
        let _ = events.send(TrayEvent::Host { host, via });
    }

    /// Поток трея StatusNotifier (ADR-6): `clock` ставит метки `TrayScroll`
    /// (ADR-8). После попытки регистрации шлёт в `port.ready` `true` (хост
    /// найден) или `false` (нет D-Bus / хоста StatusNotifier). Независимо от
    /// исхода регистрации один раз шлёт в `events` `TrayEvent::Host` (ADR-8,
    /// §6.11 «Хост трея»).
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
                    // Без рантайма D-Bus недоступен: хост — по окружению
                    // (ADR-8, §6.11 «Хост трея»).
                    send_host(&events, None);
                    return;
                }
            };
            rt.block_on(async move {
                // Хост трея определяется отдельной задачей того же рантайма:
                // не задерживает регистрацию значка и не зависит от её исхода
                // (ADR-8, §6.11 «Хост трея», В-5).
                let host_events = events.clone();
                let host_task = tokio::spawn(async move {
                    let comm = watcher_owner_comm().await;
                    send_host(&host_events, comm.as_deref());
                });
                let tray = PlayerTray {
                    notifier: events,
                    clock,
                    state: TrayState::default(),
                };
                let Ok(handle) = tray.spawn().await else {
                    let _ = ready_tx.send(false);
                    // Значка нет, но хост должен дойти до UI: задача
                    // завершается до остановки рантайма.
                    let _ = host_task.await;
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

    #[cfg(test)]
    mod tests {
        use super::{classify_host, HostSource, TrayHost};

        #[test]
        fn classify_host_plasmashell_owner() {
            assert_eq!(
                classify_host(Some("plasmashell\n"), Some("GNOME")),
                (TrayHost::KdePlasma, HostSource::DbusOwner)
            );
        }

        #[test]
        fn classify_host_gnome_shell_owner() {
            assert_eq!(
                classify_host(Some("gnome-shell\n"), None),
                (TrayHost::GnomeAppIndicator, HostSource::DbusOwner)
            );
        }

        #[test]
        fn classify_host_unknown_owner_falls_back_to_xdg_kde() {
            assert_eq!(
                classify_host(Some("waybar\n"), Some("KDE")),
                (TrayHost::KdePlasma, HostSource::XdgCurrentDesktop)
            );
        }

        #[test]
        fn classify_host_xdg_list_with_gnome() {
            assert_eq!(
                classify_host(None, Some("ubuntu:GNOME")),
                (TrayHost::GnomeAppIndicator, HostSource::XdgCurrentDesktop)
            );
        }

        #[test]
        fn classify_host_nothing_known() {
            assert_eq!(
                classify_host(None, None),
                (TrayHost::Unknown, HostSource::None)
            );
            assert_eq!(
                classify_host(Some("waybar"), Some("sway")),
                (TrayHost::Unknown, HostSource::None)
            );
        }
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
