//! Модель пути сигнала: план, этапы, преобразования и их дескриптор
//! (AM1.0 §3.1–§3.3, ТЗ-38, ТЗ-70…ТЗ-80).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

pub mod plan;
pub mod signal;
pub mod transform;

pub use self::plan::plan;

use smallvec::SmallVec;

use crate::audio::backend::{AccessKind, DeviceBufferActual, DriverFormat, OutputApi};
use crate::audio::error::Incompatibility;
use crate::audio::format::{DsdRate, SampleRate, SourceFormat, StreamFormat};
use crate::audio::session::RingPayload;
use crate::settings::playback::{DsdFilter, ModeKind};

use self::transform::{Confidence, Lossiness, Transform};

// ---------------------------------------------------------------------------
// §3.1. План и снимок
// ---------------------------------------------------------------------------

/// План, построенный до открытия потока (§3.1, ТЗ-38). Строится чистой
/// функцией без I/O.
#[derive(Clone, PartialEq, Debug)]
pub struct PathPlan {
    /// Режим, для которого построен план (ADR-04).
    pub mode: ModeKind,
    /// Формат источника (ТЗ-74).
    pub source: SourceFormat,
    /// Маршрут: PCM, DoP или DSD→PCM (§3.1).
    pub route: Route,
    /// Преобразования в порядке применения (§3.3, ТЗ-71).
    pub transforms: SmallVec<[Transform; 6]>,
    /// Формат, запрашиваемый у устройства.
    pub target: StreamFormat,
    /// Полезная нагрузка кольцевого буфера — тот же тип, что в §2.9 (ОВС-10 п. 1).
    pub ring: RingPayload,
    /// Шаги DSD-цепочки с их статусом для показа в UI (ТЗ-23, ТЗ-40).
    pub dsd_chain: Option<DsdChainView>,
}

/// Маршрут сигнала (§3.1). Варианта Native нет: native DSD не пробуется (И-Т5, ТЗ-40).
#[derive(Clone, PartialEq, Debug)]
pub enum Route {
    /// Обычный PCM.
    Pcm,
    /// DoP: строится только для Exclusive (И-Т3).
    Dop(DopRoute),
    /// Преобразование DSD→PCM (ОВС-6, ОВС-6б).
    DsdToPcm { dsd: DsdRate, pcm_rate: SampleRate, filter: DsdFilter, stop_hz: u32, gain_comp: bool },
}

/// План остановлен: на каком этапе, по какой причине и что успели построить (§3.1, ТЗ-80).
#[derive(Clone, PartialEq, Debug)]
pub struct Blocked {
    pub at: StageId,
    pub reason: Incompatibility,
    pub partial: PathPlan,
}

/// Факты, известные только после открытия потока (§3.1). Вместе с планом
/// дают снимок пути сигнала.
#[derive(Clone, PartialEq, Debug)]
pub struct OpenedFacts {
    pub access: AccessKind,
    pub api: OutputApi,
    pub device_name: String,
    pub driver: DriverFormat,
    pub reservation: ReservationView,
    /// Этап ОС (ОВС-10 п. 4).
    pub os: OsStage,
    /// Exclusive: фактические период и буфер устройства (ТЗ-76, ТЗ-131); Shared — None.
    pub device_buffer: Option<DeviceBufferActual>,
}

/// Маршрут DoP (§3.1, И-Т3, ТЗ-1, ТЗ-23).
///
/// Спецификация тип не определяет; решение шага AM1.0-8.5: поля приватны,
/// конструктор принимает способ доступа и строит маршрут только для
/// `AccessKind::Exclusive`. Это реализация И-Т3 до появления `DeviceClaim`:
/// план строится из `DeviceCaps`, у которых есть `access`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DopRoute {
    dsd: DsdRate,
    pcm_rate: SampleRate,
}

impl DopRoute {
    /// Маршрут DoP для `dsd` в PCM-контейнере `pcm_rate`; `None`, если доступ
    /// не монопольный (И-Т3). Вызывающий — `plan()` (шаг 1 §6.3, §6.9).
    pub(crate) fn new(dsd: DsdRate, pcm_rate: SampleRate, access: AccessKind) -> Option<DopRoute> {
        match access {
            AccessKind::Exclusive => Some(DopRoute { dsd, pcm_rate }),
            AccessKind::Shared => None,
        }
    }

    /// DSD-частота источника.
    pub fn dsd(self) -> DsdRate {
        self.dsd
    }

    /// Частота PCM-контейнера DoP.
    pub fn pcm_rate(self) -> SampleRate {
        self.pcm_rate
    }
}

