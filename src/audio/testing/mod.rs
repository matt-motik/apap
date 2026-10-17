//! Тестовая инфраструктура тракта (AM1.0 §7.1, ADR-20). Компилируется всегда,
//! как подмены платформенных модулей (ADR-6 задачи 02): её используют тесты
//! бинарника и `tests/`, которые собирают библиотеку без `cfg(test)`. Фейки
//! трейтов `EngineDeps` добавляются по этапам (таблица «Трейт → этап» под §8).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

pub mod signals;

use crate::audio::backend::catalog::HwDeviceInfo;
use crate::audio::backend::exclusive::{
    ClaimInfo, ClaimJob, ConfigureError, DeviceClaim, ExclusiveBackend, ExclusiveRender, PcmHandle, StartError,
    StreamHandle,
};
use crate::audio::backend::{AccessKind, BackendCaps, BackendError, DeviceBufferActual, DeviceCaps, DriverFormat, HwDeviceId, OutputApi, RateSet};
use crate::audio::clock::{Clock, ClockInstant};
use crate::audio::error::CaptureFailure;
use crate::audio::format::{OutputSampleFormat, SampleRate, StreamFormat};
use crate::audio::reservation::{
    self, AudioServerProbe, CallError, LostNotify, NameFlags, RequestNameReply, Reservation, ReserveBus, ReserveObject,
    SinkCard, OWNER_NAME_TIMEOUT, RELEASE_TIMEOUT,
};
use crate::audio::session::SessionShared;
use crate::settings::playback::DeviceBuffer;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::num::NonZeroU8;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Часы теста (ADR-20): время идёт только по `advance`. Клоны делят одно время.
#[derive(Clone, Default)]
pub struct ManualClock {
    nanos: Arc<AtomicU64>,
}

impl ManualClock {
    /// Часы в `ClockInstant::START`.
    pub fn new() -> ManualClock {
        ManualClock::default()
    }

    /// Сдвинуть время вперёд на `d` (насыщается на `u64::MAX` нс ≈ 584 года).
    pub fn advance(&self, d: Duration) {
        let add = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let mut cur = self.nanos.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_add(add);
            match self.nanos.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> ClockInstant {
        ClockInstant::from_start(Duration::from_nanos(self.nanos.load(Ordering::Acquire)))
    }
}

/// Наличие шины и аудиосервера для тестов ОВ-35 (таблица «Трейт → этап», С1);
/// карта системного выхода для автовыбора устройства (ADR-24, ТЗ-143).
#[derive(Debug)]
pub struct FakeServerProbe {
    pub session_bus: bool,
    pub server_running: bool,
    pub sink_card: Option<SinkCard>,
    pub sink_card_calls: AtomicUsize,
}

impl FakeServerProbe {
    /// Обычный рабочий стол: шина есть, PipeWire запущен, карта выхода не задана.
    pub fn desktop() -> FakeServerProbe {
        FakeServerProbe {
            session_bus: true,
            server_running: true,
            sink_card: None,
            sink_card_calls: AtomicUsize::new(0),
        }
    }

    /// Карта, которую должен вернуть `default_sink_card` (ADR-24, ТЗ-143).
    pub fn with_sink_card(mut self, card: Option<SinkCard>) -> FakeServerProbe {
        self.sink_card = card;
        self
    }

    /// Сколько раз был вызван `default_sink_card` (ADR-24, ТЗ-143).
    pub fn sink_card_calls(&self) -> usize {
        self.sink_card_calls.load(Ordering::Acquire)
    }
}

impl AudioServerProbe for FakeServerProbe {
    fn session_bus(&self) -> bool {
        self.session_bus
    }

    fn server_running(&self) -> bool {
        self.server_running
    }

