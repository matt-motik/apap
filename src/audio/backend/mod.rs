//! Устройства и бэкенды вывода (AM1.0 §2.3). Типы и трейты появляются по
//! этапам (таблица «Трейт → этап» под §8); в С0 — набор частот устройства.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::fmt;
use std::str::FromStr;

use crate::audio::format::{OutputSampleFormat, SampleRate, StreamFormat};
use crate::settings::playback::ModeKind;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

#[cfg(target_os = "linux")]
pub mod alsa;
pub mod catalog;
pub mod exclusive;
pub mod os_probe;
pub mod shared;

/// Стабильный идентификатор аппаратного устройства ALSA: `CARD=<id>` +
/// номер устройства, а не номер карты, который может смениться после
/// переподключения (§2.3, ADR-24, ТЗ-143). Форма строки — имя PCM ALSA
/// `hw:CARD=<id>,DEV=<d>` (§6.6).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HwDeviceId(String);

/// Строка не прошла разбор `HwDeviceId` (§2.3, ADR-24).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HwDeviceIdError {
    input: String,
}

impl fmt::Display for HwDeviceIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid HwDeviceId: `{}`", self.input)
    }
}

impl std::error::Error for HwDeviceIdError {}

impl FromStr for HwDeviceId {
    type Err = HwDeviceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(HwDeviceIdError { input: s.to_string() });
        }
        Ok(HwDeviceId(trimmed.to_string()))
    }
}

impl TryFrom<String> for HwDeviceId {
    type Error = HwDeviceIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<HwDeviceId> for String {
    fn from(value: HwDeviceId) -> Self {
        value.0
    }
}

impl fmt::Display for HwDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl HwDeviceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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

/// Способ доступа к устройству: через системный микшер или монопольно
/// (§2.3, ТЗ-109).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccessKind {
    Shared,
    Exclusive,
}

/// Системный API, через который идёт вывод (§2.3, ТЗ-109, ТЗ-110).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputApi {
    AlsaHw,
    PipeWire,
    PulseAudio,
    WasapiShared,
    CoreAudio,
}

/// Возможности устройства. Для Exclusive снимаются с захваченного PCM
/// (§2.3, ТЗ-109).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeviceCaps {
    pub api: OutputApi,
    pub access: AccessKind,
    pub name: String,
    pub rates: RateSet,
    pub formats: SmallVec<[OutputSampleFormat; 5]>,
    pub channels: SmallVec<[u8; 4]>,
}

/// Результат чтения hw_params после открытия (§2.3, ОВ-14).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriverFormat {
    /// Прочитано у драйвера.
    Confirmed(StreamFormat),
    /// Прочитать не удалось (ОВ-14): бейдж — «параметры драйвера не
    /// подтверждены».
    Unreadable { requested: StreamFormat },
}

/// Фактические период и буфер из hw_params_current (§2.3, ТЗ-76, ТЗ-131).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceBufferActual {
    pub period_frames: u32,
    pub buffer_frames: u32,
}

/// Что умеет бэкенд на этой платформе (§2.3, ОВС-12, ТЗ-109, ТЗ-110).
/// Статичен для платформы: `ExclusiveBackend::caps()` не выполняет I/O,
/// поэтому UI знает доступность режимов синхронно (§2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCaps {
    /// Доступен ли Exclusive-бэкенд на этой сборке. Временное соответствие
    /// старому exclusive-пути (ОВС-12): `true` только на Linux, т.к. других
    /// реализаций `ExclusiveBackend` пока нет (ТЗ-109, ТЗ-110).
    pub exclusive: bool,
}

impl BackendCaps {
    /// Возможности текущей сборки (ОВС-12, ТЗ-109, ТЗ-110).
    pub fn current() -> Self {
        Self { exclusive: cfg!(target_os = "linux") }
    }

    /// Доступность режима `mode` при данных возможностях бэкенда (ТЗ-109):
    /// Совместимый доступен всегда, Оптимальный/Строгий — только при
    /// `exclusive`.
    pub fn available(&self, mode: ModeKind) -> bool {
        mode == ModeKind::Compatible || self.exclusive
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
    fn hw_device_id_from_str_display_roundtrip() {
        let id: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().unwrap();
        assert_eq!(id.as_str(), "hw:CARD=RU3,DEV=0");
        assert_eq!(id.to_string(), "hw:CARD=RU3,DEV=0");
    }

    #[test]
    fn hw_device_id_rejects_empty() {
        assert!("".parse::<HwDeviceId>().is_err());
        assert!("   ".parse::<HwDeviceId>().is_err());
    }

    #[test]
    fn hw_device_id_serde_toml_roundtrip() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            device: HwDeviceId,
        }
        let w = Wrap { device: "hw:CARD=RU3,DEV=0".parse().unwrap() };
        let s = toml::to_string(&w).unwrap();
        let back: Wrap = toml::from_str(&s).unwrap();
        assert_eq!(back.device.as_str(), "hw:CARD=RU3,DEV=0");
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

    #[test]
    fn backend_caps_available_per_mode() {
        let shared_only = BackendCaps { exclusive: false };
        assert!(shared_only.available(ModeKind::Compatible));
        assert!(!shared_only.available(ModeKind::Optimal) && !shared_only.available(ModeKind::Strict));
        let full = BackendCaps { exclusive: true };
        assert!(full.available(ModeKind::Compatible) && full.available(ModeKind::Optimal) && full.available(ModeKind::Strict));
    }
}
