//! Реальная сессионная шина для ReserveDevice1 (ADR-08, §6.6): [`DbusReserveBus`]
//! поверх `zbus::blocking`, проверка аудиосервера [`SystemServerProbe`] (ОВ-35)
//! и номер карты ALSA по id устройства ([`alsa_card_index`]). Только Linux.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use super::{
    reserve_name, reserve_path, AudioServerProbe, CallError, LostNotify, NameFlags, RequestNameReply, ReserveBus,
    ReserveObject, OWNER_NAME_TIMEOUT, RELEASE_TIMEOUT, RESERVE_NAME_PREFIX,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use zbus::blocking::{connection, Connection, MessageIterator};
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedValue;

const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const RESERVE_IFACE: &str = "org.freedesktop.ReserveDevice1";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// zbus-интерфейс объекта резервирования: только делегирует [`ReserveObject`].
struct ReserveIface(ReserveObject);

#[zbus::interface(name = "org.freedesktop.ReserveDevice1")]
impl ReserveIface {
    fn request_release(&self, priority: i32) -> bool {
        self.0.request_release(priority)
    }

    #[zbus(property)]
    fn application_name(&self) -> String {
        self.0.application_name().to_owned()
    }

    #[zbus(property)]
    fn application_device_name(&self) -> String {
        self.0.application_device_name().to_owned()
    }

    #[zbus(property)]
    fn priority(&self) -> i32 {
        self.0.priority()
    }
}

type LostMap = Arc<Mutex<HashMap<u32, Arc<LostNotify>>>>;

fn lock_map(map: &LostMap) -> MutexGuard<'_, HashMap<u32, Arc<LostNotify>>> {
    map.lock().unwrap_or_else(|p| p.into_inner())
}

fn call_error(e: &zbus::Error) -> CallError {
    match e {
        zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut => CallError::Timeout,
        _ => CallError::Failed,
    }
}

/// [`ReserveBus`] поверх сессионной шины. Соединение открывается при первом
/// `export` (на машине без шины конструктор не падает — ОВ-35 решает `acquire`).
/// `NameLost` слушает поток `apap-reserve-lost`, живущий вместе с соединением.
pub struct DbusReserveBus {
    live: Mutex<Option<Live>>,
    lost: LostMap,
}

/// Соединение и рантайм, на котором zbus держит свои задачи (диспетчер
/// `ObjectServer`): с бэкендом tokio они порождаются в контексте рантайма.
struct Live {
    conn: Connection,
    rt: Arc<tokio::runtime::Runtime>,
}

impl Clone for Live {
    fn clone(&self) -> Live {
        Live { conn: self.conn.clone(), rt: Arc::clone(&self.rt) }
    }
}

impl Default for DbusReserveBus {
    fn default() -> Self {
        DbusReserveBus::new()
    }
}

