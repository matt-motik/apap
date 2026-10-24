//! Shared-бэкенд вывода (§2.3, этап С3). Реализовано только подмножество
//! `enumerate`: `caps`/`start` появятся вместе со `StreamFormat`/
//! `SharedRender`/`StartError` на следующих этапах.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::alsa::configure_and_start_shared;
use super::catalog::hw_device_label;
use super::exclusive::{ExclusiveRender, StartError, StreamHandle};
use super::pw_dump::{self, PwSink};
use super::{BackendError, SharedDeviceId, SharedDeviceInfo};
use crate::audio::format::StreamFormat;
use crate::audio::output::{classify_device, AudioHost, CpalHost, DeviceInfo};
use crate::audio::session::SessionShared;
use crate::i18n::errors as i18n_errors;
use crate::settings::playback::DeviceBuffer;

/// Итог фонового Shared-перечисления (`SharedBackend::enumerate_job`, §2.3,
/// ADR-07 «Уточнение», AM1.0-B37, продолжение): `devices` — тот же
/// агрегированный список, что строит `enumerate()` (базовый список хоста +
/// per-sink узлы PipeWire); `pw_sink_error` — ошибка `PwSinkSource`
/// (`pw-dump` недоступен/не ответил за таймаут/вывод не разобрался),
/// снятая отдельно от `devices`, чтобы вызывающая сторона
/// (`src/engine/run.rs::handle_internal`) могла записать её в `Journal`
/// (`JournalRecord::PwSinkEnumerate`), даже когда сам список при этом не
/// пуст (честность сигнала важнее тишины об ошибке, ADR-07 «Уточнение»).
#[derive(Debug)]
pub struct SharedEnumerateOutcome {
    pub devices: Vec<SharedDeviceInfo>,
    pub pw_sink_error: Option<String>,
}

/// Задание фонового Shared-перечисления, не заимствующее бэкенд (§2.3,
/// AM1.0-B37, продолжение) — по образцу `ClaimJob`/`ProbeJob`
/// (`exclusive.rs`): выполняется в служебном потоке `apap-shared-enum`
/// (`start_shared_enumerate`, `src/engine/run.rs`), т.к. `pw-dump` внутри
/// может занять до 2 с (таймаут `pw_dump.rs`) — цикл движка не ждёт его
/// синхронно.
pub type SharedEnumerateJob = Box<dyn FnOnce() -> Result<SharedEnumerateOutcome, BackendError> + Send>;

/// Перечисление Shared-устройств (§2.3). `caps`/`start` — подмножество
/// полного трейта из спецификации появится на последующих этапах.
pub trait SharedBackend: Send {
    fn enumerate(&mut self) -> Result<Vec<SharedDeviceInfo>, BackendError>;
    /// Задание того же перечисления для служебного потока (§2.3, AM1.0-B37,
    /// продолжение): движок (`src/engine/run.rs::refresh_devices`) вызывает
    /// только этот метод, не `enumerate()`, — `pw-dump` внутри не должен
    /// держать цикл движка.
    fn enumerate_job(&self) -> SharedEnumerateJob;
}

/// Источник per-sink узлов PipeWire (`pw-dump`) для `CpalSharedBackend`
/// (ADR-07 «Уточнение», AM1.0-B37): отдельный трейт, а не прямой вызов
/// `pw_dump::enumerate_pipewire_sinks` внутри `enumerate()`, чтобы
/// `CpalSharedBackend<MockHost>` в существующих тестах не запускал
/// реальный процесс — `with_host` не подставляет источник (`pw_sinks:
/// None`), сценарий агрегации проверяется отдельно через `FakePwSinkSource`
/// (`with_pw_sinks`).
pub trait PwSinkSource: Send {
    /// `Ok` — список узлов (пустой — сервер не PipeWire или узлов и правда
    /// нет); `Err` — `pw-dump` недоступен или не разобрался, текстом для
    /// журнала. Этот шаг (AM1.0-B37, шаг 3) не пишет в `Journal` — нет
    /// вызова в вайтлисте этого файла (заметка плана сессии); `enumerate()`
    /// просто не добавляет per-sink записей, не теряя «Системное по
    /// умолчанию» и не падая целиком (ADR-07 «Уточнение»: честность сигнала
    /// важнее полноты списка).
    fn sinks(&mut self) -> Result<Vec<PwSink>, String>;
    /// Задание того же опроса, не заимствующее источник (AM1.0-B37,
    /// продолжение, по образцу `ClaimJob`/`ProbeJob`): выполняется в
    /// служебном потоке `CpalSharedBackend::enumerate_job`, ошибка — текстом
    /// для `JournalRecord::PwSinkEnumerate` (`src/engine/run.rs`).
    fn job(&self) -> Box<dyn FnOnce() -> Result<Vec<PwSink>, String> + Send>;
}

