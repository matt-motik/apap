//! Тесты движка `apap-engine` на фейках (§7, ADR-20, ТЗ-103, ТЗ-88, ТЗ-105,
//! ТЗ-134, ТЗ-45): неблокирующая отправка команд при медленном `probe`
//! (ТЗ-103), отчёт об отказе запуска потока движка (ТЗ-88), разовое
//! перечисление устройств при старте и по сигналу `DeviceWatcher` (ТЗ-105,
//! ADR-16), отсутствие файлового I/O при `SetModeSettings` (ТЗ-134, И-Р24) и
//! ровно один `ShutdownComplete` при явном `Shutdown` и при `Drop`
//! `EngineHandle` (ТЗ-45). Все тесты работают только через фейковые
//! зависимости — без реального аудио-устройства (ТЗ-114).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::backend::catalog::{DeviceWatcher, FakeDeviceWatcher};
use crate::audio::backend::shared::{fake_device, FakeSharedBackend, SharedBackend};
use crate::audio::backend::{BackendError, SharedDeviceInfo};
use crate::audio::clock::MonotonicClock;
use crate::audio::error::{EngineFault, OpenError};
use crate::audio::format::{ChannelLayout, Codec, Container, SampleRate, SourceFormat, SourceKind};
use crate::engine::deps::EngineDeps;
use crate::engine::messages::{EngineCmd, EngineEvent, TransportState};
use crate::engine::run::{effective_gain, mode_apply_action, BackendCaps, EngineHandle, ModeApplyAction};
use crate::engine::sink::{EventSink, VecSink};
use crate::engine::source::{FakeSource, SourceOpener};
use crate::engine::spawner::{FailingSpawner, StdSpawner, ThreadSpawner};
use crate::platform::fs::{FsPersistStore, MemStore};
use crate::settings::params::ModeSettingsUpdate;
use crate::settings::playback::{BufferMs, ModeKind, ModeSettings, RateFallbackRule, SrcFilter};

fn pcm_format() -> SourceFormat {
    SourceFormat {
        container: Container::Flac,
        codec: Codec::Flac,
        lossy: false,
        rate: SampleRate::new(44_100).expect("rate"),
        layout: ChannelLayout::stereo(),
        kind: SourceKind::Pcm { bits: crate::audio::format::BitDepth::new(16).expect("bits") },
    }
}

/// Собирает `EngineDeps` целиком из переданных фейков (§2.10, ADR-20):
/// каждый тест выбирает только то, что ему нужно подменить, остальное — через
/// `fake_deps`.
fn build_deps(
    sources: Arc<dyn SourceOpener>,
    shared: Box<dyn SharedBackend>,
    watcher: Box<dyn DeviceWatcher>,
    events: Box<dyn EventSink>,
    spawner: Arc<dyn ThreadSpawner>,
    mem: MemStore,
) -> EngineDeps {
    EngineDeps {
        shared,
        watcher,
        sources,
        clock: Box::new(MonotonicClock::new()),
        store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem), PathBuf::from("/cfg"))),
        events,
        spawner,
    }
}

/// Минимальный набор зависимостей: пустой список устройств, тихий `FakeSource`.
fn fake_deps(events: Box<dyn EventSink>, spawner: Arc<dyn ThreadSpawner>) -> EngineDeps {
    build_deps(
        Arc::new(FakeSource::with_format(pcm_format())),
        Box::new(FakeSharedBackend::with_devices(Vec::new())),
        Box::new(FakeDeviceWatcher::new()),
        events,
        spawner,
        MemStore::new(),
    )
}