    fn default_sink_card(&self) -> Option<SinkCard> {
        self.sink_card_calls.fetch_add(1, Ordering::AcqRel);
        self.sink_card
    }
}

/// Поведение текущего владельца имени `ReserveDevice1.AudioN` (§6.6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FakeOwner {
    /// Имя свободно.
    Free,
    /// Отвечает на `RequestRelease` `true` через `reply_after`.
    Grants { app: Option<String>, reply_after: Duration },
    /// Отвечает `false` сразу.
    Denies { app: Option<String> },
    /// Отвечает `true` через `reply_after`; дольше `RELEASE_TIMEOUT` — таймаут.
    Silent { app: Option<String>, reply_after: Duration },
    /// Вызовы к владельцу завершаются ошибкой (не таймаут).
    CallFails,
}

/// Запись журнала `FakeReserveBus` (И-Р1, И-Р21).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BusCall {
    Export { card: u32 },
    Unexport { card: u32 },
    RequestName { card: u32, flags: NameFlags },
    GetApplicationName { card: u32 },
    RequestRelease { card: u32, priority: i32 },
    ReleaseName { card: u32 },
    /// Открытие/закрытие PCM фейком [`FakePcm`] — для проверки порядка И-Р1.
    PcmOpen { card: u32 },
    PcmClose { card: u32 },
}

/// Фейковый PCM: открытие и закрытие (drop) пишутся в журнал `FakeReserveBus`.
pub struct FakePcm {
    card: u32,
    state: Arc<Mutex<FakeBusState>>,
}

impl Drop for FakePcm {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.calls.push(BusCall::PcmClose { card: self.card });
    }
}

struct Exported {
    object: ReserveObject,
    on_lost: Arc<LostNotify>,
}

struct FakeBusState {
    owner: FakeOwner,
    owned: HashMap<u32, bool>,
    exported: HashMap<u32, Exported>,
    calls: Vec<BusCall>,
}

/// Фейк сессионной шины для резервирования (ADR-08, §6.6): ответы «разрешить»,
/// «отказать», «молчать N с», потеря имени; время ожидания идёт по
/// `ManualClock`, журнал вызовов — `calls()`. Клоны делят состояние.
#[derive(Clone)]
pub struct FakeReserveBus {
    clock: ManualClock,
    state: Arc<Mutex<FakeBusState>>,
}

impl FakeReserveBus {
    pub fn new(clock: ManualClock, owner: FakeOwner) -> FakeReserveBus {
        let state = FakeBusState { owner, owned: HashMap::new(), exported: HashMap::new(), calls: Vec::new() };
        FakeReserveBus { clock, state: Arc::new(Mutex::new(state)) }
    }

    fn lock(&self) -> MutexGuard<'_, FakeBusState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Открыть фейковый PCM карты `card` (запись `PcmOpen` в журнал).
    pub fn open_pcm(&self, card: u32) -> FakePcm {
        self.lock().calls.push(BusCall::PcmOpen { card });
        FakePcm { card, state: Arc::clone(&self.state) }
    }

    pub fn calls(&self) -> Vec<BusCall> {
        self.lock().calls.clone()
    }

    /// Флаги всех вызовов `RequestName` по порядку (И-Р21).
    pub fn requested_flags(&self) -> Vec<NameFlags> {
        self.lock()
            .calls
            .iter()
            .filter_map(|c| match c {
                BusCall::RequestName { flags, .. } => Some(*flags),
                _ => None,
            })
            .collect()
    }

    pub fn exported(&self, card: u32) -> Option<ReserveObject> {
        self.lock().exported.get(&card).map(|e| e.object.clone())
    }

    /// Чужой `RequestRelease` к экспортированному объекту; `None` — объекта нет.
    pub fn incoming_request_release(&self, card: u32, priority: i32) -> Option<bool> {
        self.lock().exported.get(&card).map(|e| e.object.request_release(priority))
    }

    /// Имя отобрано извне (перезапуск шины, внешний `ReleaseName`): сигнал
    /// `NameLost`, если на него есть подписка. Возвращает, был ли доставлен.
    pub fn lose_name(&self, card: u32) -> bool {
        let notify = {
            let mut st = self.lock();
            st.owned.insert(card, false);
            st.exported.get(&card).map(|e| Arc::clone(&e.on_lost))
        };
        match notify {
            Some(n) => {
                n();
                true
            }
            None => false,
        }
    }
}

