//! Эксклюзивный бэкенд ALSA: перечисление карт через `ctl` и возможности
//! открытого PCM (§2.3, §6.6, ADR-24, ТЗ-42…ТЗ-47, ТЗ-143). Резервирование,
//! открытие `hw:` PCM и поток вывода — отдельные этапы (`acquire`,
//! `configure`, `start` трейта `ExclusiveBackend`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::num::NonZeroU8;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use smallvec::SmallVec;

use super::alsa_out;
use super::catalog::HwDeviceInfo;
use super::exclusive::{
    ClaimInfo, ClaimJob, ConfigureError, DeviceClaim, ExclusiveBackend, ExclusiveRender, PcmHandle, StartError,
    StreamHandle,
};
use super::{
    AccessKind, BackendCaps, BackendError, DeviceBufferActual, DeviceCaps, DriverFormat, HwDeviceId, OutputApi, RateSet,
};
use crate::audio::clock::{Clock, MonotonicClock};
use crate::audio::error::CaptureFailure;
use crate::audio::format::{OutputSampleFormat, SampleRate, StreamFormat};
use crate::audio::reservation::dbus::{alsa_card_index, DbusReserveBus, SystemServerProbe};
use crate::audio::reservation::{self, LostNotify, Reservation, ReserveBus};
use crate::audio::session::SessionShared;
use crate::settings::playback::DeviceBuffer;

/// Дедлайн ожидания `EBUSY` при открытии `hw:` от момента вызова OPEN (§6.6,
/// ТЗ-48, ТЗ-122).
const OPEN_EBUSY_DEADLINE: Duration = Duration::from_secs(1);

/// Шаг повтора открытия после `EBUSY` (§6.6, ТЗ-122).
const OPEN_RETRY_STEP: Duration = Duration::from_millis(50);

/// Частоты, которые проверяются первыми через `test_rate` (§6.6): если
/// устройство принимает любую из них, `DeviceCaps::rates` — дискретный
/// список, иначе — диапазон `get_rate_min`/`get_rate_max`.
const STANDARD_RATES_HZ: [u32; 10] =
    [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000];

/// Соответствие форматов ALSA форматам `OutputSampleFormat` (§2.1, ОВ-32):
/// `S243LE` — имя варианта крейта `alsa` для `SND_PCM_FORMAT_S24_3LE`.
const FORMAT_MAP: [(alsa::pcm::Format, OutputSampleFormat); 5] = [
    (alsa::pcm::Format::S16LE, OutputSampleFormat::S16Le),
    (alsa::pcm::Format::S243LE, OutputSampleFormat::S24_3Le),
    (alsa::pcm::Format::S24LE, OutputSampleFormat::S24Le),
    (alsa::pcm::Format::S32LE, OutputSampleFormat::S32Le),
    (alsa::pcm::Format::FloatLE, OutputSampleFormat::F32),
];

/// Эксклюзивный бэкенд ALSA (§2.3, ADR-24). Перечисление и возможности
/// устройства не хранят состояния между вызовами — поля появятся вместе с
/// `acquire`/`configure`/`start` (ADR-08).
#[derive(Debug, Default)]
pub struct AlsaExclusiveBackend;

/// Ошибка `reconfigure` держимого PCM при смене частоты (ТЗ-49) или «Буфера
/// устройства» (ОВС-17, ТЗ-131, §6.7): `Configure` — `hw_params`/`sw_params`
/// отказали без потери PCM, `Lost` — переоткрытие после такого отказа тоже
/// не удалось (`CaptureFailure::LostOnReopen`).
#[derive(Clone, PartialEq, Debug)]
pub enum ReconfigureError {
    Configure(ConfigureError),
    Lost(CaptureFailure),
}

impl AlsaExclusiveBackend {
    pub fn new() -> AlsaExclusiveBackend {
        AlsaExclusiveBackend
    }

    /// Возможности бэкенда на этой платформе (§2.3, ТЗ-109, ТЗ-110). Модуль
    /// собирается только на Linux, поэтому Exclusive всегда доступен.
    pub fn caps(&self) -> BackendCaps {
        BackendCaps::current()
    }

    /// Перечисление карт ALSA и их PCM-устройств воспроизведения (§6.6,
    /// ADR-24, ТЗ-143): id строится по стабильному `CARD=<id>`, а не по
    /// номеру карты, который может смениться после переподключения
    /// (ТЗ-126). Отказ `card::Iter` — сбой перечисления целиком; отказ
    /// отдельной карты (`Ctl`, `CardInfo`) или устройства без потока
    /// воспроизведения — карта/устройство пропускаются, перечисление
    /// продолжается остальными картами.
    pub fn enumerate(&mut self) -> Result<Vec<HwDeviceInfo>, BackendError> {
        let mut out = Vec::new();
        for card in alsa::card::Iter::new() {
            let card = card.map_err(|e| BackendError::Unavailable(e.to_string()))?;
            let Ok(card_index) = u32::try_from(card.get_index()) else { continue };
            let Ok(ctl) = alsa::ctl::Ctl::from_card(&card, false) else { continue };
            let Ok(card_info) = ctl.card_info() else { continue };
            let Ok(card_id) = card_info.get_id() else { continue };
            let card_id = card_id.to_string();

            for device in alsa::ctl::DeviceIter::new(&ctl) {
                let Ok(device_index) = u32::try_from(device) else { continue };
                let Ok(pcm_info) = ctl.pcm_info(device_index, 0, alsa::Direction::Playback) else {
                    continue;
                };
                let Some(id) = build_hw_id(&card_id, device_index) else { continue };
                let name = pcm_info.get_name().unwrap_or_default().to_string();
                out.push(HwDeviceInfo { id, card: card_index, device: device_index, name });
            }
        }
        Ok(out)
    }