/// Ждёт, пока `cond` не станет `true`, но не дольше `timeout` (шаг 5 мс
/// между проверками). Нужен для детерминированного ожидания фонового события
/// движка без безусловного `sleep`.
fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Фейковый `SharedBackend`, считающий вызовы `enumerate` извне (через
/// клон), с изменяемым на ходу списком устройств (ТЗ-105): `FakeSharedBackend`
/// не подходит — счётчик недоступен после перемещения в `Box<dyn SharedBackend>`.
#[derive(Clone)]
struct CountingShared {
    state: Arc<Mutex<CountingState>>,
}

struct CountingState {
    devices: Vec<SharedDeviceInfo>,
    enumerations: usize,
}

impl CountingShared {
    fn new(devices: Vec<SharedDeviceInfo>) -> CountingShared {
        CountingShared { state: Arc::new(Mutex::new(CountingState { devices, enumerations: 0 })) }
    }

    fn enumerations(&self) -> usize {
        self.state.lock().expect("lock").enumerations
    }

    fn set_devices(&self, devices: Vec<SharedDeviceInfo>) {
        self.state.lock().expect("lock").devices = devices;
    }
}

impl SharedBackend for CountingShared {
    fn enumerate(&mut self) -> Result<Vec<SharedDeviceInfo>, BackendError> {
        let mut state = self.state.lock().expect("lock");
        state.enumerations += 1;
        Ok(state.devices.clone())
    }
}

/// ТЗ-103: отправка команд в движок (`EngineHandle::send`) не ждёт, пока
/// движок закончит обработку предыдущей — даже если та заняла сотни
/// миллисекунд на медленном `probe`.
#[test]
fn ui_handlers_do_not_block_on_slow_open() {
    let sink = VecSink::new();
    let slow_source: Arc<dyn SourceOpener> =
        Arc::new(FakeSource::with_format(pcm_format()).with_probe_delay(Duration::from_millis(300)));
    let deps = build_deps(
        slow_source,
        Box::new(FakeSharedBackend::with_devices(Vec::new())),
        Box::new(FakeDeviceWatcher::new()),
        Box::new(sink.clone()),
        Arc::new(StdSpawner),
        MemStore::new(),
    );

    let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");

    let start = Instant::now();
    assert!(handle.send(EngineCmd::Open {
        req_gen: 1,
        path: Arc::from(Path::new("fake.flac")),
        start_secs: 0.0,
        autoplay: false,
    }));
    assert!(handle.send(EngineCmd::SetVolume(0.5)));
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(50), "send() заняла {elapsed:?}, хотя probe идёт в движке (ТЗ-103)");

    assert!(handle.send(EngineCmd::Shutdown));
    handle.join();

    let events = sink.take();
    assert!(events.iter().any(|e| matches!(e, EngineEvent::ShutdownComplete)));
}

/// ТЗ-88: отказ запуска самого потока `apap-engine` — `EngineHandle::spawn`
/// возвращает ошибку синхронно, без запуска движка.
///
/// Под-вариант «отказ запуска `apap-decode` при `Open`» не покрывается
/// здесь: в легаси-пути (мост С3) `Player::open` сначала строит реальный
/// `cpal`-поток (`start_engine`/`build_engine_stream`) и только потом
/// запускает поток декодера — то есть дойти до `spawn_decoder` без реального
/// аудио-устройства невозможно, а тесты обязаны работать без него (ТЗ-114).
#[test]
fn spawn_failure_reports_error() {
    let sink = VecSink::new();
    let deps = fake_deps(Box::new(sink), Arc::new(FailingSpawner::new(1)));

    match EngineHandle::spawn(deps) {
        Err(EngineFault::SpawnFailed { what: "apap-engine" }) => {}
        Err(other) => panic!("expected SpawnFailed, got {other:?}"),
        Ok(_) => panic!("expected spawn to fail"),
    }
}

/// ТЗ-105, ADR-16, §6.2 п.3: за время сессии (старт + несколько команд,
/// список устройств не меняется) `enumerate` вызывается ровно один раз, и UI
/// получает ровно одно событие `Devices`.
#[test]
fn device_catalog_enumerated_once() {
    let sink = VecSink::new();
    let shared = CountingShared::new(vec![fake_device("hw:0", "Speakers")]);
    let probe = shared.clone();
    let deps = build_deps(
        Arc::new(FakeSource::with_format(pcm_format())),
        Box::new(shared),
        Box::new(FakeDeviceWatcher::new()),
        Box::new(sink.clone()),
        Arc::new(StdSpawner),
        MemStore::new(),
    );

    let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
    assert!(handle.send(EngineCmd::SetVolume(0.4)));
    assert!(handle.send(EngineCmd::SetVolume(0.6)));
    assert!(handle.send(EngineCmd::Shutdown));
    handle.join();

    assert_eq!(probe.enumerations(), 1);
    let events = sink.take();
    assert_eq!(events.iter().filter(|e| matches!(e, EngineEvent::Devices(_))).count(), 1);
}