/// Боевая реализация `PwSinkSource` (ADR-07 «Уточнение», AM1.0-B37):
/// пропускает запрос, если активный аудиосервер не PipeWire (детектор
/// `active_server_name`, AM1.0-B32, `src/audio/reservation/dbus.rs`) — там,
/// где играет Pulse без PipeWire поверх (или вовсе ни один сокет не живой),
/// `pw-dump` не несёт узлов `Audio/Sink`, и лишний запуск процесса на
/// каждое перечисление не нужен.
#[derive(Default)]
pub struct SystemPwSinkSource;

/// Тело `SystemPwSinkSource::sinks`/`job` — свободная функция, общая для
/// обоих методов (AM1.0-B37, продолжение): `job()` строит задание служебного
/// потока без заимствования `self` (источник без состояния), `sinks()`
/// остаётся для существующего синхронного пути `enumerate()`.
fn system_pw_sinks() -> Result<Vec<PwSink>, String> {
    #[cfg(target_os = "linux")]
    {
        if crate::audio::reservation::dbus::active_server_name() != Some("PipeWire") {
            return Ok(Vec::new());
        }
        pw_dump::enumerate_pipewire_sinks().map_err(|err| err.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    Ok(Vec::new())
}

impl PwSinkSource for SystemPwSinkSource {
    fn sinks(&mut self) -> Result<Vec<PwSink>, String> {
        system_pw_sinks()
    }

    fn job(&self) -> Box<dyn FnOnce() -> Result<Vec<PwSink>, String> + Send> {
        Box::new(system_pw_sinks)
    }
}

/// Shared-бэкенд поверх `AudioHost` (cpal) плюс, на Linux, per-sink узлов
/// PipeWire из `pw-dump` (`pw_sinks`, ADR-07 «Уточнение», AM1.0-B37).
/// Фильтрация до выходов аудиосервера (default/PipeWire/Pulse) реализована в
/// `CpalHost::devices()` (ADR-07, AM1.0-B32, фаза 1): `enumerate` передаёт
/// то, что вернул `AudioHost`, и дополняет его per-sink записями из
/// `pw_sinks`, если источник подставлен. На Windows/macOS список
/// `AudioHost` остаётся нефильтрованным, как и легаси-UI (мост С3), и
/// `pw_sinks` для прод-хоста всё равно пуст (`SystemPwSinkSource` на этих
/// платформах не запускает процесс).
pub struct CpalSharedBackend<H: AudioHost + Send = CpalHost> {
    host: H,
    pw_sinks: Option<Box<dyn PwSinkSource>>,
}

impl CpalSharedBackend<CpalHost> {
    pub fn new() -> CpalSharedBackend<CpalHost> {
        CpalSharedBackend { host: CpalHost, pw_sinks: Some(Box::new(SystemPwSinkSource)) }
    }
}

impl Default for CpalSharedBackend<CpalHost> {
    fn default() -> Self {
        CpalSharedBackend::new()
    }
}

impl<H: AudioHost + Send> CpalSharedBackend<H> {
    pub fn with_host(host: H) -> CpalSharedBackend<H> {
        CpalSharedBackend { host, pw_sinks: None }
    }

    /// Подставляет источник per-sink узлов PipeWire явно (тесты: проверка
    /// агрегации `enumerate()` без реального `pw-dump`, AM1.0-B37).
    pub fn with_pw_sinks(mut self, source: Box<dyn PwSinkSource>) -> CpalSharedBackend<H> {
        self.pw_sinks = Some(source);
        self
    }
}

// `Clone + 'static` сверх `AudioHost + Send` нужны только `enumerate_job`
// (AM1.0-B37, продолжение): `host.clone()` снимается сейчас, в замыкание,
// которое выполняется позже в служебном потоке (§2.3). `CpalHost` и
// тестовый `MockHost` ниже — оба дешёво `Clone` (без состояния/маленький
// снимок), отдельный бэкенд не потребовался.
impl<H: AudioHost + Clone + Send + 'static> SharedBackend for CpalSharedBackend<H> {
    fn enumerate(&mut self) -> Result<Vec<SharedDeviceInfo>, BackendError> {
        let devices = self.host.devices();
        let default_name = self.host.default_name();
        let mut infos: Vec<SharedDeviceInfo> = devices
            .into_iter()
            .map(|dev| {
                let is_default = default_name.as_deref() == Some(dev.name.as_str());
                SharedDeviceInfo {
                    id: SharedDeviceId::new(dev.id.clone()),
                    name: dev.name.clone(),
                    is_default,
                    legacy: dev,
                }
            })
            .collect();

        if let Some(source) = self.pw_sinks.as_mut() {
            if let Ok(sinks) = source.sinks() {
                infos.extend(sinks.iter().map(pw_sink_to_shared_device));
            }
            // `Err` намеренно отбрасывается здесь (см. doc-комментарий
            // `PwSinkSource::sinks`): этот синхронный путь больше не
            // вызывается движком (`enumerate_job` ниже — его асинхронная
            // замена, AM1.0-B37, продолжение), запись в журнал для него не
            // нужна.
        }

        Ok(infos)
    }

    /// Асинхронная замена `enumerate()` для служебного потока (§2.3,
    /// AM1.0-B37, продолжение): `host`/`pw_sinks.job()` снимаются сейчас
    /// (клон `H`/боксед-замыкание — обе операции без I/O), а сам опрос —
    /// `host.devices()`/`default_name()` (без процесса, не блокирует) и
    /// `pw_sinks`-задание (может ждать `pw-dump` до 2 с) — выполняется
    /// внутри возвращённого замыкания, в служебном потоке, не здесь.
    fn enumerate_job(&self) -> SharedEnumerateJob {
        let host = self.host.clone();
        let pw_job = self.pw_sinks.as_ref().map(|source| source.job());
        Box::new(move || {
            let devices = host.devices();
            let default_name = host.default_name();
            let mut infos: Vec<SharedDeviceInfo> = devices
                .into_iter()
                .map(|dev| {
                    let is_default = default_name.as_deref() == Some(dev.name.as_str());
                    SharedDeviceInfo {
                        id: SharedDeviceId::new(dev.id.clone()),
                        name: dev.name.clone(),
                        is_default,
                        legacy: dev,
                    }
                })
                .collect();
            let pw_sink_error = match pw_job {
                Some(job) => match job() {
                    Ok(sinks) => {
                        infos.extend(sinks.iter().map(pw_sink_to_shared_device));
                        None
                    }
                    Err(err) => Some(err),
                },
                None => None,
            };
            Ok(SharedEnumerateOutcome { devices: infos, pw_sink_error })
        })
    }
}

/// Идентификатор per-sink записи PipeWire (ADR-07 «Уточнение», AM1.0-B37):
/// открывается крейтом `alsa` (не `cpal`) по этому же имени PCM — следующий
/// шаг сессии (§6, шаг 5).
fn pw_sink_pcm_name(node_name: &str) -> String {
    format!("pipewire:NODE={node_name}")
}

/// Открывает Shared-поток на per-sink узле PipeWire напрямую крейтом `alsa`
/// (ADR-07 «Уточнение», AM1.0-B37, шаг 5), минуя `cpal`: `cpal` не умеет
/// открыть PCM по произвольному параметризованному имени, которое не входит
/// в его собственное перечисление устройств (ADR-07 «Уточнение»). Имя PCM —
/// `pipewire:NODE=<node_name>` (`pw_sink_pcm_name`), тот самый параметризованный
/// PCM, который объявляет ALSA-плагин PipeWire (`pcm.pipewire`,
/// `50-pipewire.conf`); `EXCLUSIVE` у него по умолчанию `false` — честный
/// Shared-выход, не захват (другие приложения звучат на этом узле
/// одновременно), поэтому резервирование `DeviceClaim`/`Reservation`
/// (ADR-08, только Exclusive) здесь не нужно и не выполняется. Конфигурация
/// PCM и писатель-поток `apap-alsa-out` (порог старта, сторож, §6.7) —
/// общая функция `alsa::configure_and_start_shared`, та же, что использует
/// `AlsaExclusiveBackend` — здесь не заводится новый цикл потока.
///
/// Без юнит-теста на реальном вызове (как и `AlsaExclusiveBackend::acquire`/
/// `configure`/`start` в `alsa.rs`): открытие `pipewire:NODE=<node_name>` —
/// живое действие над настоящим PipeWire, и несуществующий `node_name` не
/// гарантирует отказ открытия — проверено вживую на разработческой машине
/// 2026-10-24: WirePlumber может связать поток с узлом по умолчанию вместо
/// отказа (`PW_KEY_TARGET_OBJECT` — подсказка маршрутизации, не жёсткое
/// требование), то есть случайный тестовый запуск этой функции против
/// настоящего PipeWire реально открывает звуковой поток на устройстве
/// пользователя — здесь такого вызова нет и не должно быть (AGENTS.md: не
/// играть звук на устройствах пользователя). Поэтому пропавший узел
/// (шаг 7) распознаётся по списку каталога (`DeviceCatalog`/`pw-dump`), а
/// не по отказу `open_pw_node_stream` — опора на провал открытия была бы
/// небезопасной. Безопасная часть конфигурации (hw_params/sw_params/запуск
/// потока) покрыта тестом `configure_and_start_shared` на ALSA `null`
/// (`alsa.rs`, тот же шаг) — он не достигает ни PipeWire, ни оборудования.
pub fn open_pw_node_stream(
    node_name: &str,
    fmt: StreamFormat,
    buffer: DeviceBuffer,
    render: ExclusiveRender,
    shared: Arc<SessionShared>,
) -> Result<Box<dyn StreamHandle>, StartError> {
    let pcm_name = pw_sink_pcm_name(node_name);
    let pcm = alsa::PCM::new(&pcm_name, alsa::Direction::Playback, false)
        .map_err(|e| StartError(i18n_errors::alsa_shared_pcm_open_failed_text(&pcm_name, &e.to_string())))?;
    configure_and_start_shared(pcm, fmt, buffer, render, shared)
}

/// Подпись per-sink записи (ADR-07 «Уточнение»): `hw_device_label` при
/// соответствии карте ALSA — тот же текст, что показывают списки
/// Оптимального/Строгого режима для той же карты (AM1.0-B31); иначе —
/// `node.description`, а если его тоже нет (узел совсем без описания — в
/// реальном `pw-dump` не встречалось, но парсер `pw_dump::parse_sinks` это
/// допускает для Bluetooth-узлов без `alsa.*`) — `node.name` как последний,
/// не пустой вариант.
fn pw_sink_label(sink: &PwSink) -> String {
    if let Some(card_name) = &sink.alsa_card_name {
        let pcm_name = sink.device_profile_description.as_deref().unwrap_or("");
        return hw_device_label(card_name, pcm_name);
    }
    sink.node_description.clone().unwrap_or_else(|| sink.node_name.clone())
}

/// Собирает `SharedDeviceInfo` для одного узла PipeWire `Audio/Sink`,
/// перечисленного `pw-dump` (ADR-07 «Уточнение», AM1.0-B37). Никогда не
/// отмечается `is_default`: единственная запись по умолчанию — «Системное
/// по умолчанию (PipeWire)» из `AudioHost::devices()`, независимо от того,
/// какой из перечисленных узлов сейчас служит системным sink'ом (ADR-07
/// «Уточнение» этого не требует).
fn pw_sink_to_shared_device(sink: &PwSink) -> SharedDeviceInfo {
    let id = pw_sink_pcm_name(&sink.node_name);
    let label = pw_sink_label(sink);
    let legacy = bridge_device_info(&id, &label);
    SharedDeviceInfo { id: SharedDeviceId::new(id), name: label, is_default: false, legacy }
}

/// Фейк для тестов: отдаёт заранее заданный список устройств или ошибку и
/// считает число вызовов `enumerate`/`enumerate_job` (§2.3). Счётчик —
/// `Arc<AtomicUsize>` (не простой `usize`), т.к. `enumerate_job` строит
/// задание из `&self` (AM1.0-B37, продолжение, по образцу `ClaimJob`/
/// `ProbeJob`) — инкремент происходит внутри замыкания, когда оно
/// фактически выполняется служебным потоком, а не когда задание только
/// построено.
pub struct FakeSharedBackend {
    result: Result<Vec<SharedDeviceInfo>, BackendError>,
    /// Ошибка `pw-dump`, которую несёт `enumerate_job`, помимо `devices` из
    /// `result` (AM1.0-B37, продолжение) — тесты движка, проверяющие запись
    /// в журнал без реального `CpalSharedBackend`/`PwSinkSource`.
    pw_sink_error: Option<String>,
    enumerations: Arc<AtomicUsize>,
}

impl FakeSharedBackend {
    pub fn with_devices(devices: Vec<SharedDeviceInfo>) -> FakeSharedBackend {
        FakeSharedBackend { result: Ok(devices), pw_sink_error: None, enumerations: Arc::new(AtomicUsize::new(0)) }
    }

    pub fn with_error(err: BackendError) -> FakeSharedBackend {
        FakeSharedBackend { result: Err(err), pw_sink_error: None, enumerations: Arc::new(AtomicUsize::new(0)) }
    }

    /// Задаёт ошибку `pw-dump`, несомую `enumerate_job` рядом со списком
    /// устройств из `with_devices` (AM1.0-B37, продолжение): симулирует
    /// отказ/таймаут/неразбор `pw-dump` без реального `CpalSharedBackend`.
    pub fn with_pw_sink_error(mut self, err: impl Into<String>) -> FakeSharedBackend {
        self.pw_sink_error = Some(err.into());
        self
    }

    /// Сколько раз фактически выполнено перечисление (`enumerate` синхронно
    /// или `enumerate_job` — когда служебный поток вызвал построенное
    /// задание).
    pub fn enumerations(&self) -> usize {
        self.enumerations.load(Ordering::SeqCst)
    }
}

impl SharedBackend for FakeSharedBackend {
    fn enumerate(&mut self) -> Result<Vec<SharedDeviceInfo>, BackendError> {
        self.enumerations.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }

    fn enumerate_job(&self) -> SharedEnumerateJob {
        let result = self.result.clone();
        let pw_sink_error = self.pw_sink_error.clone();
        let counter = Arc::clone(&self.enumerations);
        Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            result.map(|devices| SharedEnumerateOutcome { devices, pw_sink_error })
        })
    }
}

