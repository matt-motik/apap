//! Резервирование карты по протоколу `org.freedesktop.ReserveDevice1` до
//! открытия `hw:` (ADR-08, §6.6, ТЗ-48, ТЗ-118, ТЗ-119; ОВ-33…ОВ-35).
//!
//! Граница с D-Bus — трейт [`ReserveBus`]: низкоуровневые вызовы шины, которые
//! реализуют `DbusReservation` (zbus) и `testing::FakeReserveBus`. Алгоритм
//! §6.6 до шага OPEN — [`acquire`]; служба для старого пути и движка —
//! [`BusReservationService`] (запрос в служебном потоке, ответ сообщением
//! [`ReservationMsg`], ОВС-10 п. 6).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::error::CaptureFailure;
use std::fmt;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// Префикс имени шины; полное имя — `org.freedesktop.ReserveDevice1.AudioN`.
pub const RESERVE_NAME_PREFIX: &str = "org.freedesktop.ReserveDevice1.Audio";
/// Префикс пути объекта; полный путь — `/org/freedesktop/ReserveDevice1/AudioN`.
pub const RESERVE_PATH_PREFIX: &str = "/org/freedesktop/ReserveDevice1/Audio";
/// Значение свойства `ApplicationName` (ОВ-33).
pub const APPLICATION_NAME: &str = "APAP";
/// Приоритет запроса и свойство `Priority` (ОВ-33).
pub const PRIORITY: i32 = 0;
/// Таймаут ответа владельца на `RequestRelease` (ОВ-34, ТЗ-119).
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// Таймаут чтения `ApplicationName` у владельца (§6.6).
pub const OWNER_NAME_TIMEOUT: Duration = Duration::from_millis(500);

/// Имя шины резервирования карты `card` (ADR-08).
pub fn reserve_name(card: u32) -> String {
    format!("{RESERVE_NAME_PREFIX}{card}")
}

/// Путь объекта резервирования карты `card` (ADR-08).
pub fn reserve_path(card: u32) -> String {
    format!("{RESERVE_PATH_PREFIX}{card}")
}

/// Флаги `RequestName` (значения спецификации D-Bus).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NameFlags(u32);

impl NameFlags {
    /// Плеер этот флаг не ставит никогда (ADR-08 п. 1, ТЗ-118, И-Р21).
    pub const ALLOW_REPLACEMENT: NameFlags = NameFlags(0x1);
    pub const REPLACE_EXISTING: NameFlags = NameFlags(0x2);
    pub const DO_NOT_QUEUE: NameFlags = NameFlags(0x4);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn union(self, other: NameFlags) -> NameFlags {
        NameFlags(self.0 | other.0)
    }

    pub const fn contains(self, other: NameFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Ответ `RequestName` (спецификация D-Bus).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestNameReply {
    PrimaryOwner,
    InQueue,
    Exists,
    AlreadyOwner,
}

/// Ошибка вызова шины: таймаут отличается от прочих отказов (§6.6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallError {
    Timeout,
    Failed,
}

/// Уведомление о сигнале `NameLost` для имени карты (ADR-08 п. 4, И-Р20).
pub type LostNotify = Box<dyn Fn() + Send + Sync>;

/// Экспортируемый объект резервирования (ADR-08 п. 3, ОВ-33). Логика ответов
/// живёт здесь, чтобы её проверяли тесты без шины; zbus-интерфейс в
/// `dbus.rs` только делегирует.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReserveObject {
    device_name: String,
}

impl ReserveObject {
    pub fn new(device_name: &str) -> ReserveObject {
        ReserveObject { device_name: device_name.to_owned() }
    }

    /// Чужой `RequestRelease` всегда отклоняется: имя держится только при
    /// закрытом замке (ТЗ-118).
    pub fn request_release(&self, _priority: i32) -> bool {
        false
    }

    pub fn application_name(&self) -> &str {
        APPLICATION_NAME
    }

    pub fn application_device_name(&self) -> &str {
        &self.device_name
    }

    pub fn priority(&self) -> i32 {
        PRIORITY
    }
}