/// Шаги DSD-цепочки для показа в UI (§3.1, ТЗ-23, ТЗ-40).
///
/// Спецификация тип не определяет; решение шага AM1.0-8.5: список шагов
/// (Native, DoP, PCM) со статусом каждого.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DsdChainView {
    pub steps: SmallVec<[DsdChainStep; 3]>,
}

/// Шаг DSD-цепочки и его статус (§3.1, ТЗ-40). Решение шага AM1.0-8.5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DsdChainStep {
    pub kind: DsdStepKind,
    pub status: DsdStepStatus,
}

/// Вид шага DSD-цепочки (ТЗ-40). Решение шага AM1.0-8.5. `Native` — только
/// для показа, его статус всегда `NotImplemented` (И-Т5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DsdStepKind {
    Native,
    Dop,
    Pcm,
}

/// Статус шага DSD-цепочки (ТЗ-23, ТЗ-40). Решение шага AM1.0-8.5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DsdStepStatus {
    /// Шаг выбран планом.
    Chosen,
    /// Устройство не поддерживает.
    Unsupported,
    /// Запрещено режимом.
    DisabledByMode,
    /// Не реализовано (Native, И-Т5).
    NotImplemented,
    /// Не пробовался: выбран более ранний шаг.
    NotTried,
}

/// Состояние резервирования устройства для показа (§3.1, ТЗ-76, ОВ-35).
///
/// Спецификация тип не определяет; решение шага AM1.0-8.5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReservationView {
    /// Shared: резервирование не применяется.
    NotApplicable,
    /// Устройство зарезервировано.
    Held,
    /// Сервера резервирования нет: `hw:` без резервирования (ОВ-35).
    UnavailableNoServer,
}

/// Аудиосервер ОС, через который идёт Shared-вывод (§3.2, ТЗ-77).
///
/// Спецификация тип не определяет; решение шага AM1.0-8.5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OsServer {
    WasapiMixer,
    PipeWire,
    PulseAudio,
}

// ---------------------------------------------------------------------------
// §3.2. Этапы
// ---------------------------------------------------------------------------

/// Этап пути сигнала; порядок показа фиксирован (§3.2, ТЗ-70).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StageId {
    Source,
    Decoder,
    Processing,
    Output,
    OsLevel,
    Device,
}

/// Статус этапа (§3.2, термин ТЗ §2). Упорядочен: худший статус этапа — максимум.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum StageStatus {
    Unchanged,
    Lossless,
    Unknown,
    Lossy,
}

/// Этап «Источник» (§3.2, ТЗ-74).
#[derive(Clone, PartialEq, Debug)]
pub struct SourceStage {
    pub fmt: SourceFormat,
}

/// Этап «Декодер» (§3.2, ТЗ-75); MD5 и ошибки — в `DynamicPathState`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecoderStage {
    pub lossless: bool,
}

/// Этап «Обработка»: преобразования плеера (§3.2, ТЗ-71, ТЗ-72).
#[derive(Clone, PartialEq, Debug)]
pub struct ProcessingStage {
    pub transforms: SmallVec<[Transform; 6]>,
}

impl ProcessingStage {
    /// Статус вычисляется (§3.2): `Unchanged` без преобразований, иначе
    /// максимум по `Transform::lossiness()` (Lossless → Lossless, Lossy → Lossy).
    pub fn status(&self) -> StageStatus {
        self.transforms
            .iter()
            .map(|t| match t.kind.lossiness() {
                Lossiness::Lossless => StageStatus::Lossless,
                Lossiness::Lossy => StageStatus::Lossy,
            })
            .max()
            .unwrap_or(StageStatus::Unchanged)
    }
}

/// Этап «Вывод» (§3.2, ТЗ-76).
#[derive(Clone, PartialEq, Debug)]
pub struct OutputStage {
    pub api: OutputApi,
    pub access: AccessKind,
    pub device_name: String,
    pub requested: StreamFormat,
    pub driver: DriverFormat,
    /// ОВ-35: `hw:` без резервирования.
    pub reservation_warning: bool,
    /// ОВС-13; заметка RT-приоритета — в `DynamicPathState` (ОВС-8).
    pub device_buffer: Option<DeviceBufferActual>,
}

impl OutputStage {
    /// Статус вычисляется (§3.2, ОВ-14): `Unknown`, если параметры драйвера
    /// прочитать не удалось (`DriverFormat::Unreadable`), иначе `Unchanged`.
    pub fn status(&self) -> StageStatus {
        match self.driver {
            DriverFormat::Unreadable { .. } => StageStatus::Unknown,
            DriverFormat::Confirmed(_) => StageStatus::Unchanged,
        }
    }
}