    /// Возможности захваченного устройства (§2.3, §6.6, ТЗ-109, ТЗ-110):
    /// снимаются с PCM, которым владеет `claim`. Пока PCM у потока вывода
    /// (между `start` и `stop`), `claim.pcm_mut()` — `None`; отдельного
    /// варианта `CaptureFailure` для этого состояния спецификация не
    /// задаёт, поэтому используется `DeviceDisconnected` — возможности
    /// снять физически нельзя.
    pub fn device_caps(&mut self, claim: &mut DeviceClaim) -> Result<DeviceCaps, CaptureFailure> {
        let Some(handle) = claim.pcm_mut().as_ref() else {
            return Err(CaptureFailure::DeviceDisconnected);
        };
        Self::device_caps_of(handle.pcm())
    }

    /// Возможности открытого PCM (§2.3, §6.6, ТЗ-109, ТЗ-110): свободная от
    /// захвата функция — `device_caps` делегирует ей, тесты с реальным
    /// устройством могут вызывать её напрямую над `alsa::PCM`.
    pub fn device_caps_of(pcm: &alsa::PCM) -> Result<DeviceCaps, CaptureFailure> {
        let hw = alsa::pcm::HwParams::any(pcm).map_err(driver_refused)?;
        let name = pcm.info().and_then(|info| info.get_name().map(str::to_string)).unwrap_or_default();

        let mut formats = SmallVec::new();
        for (alsa_fmt, out_fmt) in FORMAT_MAP {
            if hw.test_format(alsa_fmt).is_ok() {
                formats.push(out_fmt);
            }
        }

        let ch_min = hw.get_channels_min().map_err(driver_refused)?;
        let ch_max = hw.get_channels_max().map_err(driver_refused)?;
        let mut channels = SmallVec::new();
        for ch in ch_min..=ch_max {
            let Ok(c) = u8::try_from(ch) else { break };
            if hw.test_channels(ch).is_ok() {
                channels.push(c);
            }
        }

        let rates = probe_rates(&hw)?;

        Ok(DeviceCaps { api: OutputApi::AlsaHw, access: AccessKind::Exclusive, name, rates, formats, channels })
    }

    /// Резервирование карты `dev` — первая часть `ExclusiveBackend::acquire`
    /// (§6.6, ТЗ-48, ТЗ-117): открытие `hw:` PCM следует отдельным шагом.
    /// Номер карты — по `HwDeviceId` (ТЗ-126); отказ D-Bus уже классифицирован
    /// `reservation::acquire` (`ReservationDenied` с владельцем — AM1.0-B2,
    /// `OwnerNotResponding`, `NoSessionBus`), эта функция его не переопределяет.
    /// Вызывающая сторона — `claim_job` (ADR-01, §6.6): задание служебного
    /// потока сначала резервирует карту, затем открывает PCM.
    pub(crate) fn reserve(
        &mut self,
        dev: &HwDeviceId,
        device_name: &str,
        on_lost: LostNotify,
    ) -> Result<Reservation, CaptureFailure> {
        let card = card_number(dev)?;
        let bus: Arc<dyn ReserveBus> = Arc::new(DbusReserveBus::new());
        reservation::acquire(&bus, &SystemServerProbe, card, device_name, on_lost)
    }

    /// Открытие `hw:` после уже полученного резервирования — вторая часть
    /// `ExclusiveBackend::acquire` (§6.6 OPEN, ТЗ-48, ТЗ-49, ADR-08). Вызывающая
    /// сторона — `claim_job` (ADR-01) и реализация трейта `acquire`. Выполняется
    /// в служебном потоке захвата — блокирующий `PCM::new` и реальный `sleep`
    /// здесь допустимы (не аудио-колбэк).
    pub(crate) fn acquire(&mut self, dev: &HwDeviceId, reservation: Reservation) -> Result<DeviceClaim, CaptureFailure> {
        let hw_name = dev.as_str().to_string();
        let clock = MonotonicClock::new();
        let (pcm, reservation) = open_claimed(reservation, &clock, std::thread::sleep, || {
            alsa::PCM::new(&hw_name, alsa::Direction::Playback, false).map_err(|e| e.errno())
        })?;
        Ok(DeviceClaim::new(Some(PcmHandle::new(pcm)), reservation, ClaimInfo { device: dev.clone() }))
    }

    /// Установка sw_params и однократное чтение фактических параметров
    /// (§6.7 п. 2–3, ТЗ-113). `avail_min`/`start_threshold` ставятся по
    /// периоду/буферу в кадрах из того же чтения `hw_params_current()`, что
    /// и `DriverFormat`/`DeviceBufferActual` — второго обращения к ALSA нет.
    /// Сравнение подтверждённых параметров с запрошенными — за бейджем
    /// «Bit-perfect» (ТЗ-52, §2 `DriverFormat::Confirmed`): здесь хранится
    /// то, что принял драйвер, даже если это не совпадает с `fmt`. Ошибка
    /// чтения → `Unreadable`, но период и буфер в ответе — запрошенные
    /// (§6.7 п. 3), не `None`.
    pub fn configure(
        &mut self,
        claim: &mut DeviceClaim,
        fmt: StreamFormat,
        buffer: DeviceBuffer,
    ) -> Result<(DriverFormat, Option<DeviceBufferActual>), ConfigureError> {
        let Some(handle) = claim.pcm_mut().as_ref() else {
            return Err(ConfigureError("configure вызван без открытого PCM (acquire не выполнен, §6.7)".to_string()));
        };
        let pcm = handle.pcm();
        apply_hw_params(pcm, &fmt, buffer)?;

        let read = read_hw_params_current(pcm);
        let (requested_period_frames, requested_buffer_frames) = requested_frames(buffer, fmt.rate);
        let (period_frames, buffer_frames) = match &read {
            Some(r) => (r.period_frames, r.buffer_frames),
            None => (requested_period_frames, requested_buffer_frames),
        };
        apply_sw_params(pcm, period_frames, buffer_frames)?;

        Ok(build_driver_format(fmt, read, requested_period_frames, requested_buffer_frames))
    }

