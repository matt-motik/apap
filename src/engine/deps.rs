//! Зависимости движка этапа С3 (§2.10, ADR-20, ТЗ-114, ТЗ-115, ТЗ-116):
//! подмножество `EngineDeps` спецификации без `exclusive`, `reservation`,
//! `server_probe`, `os_probe` — эти четыре поля пока живут внутри легаси
//! `Player` (мост С3) и добавятся на этапах С6/С7 вместе с реализующими их
//! трейтами.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::PathBuf;
use std::sync::Arc;

use crate::audio::backend::catalog::DeviceWatcher;
use crate::audio::backend::shared::{CpalSharedBackend, SharedBackend};
use crate::audio::clock::{Clock, MonotonicClock};
use crate::engine::sink::EventSink;
use crate::engine::source::{SourceOpener, SymphoniaSourceOpener};
use crate::engine::spawner::{StdSpawner, ThreadSpawner};
use crate::journal::Journal;
use crate::platform::fs::{os_fs, FsPersistStore, PersistStore};

/// Все внешние зависимости движка (§2.10, ADR-20). В тестах каждая
/// подменяется фейком.
pub struct EngineDeps {
    pub shared: Box<dyn SharedBackend>,
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
}

impl EngineDeps {
    /// Боевой набор зависимостей (§2.10, ADR-20). `dir` и `journal` —
    /// каталог настроек и журнал, построенные только в `main` (ADR-19,
    /// И-Т1): этот модуль их не ищет сам.
    pub fn system(events: Box<dyn EventSink>, dir: PathBuf, journal: Arc<dyn Journal>) -> EngineDeps {
        let (reader, _writer, store_writer) = os_fs(journal);
        EngineDeps {
            shared: Box::new(CpalSharedBackend::default()),
            watcher: crate::platform::devwatch::system_device_watcher(),
            sources: Arc::new(SymphoniaSourceOpener),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(reader, store_writer, dir)),
            events,
            spawner: Arc::new(StdSpawner),
        }
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
    use crate::audio::format::{ChannelLayout, Codec, Container, SampleRate, SourceFormat, SourceKind};
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
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::clone(&fake_source) as Arc<dyn SourceOpener>,
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events: Box::new(VecSink::new()),
            spawner: Arc::new(StdSpawner),
        };

        let format = deps.sources.probe(Path::new("fake.flac")).expect("probe ok");
        assert_eq!(format, pcm_format());
        assert_eq!(fake_source.probes(), 1);

        deps.store.save(crate::platform::fs::EngineFile::BpTests, b"[[test]]\n").expect("save ok");
        assert_eq!(deps.store.load(crate::platform::fs::EngineFile::BpTests).expect("load ok"), Some(b"[[test]]\n".to_vec()));
    }
}