/// Этап «Уровень ОС» (§3.2, ТЗ-77).
#[derive(Clone, PartialEq, Debug)]
pub enum OsStage {
    /// Exclusive: ОС в тракте не участвует.
    NotInvolved,
    /// Аудиосервер известен; частота и формат — если удалось узнать.
    Known { server: OsServer, rate: Option<SampleRate>, format: Option<String>, confidence: Confidence },
    /// «Не контролируется плеером, возможно преобразование».
    Unknown,
}

impl OsStage {
    /// Статус вычисляется (§3.2): `NotInvolved` → `Unchanged`, `Unknown` →
    /// `Unknown`. Для `Known` по спецификации `Unknown`, если `rate`/`format`
    /// отличаются от требуемого. Решение шага 14 AM1.0-8.5: `Known` всегда
    /// даёт `Unknown` — системный микшер плеер не контролирует (ТЗ-77), и даже
    /// совпавшая частота не исключает пересчёта громкости или формата (ТЗ-52);
    /// совпадение `rate` учитывают только рекомендации (§6.25).
    pub fn status(&self) -> StageStatus {
        match self {
            OsStage::NotInvolved => StageStatus::Unchanged,
            OsStage::Known { .. } | OsStage::Unknown => StageStatus::Unknown,
        }
    }
}

/// Этап «Устройство» (§3.2, ТЗ-78); граница гарантии — константный текст.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceStage {
    pub accepted: Option<StreamFormat>,
}

impl DeviceStage {
    /// Всегда `Unknown`: за ЦАП плеер не отвечает — граница гарантии (§3.2, ТЗ-78).
    pub fn status(&self) -> StageStatus {
        StageStatus::Unknown
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use std::num::NonZeroU8;

    use super::transform::{Cause, TransformKind};
    use super::*;
    use crate::audio::format::OutputSampleFormat;

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn fmt() -> StreamFormat {
        StreamFormat {
            rate: sr(44_100),
            sample: OutputSampleFormat::S32Le,
            channels: NonZeroU8::new(2).unwrap(),
        }
    }

    fn tr(kind: TransformKind) -> Transform {
        Transform { kind, cause: Cause::ModeRule, confidence: Confidence::Assumed }
    }

    /// §3.2: худший статус — максимум (Unchanged < Lossless < Unknown < Lossy).
    #[test]
    fn stage_status_order_worst_is_max() {
        use StageStatus::*;
        assert!(Unchanged < Lossless && Lossless < Unknown && Unknown < Lossy);
        assert_eq!([Lossless, Lossy, Unknown, Unchanged].into_iter().max(), Some(Lossy));
        assert_eq!([Lossless, Unknown].into_iter().max(), Some(Unknown));
    }

    /// §3.2: статус обработки — максимум по lossiness().
    #[test]
    fn processing_status_is_max_of_transforms() {
        let empty = ProcessingStage { transforms: SmallVec::new() };
        assert_eq!(empty.status(), StageStatus::Unchanged);

        let lossless = ProcessingStage {
            transforms: [tr(TransformKind::MonoToStereo)].into_iter().collect(),
        };
        assert_eq!(lossless.status(), StageStatus::Lossless);

        let mixed = ProcessingStage {
            transforms: [tr(TransformKind::MonoToStereo), tr(TransformKind::Mute)]
                .into_iter()
                .collect(),
        };
        assert_eq!(mixed.status(), StageStatus::Lossy);
    }

    /// §3.2, ОВ-14: нечитаемые hw_params → Unknown; ОС и устройство.
    #[test]
    fn output_status_unknown_when_driver_unreadable() {
        let mut out = OutputStage {
            api: OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            device_name: "hw:0".to_owned(),
            requested: fmt(),
            driver: DriverFormat::Unreadable { requested: fmt() },
            reservation_warning: false,
            device_buffer: None,
        };
        assert_eq!(out.status(), StageStatus::Unknown);
        out.driver = DriverFormat::Confirmed(fmt());
        assert_eq!(out.status(), StageStatus::Unchanged);

        assert_eq!(OsStage::NotInvolved.status(), StageStatus::Unchanged);
        assert_eq!(OsStage::Unknown.status(), StageStatus::Unknown);
        assert_eq!(DeviceStage { accepted: Some(fmt()) }.status(), StageStatus::Unknown);
    }

    /// И-Т3: DoP строится только при монопольном доступе.
    #[test]
    fn dop_route_only_exclusive() {
        assert!(DopRoute::new(DsdRate::Dsd64, sr(176_400), AccessKind::Shared).is_none());
        let r = DopRoute::new(DsdRate::Dsd64, sr(176_400), AccessKind::Exclusive).unwrap();
        assert_eq!((r.dsd(), r.pcm_rate()), (DsdRate::Dsd64, sr(176_400)));
    }
}