    /// Переконфигурация держимого PCM при смене частоты (ТЗ-49) или «Буфера
    /// устройства» (ОВС-17, ТЗ-131): `pcm.drop()` сбрасывает незавершённый
    /// поток, затем на том же PCM без закрытия повторяются шаги `configure`
    /// (§6.7). Крейт `alsa` 0.11 не оборачивает `snd_pcm_hw_free`: после
    /// `drop()` PCM переходит в `SETUP`, а не `OPEN`, но `hw_params()`
    /// переустанавливает полный набор параметров и из `SETUP` — отдельный
    /// вызов `hw_free()` не нужен для результата, который требует спецификация
    /// (новые hw_params/sw_params на том же PCM). Если `hw_params` всё же
    /// отказал без закрытия PCM — PCM закрывается (`claim.pcm_mut()` → `None`)
    /// и открывается заново под тем же резервированием через цикл OPEN (§6.6,
    /// `OPEN_EBUSY_DEADLINE`); резервирование всё это время остаётся в
    /// `claim` и этой функцией не трогается — она не извлекает и не снимает
    /// его, снятие (если переоткрытие не удалось) — задача вызывающей стороны
    /// вместе с освобождением всего `claim` (И-Р1, порядок полей
    /// `DeviceClaim`). Любой отказ переоткрытия возвращается как
    /// `CaptureFailure::LostOnReopen` (§6.7) независимо от конкретного errno —
    /// в отличие от первого захвата (`acquire`), здесь устройство уже было
    /// открыто и для вызывающей стороны важен только факт потери. Отказ
    /// `configure` после успешного переоткрытия спецификацией отдельно не
    /// определён — возвращается как обычный `Configure`.
    pub fn reconfigure(
        &mut self,
        claim: &mut DeviceClaim,
        fmt: StreamFormat,
        buffer: DeviceBuffer,
    ) -> Result<(DriverFormat, Option<DeviceBufferActual>), ReconfigureError> {
        {
            let Some(handle) = claim.pcm_mut().as_ref() else {
                return Err(ReconfigureError::Configure(ConfigureError(
                    "reconfigure вызван без открытого PCM (acquire не выполнен, §6.7)".to_string(),
                )));
            };
            handle.pcm().drop().map_err(|e| ReconfigureError::Configure(configure_refused(e)))?;
        }

        if let Ok(result) = self.configure(claim, fmt, buffer) {
            return Ok(result);
        }

        let dev = claim.info().device.clone();
        *claim.pcm_mut() = None;
        let pcm = reopen_pcm(&dev).map_err(|_| ReconfigureError::Lost(CaptureFailure::LostOnReopen))?;
        *claim.pcm_mut() = Some(PcmHandle::new(pcm));

        self.configure(claim, fmt, buffer).map_err(ReconfigureError::Configure)
    }

    /// Поток вывода над захваченным PCM (§2.3, §6.7, ТЗ-51, ТЗ-76, ТЗ-112):
    /// PCM целиком переходит в поток `apap-alsa-out` (`alsa_out::spawn_output_thread`)
    /// до вызова `StreamHandle::stop` у возвращённого хендла. `stop`
    /// возвращает PCM значением (`Option<PcmHandle>`, трейт `exclusive.rs`);
    /// вызывающая сторона кладёт его в `claim.pcm_mut()`. Вся проверяемая
    /// логика (байт на сэмпл, размер периода, таймаут `wait`) выполняется до
    /// `claim.pcm_mut().take()` — при любом отказе до запуска потока PCM
    /// остаётся в `claim`, а не теряется; отказ самого `spawn` (ОС не создала
    /// поток) — вне этого гарантии, см. `alsa_out::spawn_output_thread`.
    pub fn start(
        &mut self,
        claim: &mut DeviceClaim,
        render: ExclusiveRender,
        fmt: StreamFormat,
        period_frames: u32,
        shared: Arc<SessionShared>,
    ) -> Result<Box<dyn StreamHandle>, StartError> {
        let bytes_per_sample = bytes_per_sample(fmt.sample)?;
        let period_bytes = alsa_out::period_buffer_bytes(period_frames, bytes_per_sample, fmt.channels)
            .ok_or_else(|| StartError("размер периода переполняет буфер (§6.7, ТЗ-112)".to_string()))?;
        let wait_timeout_ms = alsa_out::wait_timeout_ms(period_frames, fmt.rate.hz())
            .ok_or_else(|| StartError("таймаут wait переполнен или частота равна нулю (§6.7)".to_string()))?;

        let Some(pcm) = claim.pcm_mut().take() else {
            return Err(StartError("start вызван без открытого PCM (acquire не выполнен, §6.7)".to_string()));
        };

        let stop = Arc::new(AtomicBool::new(false));
        let (tid_tx, tid_rx) = mpsc::channel();

        let timing = alsa_out::LoopTiming { period_frames, period_bytes, wait_timeout_ms };
        let join = match alsa_out::spawn_output_thread(pcm, render, timing, shared, Arc::clone(&stop), tid_tx) {
            Ok(join) => join,
            Err(e) => return Err(StartError(format!("не удалось создать поток apap-alsa-out: {e}"))),
        };

        let handle = alsa_out::AlsaStreamHandle::new(stop, join, tid_rx);
        Ok(Box::new(handle))
    }
}

impl ExclusiveBackend for AlsaExclusiveBackend {
    fn caps(&self) -> BackendCaps {
        AlsaExclusiveBackend::caps(self)
    }

    fn enumerate(&mut self) -> Result<Vec<HwDeviceInfo>, BackendError> {
        AlsaExclusiveBackend::enumerate(self)
    }

    fn acquire(&mut self, dev: &HwDeviceId, reservation: Reservation) -> Result<DeviceClaim, CaptureFailure> {
        AlsaExclusiveBackend::acquire(self, dev, reservation)
    }

    fn claim_job(&self, dev: HwDeviceId, device_name: String, on_lost: LostNotify) -> ClaimJob {
        Box::new(move || {
            let mut backend = AlsaExclusiveBackend::new();
            let reservation = backend.reserve(&dev, &device_name, on_lost)?;
            backend.acquire(&dev, reservation)
        })
    }