impl ReserveBus for FakeReserveBus {
    fn export(&self, card: u32, object: ReserveObject, on_lost: LostNotify) -> Result<(), CallError> {
        let mut st = self.lock();
        st.calls.push(BusCall::Export { card });
        st.exported.insert(card, Exported { object, on_lost: Arc::new(on_lost) });
        Ok(())
    }

    fn unexport(&self, card: u32) {
        let mut st = self.lock();
        st.calls.push(BusCall::Unexport { card });
        st.exported.remove(&card);
    }

    fn request_name(&self, card: u32, flags: NameFlags) -> Result<RequestNameReply, CallError> {
        let mut st = self.lock();
        st.calls.push(BusCall::RequestName { card, flags });
        if st.owned.get(&card).copied().unwrap_or(false) {
            return Ok(RequestNameReply::AlreadyOwner);
        }
        // Владелец, ответивший `true` на `RequestRelease`, становится `Free`.
        if matches!(st.owner, FakeOwner::Free) {
            st.owned.insert(card, true);
            return Ok(RequestNameReply::PrimaryOwner);
        }
        Ok(RequestNameReply::Exists)
    }

    fn owner_application_name(&self, card: u32) -> Option<String> {
        let owner = {
            let mut st = self.lock();
            st.calls.push(BusCall::GetApplicationName { card });
            st.owner.clone()
        };
        match owner {
            FakeOwner::Free | FakeOwner::CallFails => None,
            FakeOwner::Grants { app, .. } | FakeOwner::Denies { app } => app,
            FakeOwner::Silent { .. } => {
                self.clock.advance(OWNER_NAME_TIMEOUT);
                None
            }
        }
    }

    fn request_release(&self, card: u32, priority: i32) -> Result<bool, CallError> {
        let owner = {
            let mut st = self.lock();
            st.calls.push(BusCall::RequestRelease { card, priority });
            st.owner.clone()
        };
        let reply = match owner {
            FakeOwner::Free => Ok(true),
            FakeOwner::Denies { .. } => Ok(false),
            FakeOwner::CallFails => Err(CallError::Failed),
            FakeOwner::Grants { reply_after, .. } | FakeOwner::Silent { reply_after, .. } => {
                if reply_after > RELEASE_TIMEOUT {
                    self.clock.advance(RELEASE_TIMEOUT);
                    Err(CallError::Timeout)
                } else {
                    self.clock.advance(reply_after);
                    Ok(true)
                }
            }
        };
        if reply == Ok(true) {
            self.lock().owner = FakeOwner::Free;
        }
        reply
    }

    fn release_name(&self, card: u32) {
        let mut st = self.lock();
        st.calls.push(BusCall::ReleaseName { card });
        st.owned.insert(card, false);
    }
}

/// Запись журнала `FakeExclusiveBackend` (§7.1, ТЗ-114).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExclusiveCall {
    Enumerate,
    Acquire(HwDeviceId),
    DeviceCaps,
    Configure,
    Start,
    /// `StreamHandle::stop` фейкового потока — пишется в тот же журнал, что
    /// и вызовы бэкенда, поэтому порядок освобождения виден одним списком.
    Stop,
}

/// Частота-константа; ноль отвергается при компиляции (как
/// `path::plan::const_rate` — своя копия, та функция не публична).
const fn const_rate(hz: u32) -> SampleRate {
    match SampleRate::new(hz) {
        Some(rate) => rate,
        None => panic!("частота-константа должна быть ненулевой"),
    }
}

/// Число каналов-константа; ноль отвергается при компиляции.
const fn const_channels(n: u8) -> NonZeroU8 {
    match NonZeroU8::new(n) {
        Some(n) => n,
        None => panic!("число каналов-константа должно быть ненулевым"),
    }
}

