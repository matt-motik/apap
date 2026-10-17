//! Эксклюзивный бэкенд ALSA: перечисление карт через `ctl` и возможности
//! открытого PCM (§2.3, §6.6, ADR-24, ТЗ-42…ТЗ-47, ТЗ-143). Резервирование,
//! открытие `hw:` PCM и поток вывода — отдельные этапы (`acquire`,
//! `configure`, `start` трейта `ExclusiveBackend`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::sync::Arc;
use std::time::Duration;

use smallvec::SmallVec;

use super::catalog::HwDeviceInfo;
use super::exclusive::{ClaimInfo, ConfigureError, DeviceClaim, PcmHandle};
use super::{AccessKind, BackendCaps, BackendError, DeviceCaps, HwDeviceId, OutputApi, RateSet};
use crate::audio::clock::{Clock, MonotonicClock};
use crate::audio::error::CaptureFailure;
use crate::audio::format::{OutputSampleFormat, SampleRate, StreamFormat};
use crate::audio::reservation::dbus::{alsa_card_index, DbusReserveBus, SystemServerProbe};
use crate::audio::reservation::{self, LostNotify, Reservation, ReserveBus};
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
    /// Вызывающей стороны пока нет — появится вместе с реализацией
    /// `ExclusiveBackend::acquire` (часть 2).
    #[allow(dead_code)]
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
    /// `ExclusiveBackend::acquire` (§6.6 OPEN, ТЗ-48, ТЗ-49, ADR-08). Сигнатура
    /// совпадает с методом трейта: полная реализация `ExclusiveBackend` для
    /// `AlsaExclusiveBackend` появится вместе с `configure`/`start`. Выполняется
    /// в служебном потоке захвата — блокирующий `PCM::new` и реальный `sleep`
    /// здесь допустимы (не аудио-колбэк).
    #[allow(dead_code)]
    pub(crate) fn acquire(&mut self, dev: &HwDeviceId, reservation: Reservation) -> Result<DeviceClaim, CaptureFailure> {
        let hw_name = dev.as_str().to_string();
        let clock = MonotonicClock::new();
        let (pcm, reservation) = open_claimed(reservation, &clock, std::thread::sleep, || {
            alsa::PCM::new(&hw_name, alsa::Direction::Playback, false).map_err(|e| e.errno())
        })?;
        Ok(DeviceClaim::new(Some(PcmHandle::new(pcm)), reservation, ClaimInfo { device: dev.clone() }))
    }
}

/// hw_params Exclusive-потока: доступ `RW_INTERLEAVED`, формат, число
/// каналов, частота без ресэмплинга драйвера, период и буфер по настройке
/// «Буфер устройства» (§6.7 п. 1, ТЗ-50, ТЗ-112, ТЗ-131). Порядок вызовов
/// обязателен: ALSA отвергает период/буфер, заданные раньше access/format/
/// channels/rate. Фактические параметры здесь не перечитываются — это
/// вторая часть `configure` (ТЗ-113, §2.3). Вызывающей стороны пока нет —
/// появится вместе с реализацией `ExclusiveBackend::configure`.
#[allow(dead_code)]
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

/// Цикл OPEN (§6.6, ТЗ-48, ТЗ-49, ТЗ-122): `EBUSY` повторяется с шагом
/// `OPEN_RETRY_STEP`, пока `clock.now()` не достигнет дедлайна
/// `OPEN_EBUSY_DEADLINE` от момента вызова, иначе — отказ
/// `BusyOutsideProtocol`; `ENOENT`/`ENODEV` — устройство отключено,
/// `DeviceDisconnected`; любой другой errno — `DriverRefused`.
/// Резервирование при любом отказе явно снимается (`drop`) до возврата —
/// bug AM1.0-B1 был вызван тем, что старый путь мог оставить резервирование
/// удержанным без открытого PCM (устройство не возвращалось PipeWire); здесь
/// `reservation` либо переходит в успешный результат вместе с открытым `P`,
/// либо снимается в той же ветке, что возвращает отказ. `clock`/`sleep`/`open`
/// инжектируются (ADR-20): тест на дедлайн 1 с не ждёт реальное время. `P` —
/// открытый хэндл (в проде `alsa::PCM`, в тестах — фейк); функция не строит
/// `DeviceClaim` сама, это делает вызывающая сторона на успехе.
#[allow(dead_code)]
fn open_claimed<P>(
    reservation: Reservation,
    clock: &dyn Clock,
    mut sleep: impl FnMut(Duration),
    mut open: impl FnMut() -> Result<P, i32>,
) -> Result<(P, Reservation), CaptureFailure> {
    let deadline = clock.now().saturating_add(OPEN_EBUSY_DEADLINE);
    loop {
        match open() {
            Ok(pcm) => return Ok((pcm, reservation)),
            Err(errno) if errno == libc::EBUSY && clock.now() < deadline => sleep(OPEN_RETRY_STEP),
            Err(errno) if errno == libc::EBUSY => {
                drop(reservation);
                return Err(CaptureFailure::BusyOutsideProtocol);
            }
            Err(errno) if errno == libc::ENOENT || errno == libc::ENODEV => {
                drop(reservation);
                return Err(CaptureFailure::DeviceDisconnected);
            }
            Err(errno) => {
                drop(reservation);
                return Err(CaptureFailure::DriverRefused { errno });
            }
        }
    }
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
}