    fn device_caps(&mut self, claim: &mut DeviceClaim) -> Result<DeviceCaps, CaptureFailure> {
        AlsaExclusiveBackend::device_caps(self, claim)
    }

    fn configure(
        &mut self,
        claim: &mut DeviceClaim,
        fmt: StreamFormat,
        buffer: DeviceBuffer,
    ) -> Result<(DriverFormat, Option<DeviceBufferActual>), ConfigureError> {
        AlsaExclusiveBackend::configure(self, claim, fmt, buffer)
    }

    fn start(
        &mut self,
        claim: &mut DeviceClaim,
        render: ExclusiveRender,
        fmt: StreamFormat,
        period_frames: u32,
        shared: Arc<SessionShared>,
    ) -> Result<Box<dyn StreamHandle>, StartError> {
        AlsaExclusiveBackend::start(self, claim, render, fmt, period_frames, shared)
    }
}

/// hw_params Exclusive-потока: доступ `RW_INTERLEAVED`, формат, число
/// каналов, частота без ресэмплинга драйвера, период и буфер по настройке
/// «Буфер устройства» (§6.7 п. 1, ТЗ-50, ТЗ-112, ТЗ-131). Порядок вызовов
/// обязателен: ALSA отвергает период/буфер, заданные раньше access/format/
/// channels/rate. Фактические параметры здесь не перечитываются — это
/// вторая часть `configure` (ТЗ-113, §2.3).
pub(crate) fn apply_hw_params(pcm: &alsa::PCM, fmt: &StreamFormat, buffer: DeviceBuffer) -> Result<(), ConfigureError> {
    let hw = alsa::pcm::HwParams::any(pcm).map_err(configure_refused)?;
    hw.set_access(alsa::pcm::Access::RWInterleaved).map_err(configure_refused)?;
    hw.set_format(alsa_format(fmt.sample)?).map_err(configure_refused)?;
    hw.set_channels(u32::from(fmt.channels.get())).map_err(configure_refused)?;
    hw.set_rate_resample(false).map_err(configure_refused)?;
    hw.set_rate(fmt.rate.hz(), alsa::ValueOr::Nearest).map_err(configure_refused)?;
    let (buffer_us, period_us) = buffer_period_us(buffer);
    hw.set_period_time_near(period_us, alsa::ValueOr::Nearest).map_err(configure_refused)?;
    hw.set_buffer_time_near(buffer_us, alsa::ValueOr::Nearest).map_err(configure_refused)?;
    pcm.hw_params(&hw).map_err(configure_refused)
}

/// Формат ALSA из формата потока (§6.7, ТЗ-112, ОВ-32): Exclusive hw_params
/// принимает только `S16_LE`/`S24_3_LE`/`S24_LE`/`S32_LE` — `F32` входит в
/// `FORMAT_MAP` для возможностей устройства (`device_caps_of`), но вне
/// объёма hw_params Exclusive, поэтому исключается явно, не доходя до
/// обратного поиска по `FORMAT_MAP`.
fn alsa_format(sample: OutputSampleFormat) -> Result<alsa::pcm::Format, ConfigureError> {
    if sample == OutputSampleFormat::F32 {
        return Err(ConfigureError(format!("формат {sample:?} не поддерживается Exclusive (ТЗ-112)")));
    }
    FORMAT_MAP
        .iter()
        .find(|(_, out)| *out == sample)
        .map(|(alsa_fmt, _)| *alsa_fmt)
        .ok_or_else(|| ConfigureError(format!("формат {sample:?} не найден в FORMAT_MAP")))
}

/// Байт на сэмпл формата устройства (§6.7, ТЗ-112): ширина контейнера ALSA
/// (`physical_width`, в битах) формата из `alsa_format`, а не значащих бит
/// (`width`) — например, `S243LE` занимает контейнер 32 бита при 24
/// значащих. Нужен для размера буфера периода потока вывода (§6.7).
fn bytes_per_sample(sample: OutputSampleFormat) -> Result<u32, StartError> {
    let fmt = alsa_format(sample).map_err(|e| StartError(e.0))?;
    let bits = fmt.physical_width().map_err(|e| StartError(e.to_string()))?;
    let bits = u32::try_from(bits).map_err(|_| StartError(format!("ширина формата {sample:?} отрицательна (§6.7)")))?;
    Ok(bits / 8)
}

/// Период и буфер в микросекундах для `period_time_near`/`buffer_time_near`
/// (§6.7, ОВС-13): буфер — «Буфер устройства» целиком, период — его
/// четверть (`DeviceBuffer::period_ms`). Умножение `u16 × 1000` не
/// переполняет `u32` при любом значении перечисления, поэтому функция
/// возвращает значения напрямую, без `Result`.
fn buffer_period_us(buffer: DeviceBuffer) -> (u32, u32) {
    let buffer_us = u32::from(buffer.ms()) * 1_000;
    let period_us = u32::from(buffer.period_ms()) * 1_000;
    (buffer_us, period_us)
}

/// Отказ ALSA при установке hw_params (§6.7, ТЗ-113): `ConfigureError` не
/// задаёт вариантов — текст ошибки хоста ALSA.
fn configure_refused(e: alsa::Error) -> ConfigureError {
    ConfigureError(e.to_string())
}

/// Показания драйвера из одного чтения `hw_params_current` (§6.7 п. 3,
/// ТЗ-113): питает и sw_params (период/буфер в кадрах), и
/// `DriverFormat`/`DeviceBufferActual` — второго обращения к ALSA нет.
struct DriverReadout {
    rate_hz: u32,
    sample: OutputSampleFormat,
    channels: u8,
    period_frames: alsa::pcm::Frames,
    buffer_frames: alsa::pcm::Frames,
}

