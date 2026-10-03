//! Тестовая инфраструктура тракта (AM1.0 §7.1, ADR-20). Компилируется всегда,
//! как подмены платформенных модулей (ADR-6 задачи 02): её используют тесты
//! бинарника и `tests/`, которые собирают библиотеку без `cfg(test)`. Фейки
//! трейтов `EngineDeps` добавляются по этапам (таблица «Трейт → этап» под §8).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

pub mod signals;

use crate::audio::clock::{Clock, ClockInstant};
use crate::audio::reservation::{
    AudioServerProbe, CallError, LostNotify, NameFlags, RequestNameReply, ReserveBus, ReserveObject,
    OWNER_NAME_TIMEOUT, RELEASE_TIMEOUT,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Часы теста (ADR-20): время идёт только по `advance`. Клоны делят одно время.
#[derive(Clone, Default)]
pub struct ManualClock {
    nanos: Arc<AtomicU64>,
}

impl ManualClock {
    /// Часы в `ClockInstant::START`.
    pub fn new() -> ManualClock {
        ManualClock::default()
    }

    /// Сдвинуть время вперёд на `d` (насыщается на `u64::MAX` нс ≈ 584 года).
    pub fn advance(&self, d: Duration) {
        let add = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let mut cur = self.nanos.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_add(add);
            match self.nanos.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> ClockInstant {
        ClockInstant::from_start(Duration::from_nanos(self.nanos.load(Ordering::Acquire)))
    }
}

/// Наличие шины и аудиосервера для тестов ОВ-35 (таблица «Трейт → этап», С1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FakeServerProbe {
    pub session_bus: bool,
    pub server_running: bool,
}

impl FakeServerProbe {
    /// Обычный рабочий стол: шина есть, PipeWire запущен.
    pub fn desktop() -> FakeServerProbe {
        FakeServerProbe { session_bus: true, server_running: true }
    }
}

impl AudioServerProbe for FakeServerProbe {
    fn session_bus(&self) -> bool {
        self.session_bus
    }

    fn server_running(&self) -> bool {
        self.server_running
    }
}

/// Поведение текущего владельца имени `ReserveDevice1.AudioN` (§6.6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FakeOwner {
    /// Имя свободно.
    Free,
    /// Отвечает на `RequestRelease` `true` через `reply_after`.
    Grants { app: Option<String>, reply_after: Duration },
    /// Отвечает `false` сразу.
    Denies { app: Option<String> },
    /// Отвечает `true` через `reply_after`; дольше `RELEASE_TIMEOUT` — таймаут.
    Silent { app: Option<String>, reply_after: Duration },
    /// Вызовы к владельцу завершаются ошибкой (не таймаут).
    CallFails,
}

/// Запись журнала `FakeReserveBus` (И-Р1, И-Р21).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BusCall {
    Export { card: u32 },
    Unexport { card: u32 },
    RequestName { card: u32, flags: NameFlags },
    GetApplicationName { card: u32 },
    RequestRelease { card: u32, priority: i32 },
    ReleaseName { card: u32 },
}

struct Exported {
    object: ReserveObject,
    on_lost: Arc<LostNotify>,
}

struct FakeBusState {
    owner: FakeOwner,
    owned: HashMap<u32, bool>,
    exported: HashMap<u32, Exported>,
    calls: Vec<BusCall>,
}

/// Фейк сессионной шины для резервирования (ADR-08, §6.6): ответы «разрешить»,
/// «отказать», «молчать N с», потеря имени; время ожидания идёт по
/// `ManualClock`, журнал вызовов — `calls()`. Клоны делят состояние.
#[derive(Clone)]
pub struct FakeReserveBus {
    clock: ManualClock,
    state: Arc<Mutex<FakeBusState>>,
}

impl FakeReserveBus {
    pub fn new(clock: ManualClock, owner: FakeOwner) -> FakeReserveBus {
        let state = FakeBusState { owner, owned: HashMap::new(), exported: HashMap::new(), calls: Vec::new() };
        FakeReserveBus { clock, state: Arc::new(Mutex::new(state)) }
    }