/// ТЗ-105, ADR-16: сигнал `DeviceWatcher` вызывает ровно одно повторное
/// перечисление (не больше, даже если между обработкой команд были
/// тайм-ауты цикла), и второе событие `Devices` отражает новый список.
#[test]
fn device_hotplug_single_enumeration() {
    let sink = VecSink::new();
    let shared = CountingShared::new(vec![fake_device("hw:0", "Speakers")]);
    let probe = shared.clone();
    let watcher = FakeDeviceWatcher::new();
    let trigger = watcher.trigger_handle();
    let deps = build_deps(
        Arc::new(FakeSource::with_format(pcm_format())),
        Box::new(shared),
        Box::new(watcher),
        Box::new(sink.clone()),
        Arc::new(StdSpawner),
        MemStore::new(),
    );

    let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");

    // Дождаться первого (стартового) перечисления, прежде чем менять список
    // и дёргать триггер: иначе возможна гонка, при которой первое
    // перечисление уже увидело бы новый список.
    assert!(wait_for(Duration::from_secs(1), || probe.enumerations() >= 1));

    probe.set_devices(vec![fake_device("hw:0", "Speakers"), fake_device("hw:1", "HDMI")]);
    assert!(trigger.fire());
    assert!(handle.send(EngineCmd::SetVolume(0.5)));
    assert!(handle.send(EngineCmd::Shutdown));
    handle.join();

    assert_eq!(probe.enumerations(), 2);
    let events = sink.take();
    let devices: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Devices(catalog) => Some(catalog.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].shared.len(), 1);
    assert_eq!(devices[1].shared.len(), 2);
}

/// ТЗ-134, И-Р24: `SetModeSettings` с `diff: None` (начальная загрузка, ещё
/// нет потока для переоткрытия) применяет параметры в памяти движка без
/// файлового I/O — `MemStore` не фиксирует ни одного вызова.
#[test]
fn set_mode_settings_performs_no_io() {
    let sink = VecSink::new();
    let mem = MemStore::new();
    let deps = build_deps(
        Arc::new(FakeSource::with_format(pcm_format())),
        Box::new(FakeSharedBackend::with_devices(Vec::new())),
        Box::new(FakeDeviceWatcher::new()),
        Box::new(sink.clone()),
        Arc::new(StdSpawner),
        mem.clone(),
    );

    let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
    assert!(handle.send(EngineCmd::SetModeSettings(ModeSettingsUpdate {
        settings: ModeSettings::default(),
        diff: None,
    })));
    assert!(handle.send(EngineCmd::Shutdown));
    handle.join();

    assert!(mem.calls().is_empty(), "SetModeSettings не должен трогать файловую систему (ТЗ-134, И-Р24)");

    // `poll_session` синхронизирует `Transport`/`Position` по состоянию
    // `Player` (§6.1 п.4-5, И-Р13) независимо от `SetModeSettings` — это
    // не файловый I/O, поэтому тест проверяет только отсутствие записи на
    // диск (выше), а не точный список событий.
    let events = sink.take();
    assert!(events
        .iter()
        .all(|e| matches!(e, EngineEvent::Devices(_) | EngineEvent::ShutdownComplete | EngineEvent::Transport { .. } | EngineEvent::Position { .. })));
}

