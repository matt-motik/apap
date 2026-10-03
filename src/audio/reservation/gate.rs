//! Ворота exclusive-открытия старого пути (С1): `hw:` открывается только при
//! удерживаемом резервировании (ТЗ-48), повтор `EBUSY` не дольше 1 с
//! (ТЗ-122, И-Р21), освобождение — PCM раньше `ReleaseName` (И-Р1), потеря
//! имени закрывает PCM немедленно (И-Р20). Ожидание ответа службы не
//! блокирует: вызывающий опрашивает `poll()` на своём тике (§8 С1).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use super::{Reservation, ReservationMsg, ReservationService};
use crate::audio::clock::ClockInstant;
use crate::audio::error::CaptureFailure;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

/// Сколько повторяется `EBUSY` после получения резервирования (ТЗ-122, И-Р21).
pub const EBUSY_RETRY_WINDOW: Duration = Duration::from_secs(1);

/// Причина отказа открытия PCM, классифицированная вызывающим (§6.6 OPEN).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PcmOpenError {
    /// `EBUSY`: прежний владелец ещё закрывает PCM.
    Busy,
    /// `ENOENT` / `ENODEV`.
    Disconnected,
    Refused { errno: i32 },
}

/// Событие ворот для вызывающего.
#[derive(Clone, PartialEq, Debug)]
pub enum GateEvent {
    /// Резервирование получено (или недоступно по ОВ-35) — можно открывать PCM.
    Ready,
    Failed(CaptureFailure),
    /// `NameLost`: вызывающий немедленно вызывает `release(&mut pcm)` (И-Р20).
    Lost,
}

/// Итог одной попытки открытия PCM.
#[derive(Debug)]
pub enum OpenOutcome<P> {
    Opened(P),
    /// `EBUSY` в пределах окна — повторить на следующем тике.
    Retry,
    Failed(CaptureFailure),
    /// Резервирования нет: открытие `hw:` запрещено (ТЗ-48).
    NotHeld,
}

/// Наблюдаемое состояние ворот (для замка/статуса).
#[derive(Clone, PartialEq, Debug)]
pub enum GateStatus {
    Idle,
    /// Ожидание ответа службы («захват…», ТЗ-119).
    Pending,
    Held,
    /// Имя потеряно, PCM ещё не закрыт вызывающим.
    Lost,
    Failed(CaptureFailure),
}

enum State {
    Idle,
    Pending { attempt: u64, card: u32 },
    Held { card: u32, reservation: Reservation, busy_since: Option<ClockInstant> },
    /// Резервирование держится только до `release` (drop после закрытия PCM, И-Р1).
    Lost { _reservation: Reservation },
    Failed(CaptureFailure),
}

/// Ворота exclusive-открытия поверх [`ReservationService`] (ADR-08, §6.6).
pub struct ExclusiveGate {
    service: Box<dyn ReservationService>,
    rx: Receiver<ReservationMsg>,
    next_attempt: u64,
    state: State,
}

impl ExclusiveGate {
    /// `rx` — приёмник канала, отправитель которого отдан `service`.
    pub fn new(service: Box<dyn ReservationService>, rx: Receiver<ReservationMsg>) -> ExclusiveGate {
        ExclusiveGate { service, rx, next_attempt: 0, state: State::Idle }
    }

    pub fn status(&self) -> GateStatus {
        match &self.state {
            State::Idle => GateStatus::Idle,
            State::Pending { .. } => GateStatus::Pending,
            State::Held { .. } => GateStatus::Held,
            State::Lost { .. } => GateStatus::Lost,
            State::Failed(f) => GateStatus::Failed(f.clone()),
        }
    }

    /// `hw:` открыт без резервирования (ОВ-35): этап «Вывод» предупреждает.
    pub fn unreserved(&self) -> bool {
        matches!(&self.state, State::Held { reservation: Reservation::UnavailableNoServer, .. })
    }