/// Низкоуровневые вызовы сессионной шины для резервирования (ADR-08).
/// Таймауты вызовов (`RELEASE_TIMEOUT`, `OWNER_NAME_TIMEOUT`) соблюдает
/// реализация и возвращает `CallError::Timeout`.
pub trait ReserveBus: Send + Sync {
    /// Экспортировать [`ReserveObject`] по `reserve_path(card)` и подписаться на
    /// `NameLost` для `reserve_name(card)`; оба — до `RequestName` (§6.6).
    fn export(&self, card: u32, object: ReserveObject, on_lost: LostNotify) -> Result<(), CallError>;
    /// Снять объект и подписку на `NameLost`.
    fn unexport(&self, card: u32);
    fn request_name(&self, card: u32, flags: NameFlags) -> Result<RequestNameReply, CallError>;
    /// `Get(ApplicationName)` у текущего владельца; при ошибке или таймауте `None`.
    fn owner_application_name(&self, card: u32) -> Option<String>;
    /// `RequestRelease(priority)` у текущего владельца.
    fn request_release(&self, card: u32, priority: i32) -> Result<bool, CallError>;
    /// `ReleaseName`. Собственное освобождение не доставляет `NameLost`.
    fn release_name(&self, card: u32);
}

/// Удерживаемое имя карты. `Drop`: `ReleaseName`, затем unexport (И-Р1).
/// В `DeviceClaim` объявляется после PCM, поэтому PCM закрывается раньше.
pub struct ReservationGuard {
    card: u32,
    bus: Arc<dyn ReserveBus>,
}

impl ReservationGuard {
    pub fn card(&self) -> u32 {
        self.card
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        self.bus.release_name(self.card);
        self.bus.unexport(self.card);
    }
}

impl fmt::Debug for ReservationGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReservationGuard").field("card", &self.card).finish()
    }
}

/// Итог резервирования (§2.3).
#[derive(Debug)]
pub enum Reservation {
    /// Имя `org.freedesktop.ReserveDevice1.AudioN` принадлежит плееру.
    Held(ReservationGuard),
    /// ОВ-35: нет сессионной шины и нет аудиосервера — `hw:` без резервирования,
    /// этап «Вывод» показывает предупреждение.
    UnavailableNoServer,
}

/// Наличие сессионной шины и аудиосервера (ОВ-35, §2.3).
pub trait AudioServerProbe: Send {
    fn session_bus(&self) -> bool;
    fn server_running(&self) -> bool;
}

/// Снимок проверки ОВ-35: передаётся в служебный поток вместо самой проверки.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProbeSnapshot {
    pub session_bus: bool,
    pub server_running: bool,
}

impl ProbeSnapshot {
    pub fn take(probe: &dyn AudioServerProbe) -> ProbeSnapshot {
        ProbeSnapshot { session_bus: probe.session_bus(), server_running: probe.server_running() }
    }
}

impl AudioServerProbe for ProbeSnapshot {
    fn session_bus(&self) -> bool {
        self.session_bus
    }

    fn server_running(&self) -> bool {
        self.server_running
    }
}

/// Резервирование карты по §6.6 до шага OPEN (ADR-08, ТЗ-48, ОВ-33…ОВ-35).
/// Блокирует до ответа владельца (≤ `OWNER_NAME_TIMEOUT` + `RELEASE_TIMEOUT`),
/// поэтому вызывается только в служебном потоке.
pub fn acquire(
    bus: &Arc<dyn ReserveBus>,
    probe: &dyn AudioServerProbe,
    card: u32,
    device_name: &str,
    on_lost: LostNotify,
) -> Result<Reservation, CaptureFailure> {
    if !probe.session_bus() {
        if probe.server_running() {
            return Err(CaptureFailure::NoSessionBus);
        }
        return Ok(Reservation::UnavailableNoServer);
    }
    // Шина объявлена, но соединение не установилось — тот же случай ОВ-35.
    if bus.export(card, ReserveObject::new(device_name), on_lost).is_err() {
        return Err(CaptureFailure::NoSessionBus);
    }
    let held = || Ok(Reservation::Held(ReservationGuard { card, bus: Arc::clone(bus) }));
    let deny = |failure: CaptureFailure| {
        bus.unexport(card);
        Err(failure)
    };
    match bus.request_name(card, NameFlags::DO_NOT_QUEUE) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => return held(),
        Ok(RequestNameReply::Exists) => {}
        Ok(RequestNameReply::InQueue) | Err(_) => return deny(CaptureFailure::ReservationDenied { owner: None }),
    }
    let owner = bus.owner_application_name(card);
    match bus.request_release(card, PRIORITY) {
        Ok(true) => {}
        Ok(false) | Err(CallError::Failed) => return deny(CaptureFailure::ReservationDenied { owner }),
        Err(CallError::Timeout) => return deny(CaptureFailure::OwnerNotResponding),
    }
    match bus.request_name(card, NameFlags::REPLACE_EXISTING.union(NameFlags::DO_NOT_QUEUE)) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => held(),
        _ => deny(CaptureFailure::ReservationDenied { owner }),
    }
}