/// Нейтральные легаси-поля `DeviceInfo` без реального опроса возможностей
/// устройства: общий код `fake_device` (тесты) и per-sink записей PipeWire
/// (`pw_sink_to_shared_device`, ADR-07 «Уточнение», AM1.0-B37) — обеим
/// сторонам на этом шаге не нужны настоящие частоты/форматы (мост С3 —
/// `DeviceInfo` нужен только для совместимости со списком устройств в UI).
fn bridge_device_info(id: &str, name: &str) -> DeviceInfo {
    let category = classify_device(id, name);
    DeviceInfo {
        id: id.to_string(),
        name: name.to_string(),
        channels: 2,
        sample_rate: 44_100,
        sample_format: cpal::SampleFormat::F32,
        buffer_size: cpal::SupportedBufferSize::Range { min: 64, max: 4096 },
        supported: Vec::new(),
        category,
        supported_rates: vec![44_100],
        supported_formats: vec![cpal::SampleFormat::F32],
    }
}

/// Готовый `SharedDeviceInfo` с нейтральными легаси-полями для тестов без
/// реального cpal-хоста (мост С3 — `DeviceInfo` нужен только для совместимости
/// со списком устройств в UI).
pub fn fake_device(id: &str, name: &str) -> SharedDeviceInfo {
    let legacy = bridge_device_info(id, name);
    SharedDeviceInfo { id: SharedDeviceId::new(id), name: name.to_string(), is_default: false, legacy }
}

