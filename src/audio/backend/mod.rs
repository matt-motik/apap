//! Устройства и бэкенды вывода (AM1.0 §2.3). Типы и трейты появляются по
//! этапам (таблица «Трейт → этап» под §8); в С0 — набор частот устройства.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::format::SampleRate;
use smallvec::SmallVec;

pub mod shared;

/// Стабильный идентификатор Shared-устройства: имя PCM/CoreAudio-узла, в
/// отличие от `HwDeviceId` не привязан к конкретной карте ALSA (§2.3, ADR-07).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SharedDeviceId(String);

impl SharedDeviceId {
    pub fn new(id: impl Into<String>) -> SharedDeviceId {
        SharedDeviceId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Устройство в Shared-перечислении (§2.3). Перечисление кэшируется и
/// обновляется только по событию или «Обновить» (ADR-16).
#[derive(Clone, Debug)]
pub struct SharedDeviceInfo {
    pub id: SharedDeviceId,
    pub name: String,
    pub is_default: bool,
    /// Мост этапа С3: пока таблица устройств в UI не переведена на
    /// `DeviceCaps`, легаси-поля `DeviceInfo` нужны для отрисовки того же
    /// списка (§2.3).
    pub legacy: crate::audio::output::DeviceInfo,
}

/// Перечисление Shared-устройств недоступно — хост не ответил (§2.3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BackendError {
    Unavailable(String),
}

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
