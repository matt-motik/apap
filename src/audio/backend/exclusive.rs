//! Эксклюзивный бэкенд ALSA: захват устройства, его параметры и поток
//! вывода (§2.3, ADR-08, ADR-24, ТЗ-117…ТЗ-123).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::marker::PhantomData;

use super::catalog::HwDeviceInfo;
use super::{BackendCaps, BackendError, DeviceBufferActual, DeviceCaps, DriverFormat, HwDeviceId};
use crate::audio::error::CaptureFailure;
use crate::audio::format::StreamFormat;
use crate::audio::render::dop::DopRender;
use crate::audio::render::gain::{AtomicGain, GainStage, NoGain};
use crate::audio::render::out::OutFormat;
use crate::audio::render::pcm::{PcmRender, PcmSample};
use crate::audio::reservation::Reservation;
use crate::settings::playback::DeviceBuffer;

/// Эксклюзивный бэкенд вывода: резервирование и ALSA `hw:` (§2.3, ADR-08).
pub trait ExclusiveBackend: Send {
    fn caps(&self) -> BackendCaps;
    fn enumerate(&mut self) -> Result<Vec<HwDeviceInfo>, BackendError>;
    /// Резервирование, затем открытие PCM. Без резервирования PCM не
    /// открывается (ТЗ-48, §2.3).
    fn acquire(&mut self, dev: &HwDeviceId, reservation: Reservation) -> Result<DeviceClaim, CaptureFailure>;
    fn device_caps(&mut self, claim: &mut DeviceClaim) -> Result<DeviceCaps, CaptureFailure>;
    /// Установка hw_params и однократное чтение фактических параметров
    /// (ТЗ-113, §2.3). `buffer` — «Буфер устройства» из опций режима: буфер
    /// = buffer, период = buffer/4 (ОВС-13).
    fn configure(
        &mut self,
        claim: &mut DeviceClaim,
        fmt: StreamFormat,
        buffer: DeviceBuffer,
    ) -> Result<(DriverFormat, Option<DeviceBufferActual>), ConfigureError>;
    /// Поток вывода над захваченным PCM; `stop` возвращает PCM в `claim`
    /// без закрытия (§2.3, §6.7).
    fn start(&mut self, claim: &mut DeviceClaim, render: ExclusiveRender) -> Result<Box<dyn StreamHandle>, StartError>;
}

/// Рендер периода в LE-байты формата устройства: общий вход потока вывода
/// для всех форматов (§6.7 `io_bytes`). Стирает параметры ring и формата
/// выхода, выбранные при сборке сессии.
trait ByteRender: Send {
    fn render(&mut self, out: &mut [u8]);
}

impl<P: PcmSample, O: OutFormat, G: GainStage> ByteRender for PcmRender<P, O, G>
where
    PcmRender<P, O, G>: Send,
{
    fn render(&mut self, out: &mut [u8]) {
        PcmRender::render(self, out);
    }
}

impl<O: OutFormat> ByteRender for DopRender<O>
where
    DopRender<O>: Send,
{
    fn render(&mut self, out: &mut [u8]) {
        DopRender::render(self, out);
    }
}

/// PCM-рендер эксклюзивного вывода со стадией усиления `G` (§2.9): тип ring
/// и формат выхода стёрты, стадия усиления остаётся в типе (И-Т6, И-Т21).
pub struct ExclusivePcm<G: GainStage> {
    inner: Box<dyn ByteRender>,
    gain: PhantomData<G>,
}

impl<G: GainStage> ExclusivePcm<G> {
    pub fn new<P: PcmSample, O: OutFormat>(render: PcmRender<P, O, G>) -> ExclusivePcm<G>
    where
        PcmRender<P, O, G>: Send,
    {
        ExclusivePcm { inner: Box::new(render), gain: PhantomData }
    }
}

/// DoP-рендер эксклюзивного вывода с любым контейнером выхода (§2.9, §6.15).
pub struct DopRenderAny {
    inner: Box<dyn ByteRender>,
}

impl DopRenderAny {
    pub fn new<O: OutFormat>(render: DopRender<O>) -> DopRenderAny
    where
        DopRender<O>: Send,
    {
        DopRenderAny { inner: Box::new(render) }
    }
}

/// Рендер Строгого режима: без ступени усиления (§2.9, И-Т6).
pub enum StrictRender {
    /// ExactI32 → S16/S24_3/S24/S32.
    Pcm(ExclusivePcm<NoGain>),
    Dop(DopRenderAny),
}