    fn lock(&self) -> MutexGuard<'_, FakeBusState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn calls(&self) -> Vec<BusCall> {
        self.lock().calls.clone()
    }

    /// Флаги всех вызовов `RequestName` по порядку (И-Р21).
    pub fn requested_flags(&self) -> Vec<NameFlags> {
        self.lock()
            .calls
            .iter()
            .filter_map(|c| match c {
                BusCall::RequestName { flags, .. } => Some(*flags),
                _ => None,
            })
            .collect()
    }

    pub fn exported(&self, card: u32) -> Option<ReserveObject> {
        self.lock().exported.get(&card).map(|e| e.object.clone())
    }

    /// Чужой `RequestRelease` к экспортированному объекту; `None` — объекта нет.
    pub fn incoming_request_release(&self, card: u32, priority: i32) -> Option<bool> {
        self.lock().exported.get(&card).map(|e| e.object.request_release(priority))
    }

    /// Имя отобрано извне (перезапуск шины, внешний `ReleaseName`): сигнал
    /// `NameLost`, если на него есть подписка. Возвращает, был ли доставлен.
    pub fn lose_name(&self, card: u32) -> bool {
        let notify = {
            let mut st = self.lock();
            st.owned.insert(card, false);
            st.exported.get(&card).map(|e| Arc::clone(&e.on_lost))
        };
        match notify {
            Some(n) => {
                n();
                true
            }
            None => false,
        }
    }
}

impl ReserveBus for FakeReserveBus {
    fn export(&self, card: u32, object: ReserveObject, on_lost: LostNotify) -> Result<(), CallError> {
        let mut st = self.lock();
        st.calls.push(BusCall::Export { card });
        st.exported.insert(card, Exported { object, on_lost: Arc::new(on_lost) });
        Ok(())
    }

    fn unexport(&self, card: u32) {
        let mut st = self.lock();
        st.calls.push(BusCall::Unexport { card });
        st.exported.remove(&card);
    }

    fn request_name(&self, card: u32, flags: NameFlags) -> Result<RequestNameReply, CallError> {
        let mut st = self.lock();
        st.calls.push(BusCall::RequestName { card, flags });
        if st.owned.get(&card).copied().unwrap_or(false) {
            return Ok(RequestNameReply::AlreadyOwner);
        }
        // Владелец, ответивший `true` на `RequestRelease`, становится `Free`.
        if matches!(st.owner, FakeOwner::Free) {
            st.owned.insert(card, true);
            return Ok(RequestNameReply::PrimaryOwner);
        }
        Ok(RequestNameReply::Exists)
    }

    fn owner_application_name(&self, card: u32) -> Option<String> {
        let owner = {
            let mut st = self.lock();
            st.calls.push(BusCall::GetApplicationName { card });
            st.owner.clone()
        };
        match owner {
            FakeOwner::Free | FakeOwner::CallFails => None,
            FakeOwner::Grants { app, .. } | FakeOwner::Denies { app } => app,
            FakeOwner::Silent { .. } => {
                self.clock.advance(OWNER_NAME_TIMEOUT);
                None
            }
        }
    }

    fn request_release(&self, card: u32, priority: i32) -> Result<bool, CallError> {
        let owner = {
            let mut st = self.lock();
            st.calls.push(BusCall::RequestRelease { card, priority });
            st.owner.clone()
        };
        let reply = match owner {
            FakeOwner::Free => Ok(true),
            FakeOwner::Denies { .. } => Ok(false),
            FakeOwner::CallFails => Err(CallError::Failed),
            FakeOwner::Grants { reply_after, .. } | FakeOwner::Silent { reply_after, .. } => {
                if reply_after > RELEASE_TIMEOUT {
                    self.clock.advance(RELEASE_TIMEOUT);
                    Err(CallError::Timeout)
                } else {
                    self.clock.advance(reply_after);
                    Ok(true)
                }
            }
        };
        if reply == Ok(true) {
            self.lock().owner = FakeOwner::Free;
        }
        reply
    }

    fn release_name(&self, card: u32) {
        let mut st = self.lock();
        st.calls.push(BusCall::ReleaseName { card });
        st.owned.insert(card, false);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_on_advance_and_clones_share_time() {
        let c = ManualClock::new();
        assert_eq!(c.now(), ClockInstant::START);
        let other = c.clone();
        c.advance(Duration::from_millis(250));
        assert_eq!(other.now().since_start(), Duration::from_millis(250));
        other.advance(Duration::MAX);
        assert_eq!(c.now().since_start(), Duration::from_nanos(u64::MAX));
    }
}