    /// Запросить резервирование карты. Если оно уже удерживается для этой
    /// карты (переоткрытие между треками), сразу `Some(Ready)`. Удерживаемое
    /// резервирование другой карты снимается: PCM к этому моменту закрыт
    /// вызывающим через `release`.
    pub fn begin(&mut self, card: u32, device_name: &str) -> Option<GateEvent> {
        match &mut self.state {
            // Окно повтора `EBUSY` не сбрасывается: повтор идёт через `begin` +
            // `try_open` на тике, и сброс сделал бы его бесконечным (ТЗ-122).
            State::Held { card: held, .. } if *held == card => return Some(GateEvent::Ready),
            State::Pending { card: pending, .. } if *pending == card => return None,
            _ => {}
        }
        self.next_attempt = self.next_attempt.wrapping_add(1);
        let attempt = self.next_attempt;
        self.state = State::Pending { attempt, card };
        self.service.request(attempt, card, device_name);
        None
    }

    /// Разобрать ответы службы без ожидания. Ответы отменённых попыток
    /// отбрасываются (их резервирование освобождается при drop).
    pub fn poll(&mut self) -> Option<GateEvent> {
        loop {
            let msg = match self.rx.try_recv() {
                Ok(m) => m,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            };
            if let Some(ev) = self.apply(msg) {
                return Some(ev);
            }
        }
    }

    fn apply(&mut self, msg: ReservationMsg) -> Option<GateEvent> {
        match msg {
            ReservationMsg::Done { attempt, result } => {
                let State::Pending { attempt: want, card } = self.state else { return None };
                if attempt != want {
                    return None;
                }
                match result {
                    Ok(reservation) => {
                        self.state = State::Held { card, reservation, busy_since: None };
                        Some(GateEvent::Ready)
                    }
                    Err(f) => {
                        self.state = State::Failed(f.clone());
                        Some(GateEvent::Failed(f))
                    }
                }
            }
            ReservationMsg::Lost { card } => {
                let held_card = match &self.state {
                    State::Held { card: c, reservation: Reservation::Held(_), .. } => *c,
                    _ => return None,
                };
                if held_card != card {
                    return None;
                }
                let State::Held { reservation, .. } = std::mem::replace(&mut self.state, State::Idle) else {
                    return None;
                };
                self.state = State::Lost { _reservation: reservation };
                Some(GateEvent::Lost)
            }
        }
    }

    /// Одна попытка открыть PCM при удерживаемом резервировании (§6.6 OPEN).
    /// `EBUSY` повторяется, пока с первого `EBUSY` прошло меньше 1 с; затем
    /// `ReleaseName` и `BusyOutsideProtocol` (ТЗ-122). Прочие ошибки — без повтора.
    pub fn try_open<P>(&mut self, now: ClockInstant, open: impl FnOnce() -> Result<P, PcmOpenError>) -> OpenOutcome<P> {
        let State::Held { busy_since, .. } = &mut self.state else { return OpenOutcome::NotHeld };
        let failure = match open() {
            Ok(pcm) => {
                *busy_since = None;
                return OpenOutcome::Opened(pcm);
            }
            Err(PcmOpenError::Busy) => {
                let since = *busy_since.get_or_insert(now);
                if now.saturating_since(since) < EBUSY_RETRY_WINDOW {
                    return OpenOutcome::Retry;
                }
                CaptureFailure::BusyOutsideProtocol
            }
            Err(PcmOpenError::Disconnected) => CaptureFailure::DeviceDisconnected,
            Err(PcmOpenError::Refused { errno }) => CaptureFailure::DriverRefused { errno },
        };
        // Старое состояние (с резервированием) падает здесь: ReleaseName + unexport.
        self.state = State::Failed(failure.clone());
        OpenOutcome::Failed(failure)
    }