/// Сообщения службы резервирования движку / старому пути (ОВС-10 п. 6).
#[derive(Debug)]
pub enum ReservationMsg {
    Done { attempt: u64, result: Result<Reservation, CaptureFailure> },
    /// `NameLost` для удерживаемого имени: закрыть PCM немедленно (И-Р20).
    Lost { card: u32 },
}

/// Запрос резервирования; ответ — `ReservationMsg::Done { attempt, .. }` (§2.3).
pub trait ReservationService: Send {
    fn request(&mut self, attempt: u64, card: u32, device_name: &str);
}

/// Служба поверх [`ReserveBus`]: проверка ОВ-35 на месте, [`acquire`] — в
/// служебном потоке `apap-reserve`, чтобы ожидание до 5 с не блокировало
/// вызывающего (ТЗ-119).
pub struct BusReservationService {
    bus: Arc<dyn ReserveBus>,
    probe: Box<dyn AudioServerProbe>,
    tx: Sender<ReservationMsg>,
}

impl BusReservationService {
    pub fn new(bus: Arc<dyn ReserveBus>, probe: Box<dyn AudioServerProbe>, tx: Sender<ReservationMsg>) -> BusReservationService {
        BusReservationService { bus, probe, tx }
    }
}

impl ReservationService for BusReservationService {
    fn request(&mut self, attempt: u64, card: u32, device_name: &str) {
        let probe = ProbeSnapshot::take(self.probe.as_ref());
        let bus = Arc::clone(&self.bus);
        let tx = self.tx.clone();
        let name = device_name.to_owned();
        let job = move || {
            let lost_tx = tx.clone();
            let on_lost: LostNotify = Box::new(move || {
                // Получатель мог завершиться; потеря имени тогда никому не нужна.
                let _ = lost_tx.send(ReservationMsg::Lost { card });
            });
            let result = acquire(&bus, &probe, card, &name, on_lost);
            // Получатель ушёл — `Reservation` падает здесь, имя освобождается (И-Р1).
            let _ = tx.send(ReservationMsg::Done { attempt, result });
        };
        if let Err(e) = std::thread::Builder::new().name("apap-reserve".into()).spawn(job) {
            // Поток не создан (нехватка ресурсов ОС): резервирования нет, `hw:` не открывать.
            let _ = self.tx.send(ReservationMsg::Done {
                attempt,
                result: Err(CaptureFailure::DriverRefused { errno: e.raw_os_error().unwrap_or(0) }),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::clock::Clock;
    use crate::audio::testing::{BusCall, FakeOwner, FakeReserveBus, FakeServerProbe, ManualClock};
    use std::sync::mpsc;

    fn noop() -> LostNotify {
        Box::new(|| {})
    }

    fn bus_dyn(fake: &FakeReserveBus) -> Arc<dyn ReserveBus> {
        Arc::new(fake.clone())
    }

    #[test]
    fn reservation_free_name_held_without_allow_replacement() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let r = acquire(&bus_dyn(&fake), &FakeServerProbe::desktop(), 1, "USB DAC", noop()).unwrap();
        assert!(matches!(&r, Reservation::Held(g) if g.card() == 1));
        assert_eq!(
            fake.calls(),
            vec![BusCall::Export { card: 1 }, BusCall::RequestName { card: 1, flags: NameFlags::DO_NOT_QUEUE }]
        );
        drop(r);
        let calls = fake.calls();
        assert_eq!(&calls[2..], &[BusCall::ReleaseName { card: 1 }, BusCall::Unexport { card: 1 }]);
        assert!(fake.requested_flags().iter().all(|f| !f.contains(NameFlags::ALLOW_REPLACEMENT)));
    }

    #[test]
    fn reservation_no_bus_no_server_is_unavailable_without_bus_calls() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let probe = FakeServerProbe { session_bus: false, server_running: false };
        let r = acquire(&bus_dyn(&fake), &probe, 0, "DAC", noop()).unwrap();
        assert!(matches!(r, Reservation::UnavailableNoServer));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn reservation_no_bus_with_server_is_no_session_bus() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let probe = FakeServerProbe { session_bus: false, server_running: true };
        let r = acquire(&bus_dyn(&fake), &probe, 0, "DAC", noop());
        assert!(matches!(r, Err(CaptureFailure::NoSessionBus)));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn reservation_incoming_request_release_denied() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let _r = acquire(&bus_dyn(&fake), &FakeServerProbe::desktop(), 2, "USB DAC", noop()).unwrap();
        assert_eq!(fake.incoming_request_release(2, 0), Some(false));
        assert_eq!(fake.incoming_request_release(2, i32::MAX), Some(false));
        let obj = fake.exported(2).unwrap();
        assert_eq!(obj.application_name(), "APAP");
        assert_eq!(obj.application_device_name(), "USB DAC");
        assert_eq!(obj.priority(), 0);
    }

    #[test]
    fn reservation_denied_and_timeout() {
        let clock = ManualClock::new();
        let fake = FakeReserveBus::new(clock.clone(), FakeOwner::Denies { app: Some("WirePlumber".into()) });
        let r = acquire(&bus_dyn(&fake), &FakeServerProbe::desktop(), 0, "DAC", noop());
        assert_eq!(r.unwrap_err(), CaptureFailure::ReservationDenied { owner: Some("WirePlumber".into()) });
        assert_eq!(fake.calls().last(), Some(&BusCall::Unexport { card: 0 }));

        let clock = ManualClock::new();
        let fake = FakeReserveBus::new(
            clock.clone(),
            FakeOwner::Silent { app: Some("WirePlumber".into()), reply_after: Duration::from_secs(30) },
        );
        let r = acquire(&bus_dyn(&fake), &FakeServerProbe::desktop(), 0, "DAC", noop());
        assert_eq!(r.unwrap_err(), CaptureFailure::OwnerNotResponding);
        let waited = clock.now().since_start();
        assert!(waited >= Duration::from_millis(4500) && waited <= Duration::from_millis(5500), "{waited:?}");
        assert_eq!(fake.calls().last(), Some(&BusCall::Unexport { card: 0 }));

        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::CallFails);
        let r = acquire(&bus_dyn(&fake), &FakeServerProbe::desktop(), 0, "DAC", noop());
        assert_eq!(r.unwrap_err(), CaptureFailure::ReservationDenied { owner: None });
    }

    #[test]
    fn reservation_owner_replies_after_3s_succeeds() {
        let clock = ManualClock::new();
        let fake = FakeReserveBus::new(
            clock.clone(),
            FakeOwner::Grants { app: Some("WirePlumber".into()), reply_after: Duration::from_secs(3) },
        );
        let r = acquire(&bus_dyn(&fake), &FakeServerProbe::desktop(), 3, "DAC", noop()).unwrap();
        assert!(matches!(r, Reservation::Held(_)));
        assert_eq!(clock.now().since_start(), Duration::from_secs(3));
        assert_eq!(
            fake.requested_flags(),
            vec![NameFlags::DO_NOT_QUEUE, NameFlags::REPLACE_EXISTING.union(NameFlags::DO_NOT_QUEUE)]
        );
        assert!(fake.calls().contains(&BusCall::RequestRelease { card: 3, priority: 0 }));
    }

    #[test]
    fn reservation_service_replies_with_attempt_and_reports_name_lost() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let (tx, rx) = mpsc::channel();
        let mut svc = BusReservationService::new(bus_dyn(&fake), Box::new(FakeServerProbe::desktop()), tx);
        svc.request(7, 1, "DAC");
        let msg = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let ReservationMsg::Done { attempt: 7, result: Ok(Reservation::Held(guard)) } = msg else {
            panic!("unexpected {msg:?}");
        };
        assert!(fake.lose_name(1));
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), ReservationMsg::Lost { card: 1 }));
        drop(guard);
        assert!(!fake.lose_name(1), "после unexport подписки нет");
    }

    #[test]
    fn reservation_names_and_paths() {
        assert_eq!(reserve_name(2), "org.freedesktop.ReserveDevice1.Audio2");
        assert_eq!(reserve_path(0), "/org/freedesktop/ReserveDevice1/Audio0");
        assert_eq!(NameFlags::REPLACE_EXISTING.union(NameFlags::DO_NOT_QUEUE).bits(), 6);
    }
}