/// ТЗ-45: явный `Shutdown` и `Drop` без него дают ровно один
/// `ShutdownComplete`, и он — последнее событие сессии.
#[test]
fn shutdown_emits_complete() {
    let sink = VecSink::new();
    let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));

    let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
    assert!(handle.send(EngineCmd::SetVolume(0.3)));
    assert!(handle.send(EngineCmd::Shutdown));
    handle.join();

    let events = sink.take();
    let complete_count = events.iter().filter(|e| matches!(e, EngineEvent::ShutdownComplete)).count();
    assert_eq!(complete_count, 1);
    assert!(matches!(events.last(), Some(EngineEvent::ShutdownComplete)));

    let sink2 = VecSink::new();
    let deps2 = fake_deps(Box::new(sink2.clone()), Arc::new(StdSpawner));
    let (handle2, _caps2) = EngineHandle::spawn(deps2).expect("spawn ok");
    // `Drop` сам шлёт `Shutdown` и join'ится синхронно (ТЗ-45) — к моменту
    // возврата из `drop` события уже на месте.
    drop(handle2);

    let events2 = sink2.take();
    assert_eq!(events2.iter().filter(|e| matches!(e, EngineEvent::ShutdownComplete)).count(), 1);
}

/// §6.18, И-Р26: изменение параметра активного режима с `ApplyKind::ReopenAtPosition`
/// (буфер декодирование→вывод) на играющем транспорте требует переоткрытия.
#[test]
fn mode_apply_action_reopen_at_position_for_active_mode_param() {
    let base = ModeSettings::default();
    let mut changed = base.clone();
    changed.compatible.buffer = BufferMs::new(2_000).expect("2000 мс — допустимый буфер плеера");
    let diff = base.diff(&changed);

    assert_eq!(
        mode_apply_action(Some(&diff), ModeKind::Compatible, false),
        ModeApplyAction::ReopenAtPosition
    );
}

/// §6.18, И-Р26: изменение параметра неактивного режима не даёт эффекта —
/// `diff.strongest(active)` его не учитывает, настройки только сохраняются.
#[test]
fn mode_apply_action_store_for_inactive_mode_param() {
    let base = ModeSettings::default();
    let mut changed = base.clone();
    changed.optimal.rate_fallback = RateFallbackRule::Nearest;
    let diff = base.diff(&changed);

    assert_eq!(mode_apply_action(Some(&diff), ModeKind::Compatible, false), ModeApplyAction::Store);
}

/// §6.18, И-Р26: `ApplyKind::SwitchDevice` сильнее `ReopenAtPosition` и
/// поглощает остальные изменения активного режима — одно переоткрытие на
/// новом устройстве вместо двух последовательных.
#[test]
fn mode_apply_action_switch_device_absorbs_other_change() {
    let base = ModeSettings::default();
    let mut changed = base.clone();
    changed.optimal.device = Some(String::from("hw:CARD=X"));
    changed.optimal.src_filter = SrcFilter::VerySlow;
    let diff = base.diff(&changed);

    assert_eq!(
        mode_apply_action(Some(&diff), ModeKind::Optimal, false),
        ModeApplyAction::SwitchDevice
    );
}

/// §6.18, И-Р26: на остановленном транспорте нет сессии для переоткрытия —
/// даже изменение класса `ReopenAtPosition`/`SwitchDevice` даёт только
/// чистую запись параметров (`Store`), как у `apply_active_mode` сегодня.
#[test]
fn mode_apply_action_store_when_stopped_despite_reopen_class() {
    let base = ModeSettings::default();
    let mut changed = base.clone();
    changed.compatible.buffer = BufferMs::new(2_000).expect("2000 мс — допустимый буфер плеера");
    let diff = base.diff(&changed);

    assert_eq!(mode_apply_action(Some(&diff), ModeKind::Compatible, true), ModeApplyAction::Store);
}

/// §6.18, ОВС-19, ADR-23, ТЗ-140: «Фиксировать громкость на 100 %» в
/// Оптимальном режиме убирает ступень громкости — `SetVolume` сохраняет
/// запрошенное значение (см. `Engine::apply_gain`), но `effective_gain`
/// возвращает `1.0` независимо от него. Bit-exact часть этого же теста
/// спецификации (`PcmLocked(ExclusivePcm<NoGain>)`, И-Т21) требует реального
/// рендер-пути и здесь не покрывается (ТЗ-114) — только правило выбора
/// громкости.
#[test]
fn optimal_volume_lock_uses_nogain() {
    let mut settings = ModeSettings::default();
    settings.optimal.volume_lock = true;

    assert_eq!(effective_gain(&settings, ModeKind::Optimal, 0.5, false), 1.0);
}