    /// Освобождение (стоп, смена устройства/режима, выход, `Lost`): сначала
    /// закрыть PCM, затем снять резервирование (И-Р1). После `Lost` ворота
    /// остаются в `Failed(ReservationLost)` (И-Р20).
    pub fn release<P>(&mut self, pcm: &mut Option<P>) {
        *pcm = None;
        let next = match self.state {
            State::Lost { .. } => State::Failed(CaptureFailure::ReservationLost),
            _ => State::Idle,
        };
        self.state = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::clock::Clock;
    use crate::audio::reservation::{BusReservationService, ReserveBus};
    use crate::audio::testing::{BusCall, FakeOwner, FakePcm, FakeReserveBus, FakeServerProbe, ManualClock};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Instant;

    fn gate(fake: &FakeReserveBus, probe: FakeServerProbe) -> ExclusiveGate {
        let (tx, rx) = mpsc::channel();
        let bus: Arc<dyn ReserveBus> = Arc::new(fake.clone());
        ExclusiveGate::new(Box::new(BusReservationService::new(bus, Box::new(probe), tx)), rx)
    }

    /// Опрос как на тике UI, но без реального ожидания тика.
    fn wait(g: &mut ExclusiveGate) -> GateEvent {
        let start = Instant::now();
        loop {
            if let Some(ev) = g.poll() {
                return ev;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "служба не ответила");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn open_ok(fake: &FakeReserveBus, card: u32) -> impl FnOnce() -> Result<FakePcm, PcmOpenError> + '_ {
        move || Ok(fake.open_pcm(card))
    }

    #[test]
    fn reservation_before_pcm_order() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let mut g = gate(&fake, FakeServerProbe::desktop());
        assert!(matches!(g.try_open(ClockInstant::START, open_ok(&fake, 1)), OpenOutcome::NotHeld));
        assert_eq!(g.begin(1, "DAC"), None);
        assert_eq!(wait(&mut g), GateEvent::Ready);
        let OpenOutcome::Opened(pcm) = g.try_open(ClockInstant::START, open_ok(&fake, 1)) else { panic!() };
        let calls = fake.calls();
        let req = calls.iter().position(|c| matches!(c, BusCall::RequestName { .. })).unwrap();
        let open = calls.iter().position(|c| matches!(c, BusCall::PcmOpen { .. })).unwrap();
        assert!(req < open, "{calls:?}");
        // Переоткрытие между треками: резервирование не запрашивается заново.
        assert_eq!(g.begin(1, "DAC"), Some(GateEvent::Ready));
        let mut pcm = Some(pcm);
        g.release(&mut pcm);
        assert_eq!(g.status(), GateStatus::Idle);
    }

    #[test]
    fn shutdown_releases_pcm_then_reservation() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let mut g = gate(&fake, FakeServerProbe::desktop());
        g.begin(0, "DAC");
        assert_eq!(wait(&mut g), GateEvent::Ready);
        let OpenOutcome::Opened(pcm) = g.try_open(ClockInstant::START, open_ok(&fake, 0)) else { panic!() };
        let mut pcm = Some(pcm);
        g.release(&mut pcm);
        let calls = fake.calls();
        let tail = &calls[calls.len() - 3..];
        assert_eq!(tail, &[BusCall::PcmClose { card: 0 }, BusCall::ReleaseName { card: 0 }, BusCall::Unexport { card: 0 }]);
    }

    fn busy_case(busy_for: Duration) -> (FakeReserveBus, OpenOutcome<FakePcm>) {
        let clock = ManualClock::new();
        let fake = FakeReserveBus::new(clock.clone(), FakeOwner::Free);
        let mut g = gate(&fake, FakeServerProbe::desktop());
        g.begin(0, "DAC");
        assert_eq!(wait(&mut g), GateEvent::Ready);
        let start = clock.now();
        loop {
            let busy = clock.now().saturating_since(start) < busy_for;
            let out = g.try_open(clock.now(), || if busy { Err(PcmOpenError::Busy) } else { Ok(fake.open_pcm(0)) });
            match out {
                OpenOutcome::Retry => clock.advance(Duration::from_millis(50)),
                other => return (fake, other),
            }
        }
    }

