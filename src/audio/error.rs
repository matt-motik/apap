//! Ошибки тракта (AM1.0 §2.4). `OpenError`, `Reaction`, `classify`, `reaction`
//! (С3, ADR-14, ТЗ-86) классифицируют ошибку открытия трека и определяют
//! реакцию по режиму; `ModeUnavailable(ModeKind)` (С4, ТЗ-109, ТЗ-111, §6.18
//! п. 2) — сохранённый режим недоступен на платформе; `Incompatible(Blocked)`
//! (С5, ТЗ-57, ТЗ-80) — трек несовместим с устройством в Строгом режиме.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::backend::RateSet;
use crate::audio::format::{BitDepth, DsdRate, OutputSampleFormat, SampleRate};
use crate::audio::path::Blocked;
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
    /// Устройство режима не выбрано, автовыбор не нашёл аппаратного устройства системного
    /// выхода (ТЗ-144, ADR-24). Захвата не было; текст замка: «устройство не выбрано».
    NoDeviceSelected,
}

/// Текст отказа захвата для строки состояния старого пути (§8 С1, ТЗ-119,
/// ТЗ-122). Замок нового пути строит тексты в `app/lock_view.rs` (ТЗ-118).
pub fn capture_message(f: &CaptureFailure) -> String {
    match f {
        CaptureFailure::ReservationDenied { owner: Some(app) } => {
            format!("Устройство занято: {app} не освобождает его")
        }
        CaptureFailure::ReservationDenied { owner: None } => "Устройство занято другим приложением".into(),
        CaptureFailure::OwnerNotResponding => "Владелец устройства не отвечает".into(),
        CaptureFailure::NoSessionBus => "Нет сессионной шины D-Bus: резервирование устройства невозможно".into(),
        CaptureFailure::BusyOutsideProtocol => "Устройство занято вне протокола резервирования".into(),
        CaptureFailure::DriverRefused { errno } => format!("Драйвер отказал в открытии устройства (errno {errno})"),
        CaptureFailure::LostOnReopen => "Устройство потеряно при переоткрытии".into(),
        CaptureFailure::DeviceDisconnected => "Устройство отключено".into(),
        CaptureFailure::ReservationLost => "Резервирование перехвачено".into(),
        CaptureFailure::PlatformUnsupported => "Монопольный режим не поддерживается на этой платформе".into(),
        CaptureFailure::NoDeviceSelected => "Устройство не выбрано".into(),
    }
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

/// Ошибка открытия трека на границе движка (§2.4).
// `Incompatible(Blocked)` несёт план целиком (форма задана спецификацией
// §2.4) — боксинг одного варианта расходится с объявленной сигнатурой enum.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Debug)]
pub enum OpenError {
    /// Трек несовместим с устройством в Строгом режиме: план остановлен на
    /// этапе `blocked.at` (§2.4, ТЗ-57, ТЗ-80).
    Incompatible(Blocked),
    Capture(CaptureFailure),
    DeviceLost,
    File(FileError),
    Internal(EngineFault),
    /// Сохранённый режим недоступен на платформе (С4, ТЗ-109, ТЗ-111,
    /// §6.18 п. 2): `BackendCaps::available(mode)` вернул `false` для
    /// активного режима. Сохранённый активный режим при этом не меняется
    /// (§6.2 п. 5) — только отказ открытия.
    ModeUnavailable(ModeKind),
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
        OpenError::Incompatible(_) => ErrorClass::Incompatible,
        OpenError::Capture(_) => ErrorClass::CaptureFailed,
        OpenError::DeviceLost => ErrorClass::DeviceLost,
        OpenError::File(FileError::ReadDuringPlayback { .. }) => ErrorClass::ReadError,
        OpenError::File(_) => ErrorClass::BadFile,
        OpenError::Internal(_) => ErrorClass::Internal,
        // Спецификация не вводит отдельный класс для ModeUnavailable (§6.2
        // п. 5, ТЗ-111): показ уведомления с кнопкой переключения — это
        // остановка с сообщением, а не пропуск трека, поэтому берём класс,
        // дающий `StopWithError` в любом режиме — `Internal`.
        OpenError::ModeUnavailable(_) => ErrorClass::Internal,
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
    use crate::audio::backend::{AccessKind, DeviceCaps, OutputApi};
    use crate::audio::format::{BitDepth, ChannelLayout, Codec, Container, SourceFormat, SourceKind};
    use crate::audio::path::plan::plan;
    use crate::settings::playback::{ModeSettings, PathPolicy};

    #[test]
    fn stream_fault_code_roundtrip() {
        for f in [StreamFault::None, StreamFault::DeviceUnavailable, StreamFault::BackendError, StreamFault::Stalled] {
            assert_eq!(StreamFault::from_code(f.code()), f);
        }
        assert_eq!(StreamFault::from_code(200), StreamFault::BackendError);
    }

    /// ТЗ-144: автовыбор устройства не нашёл карты — текст замка «устройство не выбрано».
    #[test]
    fn capture_message_no_device_selected() {
        assert_eq!(capture_message(&CaptureFailure::NoDeviceSelected), "Устройство не выбрано");
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
        let src = SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: SampleRate::new(192_000).unwrap(),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: BitDepth::new(24).unwrap() },
        };
        let caps = DeviceCaps {
            api: OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            name: "test".to_owned(),
            rates: RateSet::Discrete(
                [44_100, 48_000, 96_000].iter().map(|&hz| SampleRate::new(hz).unwrap()).collect(),
            ),
            formats: SmallVec::from_slice(&[OutputSampleFormat::S32Le]),
            channels: SmallVec::from_slice(&[2]),
        };
        let s = ModeSettings::default();
        let blocked = plan(&src, &caps, PathPolicy::Strict(&s.strict)).unwrap_err();
        assert_eq!(classify(&OpenError::Incompatible(blocked)), ErrorClass::Incompatible);

        assert_eq!(classify(&OpenError::Capture(CaptureFailure::OwnerNotResponding)), ErrorClass::CaptureFailed);
        assert_eq!(classify(&OpenError::DeviceLost), ErrorClass::DeviceLost);
        assert_eq!(
            classify(&OpenError::File(FileError::ReadDuringPlayback { at_frame: 10, kind: std::io::ErrorKind::Other })),
            ErrorClass::ReadError
        );
        assert_eq!(classify(&OpenError::File(FileError::Corrupt(CorruptKind::BadHeader))), ErrorClass::BadFile);
        assert_eq!(classify(&OpenError::Internal(EngineFault::StoreFailed)), ErrorClass::Internal);
        assert_eq!(classify(&OpenError::ModeUnavailable(ModeKind::Strict)), ErrorClass::Internal);
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