/// Фейк `PwSinkSource` для тестов агрегации `enumerate()` (AM1.0-B37):
/// отдаёт заранее заданный результат без реального процесса `pw-dump`.
pub struct FakePwSinkSource(Result<Vec<PwSink>, String>);

impl FakePwSinkSource {
    pub fn with_sinks(sinks: Vec<PwSink>) -> FakePwSinkSource {
        FakePwSinkSource(Ok(sinks))
    }

    pub fn with_error(err: impl Into<String>) -> FakePwSinkSource {
        FakePwSinkSource(Err(err.into()))
    }
}

impl PwSinkSource for FakePwSinkSource {
    fn sinks(&mut self) -> Result<Vec<PwSink>, String> {
        self.0.clone()
    }

    fn job(&self) -> Box<dyn FnOnce() -> Result<Vec<PwSink>, String> + Send> {
        let result = self.0.clone();
        Box::new(move || result)
    }
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<CpalSharedBackend<CpalHost>>();
    assert_send::<FakeSharedBackend>();
    assert_send::<SystemPwSinkSource>();
    assert_send::<FakePwSinkSource>();
    assert_send::<SharedEnumerateOutcome>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct MockHost {
        devices: Vec<DeviceInfo>,
        default_name: Option<String>,
    }

    impl AudioHost for MockHost {
        fn devices(&self) -> Vec<DeviceInfo> {
            self.devices.clone()
        }