    #[test]
    fn ebusy_after_reservation_busy_outside_protocol() {
        let (_fake, out) = busy_case(Duration::from_millis(900));
        assert!(matches!(out, OpenOutcome::Opened(_)));

        let (fake, out) = busy_case(Duration::from_millis(1100));
        assert!(matches!(out, OpenOutcome::Failed(CaptureFailure::BusyOutsideProtocol)));
        let calls = fake.calls();
        assert_eq!(&calls[calls.len() - 2..], &[BusCall::ReleaseName { card: 0 }, BusCall::Unexport { card: 0 }]);
        assert!(!calls.iter().any(|c| matches!(c, BusCall::PcmOpen { .. })));
    }

    #[test]
    fn ebusy_other_errors_not_retried() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let mut g = gate(&fake, FakeServerProbe::desktop());
        g.begin(0, "DAC");
        wait(&mut g);
        let out = g.try_open::<FakePcm>(ClockInstant::START, || Err(PcmOpenError::Disconnected));
        assert!(matches!(out, OpenOutcome::Failed(CaptureFailure::DeviceDisconnected)));
        g.begin(0, "DAC");
        wait(&mut g);
        let out = g.try_open::<FakePcm>(ClockInstant::START, || Err(PcmOpenError::Refused { errno: 22 }));
        assert!(matches!(out, OpenOutcome::Failed(CaptureFailure::DriverRefused { errno: 22 })));
    }

    #[test]
    fn name_lost_closes_pcm_immediately() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let mut g = gate(&fake, FakeServerProbe::desktop());
        g.begin(2, "DAC");
        wait(&mut g);
        let OpenOutcome::Opened(pcm) = g.try_open(ClockInstant::START, open_ok(&fake, 2)) else { panic!() };
        let mut pcm = Some(pcm);
        assert!(fake.lose_name(2));
        assert_eq!(g.poll(), Some(GateEvent::Lost));
        assert_eq!(g.status(), GateStatus::Lost);
        g.release(&mut pcm);
        assert!(pcm.is_none());
        assert_eq!(g.status(), GateStatus::Failed(CaptureFailure::ReservationLost));
        let calls = fake.calls();
        assert_eq!(&calls[calls.len() - 3..], &[BusCall::PcmClose { card: 2 }, BusCall::ReleaseName { card: 2 }, BusCall::Unexport { card: 2 }]);
    }

    #[test]
    fn no_bus_no_server_opens_hw_with_warning() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let mut g = gate(&fake, FakeServerProbe { session_bus: false, server_running: false });
        g.begin(0, "DAC");
        assert_eq!(wait(&mut g), GateEvent::Ready);
        assert!(g.unreserved());
        assert!(matches!(g.try_open(ClockInstant::START, open_ok(&fake, 0)), OpenOutcome::Opened(_)));
    }

    #[test]
    fn reservation_denied_blocks_open() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Denies { app: None });
        let mut g = gate(&fake, FakeServerProbe::desktop());
        g.begin(0, "DAC");
        assert_eq!(wait(&mut g), GateEvent::Failed(CaptureFailure::ReservationDenied { owner: None }));
        assert!(matches!(g.try_open(ClockInstant::START, open_ok(&fake, 0)), OpenOutcome::NotHeld));
    }

    #[test]
    fn reservation_stale_attempt_is_dropped_and_released() {
        let fake = FakeReserveBus::new(ManualClock::new(), FakeOwner::Free);
        let mut g = gate(&fake, FakeServerProbe::desktop());
        g.begin(0, "DAC A");
        g.begin(1, "DAC B");
        assert_eq!(wait(&mut g), GateEvent::Ready);
        // Ответ на карту 0 может прийти после ответа на карту 1: дождаться его.
        let start = Instant::now();
        while !fake.calls().contains(&BusCall::Unexport { card: 0 }) {
            g.poll();
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(g.status(), GateStatus::Held);
    }
}
