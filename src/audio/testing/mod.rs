//! Тестовая инфраструктура тракта (AM1.0 §7.1, ADR-20). Компилируется всегда,
//! как подмены платформенных модулей (ADR-6 задачи 02): её используют тесты
//! бинарника и `tests/`, которые собирают библиотеку без `cfg(test)`. Фейки
//! трейтов `EngineDeps` добавляются по этапам (таблица «Трейт → этап» под §8).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

pub mod signals;

use crate::audio::clock::{Clock, ClockInstant};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