        fn default_name(&self) -> Option<String> {
            self.default_name.clone()
        }
    }

    fn mock_device(id: &str, name: &str) -> DeviceInfo {
        fake_device(id, name).legacy
    }

    #[test]
    fn cpal_shared_backend_marks_default() {
        let host = MockHost {
            devices: vec![mock_device("hw:0", "Speakers"), mock_device("hw:1", "HDMI")],
            default_name: Some("HDMI".to_string()),
        };
        let mut backend = CpalSharedBackend::with_host(host);

        let devices = backend.enumerate().expect("enumerate ok");

        assert_eq!(devices.len(), 2);
        assert!(!devices[0].is_default);
        assert_eq!(devices[0].name, "Speakers");
        assert!(devices[1].is_default);
        assert_eq!(devices[1].name, "HDMI");
    }

    #[test]
    fn fake_shared_backend_counts_and_fails() {
        let mut ok_backend = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        assert_eq!(ok_backend.enumerate().expect("ok 1").len(), 1);
        assert_eq!(ok_backend.enumerate().expect("ok 2").len(), 1);
        assert_eq!(ok_backend.enumerations(), 2);

        let mut err_backend = FakeSharedBackend::with_error(BackendError::Unavailable("no host".to_string()));
        match err_backend.enumerate() {
            Err(BackendError::Unavailable(msg)) => assert_eq!(msg, "no host"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert_eq!(err_backend.enumerations(), 1);
    }

    fn motu_sink() -> PwSink {
        PwSink {
            node_name: "alsa_output.usb-MOTU_M2-00.HiFi__Line__sink.3".to_string(),
            node_description: Some("MOTU M2 Analog Stereo".to_string()),
            alsa_card_name: Some("USB Audio".to_string()),
            alsa_card: Some(1),
            device_profile_description: Some("Analog Stereo".to_string()),
        }
    }

    fn bluetooth_sink() -> PwSink {
        PwSink {
            node_name: "bluez_output.AA_BB_CC_DD_EE_FF.1".to_string(),
            node_description: None,
            alsa_card_name: None,
            alsa_card: None,
            device_profile_description: None,
        }
    }

    #[test]
    fn pw_sink_label_with_alsa_card_matches_hw_device_label() {
        assert_eq!(pw_sink_label(&motu_sink()), "USB Audio (Analog Stereo)");
    }

    #[test]
    fn pw_sink_label_without_alsa_card_falls_back_to_node_description() {
        let sink = PwSink {
            node_name: "virtual-sink".to_string(),
            node_description: Some("Virtual Sink".to_string()),
            alsa_card_name: None,
            alsa_card: None,
            device_profile_description: None,
        };
        assert_eq!(pw_sink_label(&sink), "Virtual Sink");
    }

    #[test]
    fn pw_sink_label_without_description_falls_back_to_node_name() {
        let sink = bluetooth_sink();
        assert_eq!(pw_sink_label(&sink), sink.node_name);
    }

    #[test]
    fn pw_sink_to_shared_device_builds_pipewire_node_id_and_never_default() {
        let dev = pw_sink_to_shared_device(&motu_sink());
        assert_eq!(dev.id.as_str(), "pipewire:NODE=alsa_output.usb-MOTU_M2-00.HiFi__Line__sink.3");
        assert_eq!(dev.name, "USB Audio (Analog Stereo)");
        assert!(!dev.is_default);
    }

    #[test]
    fn enumerate_appends_pw_sink_entries_after_base_list() {
        let host = MockHost {
            devices: vec![mock_device("pipewire", "Системное по умолчанию (PipeWire)")],
            default_name: Some("Системное по умолчанию (PipeWire)".to_string()),
        };
        let source = FakePwSinkSource::with_sinks(vec![motu_sink(), bluetooth_sink()]);
        let mut backend = CpalSharedBackend::with_host(host).with_pw_sinks(Box::new(source));

        let devices = backend.enumerate().expect("enumerate ok");

        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].name, "Системное по умолчанию (PipeWire)");
        assert!(devices[0].is_default);
        assert_eq!(devices[1].id.as_str(), "pipewire:NODE=alsa_output.usb-MOTU_M2-00.HiFi__Line__sink.3");
        assert!(!devices[1].is_default);
        assert_eq!(devices[2].id.as_str(), "pipewire:NODE=bluez_output.AA_BB_CC_DD_EE_FF.1");
        assert_eq!(devices[2].name, "bluez_output.AA_BB_CC_DD_EE_FF.1");
    }

