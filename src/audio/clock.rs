//! Инжектируемые часы движка (ADR-20, ОВС-10 п. 7): монотонное время как
//! смещение от старта. В тестах — `ManualClock` (`audio::testing`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::time::{Duration, Instant};

/// Момент времени инжектируемых часов: смещение от старта движка (ОВС-10 п. 7).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ClockInstant(Duration);

impl ClockInstant {
    pub const START: ClockInstant = ClockInstant(Duration::ZERO);

    pub const fn from_start(offset: Duration) -> ClockInstant {
        ClockInstant(offset)
    }

    pub const fn since_start(self) -> Duration {
        self.0
    }

    /// Прошло от `earlier` до `self`; ноль, если `earlier` позже.
    pub fn saturating_since(self, earlier: ClockInstant) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// Момент через `d`; насыщается на максимуме `Duration`.
    pub fn saturating_add(self, d: Duration) -> ClockInstant {
        ClockInstant(self.0.saturating_add(d))
    }
}

/// Источник монотонного времени (ADR-20).
pub trait Clock: Send {
    fn now(&self) -> ClockInstant;
}

/// Реальные часы: `Instant` от момента создания.
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    pub fn new() -> MonotonicClock {
        MonotonicClock { start: Instant::now() }
    }
}

impl Default for MonotonicClock {
    fn default() -> MonotonicClock {
        MonotonicClock::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> ClockInstant {
        ClockInstant(self.start.elapsed())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    #[test]
    fn clock_instant_arithmetic_saturates() {
        let a = ClockInstant::from_start(Duration::from_millis(100));
        let b = a.saturating_add(Duration::from_millis(50));
        assert_eq!(b.saturating_since(a), Duration::from_millis(50));
        assert_eq!(a.saturating_since(b), Duration::ZERO);
        assert!(ClockInstant::START < a);
        assert_eq!(ClockInstant::from_start(Duration::MAX).saturating_add(Duration::from_secs(1)).since_start(), Duration::MAX);
    }

    #[test]
    fn monotonic_clock_does_not_go_back() {
        let c = MonotonicClock::new();
        let t1 = c.now();
        let t2 = c.now();
        assert!(t2 >= t1);
    }
}