const FAKE_RATE_MIN: SampleRate = const_rate(44_100);
const FAKE_RATE_MAX: SampleRate = const_rate(192_000);
const FAKE_STREAM_RATE: SampleRate = const_rate(48_000);
const FAKE_CHANNELS: NonZeroU8 = const_channels(2);

/// Возможности по умолчанию для `device_caps` фейка: монопольный ALSA `hw:`,
/// 44.1–192 кГц, S16/S24/S32, стерео (§2.3).
fn fake_device_caps() -> DeviceCaps {
    DeviceCaps {
        api: OutputApi::AlsaHw,
        access: AccessKind::Exclusive,
        name: "fake exclusive".to_string(),
        rates: RateSet::Range { min: FAKE_RATE_MIN, max: FAKE_RATE_MAX },
        formats: SmallVec::from_slice(&[OutputSampleFormat::S16Le, OutputSampleFormat::S24Le, OutputSampleFormat::S32Le]),
        channels: SmallVec::from_slice(&[2]),
    }
}

/// Формат по умолчанию для `configure` фейка: 48 кГц/S32/стерео (§2.3).
fn fake_driver_format() -> DriverFormat {
    DriverFormat::Confirmed(StreamFormat { rate: FAKE_STREAM_RATE, sample: OutputSampleFormat::S32Le, channels: FAKE_CHANNELS })
}

/// Фейковый поток вывода: `stop` пишет `ExclusiveCall::Stop` в журнал бэкенда,
/// который его породил (§7.1, ТЗ-114).
struct FakeExclusiveStream {
    calls: Arc<Mutex<Vec<ExclusiveCall>>>,
}

impl StreamHandle for FakeExclusiveStream {
    fn stop(self: Box<Self>) -> Option<PcmHandle> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(ExclusiveCall::Stop);
        None
    }
}

/// Сценарный фейк `ExclusiveBackend` (§7.1, ТЗ-114): результаты `acquire`,
/// `device_caps`, `configure`, `start` и список `enumerate` задаются заранее
/// через `with_*`, порядок вызовов виден в общем журнале `calls()`. `acquire`
/// всегда перемещает полученное `Reservation` в возвращённый `DeviceClaim`
/// (`pcm = None`) — наблюдать его освобождение можно по моменту `Drop` этого
/// `DeviceClaim` в тесте. `claim_job` (ADR-01, §6.6) не заимствует фейк: в
/// задании резервирование получается через `FakeReserveBus`/`ManualClock`
/// (без D-Bus), а журнал и `acquire_result` разделяются с фейком через `Arc`.
pub struct FakeExclusiveBackend {
    enumerate_result: Result<Vec<HwDeviceInfo>, BackendError>,
    acquire_result: Result<(), CaptureFailure>,
    device_caps_result: Result<DeviceCaps, CaptureFailure>,
    configure_result: Result<(DriverFormat, Option<DeviceBufferActual>), ConfigureError>,
    start_result: Result<(), StartError>,
    calls: Arc<Mutex<Vec<ExclusiveCall>>>,
}