    #[test]
    fn enumerate_keeps_base_list_when_pw_sink_source_errors() {
        let host = MockHost {
            devices: vec![mock_device("pipewire", "Системное по умолчанию (PipeWire)")],
            default_name: Some("Системное по умолчанию (PipeWire)".to_string()),
        };
        let source = FakePwSinkSource::with_error("pw-dump did not respond in time");
        let mut backend = CpalSharedBackend::with_host(host).with_pw_sinks(Box::new(source));

        let devices = backend.enumerate().expect("enumerate ok despite pw-dump error");

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Системное по умолчанию (PipeWire)");
    }

    #[test]
    fn enumerate_without_pw_sink_source_is_unchanged() {
        // `with_host` (как в `cpal_shared_backend_marks_default`) не
        // подставляет источник — поведение до AM1.0-B37 не меняется и не
        // запускает реальный `pw-dump` в тестах.
        let host = MockHost { devices: vec![mock_device("hw:0", "Speakers")], default_name: None };
        let mut backend = CpalSharedBackend::with_host(host);
        assert_eq!(backend.enumerate().expect("enumerate ok").len(), 1);
    }

    /// Асинхронная замена `enumerate()` (AM1.0-B37, продолжение): тот же
    /// агрегированный список, `pw_sink_error: None` при успехе `pw-dump`.
    #[test]
    fn enumerate_job_appends_pw_sink_entries_after_base_list() {
        let host = MockHost {
            devices: vec![mock_device("pipewire", "Системное по умолчанию (PipeWire)")],
            default_name: Some("Системное по умолчанию (PipeWire)".to_string()),
        };
        let source = FakePwSinkSource::with_sinks(vec![motu_sink(), bluetooth_sink()]);
        let backend = CpalSharedBackend::with_host(host).with_pw_sinks(Box::new(source));

        let job = backend.enumerate_job();
        let outcome = job().expect("enumerate_job ok");

        assert!(outcome.pw_sink_error.is_none());
        assert_eq!(outcome.devices.len(), 3);
        assert_eq!(outcome.devices[0].name, "Системное по умолчанию (PipeWire)");
        assert!(outcome.devices[0].is_default);
        assert_eq!(outcome.devices[1].id.as_str(), "pipewire:NODE=alsa_output.usb-MOTU_M2-00.HiFi__Line__sink.3");
        assert_eq!(outcome.devices[2].id.as_str(), "pipewire:NODE=bluez_output.AA_BB_CC_DD_EE_FF.1");
    }

