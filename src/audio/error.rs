//! Ошибки тракта (AM1.0 §2.4). В С0 — типы, не зависящие от плана пути и
//! режимов; `OpenError`, `Reaction`, `classify`, `reaction` — в С3 вместе с
//! движком (таблица «Трейт → этап» под §8).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::backend::RateSet;
use crate::audio::format::{BitDepth, DsdRate, OutputSampleFormat, SampleRate};
use smallvec::SmallVec;

/// Несовместимость (только Строгий режим, термин ТЗ §2).
#[derive(Clone, PartialEq, Debug)]
pub enum Incompatibility {
    RateUnsupported { rate: SampleRate, supported: RateSet },
    TruncationForbidden { from: BitDepth, max: u8 },
    ChannelsUnsupported { src: u8, supported: SmallVec<[u8; 4]> },
    DopUnsupported { dsd: DsdRate, max_dop: Option<DsdRate> },
    DriverRejectedRate { requested: SampleRate, actual: SampleRate },
    DriverRejectedFormat { requested: OutputSampleFormat, actual: OutputSampleFormat },
    /// PCM с плавающей точкой в Строгом режиме (ОВС-1, ТЗ-132).
    FloatUnsupported,
}

/// Отказ захвата (ТЗ-46). Каждый вариант — своя причина в замке.
#[derive(Clone, PartialEq, Debug)]
pub enum CaptureFailure {
    ReservationDenied { owner: Option<String> },
    OwnerNotResponding,
    NoSessionBus,
    BusyOutsideProtocol,
    DriverRefused { errno: i32 },
    LostOnReopen,
    DeviceDisconnected,
    /// Имя ReserveDevice1 потеряно во время захвата (сигнал NameLost); PCM закрыт
    /// немедленно (ADR-08, И-Р20). Текст замка: «резервирование перехвачено».
    ReservationLost,
    PlatformUnsupported,
}

#[derive(Clone, PartialEq, Debug)]
pub enum FileError {
    Corrupt(CorruptKind),
    Unsupported { codec: String },
    Io(std::io::ErrorKind),
    /// Ошибка чтения во время воспроизведения (ТЗ-87).
    ReadDuringPlayback { at_frame: u64, kind: std::io::ErrorKind },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CorruptKind {
    BadHeader,
    ZeroChannels,
    BlockSizeOutOfRange,
    ChunkSizeOverflow,
    OffsetBeyondEof,
    Truncated,
    DecodeFailed,
}

/// Сбой потока. Колбэк и поток ошибок бэкенда пишут только код в атомик (ТЗ-102).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum StreamFault {
    None = 0,
    DeviceUnavailable = 1,
    BackendError = 2,
    Stalled = 3,
}

impl StreamFault {
    /// Код для `AtomicU8` (`SessionShared::fault`).
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Обратное к `code`; неизвестный код — `BackendError` (атомик пишет
    /// только `code`, поэтому неизвестный код означает повреждение состояния).
    pub const fn from_code(code: u8) -> StreamFault {
        match code {
            0 => StreamFault::None,
            1 => StreamFault::DeviceUnavailable,
            3 => StreamFault::Stalled,
            _ => StreamFault::BackendError,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum EngineFault {
    SpawnFailed { what: &'static str },
    StoreFailed,
    /// Повторное расхождение hw_params в Оптимальном режиме (§6.4, ОВС-10 п. 5).
    DriverFormatUnstable,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorClass {
    Incompatible,
    CaptureFailed,
    DeviceLost,
    BadFile,
    ReadError,
    Internal,
}

/// Выбор устройства для reaction() (ОВС-10 п. 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceChoiceKind {
    SystemDefault,
    Named,
    Hw,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    #[test]
    fn stream_fault_code_roundtrip() {
        for f in [StreamFault::None, StreamFault::DeviceUnavailable, StreamFault::BackendError, StreamFault::Stalled] {
            assert_eq!(StreamFault::from_code(f.code()), f);
        }
        assert_eq!(StreamFault::from_code(200), StreamFault::BackendError);
    }

    #[test]
    fn incompatibility_compares_by_value() {
        let r = SampleRate::new(352_800).unwrap();
        let set = RateSet::Range { min: SampleRate::new(44_100).unwrap(), max: SampleRate::new(192_000).unwrap() };
        let a = Incompatibility::RateUnsupported { rate: r, supported: set.clone() };
        assert_eq!(a, Incompatibility::RateUnsupported { rate: r, supported: set });
        assert_ne!(a, Incompatibility::FloatUnsupported);
    }
}
