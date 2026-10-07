//! Ошибки тракта (AM1.0 §2.4). `OpenError`, `Reaction`, `classify`, `reaction`
//! (С3, ADR-14, ТЗ-86) классифицируют ошибку открытия трека и определяют
//! реакцию по режиму; `Incompatible(Blocked)` (С5) и `ModeUnavailable(ModeKind)`
//! (С4) — ещё не представлены в `OpenError`, добавляются позже (таблица
//! «Трейт → этап» под §8).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::backend::RateSet;
use crate::audio::format::{BitDepth, DsdRate, OutputSampleFormat, SampleRate};
use crate::settings::ModeKind;
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

/// Ошибка открытия трека на границе движка (§2.4). Подмножество С3:
/// `Incompatible(Blocked)` — С5, `ModeUnavailable(ModeKind)` — С4.
#[derive(Clone, PartialEq, Debug)]
pub enum OpenError {
    Capture(CaptureFailure),
    DeviceLost,
    File(FileError),
    Internal(EngineFault),
}

/// Реакция плеера на ошибку открытия трека (ADR-14, ТЗ-86).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reaction {
    Skip,
    LockFailed,
    StopWithError,
    FollowSystemDefault,
}

/// Классификация `OpenError` для таблицы реакций (ADR-14, §2.4).
pub fn classify(e: &OpenError) -> ErrorClass {
    match e {
        OpenError::Capture(_) => ErrorClass::CaptureFailed,
        OpenError::DeviceLost => ErrorClass::DeviceLost,
        OpenError::File(FileError::ReadDuringPlayback { .. }) => ErrorClass::ReadError,
        OpenError::File(_) => ErrorClass::BadFile,
        OpenError::Internal(_) => ErrorClass::Internal,
    }
}

/// Реакция по классу ошибки, режиму и выбору устройства (ADR-14, таблица §2.4,
/// ОВС-10 п. 3). Матчи по `ErrorClass`/`ModeKind`/`DeviceChoiceKind` без
/// wildcard-веток: новый вариант любого из них обязан провалить сборку здесь.
pub fn reaction(class: ErrorClass, mode: ModeKind, device: DeviceChoiceKind) -> Reaction {
    match class {
        ErrorClass::Incompatible => match mode {
            // Планировщик Совместимого и Оптимального режимов не формирует
            // Incompatible (несовпадение частот/форматов отличает только
            // Строгий, ТЗ §2) — ветка документирует недостижимость, не паникует.
            ModeKind::Compatible => Reaction::StopWithError,
            ModeKind::Optimal => Reaction::StopWithError,
            ModeKind::Strict => Reaction::Skip,
        },
        ErrorClass::CaptureFailed => match mode {
            // Совместимый режим не держит эксклюзивный захват устройства —
            // CaptureFailed в нём недостижим (ADR-14); ветка не паникует.
            ModeKind::Compatible => Reaction::StopWithError,
            ModeKind::Optimal => Reaction::LockFailed,
            ModeKind::Strict => Reaction::LockFailed,
        },
        ErrorClass::DeviceLost => match mode {
            ModeKind::Compatible => match device {
                DeviceChoiceKind::SystemDefault => Reaction::FollowSystemDefault,
                DeviceChoiceKind::Named => Reaction::StopWithError,
                DeviceChoiceKind::Hw => Reaction::StopWithError,
            },
            // Оптимальный/Строгий: LockFailed(DeviceDisconnected) и точка
            // реакуайра — поведение движка (позже); реакция верхнего уровня
            // здесь — остановка с ошибкой (ADR-14).
            ModeKind::Optimal => Reaction::StopWithError,
            ModeKind::Strict => Reaction::StopWithError,
        },
        ErrorClass::BadFile => match mode {
            ModeKind::Compatible => Reaction::Skip,
            ModeKind::Optimal => Reaction::Skip,
            ModeKind::Strict => Reaction::Skip,
        },
        ErrorClass::ReadError => match mode {
            ModeKind::Compatible => Reaction::Skip,
            ModeKind::Optimal => Reaction::Skip,
            ModeKind::Strict => Reaction::Skip,
        },
        ErrorClass::Internal => match mode {
            ModeKind::Compatible => Reaction::StopWithError,
            ModeKind::Optimal => Reaction::StopWithError,
            ModeKind::Strict => Reaction::StopWithError,
        },
    }
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

    #[test]
    fn classify_maps_open_error_variants() {
        assert_eq!(classify(&OpenError::Capture(CaptureFailure::OwnerNotResponding)), ErrorClass::CaptureFailed);
        assert_eq!(classify(&OpenError::DeviceLost), ErrorClass::DeviceLost);
        assert_eq!(
            classify(&OpenError::File(FileError::ReadDuringPlayback { at_frame: 10, kind: std::io::ErrorKind::Other })),
            ErrorClass::ReadError
        );
        assert_eq!(classify(&OpenError::File(FileError::Corrupt(CorruptKind::BadHeader))), ErrorClass::BadFile);
        assert_eq!(classify(&OpenError::Internal(EngineFault::StoreFailed)), ErrorClass::Internal);
    }

    #[test]
    fn error_classes_distinct_reactions() {
        use DeviceChoiceKind::{Hw, Named, SystemDefault};
        use ErrorClass::{BadFile, CaptureFailed, DeviceLost, Internal, ReadError};
        use ModeKind::{Compatible, Optimal, Strict};
        use Reaction::{FollowSystemDefault, LockFailed, Skip, StopWithError};

        // Incompatible: только Строгий различает несовпадение частот/формата (ТЗ §2).
        assert_eq!(reaction(ErrorClass::Incompatible, Strict, SystemDefault), Skip);

        for (class, expected) in [
            (CaptureFailed, [StopWithError, LockFailed, LockFailed]),
            (BadFile, [Skip, Skip, Skip]),
            (ReadError, [Skip, Skip, Skip]),
            (Internal, [StopWithError, StopWithError, StopWithError]),
        ] {
            assert_eq!(reaction(class, Compatible, SystemDefault), expected[0]);
            assert_eq!(reaction(class, Optimal, SystemDefault), expected[1]);
            assert_eq!(reaction(class, Strict, SystemDefault), expected[2]);
        }

        // DeviceLost: Совместимый следует за системным дефолтом, иначе — стоп.
        assert_eq!(reaction(DeviceLost, Compatible, SystemDefault), FollowSystemDefault);
        assert_eq!(reaction(DeviceLost, Compatible, Named), StopWithError);
        assert_eq!(reaction(DeviceLost, Compatible, Hw), StopWithError);
        assert_eq!(reaction(DeviceLost, Optimal, SystemDefault), StopWithError);
        assert_eq!(reaction(DeviceLost, Strict, SystemDefault), StopWithError);
    }
}