    /// Отказ `pw-dump` в `enumerate_job` (AM1.0-B37, продолжение): список —
    /// только базовый (как у синхронного `enumerate()`), но ошибка не
    /// отбрасывается — уходит в `pw_sink_error` для записи в `Journal`
    /// (`src/engine/run.rs::handle_internal`, `JournalRecord::
    /// PwSinkEnumerate`).
    #[test]
    fn enumerate_job_reports_pw_sink_error_and_keeps_base_list() {
        let host = MockHost {
            devices: vec![mock_device("pipewire", "Системное по умолчанию (PipeWire)")],
            default_name: Some("Системное по умолчанию (PipeWire)".to_string()),
        };
        let source = FakePwSinkSource::with_error("pw-dump did not respond in time");
        let backend = CpalSharedBackend::with_host(host).with_pw_sinks(Box::new(source));

        let job = backend.enumerate_job();
        let outcome = job().expect("enumerate_job ok despite pw-dump error");

        assert_eq!(outcome.pw_sink_error.as_deref(), Some("pw-dump did not respond in time"));
        assert_eq!(outcome.devices.len(), 1);
        assert_eq!(outcome.devices[0].name, "Системное по умолчанию (PipeWire)");
    }

    #[test]
    fn enumerate_job_without_pw_sink_source_is_unchanged() {
        let host = MockHost { devices: vec![mock_device("hw:0", "Speakers")], default_name: None };
        let backend = CpalSharedBackend::with_host(host);
        let outcome = backend.enumerate_job()().expect("enumerate_job ok");
        assert_eq!(outcome.devices.len(), 1);
        assert!(outcome.pw_sink_error.is_none());
    }