impl DbusReserveBus {
    pub fn new() -> DbusReserveBus {
        DbusReserveBus { live: Mutex::new(None), lost: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn live(&self) -> Result<Live, CallError> {
        let mut slot = self.live.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(l) = slot.as_ref() {
            return Ok(l.clone());
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("apap-reserve-rt")
            .enable_all()
            .build()
            .map_err(|_| CallError::Failed)?;
        let rt = Arc::new(rt);
        let conn = {
            let _ctx = rt.enter();
            Connection::session().map_err(|e| call_error(&e))?
        };
        let live = Live { conn, rt };
        let watcher = live.clone();
        let lost = Arc::clone(&self.lost);
        std::thread::Builder::new()
            .name("apap-reserve-lost".into())
            .spawn(move || {
                let _ctx = watcher.rt.enter();
                watch_name_lost(&watcher.conn, &lost);
            })
            .map_err(|_| CallError::Failed)?;
        *slot = Some(live.clone());
        Ok(live)
    }

    fn existing(&self) -> Option<Live> {
        self.live.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Вызов владельцу имени с собственным таймаутом: отдельное соединение,
    /// потому что таймаут zbus задаётся на соединение.
    fn owner_call<B, R>(&self, card: u32, iface: &str, method: &str, body: &B, timeout: Duration) -> Result<R, CallError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let live = self.live()?;
        let _ctx = live.rt.enter();
        let conn = connection::Builder::session()
            .map_err(|e| call_error(&e))?
            .method_timeout(timeout)
            .build()
            .map_err(|e| call_error(&e))?;
        let reply = conn
            .call_method(Some(reserve_name(card)), reserve_path(card), Some(iface), method, body)
            .map_err(|e| call_error(&e))?;
        reply.body().deserialize::<R>().map_err(|_| CallError::Failed)
    }
}

/// Номер карты из имени `org.freedesktop.ReserveDevice1.AudioN`.
fn card_of(name: &str) -> Option<u32> {
    name.strip_prefix(RESERVE_NAME_PREFIX)?.parse().ok()
}

/// Сигналы `NameLost` приходят владельцу адресно, без правила совпадения.
/// Запоздалый сигнал от прошлого удержания игнорируется: имя снова наше.
fn watch_name_lost(conn: &Connection, lost: &LostMap) {
    let unique = conn.unique_name().map(|n| n.to_string());
    for msg in MessageIterator::from(conn.clone()) {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        if msg.message_type() != MessageType::Signal
            || header.interface().map(|i| i.as_str()) != Some(DBUS_NAME)
            || header.member().map(|m| m.as_str()) != Some("NameLost")
        {
            continue;
        }
        let Ok(name) = msg.body().deserialize::<String>() else { continue };
        let Some(card) = card_of(&name) else { continue };
        let Some(notify) = lock_map(lost).get(&card).cloned() else { continue };
        let owner: Result<String, _> = conn
            .call_method(Some(DBUS_NAME), DBUS_PATH, Some(DBUS_NAME), "GetNameOwner", &(name.as_str(),))
            .and_then(|r| r.body().deserialize::<String>());
        if owner.ok().is_some_and(|o| Some(o) == unique) {
            continue;
        }
        notify();
    }
}

impl ReserveBus for DbusReserveBus {
    fn export(&self, card: u32, object: ReserveObject, on_lost: LostNotify) -> Result<(), CallError> {
        let live = self.live()?;
        let _ctx = live.rt.enter();
        let path = reserve_path(card);
        let server = live.conn.object_server();
        // Остаток прошлого удержания той же карты заменяется новым объектом.
        let _ = server.remove::<ReserveIface, _>(path.as_str());
        server.at(path.as_str(), ReserveIface(object)).map_err(|e| call_error(&e))?;
        lock_map(&self.lost).insert(card, Arc::new(on_lost));
        Ok(())
    }

    fn unexport(&self, card: u32) {
        lock_map(&self.lost).remove(&card);
        if let Some(live) = self.existing() {
            let _ctx = live.rt.enter();
            let _ = live.conn.object_server().remove::<ReserveIface, _>(reserve_path(card).as_str());
        }
    }

    fn request_name(&self, card: u32, flags: NameFlags) -> Result<RequestNameReply, CallError> {
        let live = self.live()?;
        let _ctx = live.rt.enter();
        let reply = live
            .conn
            .call_method(Some(DBUS_NAME), DBUS_PATH, Some(DBUS_NAME), "RequestName", &(reserve_name(card), flags.bits()))
            .map_err(|e| call_error(&e))?;
        match reply.body().deserialize::<u32>() {
            Ok(1) => Ok(RequestNameReply::PrimaryOwner),
            Ok(2) => Ok(RequestNameReply::InQueue),
            Ok(3) => Ok(RequestNameReply::Exists),
            Ok(4) => Ok(RequestNameReply::AlreadyOwner),
            _ => Err(CallError::Failed),
        }
    }

    fn owner_application_name(&self, card: u32) -> Option<String> {
        let value: OwnedValue = self
            .owner_call(card, PROPERTIES_IFACE, "Get", &(RESERVE_IFACE, "ApplicationName"), OWNER_NAME_TIMEOUT)
            .ok()?;
        String::try_from(value).ok()
    }

    fn request_release(&self, card: u32, priority: i32) -> Result<bool, CallError> {
        self.owner_call(card, RESERVE_IFACE, "RequestRelease", &(priority,), RELEASE_TIMEOUT)
    }

    fn release_name(&self, card: u32) {
        // Подписчик снимается до ReleaseName: собственное освобождение не NameLost.
        lock_map(&self.lost).remove(&card);
        if let Some(live) = self.existing() {
            let _ctx = live.rt.enter();
            let _ = live.conn.call_method(Some(DBUS_NAME), DBUS_PATH, Some(DBUS_NAME), "ReleaseName", &(reserve_name(card),));
        }
    }
}

/// Проверка ОВ-35 по окружению: адрес сессионной шины и сокеты PipeWire /
/// PulseAudio в `$XDG_RUNTIME_DIR`.
pub struct SystemServerProbe;

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn socket_alive(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

impl AudioServerProbe for SystemServerProbe {
    fn session_bus(&self) -> bool {
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty())
            || runtime_dir().is_some_and(|d| socket_alive(&d.join("bus")))
    }

    fn server_running(&self) -> bool {
        runtime_dir().is_some_and(|d| socket_alive(&d.join("pipewire-0")) || socket_alive(&d.join("pulse/native")))
    }
}

/// Номер карты ALSA для id устройства (`hw:CARD=4,DEV=0`, `hw:CARD=DX,DEV=0`,
/// `hw:1,0`): числовой — как есть, символьный — по `/proc/asound/cards`.
pub fn alsa_card_index(device_id: &str) -> Option<u32> {
    let token = card_token(device_id)?;
    if let Ok(n) = token.parse() {
        return Some(n);
    }
    let cards = std::fs::read_to_string("/proc/asound/cards").ok()?;
    card_by_id(&cards, token)
}

/// Значение карты из id ALSA-устройства (`CARD=x` или первый позиционный).
fn card_token(device_id: &str) -> Option<&str> {
    let (_, args) = device_id.split_once([':', '='])?;
    let first = args.split(',').next()?.trim();
    let token = first.strip_prefix("CARD=").unwrap_or(first);
    (!token.is_empty()).then_some(token)
}

/// Строки `/proc/asound/cards` вида ` 0 [DX             ]: AV200 - Xonar DX`.
fn card_by_id(cards: &str, id: &str) -> Option<u32> {
    cards.lines().find_map(|line| {
        let (num, rest) = line.trim_start().split_once(' ')?;
        let name = rest.trim_start().strip_prefix('[')?.split(']').next()?.trim();
        if name == id {
            num.parse().ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: &str = " 0 [DX             ]: AV200 - Xonar DX\n                      Asus Virtuoso 100 at 0x4000, irq 17\n 1 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0x54410000 irq 152\n";

    #[test]
    fn reservation_card_token_forms() {
        assert_eq!(card_token("hw:CARD=4,DEV=0"), Some("4"));
        assert_eq!(card_token("hw:CARD=DX,DEV=0"), Some("DX"));
        assert_eq!(card_token("hw:1,0"), Some("1"));
        assert_eq!(card_token("hw=CARD=PCH"), Some("PCH"));
        assert_eq!(card_token("default"), None);
        assert_eq!(alsa_card_index("hw:CARD=7,DEV=0"), Some(7));
    }

    #[test]
    fn reservation_card_by_proc_id() {
        assert_eq!(card_by_id(CARDS, "DX"), Some(0));
        assert_eq!(card_by_id(CARDS, "PCH"), Some(1));
        assert_eq!(card_by_id(CARDS, "NVidia"), None);
    }

    /// Живая сессионная шина (ручной прогон: `cargo test reservation_live -- --ignored`):
    /// свободное имя несуществующей карты берётся, чужой `RequestRelease`
    /// получает `false`, `ApplicationName` = APAP, после drop имя свободно.
    #[test]
    #[ignore = "нужна сессионная шина D-Bus"]
    fn reservation_live_session_bus() {
        use crate::audio::reservation::{acquire, Reservation, APPLICATION_NAME};
        const CARD: u32 = 199;
        let bus: Arc<dyn ReserveBus> = Arc::new(DbusReserveBus::new());
        let r = acquire(&bus, &SystemServerProbe, CARD, "Live test", Box::new(|| {})).unwrap();
        assert!(matches!(r, Reservation::Held(_)));
        let other = DbusReserveBus::new();
        other.live().unwrap();
        assert_eq!(other.owner_application_name(CARD).as_deref(), Some(APPLICATION_NAME));
        assert_eq!(other.request_release(CARD, 100), Ok(false));
        assert_eq!(other.request_name(CARD, NameFlags::DO_NOT_QUEUE), Ok(RequestNameReply::Exists));
        drop(r);
        assert_eq!(other.request_name(CARD, NameFlags::DO_NOT_QUEUE), Ok(RequestNameReply::PrimaryOwner));
        other.release_name(CARD);
    }

    #[test]
    fn reservation_card_of_bus_name() {
        assert_eq!(card_of("org.freedesktop.ReserveDevice1.Audio3"), Some(3));
        assert_eq!(card_of("org.freedesktop.ReserveDevice1.AudioX"), None);
        assert_eq!(card_of("org.example"), None);
    }
}
