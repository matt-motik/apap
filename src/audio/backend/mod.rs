//! Устройства и бэкенды вывода (AM1.0 §2.3). Типы и трейты появляются по
//! этапам (таблица «Трейт → этап» под §8); в С0 — набор частот устройства.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::format::SampleRate;
use smallvec::SmallVec;

/// Частоты, которые принимает устройство: перечень или диапазон (§2.3).
/// `PartialEq` нужен `Incompatibility::RateUnsupported` (§2.4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RateSet {
    Discrete(SmallVec<[SampleRate; 16]>),
    Range { min: SampleRate, max: SampleRate },
}

impl RateSet {
    pub fn contains(&self, rate: SampleRate) -> bool {
        match self {
            RateSet::Discrete(rates) => rates.contains(&rate),
            RateSet::Range { min, max } => *min <= rate && rate <= *max,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    fn r(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    #[test]
    fn rate_set_contains() {
        let d = RateSet::Discrete(SmallVec::from_slice(&[r(44_100), r(96_000)]));
        assert!(d.contains(r(96_000)));
        assert!(!d.contains(r(48_000)));
        let range = RateSet::Range { min: r(44_100), max: r(192_000) };
        assert!(range.contains(r(44_100)) && range.contains(r(192_000)) && range.contains(r(88_200)));
        assert!(!range.contains(r(384_000)));
    }
}