/// Однократное чтение `hw_params_current` (§6.7 п. 3, ТЗ-113): `None`, если
/// ALSA не отдала параметры, либо формат вне `FORMAT_MAP`, либо число
/// каналов не укладывается в `u8` — любой из этих случаев ведёт к
/// `DriverFormat::Unreadable` (ОВ-14), а не к панике или `as`-касту.
fn read_hw_params_current(pcm: &alsa::PCM) -> Option<DriverReadout> {
    let hw = pcm.hw_params_current().ok()?;
    let rate_hz = hw.get_rate().ok()?;
    let alsa_fmt = hw.get_format().ok()?;
    let sample = FORMAT_MAP.iter().find(|(f, _)| *f == alsa_fmt).map(|(_, s)| *s)?;
    let channels = u8::try_from(hw.get_channels().ok()?).ok()?;
    let period_frames = hw.get_period_size().ok()?;
    let buffer_frames = hw.get_buffer_size().ok()?;
    Some(DriverReadout { rate_hz, sample, channels, period_frames, buffer_frames })
}

/// Установка `avail_min`/`start_threshold` по кадрам периода/буфера (§6.7
/// п. 2, ТЗ-113). Кадры приходят либо из чтения hw_params (точные значения
/// драйвера), либо, если чтение не удалось, из `requested_frames` —
/// sw_params должны быть установлены независимо от исхода чтения.
fn apply_sw_params(
    pcm: &alsa::PCM,
    period_frames: alsa::pcm::Frames,
    buffer_frames: alsa::pcm::Frames,
) -> Result<(), ConfigureError> {
    let sw = pcm.sw_params_current().map_err(configure_refused)?;
    sw.set_avail_min(period_frames).map_err(configure_refused)?;
    sw.set_start_threshold(buffer_frames).map_err(configure_refused)?;
    pcm.sw_params(&sw).map_err(configure_refused)
}

/// Период/буфер в кадрах по запрошенным параметрам (§6.7 п. 2, ТЗ-113):
/// используется для sw_params только тогда, когда чтение hw_params_current
/// не удалось — `configure` не может оставить sw_params неустановленными
/// из-за сбоя одного лишь подтверждающего чтения.
fn requested_frames(buffer: DeviceBuffer, rate: SampleRate) -> (alsa::pcm::Frames, alsa::pcm::Frames) {
    let (buffer_us, period_us) = buffer_period_us(buffer);
    (us_to_frames(period_us, rate.hz()), us_to_frames(buffer_us, rate.hz()))
}

/// Микросекунды в кадры при частоте `rate_hz` (§6.7, ТЗ-113): запрошенные
/// период/буфер заданы в микросекундах (`buffer_period_us`), sw_params —
/// в кадрах. Границы `DeviceBuffer` (≤400 мс) и `STANDARD_RATES_HZ`
/// (≤768 000 Гц) исключают переполнение `Frames` (псевдоним `i64`).
fn us_to_frames(us: u32, rate_hz: u32) -> alsa::pcm::Frames {
    alsa::pcm::Frames::from(us) * alsa::pcm::Frames::from(rate_hz) / 1_000_000
}

/// Формат из показаний драйвера (§6.7 п. 3, ТЗ-113): `None`, если частота
/// или число каналов не проходят собственные инварианты `StreamFormat`
/// (`SampleRate`/`NonZeroU8` не принимают 0) — тогда результат `configure`
/// — `Unreadable`, а не сконструированный из невалидных данных формат.
fn confirmed_format(read: &DriverReadout) -> Option<StreamFormat> {
    let rate = SampleRate::new(read.rate_hz)?;
    let channels = NonZeroU8::new(read.channels)?;
    Some(StreamFormat { rate, sample: read.sample, channels })
}

/// `DeviceBufferActual` из кадров периода/буфера (§6.7 п. 3, ТЗ-113,
/// ТЗ-76, ТЗ-131): `None`, если `Frames` не укладывается в `u32` —
/// на практике ALSA не возвращает такие значения, но тип этого не
/// гарантирует, поэтому каста `as` здесь нет. Общая для показаний драйвера
/// (`device_buffer_actual`) и для запрошенных кадров на случай ошибки
/// чтения (`build_driver_format`).
fn frames_to_buffer_actual(period_frames: alsa::pcm::Frames, buffer_frames: alsa::pcm::Frames) -> Option<DeviceBufferActual> {
    let period_frames = u32::try_from(period_frames).ok()?;
    let buffer_frames = u32::try_from(buffer_frames).ok()?;
    Some(DeviceBufferActual { period_frames, buffer_frames })
}

/// `DeviceBufferActual` из показаний драйвера (§6.7 п. 3, ТЗ-113).
fn device_buffer_actual(read: &DriverReadout) -> Option<DeviceBufferActual> {
    frames_to_buffer_actual(read.period_frames, read.buffer_frames)
}

/// Сборка результата `configure` из показаний драйвера (§6.7 п. 3, ТЗ-113,
/// ОВ-14) — чистая логика без ALSA, вынесена отдельно для юнит-тестов.
/// Ошибка чтения (или формат/частота не проходят инварианты `StreamFormat`)
/// → `Unreadable{requested}`, а период и буфер — запрошенные (§6.7 п. 3):
/// `requested_period_frames`/`requested_buffer_frames` — те же кадры, что
/// `configure` уже посчитал для sw_params (`requested_frames`), повторного
/// пересчёта здесь нет. `None` вместо них — только если даже запрошенные
/// кадры не укладываются в `u32` (переполнение, см. `frames_to_buffer_actual`).
/// Иначе — всегда `Confirmed(actual)`, даже если `actual` расходится с
/// `requested`: сравнение подтверждённых параметров с запрошенными — задача
/// бейджа «Bit-perfect» (ТЗ-52), а не `configure` (§2 `DriverFormat::Confirmed`
/// в `backend/mod.rs`).
fn build_driver_format(
    requested: StreamFormat,
    read: Option<DriverReadout>,
    requested_period_frames: alsa::pcm::Frames,
    requested_buffer_frames: alsa::pcm::Frames,
) -> (DriverFormat, Option<DeviceBufferActual>) {
    let requested_buffer_actual = || frames_to_buffer_actual(requested_period_frames, requested_buffer_frames);
    let Some(read) = read else {
        return (DriverFormat::Unreadable { requested }, requested_buffer_actual());
    };
    match confirmed_format(&read) {
        Some(actual) => (DriverFormat::Confirmed(actual), device_buffer_actual(&read)),
        None => (DriverFormat::Unreadable { requested }, requested_buffer_actual()),
    }
}

