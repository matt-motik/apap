//! Зависимости движка этапа С3 (§2.10, ADR-20, ТЗ-114, ТЗ-115, ТЗ-116):
//! подмножество `EngineDeps` спецификации без `reservation` — резервирование
//! карты по ADR-08 вызывается внутри `AlsaExclusiveBackend::acquire`
//! (`reservation::acquire`), отдельного поля под сервис резервирования в
//! `EngineDeps` не заведено.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::PathBuf;
use std::sync::Arc;

use crate::audio::backend::catalog::DeviceWatcher;
use crate::audio::backend::exclusive::ExclusiveBackend;
use crate::audio::backend::os_probe::{OsLevelProbe, SystemOsProbe};
use crate::audio::backend::BackendCaps;
use crate::audio::backend::shared::{CpalSharedBackend, SharedBackend};
use crate::audio::clock::{Clock, MonotonicClock};
use crate::audio::reservation::AudioServerProbe;
use crate::audio::rt::RtPriorityRequester;
#[cfg(target_os = "linux")]
use crate::audio::backend::alsa::AlsaExclusiveBackend;
#[cfg(target_os = "linux")]
use crate::audio::reservation::dbus::SystemServerProbe;
use crate::engine::sink::EventSink;
use crate::engine::source::{SourceOpener, SymphoniaSourceOpener};
use crate::engine::spawner::{StdSpawner, ThreadSpawner};
use crate::journal::Journal;
use crate::platform::fs::{os_fs, FsPersistStore, PersistStore};

/// Все внешние зависимости движка (§2.10, ADR-20). В тестах каждая
/// подменяется фейком.
pub struct EngineDeps {
    pub shared: Box<dyn SharedBackend>,
    /// Эксклюзивный бэкенд: `None` на Windows/macOS, где `ExclusiveBackend`
    /// пока не реализован (§2.10, ADR-20).
    pub exclusive: Option<Box<dyn ExclusiveBackend>>,
    /// Наличие сессионной шины и аудиосервера для ОВ-35 (§2.10, ADR-20).
    pub server_probe: Box<dyn AudioServerProbe>,
    pub watcher: Box<dyn DeviceWatcher>,
    /// Открытие файлов и предпроверка (§2.10).
    pub sources: Arc<dyn SourceOpener>,
    /// Монотонное время, управляемое в тестах (`ClockInstant`).
    pub clock: Box<dyn Clock>,
    /// Результаты «Теста», пометки; настройки и состояние — нет (ОВС-14).
    pub store: Box<dyn PersistStore>,
    pub events: Box<dyn EventSink>,
    /// Отказ запуска потока подменяется (ТЗ-88, §2.10).
    pub spawner: Arc<dyn ThreadSpawner>,
    /// Возможности бэкенда платформы (§2.3, ОВС-12, ТЗ-109); статичны,
    /// тесты подставляют свои.
    pub caps: BackendCaps,
    /// Уровень ОС для этапа «Уровень ОС» (§6.26, ADR-21).
    pub os_probe: Box<dyn OsLevelProbe>,
    /// Запрос RT-приоритета потока вывода Эксклюзивного пути у `rtkit`
    /// (§6.8, ТЗ-99, ТЗ-76); тесты подменяют фейком.
    pub rt: Arc<dyn RtPriorityRequester>,
}

impl EngineDeps {
    /// Боевой набор зависимостей (§2.10, ADR-20). `dir` и `journal` —
    /// каталог настроек и журнал, построенные только в `main` (ADR-19,
    /// И-Т1): этот модуль их не ищет сам.
    pub fn system(events: Box<dyn EventSink>, dir: PathBuf, journal: Arc<dyn Journal>) -> EngineDeps {
        let (reader, _writer, store_writer) = os_fs(journal);
        EngineDeps {
            shared: Box::new(CpalSharedBackend::default()),
            #[cfg(target_os = "linux")]
            exclusive: Some(Box::new(AlsaExclusiveBackend::new())),
            #[cfg(not(target_os = "linux"))]
            exclusive: None,
            #[cfg(target_os = "linux")]
            server_probe: Box::new(SystemServerProbe),
            #[cfg(not(target_os = "linux"))]
            server_probe: Box::new(NoServerProbe),
            watcher: crate::platform::devwatch::system_device_watcher(),
            sources: Arc::new(SymphoniaSourceOpener),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(reader, store_writer, dir)),
            events,
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(SystemOsProbe),
            rt: Arc::new(crate::audio::rt::RtKitRequester),
        }
    }
}

/// Нет сессионной шины ReserveDevice1 и аудиосервера на платформах без
/// `SystemServerProbe` (§2.10, ADR-20, ОВ-35): `exclusive` там же `None`,
/// поэтому значения `AudioServerProbe` этот бэкенд не использует.
#[cfg(not(target_os = "linux"))]
struct NoServerProbe;

#[cfg(not(target_os = "linux"))]
impl AudioServerProbe for NoServerProbe {
    fn session_bus(&self) -> bool {
        false
    }

    fn server_running(&self) -> bool {
        false
    }
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<EngineDeps>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::audio::backend::shared::FakeSharedBackend;
    use crate::audio::backend::catalog::FakeDeviceWatcher;
    use crate::audio::backend::os_probe::FakeOsProbe;
    use crate::audio::testing::{FakeExclusiveBackend, FakeServerProbe};
    use crate::audio::format::{ChannelLayout, Codec, Container, SampleRate, SourceFormat, SourceKind};
    use crate::audio::path::OsStage;
    use crate::engine::sink::VecSink;
    use crate::engine::source::FakeSource;
    use crate::platform::fs::MemStore;
    use std::path::Path;

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

    #[test]
    fn engine_deps_assembles_from_fakes() {
        let mem = MemStore::new();
        let fake_source = Arc::new(FakeSource::with_format(pcm_format()));
        let mut deps = EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            exclusive: Some(Box::new(FakeExclusiveBackend::new())),
            server_probe: Box::new(FakeServerProbe::desktop()),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::clone(&fake_source) as Arc<dyn SourceOpener>,
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(VecSink::new()),
            spawner: Arc::new(StdSpawner),
            caps: BackendCaps::current(),
            os_probe: Box::new(FakeOsProbe::new(OsStage::Unknown)),
            rt: Arc::new(crate::audio::testing::FakeRtRequester::new(None)),
        };

        let format = deps.sources.probe(Path::new("fake.flac")).expect("probe ok");
        assert_eq!(format, pcm_format());
        assert_eq!(fake_source.probes(), 1);

        deps.store.save(crate::platform::fs::EngineFile::BpTests, b"[[test]]\n").expect("save ok");
        assert_eq!(deps.store.load(crate::platform::fs::EngineFile::BpTests).expect("load ok"), Some(b"[[test]]\n".to_vec()));
    }
}