/// §6.18, ТЗ-41: в Строгом режиме громкости нет вовсе — запрошенное значение
/// не влияет на эффективную громкость.
#[test]
fn strict_mode_ignores_volume() {
    let settings = ModeSettings::default();

    assert_eq!(effective_gain(&settings, ModeKind::Strict, 0.5, false), 1.0);
}

/// ОВС-18: mute гасит сигнал во всех режимах (Совместимый, Оптимальный с
/// фиксацией, Строгий), независимо от правила громкости режима.
#[test]
fn mute_silences_every_mode() {
    let mut locked = ModeSettings::default();
    locked.optimal.volume_lock = true;

    assert_eq!(effective_gain(&ModeSettings::default(), ModeKind::Compatible, 0.5, true), 0.0);
    assert_eq!(effective_gain(&locked, ModeKind::Optimal, 0.5, true), 0.0);
    assert_eq!(effective_gain(&ModeSettings::default(), ModeKind::Strict, 0.5, true), 0.0);
}

/// §6.18, ОВС-20: в Совместимом режиме громкость всегда проходит
/// без изменений — там нет ни фиксации, ни запрета громкости.
#[test]
fn compatible_mode_passes_volume_through() {
    let settings = ModeSettings::default();

    assert_eq!(effective_gain(&settings, ModeKind::Compatible, 0.5, false), 0.5);
}

/// §2.3, ТЗ-109: правило доступности режима — Совместимый доступен всегда,
/// Оптимальный/Строгий — только при `exclusive`.
#[test]
fn modes_availability_from_backend_caps() {
    let no_exclusive = BackendCaps { exclusive: false };
    assert!(no_exclusive.available(ModeKind::Compatible));
    assert!(!no_exclusive.available(ModeKind::Optimal));
    assert!(!no_exclusive.available(ModeKind::Strict));

    let exclusive = BackendCaps { exclusive: true };
    assert!(exclusive.available(ModeKind::Compatible));
    assert!(exclusive.available(ModeKind::Optimal));
    assert!(exclusive.available(ModeKind::Strict));
}

/// §6.18 п. 2, §6.2 п. 5, ТЗ-109, ТЗ-111: сохранённый активный режим
/// недоступен на платформе (здесь — `BackendCaps { exclusive: false }`) —
/// `Open` не захватывает устройство и не открывает файл, а сразу шлёт
/// `OpenFailed(ModeUnavailable)`; сохранённый режим при этом не меняется
/// молча (первый `OpenFailed` всё ещё называет `Strict`, а не какой-то
/// другой режим). После переключения на доступный Совместимый режим
/// повторный `Open` не получает `ModeUnavailable` — дошёл до обычного пути
/// открытия (дальше бридж С3 легаси `Player` требует реального устройства,
/// ТЗ-114, здесь не проверяется).
#[test]
fn saved_unavailable_mode_no_playback() {
    let sink = VecSink::new();
    let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));

    let (handle, _caps) =
        EngineHandle::spawn_with_caps(deps, BackendCaps { exclusive: false }).expect("spawn ok");

    assert!(handle.send(EngineCmd::SetActiveMode(ModeKind::Strict)));
    assert!(handle.send(EngineCmd::Open {
        req_gen: 1,
        path: Arc::from(Path::new("fake.flac")),
        start_secs: 0.0,
        autoplay: true,
    }));
    assert!(handle.send(EngineCmd::SetActiveMode(ModeKind::Compatible)));
    assert!(handle.send(EngineCmd::Open {
        req_gen: 2,
        path: Arc::from(Path::new("fake.flac")),
        start_secs: 0.0,
        autoplay: true,
    }));
    assert!(handle.send(EngineCmd::Shutdown));
    handle.join();

    let events = sink.take();

    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::OpenFailed { req_gen: 1, err: OpenError::ModeUnavailable(ModeKind::Strict) }
    )));
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::Opened { req_gen: 1, .. })));
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::Transport { state: TransportState::Playing })));

    assert!(!events.iter().any(|e| matches!(
        e,
        EngineEvent::OpenFailed { req_gen: 2, err: OpenError::ModeUnavailable(_) }
    )));
}