/// Рендер Оптимального режима (§2.9, ОВС-19, И-Т20, И-Т21).
pub enum OptimalRender {
    /// ExactI32 или F32 → целый формат.
    Pcm(ExclusivePcm<AtomicGain>),
    /// «Фиксировать громкость на 100 %»: ступень усиления убрана.
    PcmLocked(ExclusivePcm<NoGain>),
    Dop(DopRenderAny),
}

/// Рендер эксклюзивного потока вывода (§2.9).
pub enum ExclusiveRender {
    Optimal(OptimalRender),
    Strict(StrictRender),
}

impl ExclusiveRender {
    /// Период в LE-байты формата устройства; один путь для всех форматов
    /// (§6.7 `io_bytes`). Без аллокаций (И-Р7).
    pub fn render(&mut self, out: &mut [u8]) {
        let inner = match self {
            ExclusiveRender::Optimal(OptimalRender::Pcm(p)) => &mut p.inner,
            ExclusiveRender::Optimal(OptimalRender::PcmLocked(p)) => &mut p.inner,
            ExclusiveRender::Optimal(OptimalRender::Dop(d)) => &mut d.inner,
            ExclusiveRender::Strict(StrictRender::Pcm(p)) => &mut p.inner,
            ExclusiveRender::Strict(StrictRender::Dop(d)) => &mut d.inner,
        };
        inner.render(out);
    }
}

/// Владение устройством. Порядок полей = порядок освобождения: PCM, затем
/// резервирование (И-Р1, §2.3). Поле `pcm` объявлено раньше `reservation`,
/// поэтому `Drop` закрывает PCM до снятия резервирования.
pub struct DeviceClaim {
    pcm: Option<PcmHandle>,
    reservation: Reservation,
    info: ClaimInfo,
}

impl DeviceClaim {
    /// Захват по уже полученным резервированию и PCM (§2.3, ТЗ-48): строит
    /// реализация `ExclusiveBackend::acquire`.
    pub fn new(pcm: Option<PcmHandle>, reservation: Reservation, info: ClaimInfo) -> DeviceClaim {
        DeviceClaim { pcm, reservation, info }
    }

    /// Сведения о захвате для замка (§2.5–§2.6).
    pub fn info(&self) -> &ClaimInfo {
        &self.info
    }

    /// Резервирование, которое держит захват (ADR-08, И-Р1).
    pub fn reservation(&self) -> &Reservation {
        &self.reservation
    }

    /// PCM захвата: `configure` ставит hw_params, `start` забирает PCM в поток
    /// вывода, `StreamHandle::stop` возвращает его без закрытия (§2.3).
    pub fn pcm_mut(&mut self) -> &mut Option<PcmHandle> {
        &mut self.pcm
    }
}

/// Сведения о захваченном устройстве (§2.3). Используется в `LockState::Held`
/// и `LockInput::CaptureResult` (§2.5–§2.6) как независимо клонируемая
/// информация о захвате, в отличие от самого `DeviceClaim`, который не
/// клонируется (владеет резервированием).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClaimInfo {
    pub device: HwDeviceId,
}

/// Открытый PCM ALSA (§2.3, §6.6). На платформах без ALSA (не Linux)
/// конструктору нет реализации — `ExclusiveBackend` на них недоступен
/// (`BackendCaps::current().exclusive == false`).
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct PcmHandle(alsa::PCM);

#[cfg(target_os = "linux")]
impl PcmHandle {
    /// Обёртка открытого `hw:CARD=<id>,DEV=<d>` (§6.6).
    pub fn new(pcm: alsa::PCM) -> PcmHandle {
        PcmHandle(pcm)
    }

    /// Открытый PCM для hw_params и записи периодов (§6.6–§6.7).
    pub fn pcm(&self) -> &alsa::PCM {
        &self.0
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct PcmHandle(std::convert::Infallible);

/// Поток вывода (§2.3). Общий для Shared- и Exclusive-бэкендов.
pub trait StreamHandle: Send {
    /// Остановить поток вывода; для Exclusive — вернуть PCM в `DeviceClaim`
    /// без закрытия (§2.3).
    fn stop(self: Box<Self>);
}

/// `ExclusiveBackend::configure` не смог установить hw_params (§2.3, ТЗ-113).
/// Спецификация не задаёт вариантов — текст ошибки хоста ALSA.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConfigureError(pub String);

/// `start` не смог поднять поток вывода (§2.3). Спецификация не задаёт
/// вариантов — текст ошибки хоста ALSA/CPAL.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StartError(pub String);