/// Ядро цикла OPEN (§6.6, ТЗ-48, ТЗ-49, ТЗ-122): `EBUSY` повторяется с шагом
/// `OPEN_RETRY_STEP`, пока `clock.now()` не достигнет дедлайна
/// `OPEN_EBUSY_DEADLINE` от момента вызова, иначе — отказ
/// `BusyOutsideProtocol`; `ENOENT`/`ENODEV` — устройство отключено,
/// `DeviceDisconnected`; любой другой errno — `DriverRefused`. Без знания о
/// резервировании — общее между первым захватом (`open_claimed`) и
/// переоткрытием при `reconfigure` (`reopen_pcm`, §6.7), которые по-разному
/// решают, что делать с резервированием при отказе. `clock`/`sleep`/`open`
/// инжектируются (ADR-20): тест на дедлайн 1 с не ждёт реальное время. `P` —
/// открытый хэндл (в проде `alsa::PCM`, в тестах — фейк).
fn open_retry<P>(
    clock: &dyn Clock,
    mut sleep: impl FnMut(Duration),
    mut open: impl FnMut() -> Result<P, i32>,
) -> Result<P, CaptureFailure> {
    let deadline = clock.now().saturating_add(OPEN_EBUSY_DEADLINE);
    loop {
        match open() {
            Ok(pcm) => return Ok(pcm),
            Err(errno) if errno == libc::EBUSY && clock.now() < deadline => sleep(OPEN_RETRY_STEP),
            Err(errno) if errno == libc::EBUSY => return Err(CaptureFailure::BusyOutsideProtocol),
            Err(errno) if errno == libc::ENOENT || errno == libc::ENODEV => return Err(CaptureFailure::DeviceDisconnected),
            Err(errno) => return Err(CaptureFailure::DriverRefused { errno }),
        }
    }
}

/// Первый захват `hw:` после резервирования (§6.6, ТЗ-48, ТЗ-49, ТЗ-122):
/// оборачивает `open_retry`, явно снимая резервирование (`drop`) при любом
/// отказе до возврата — bug AM1.0-B1 был вызван тем, что старый путь мог
/// оставить резервирование удержанным без открытого PCM (устройство не
/// возвращалось PipeWire). `reservation` либо переходит в успешный результат
/// вместе с открытым `P`, либо снимается здесь же. Функция не строит
/// `DeviceClaim` сама, это делает вызывающая сторона на успехе.
#[allow(dead_code)]
fn open_claimed<P>(
    reservation: Reservation,
    clock: &dyn Clock,
    sleep: impl FnMut(Duration),
    open: impl FnMut() -> Result<P, i32>,
) -> Result<(P, Reservation), CaptureFailure> {
    match open_retry(clock, sleep, open) {
        Ok(pcm) => Ok((pcm, reservation)),
        Err(e) => {
            drop(reservation);
            Err(e)
        }
    }
}

/// Переоткрытие `hw:` того же устройства при `reconfigure` после отказа
/// `hw_params` на держимом PCM (§6.7, ТЗ-49): то же ядро `open_retry`, что и
/// у первого захвата, но без резервирования — оно всё это время остаётся в
/// `DeviceClaim` вызывающей стороны (`reconfigure` его не трогает) и не
/// снимается здесь при отказе (И-Р1: снимается вместе со всем `claim`, не
/// раньше). Выполняется в служебном потоке захвата — блокирующий `PCM::new`
/// и реальный `sleep` здесь допустимы (не аудио-колбэк).
fn reopen_pcm(dev: &HwDeviceId) -> Result<alsa::PCM, CaptureFailure> {
    let hw_name = dev.as_str().to_string();
    let clock = MonotonicClock::new();
    open_retry(&clock, std::thread::sleep, || {
        alsa::PCM::new(&hw_name, alsa::Direction::Playback, false).map_err(|e| e.errno())
    })
}

/// Номер карты ALSA из `HwDeviceId` (`hw:CARD=<id>,DEV=<d>`, §6.6, ТЗ-126):
/// разбор делегирован `reservation::dbus::alsa_card_index`. Карта не найдена
/// в системе — устройство отключено, другого варианта `CaptureFailure` для
/// этого случая спецификация не задаёт.
#[allow(dead_code)]
fn card_number(dev: &HwDeviceId) -> Result<u32, CaptureFailure> {
    alsa_card_index(dev.as_str()).ok_or(CaptureFailure::DeviceDisconnected)
}

/// `hw:CARD=<id>,DEV=<d>` для перечисления (§6.6). `None`, если строка не
/// проходит разбор `HwDeviceId` (пустой `card_id` после обрезки пробелов).
fn build_hw_id(card_id: &str, device: u32) -> Option<HwDeviceId> {
    format!("hw:CARD={card_id},DEV={device}").parse().ok()
}

/// Отказ ALSA при чтении hw_params уже открытого PCM (§6.6): единственный
/// подходящий вариант закрытого набора `CaptureFailure`.
fn driver_refused(e: alsa::Error) -> CaptureFailure {
    CaptureFailure::DriverRefused { errno: e.errno() }
}