    /// `enumerate_job` не заимствует `&mut self` — задание выполняется в
    /// служебном потоке, не на вызывающей стороне (по образцу `ClaimJob`/
    /// `ProbeJob`, AM1.0-B37, продолжение); счётчик `FakeSharedBackend`
    /// учитывает фактическое выполнение, а не построение задания.
    #[test]
    fn fake_shared_backend_enumerate_job_counts_on_execution_not_build() {
        let backend = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        let job = backend.enumerate_job();
        assert_eq!(backend.enumerations(), 0, "построение задания не считается выполнением");

        let outcome = job().expect("enumerate_job ok");
        assert_eq!(outcome.devices.len(), 1);
        assert_eq!(backend.enumerations(), 1);
    }

    #[test]
    fn fake_shared_backend_enumerate_job_reports_pw_sink_error() {
        let backend =
            FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]).with_pw_sink_error("timeout");
        let outcome = backend.enumerate_job()().expect("enumerate_job ok");
        assert_eq!(outcome.pw_sink_error.as_deref(), Some("timeout"));
        assert_eq!(outcome.devices.len(), 1);
    }

    #[test]
    fn fake_shared_backend_enumerate_job_propagates_backend_error() {
        let backend = FakeSharedBackend::with_error(BackendError::Unavailable("no host".to_string()));
        match backend.enumerate_job()() {
            Err(BackendError::Unavailable(msg)) => assert_eq!(msg, "no host"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