impl FakeExclusiveBackend {
    /// Все вызовы успешны со значениями по умолчанию.
    pub fn new() -> FakeExclusiveBackend {
        FakeExclusiveBackend {
            enumerate_result: Ok(Vec::new()),
            acquire_result: Ok(()),
            device_caps_result: Ok(fake_device_caps()),
            configure_result: Ok((fake_driver_format(), None)),
            start_result: Ok(()),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_enumerate(mut self, result: Result<Vec<HwDeviceInfo>, BackendError>) -> FakeExclusiveBackend {
        self.enumerate_result = result;
        self
    }

    pub fn with_acquire(mut self, result: Result<(), CaptureFailure>) -> FakeExclusiveBackend {
        self.acquire_result = result;
        self
    }

    pub fn with_device_caps(mut self, result: Result<DeviceCaps, CaptureFailure>) -> FakeExclusiveBackend {
        self.device_caps_result = result;
        self
    }

    pub fn with_configure(
        mut self,
        result: Result<(DriverFormat, Option<DeviceBufferActual>), ConfigureError>,
    ) -> FakeExclusiveBackend {
        self.configure_result = result;
        self
    }

    pub fn with_start(mut self, result: Result<(), StartError>) -> FakeExclusiveBackend {
        self.start_result = result;
        self
    }

    /// Журнал вызовов на момент чтения: хэндл держат отдельно от фейка,
    /// который движок забирает в `Box<dyn ExclusiveBackend>` (§7.1).
    pub fn calls(&self) -> Vec<ExclusiveCall> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn push(&self, call: ExclusiveCall) {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(call);
    }
}

impl Default for FakeExclusiveBackend {
    fn default() -> Self {
        FakeExclusiveBackend::new()
    }
}

impl ExclusiveBackend for FakeExclusiveBackend {
    fn caps(&self) -> BackendCaps {
        BackendCaps { exclusive: true }
    }

    fn enumerate(&mut self) -> Result<Vec<HwDeviceInfo>, BackendError> {
        self.push(ExclusiveCall::Enumerate);
        self.enumerate_result.clone()
    }

    fn acquire(&mut self, dev: &HwDeviceId, reservation: Reservation) -> Result<DeviceClaim, CaptureFailure> {
        self.push(ExclusiveCall::Acquire(dev.clone()));
        self.acquire_result.clone()?;
        Ok(DeviceClaim::new(None, reservation, ClaimInfo { device: dev.clone() }))
    }

    fn claim_job(&self, dev: HwDeviceId, device_name: String, on_lost: LostNotify) -> ClaimJob {
        let calls = Arc::clone(&self.calls);
        let acquire_result = self.acquire_result.clone();
        Box::new(move || {
            calls.lock().unwrap_or_else(|e| e.into_inner()).push(ExclusiveCall::Acquire(dev.clone()));
            acquire_result?;
            let bus: Arc<dyn ReserveBus> = Arc::new(FakeReserveBus::new(ManualClock::new(), FakeOwner::Free));
            let reservation = reservation::acquire(&bus, &FakeServerProbe::desktop(), 0, &device_name, on_lost)?;
            Ok(DeviceClaim::new(None, reservation, ClaimInfo { device: dev }))
        })
    }

    fn device_caps(&mut self, _claim: &mut DeviceClaim) -> Result<DeviceCaps, CaptureFailure> {
        self.push(ExclusiveCall::DeviceCaps);
        self.device_caps_result.clone()
    }

    fn configure(
        &mut self,
        _claim: &mut DeviceClaim,
        _fmt: StreamFormat,
        _buffer: DeviceBuffer,
    ) -> Result<(DriverFormat, Option<DeviceBufferActual>), ConfigureError> {
        self.push(ExclusiveCall::Configure);
        self.configure_result.clone()
    }

    fn start(
        &mut self,
        _claim: &mut DeviceClaim,
        _render: ExclusiveRender,
        _fmt: StreamFormat,
        _period_frames: u32,
        _shared: Arc<SessionShared>,
    ) -> Result<Box<dyn StreamHandle>, StartError> {
        self.push(ExclusiveCall::Start);
        self.start_result.clone()?;
        Ok(Box::new(FakeExclusiveStream { calls: Arc::clone(&self.calls) }))
    }
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<FakeExclusiveBackend>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;
    use crate::audio::backend::exclusive::{ExclusivePcm, StrictRender};
    use crate::audio::format::BitDepth;
    use crate::audio::render::gain::NoGain;
    use crate::audio::render::out::S16Le;
    use crate::audio::render::pcm::PcmRender;
    use crate::audio::render::tpdf::Tpdf;
    use crate::audio::render::RenderCore;
    use crate::audio::session::{RingPayload, SessionShared};

    /// Минимальный `ExclusiveRender` для вызова `start` в тестах фейка:
    /// пустое ring `i32` без периодического опроса (§6.14, §7.1).
    fn fake_render() -> ExclusiveRender {
        let (_producer, consumer) = rtrb::RingBuffer::<i32>::new(16);
        let core = RenderCore::new(consumer, Arc::new(SessionShared::new()), 2, 0);
        let valid_bits = BitDepth::new(16).expect("16 бит допустимо");
        let render = PcmRender::<i32, S16Le, NoGain>::new(core, RingPayload::ExactI32 { valid_bits }, Tpdf::off());
        ExclusiveRender::Strict(StrictRender::Pcm(ExclusivePcm::new(render)))
    }

    #[test]
    fn fake_exclusive_records_call_order() {
        let mut backend = FakeExclusiveBackend::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");

        assert!(backend.enumerate().is_ok());
        let mut claim = backend.acquire(&dev, Reservation::UnavailableNoServer).expect("acquire ok");
        assert!(backend.device_caps(&mut claim).is_ok());
        let fmt = StreamFormat {
            rate: SampleRate::new(48_000).expect("rate"),
            sample: OutputSampleFormat::S32Le,
            channels: NonZeroU8::new(2).expect("channels"),
        };
        assert!(backend.configure(&mut claim, fmt, DeviceBuffer::Ms40).is_ok());
        let stream =
            backend.start(&mut claim, fake_render(), fmt, 480, Arc::new(SessionShared::new())).expect("start ok");
        stream.stop();

        assert_eq!(
            backend.calls(),
            vec![
                ExclusiveCall::Enumerate,
                ExclusiveCall::Acquire(dev),
                ExclusiveCall::DeviceCaps,
                ExclusiveCall::Configure,
                ExclusiveCall::Start,
                ExclusiveCall::Stop,
            ]
        );
    }

    #[test]
    fn fake_exclusive_scripted_failure() {
        let mut backend = FakeExclusiveBackend::new().with_acquire(Err(CaptureFailure::DeviceDisconnected));
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");

        let result = backend.acquire(&dev, Reservation::UnavailableNoServer);
        assert_eq!(result.err(), Some(CaptureFailure::DeviceDisconnected));
        assert_eq!(backend.calls(), vec![ExclusiveCall::Acquire(dev)]);
    }

    /// `claim_job` не заимствует бэкенд (ADR-01, §6.6): задание выполняется в
    /// отдельном `std::thread`, как в служебном потоке захвата.
    #[test]
    fn claim_job_runs_on_spawned_thread_and_records_acquire() {
        let backend = FakeExclusiveBackend::new();
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let job = backend.claim_job(dev.clone(), "Test DAC".to_string(), Box::new(|| {}));

        let claim = std::thread::spawn(job).join().expect("job thread joined").expect("claim ok");

        assert_eq!(claim.info().device, dev);
        assert_eq!(backend.calls(), vec![ExclusiveCall::Acquire(dev)]);
    }

    #[test]
    fn claim_job_propagates_scripted_acquire_failure() {
        let backend = FakeExclusiveBackend::new().with_acquire(Err(CaptureFailure::DeviceDisconnected));
        let dev: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let job = backend.claim_job(dev.clone(), "Test DAC".to_string(), Box::new(|| {}));

        let result = std::thread::spawn(job).join().expect("job thread joined");

        assert_eq!(result.err(), Some(CaptureFailure::DeviceDisconnected));
        assert_eq!(backend.calls(), vec![ExclusiveCall::Acquire(dev)]);
    }

    #[test]
    fn manual_clock_moves_only_on_advance_and_clones_share_time() {
        let c = ManualClock::new();
        assert_eq!(c.now(), ClockInstant::START);
        let other = c.clone();
        c.advance(Duration::from_millis(250));
        assert_eq!(other.now().since_start(), Duration::from_millis(250));
        other.advance(Duration::MAX);
        assert_eq!(c.now().since_start(), Duration::from_nanos(u64::MAX));
    }
}