/// Частоты устройства (§2.3, §6.6): дискретный список через `test_rate` по
/// стандартным частотам, диапазон `get_rate_min`/`get_rate_max` — только
/// если ни одна из них не принята.
fn probe_rates(hw: &alsa::pcm::HwParams<'_>) -> Result<RateSet, CaptureFailure> {
    let mut discrete = SmallVec::new();
    for hz in STANDARD_RATES_HZ {
        if hw.test_rate(hz).is_ok() {
            if let Some(rate) = SampleRate::new(hz) {
                discrete.push(rate);
            }
        }
    }
    if !discrete.is_empty() {
        return Ok(RateSet::Discrete(discrete));
    }

    let min = hw.get_rate_min().map_err(driver_refused)?;
    let max = hw.get_rate_max().map_err(driver_refused)?;
    match (SampleRate::new(min), SampleRate::new(max)) {
        (Some(min), Some(max)) => Ok(RateSet::Range { min, max }),
        // Драйвер вернул 0 Гц: не отказ ALSA, но представить диапазон нечем.
        // errno = 0 отличает этот случай от настоящей ошибки драйвера.
        _ => Err(CaptureFailure::DriverRefused { errno: 0 }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;
    use crate::audio::testing::{BusCall, FakeOwner, FakeReserveBus, FakeServerProbe, ManualClock};

    #[test]
    fn caps_reports_exclusive_on_linux() {
        let backend = AlsaExclusiveBackend::new();
        assert!(backend.caps().exclusive);
    }

    /// `AlsaExclusiveBackend` реализует `ExclusiveBackend` и годится в
    /// `Box<dyn ExclusiveBackend>`, которым владеет `EngineDeps` (§2.3, §6.7).
    /// Без обращения к оборудованию — только проверка типа при компиляции.
    #[test]
    fn alsa_exclusive_backend_is_boxable_dyn_exclusive_backend() {
        let _backend: Box<dyn ExclusiveBackend> = Box::new(AlsaExclusiveBackend::new());
    }

    #[test]
    fn build_hw_id_formats_card_and_device() {
        let id = build_hw_id("PCH", 1).expect("valid id");
        assert_eq!(id.as_str(), "hw:CARD=PCH,DEV=1");
    }

    #[test]
    fn format_map_covers_all_output_sample_formats() {
        assert_eq!(FORMAT_MAP.len(), 5);
        assert!(FORMAT_MAP.contains(&(alsa::pcm::Format::S16LE, OutputSampleFormat::S16Le)));
        assert!(FORMAT_MAP.contains(&(alsa::pcm::Format::FloatLE, OutputSampleFormat::F32)));
    }

    #[test]
    fn alsa_format_maps_exclusive_sample_formats() {
        assert_eq!(alsa_format(OutputSampleFormat::S16Le), Ok(alsa::pcm::Format::S16LE));
        assert_eq!(alsa_format(OutputSampleFormat::S24_3Le), Ok(alsa::pcm::Format::S243LE));
        assert_eq!(alsa_format(OutputSampleFormat::S24Le), Ok(alsa::pcm::Format::S24LE));
        assert_eq!(alsa_format(OutputSampleFormat::S32Le), Ok(alsa::pcm::Format::S32LE));
    }

    #[test]
    fn alsa_format_rejects_f32_outside_exclusive_scope() {
        assert!(alsa_format(OutputSampleFormat::F32).is_err());
    }

    #[test]
    fn buffer_period_us_is_quarter_period_in_microseconds() {
        assert_eq!(buffer_period_us(DeviceBuffer::Ms40), (40_000, 10_000));
        assert_eq!(buffer_period_us(DeviceBuffer::Ms400), (400_000, 100_000));
        assert_eq!(buffer_period_us(DeviceBuffer::Ms20), (20_000, 5_000));
    }

    #[test]
    fn build_driver_format_confirms_when_actual_matches_requested() {
        let requested = StreamFormat {
            rate: SampleRate::new(96_000).unwrap(),
            sample: OutputSampleFormat::S24_3Le,
            channels: NonZeroU8::new(2).unwrap(),
        };
        let read = DriverReadout {
            rate_hz: 96_000,
            sample: OutputSampleFormat::S24_3Le,
            channels: 2,
            period_frames: 1_200,
            buffer_frames: 4_800,
        };
        let (driver_format, buffer_actual) = build_driver_format(requested, Some(read), 1_200, 4_800);
        assert_eq!(driver_format, DriverFormat::Confirmed(requested));
        assert_eq!(buffer_actual, Some(DeviceBufferActual { period_frames: 1_200, buffer_frames: 4_800 }));
    }

    #[test]
    fn build_driver_format_confirms_actual_even_when_mismatched_with_requested() {
        // §6.7 п. 3 / ТЗ-52: сравнение с запрошенным — задача бейджа
        // «Bit-perfect», а не `configure`. Расхождение частоты не превращает
        // успешное чтение в ошибку.
        let requested = StreamFormat {
            rate: SampleRate::new(192_000).unwrap(),
            sample: OutputSampleFormat::S24_3Le,
            channels: NonZeroU8::new(2).unwrap(),
        };
        let read = DriverReadout {
            rate_hz: 96_000,
            sample: OutputSampleFormat::S24_3Le,
            channels: 2,
            period_frames: 2_400,
            buffer_frames: 9_600,
        };
        let actual = StreamFormat { rate: SampleRate::new(96_000).unwrap(), ..requested };
        let (driver_format, buffer_actual) = build_driver_format(requested, Some(read), 2_400, 9_600);
        assert_eq!(driver_format, DriverFormat::Confirmed(actual));
        assert_eq!(buffer_actual, Some(DeviceBufferActual { period_frames: 2_400, buffer_frames: 9_600 }));
    }

    #[test]
    fn build_driver_format_unreadable_returns_requested_buffer_actual_when_read_fails() {
        // §6.7 п. 3: ошибка чтения → Unreadable, период и буфер — запрошенные
        // (не None) — именно те кадры, что `configure` уже посчитал бы для
        // sw_params через `requested_frames`.
        let requested = StreamFormat {
            rate: SampleRate::new(44_100).unwrap(),
            sample: OutputSampleFormat::S16Le,
            channels: NonZeroU8::new(2).unwrap(),
        };
        let (driver_format, buffer_actual) = build_driver_format(requested, None, 200, 800);
        assert_eq!(driver_format, DriverFormat::Unreadable { requested });
        assert_eq!(buffer_actual, Some(DeviceBufferActual { period_frames: 200, buffer_frames: 800 }));
    }

    fn noop() -> LostNotify {
        Box::new(|| {})
    }

    #[test]
    fn card_number_parses_numeric_card() {
        let dev: HwDeviceId = "hw:CARD=4,DEV=0".parse().unwrap();
        assert_eq!(card_number(&dev), Ok(4));
    }

    #[test]
    fn card_number_errs_device_disconnected_for_unknown_symbolic_card() {
        let dev: HwDeviceId = "hw:CARD=NO_SUCH_CARD_IN_TEST_ENV,DEV=0".parse().unwrap();
        assert_eq!(card_number(&dev), Err(CaptureFailure::DeviceDisconnected));
    }

    #[test]
    fn reserve_short_circuits_on_unresolvable_card() {
        let dev: HwDeviceId = "hw:CARD=NO_SUCH_CARD_IN_TEST_ENV,DEV=0".parse().unwrap();
        let mut backend = AlsaExclusiveBackend::new();
        let r = backend.reserve(&dev, "USB DAC", noop());
        assert!(matches!(r, Err(CaptureFailure::DeviceDisconnected)));
    }

    /// Резервирование карты 7 через фейковую шину — общая подготовка для
    /// тестов `open_claimed` (§6.6, ТЗ-48).
    fn held(bus: &Arc<dyn ReserveBus>) -> Reservation {
        reservation::acquire(bus, &FakeServerProbe::desktop(), 7, "Test DAC", noop()).expect("резервирование удержано")
    }

    #[test]
    fn alsa_acquire_open_claimed_retries_ebusy_then_succeeds() {
        let clock = ManualClock::new();
        let fake = FakeReserveBus::new(clock.clone(), FakeOwner::Free);
        let bus: Arc<dyn ReserveBus> = Arc::new(fake.clone());
        let reservation = held(&bus);

        let mut attempts = 0;
        let (pcm, reservation) = open_claimed(reservation, &clock, |d| clock.advance(d), || {
            attempts += 1;
            if attempts < 3 {
                Err(libc::EBUSY)
            } else {
                Ok(99)
            }
        })
        .expect("успех после повторов EBUSY до дедлайна");

        assert_eq!(pcm, 99);
        assert_eq!(attempts, 3);
        assert!(!fake.calls().contains(&BusCall::ReleaseName { card: 7 }), "резервирование снято раньше срока");
        drop(reservation);
        assert!(fake.calls().contains(&BusCall::ReleaseName { card: 7 }), "резервирование не снято после использования");
    }

    #[test]
    fn alsa_acquire_open_claimed_ebusy_past_deadline_is_busy_outside_protocol() {
        let clock = ManualClock::new();
        let fake = FakeReserveBus::new(clock.clone(), FakeOwner::Free);
        let bus: Arc<dyn ReserveBus> = Arc::new(fake.clone());
        let reservation = held(&bus);

        let result: Result<(i32, Reservation), CaptureFailure> =
            open_claimed(reservation, &clock, |d| clock.advance(d), || Err(libc::EBUSY));

        assert_eq!(result.err(), Some(CaptureFailure::BusyOutsideProtocol));
        assert!(fake.calls().contains(&BusCall::ReleaseName { card: 7 }));
        assert!(fake.calls().contains(&BusCall::Unexport { card: 7 }));
    }

    #[test]
    fn alsa_acquire_open_claimed_releases_reservation_order_on_disconnect() {
        let clock = ManualClock::new();
        let fake = FakeReserveBus::new(clock.clone(), FakeOwner::Free);
        let bus: Arc<dyn ReserveBus> = Arc::new(fake.clone());
        let reservation = held(&bus);

        let result: Result<(i32, Reservation), CaptureFailure> =
            open_claimed(reservation, &clock, |d| clock.advance(d), || Err(libc::ENODEV));

        assert_eq!(result.err(), Some(CaptureFailure::DeviceDisconnected));
        let calls = fake.calls();
        let release_at =
            calls.iter().position(|c| *c == BusCall::ReleaseName { card: 7 }).expect("ReleaseName записан");
        let unexport_at = calls.iter().position(|c| *c == BusCall::Unexport { card: 7 }).expect("Unexport записан");
        assert!(release_at < unexport_at, "И-Р1/AM1.0-B1: ReleaseName должен предшествовать Unexport");
    }

    #[test]
    fn alsa_reconfigure_retry_core_retries_ebusy_then_succeeds() {
        let clock = ManualClock::new();
        let mut attempts = 0;
        let result: Result<i32, CaptureFailure> = open_retry(&clock, |d| clock.advance(d), || {
            attempts += 1;
            if attempts < 3 {
                Err(libc::EBUSY)
            } else {
                Ok(42)
            }
        });

        assert_eq!(result, Ok(42));
        assert_eq!(attempts, 3);
    }

    #[test]
    fn alsa_reconfigure_retry_core_ebusy_past_deadline_is_busy_outside_protocol() {
        let clock = ManualClock::new();
        let result: Result<i32, CaptureFailure> = open_retry(&clock, |d| clock.advance(d), || Err(libc::EBUSY));

        assert_eq!(result, Err(CaptureFailure::BusyOutsideProtocol));
    }

    #[test]
    fn alsa_reconfigure_without_open_pcm_returns_configure_error() {
        let clock = ManualClock::new();
        let fake = FakeReserveBus::new(clock.clone(), FakeOwner::Free);
        let bus: Arc<dyn ReserveBus> = Arc::new(fake.clone());
        let reservation = held(&bus);
        let dev: HwDeviceId = "hw:CARD=7,DEV=0".parse().unwrap();
        let mut claim = DeviceClaim::new(None, reservation, ClaimInfo { device: dev });
        let mut backend = AlsaExclusiveBackend::new();
        let fmt = StreamFormat {
            rate: SampleRate::new(44_100).unwrap(),
            sample: OutputSampleFormat::S16Le,
            channels: NonZeroU8::new(2).unwrap(),
        };

        let result = backend.reconfigure(&mut claim, fmt, DeviceBuffer::Ms40);

        assert!(matches!(result, Err(ReconfigureError::Configure(_))));
    }
}
