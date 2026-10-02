//! Хранилище движка (ADR-19, §2.8; ADR-20 (01_audio_modes)): `bp_tests.toml`
//! и `track_marks.toml` через тот же модуль ФС, что и рабочие файлы. Пути
//! строятся внутри из `EngineFile`, поэтому рабочие файлы через хранилище не
//! записать (И-Т1). В тестах — то же `FsPersistStore` поверх `MemStore`.

use super::{FileReader, FileWriter, ReadError, ReadErrorClass, WriteError};
use std::path::PathBuf;
use std::sync::Arc;

/// Хранилище движка (§2.8). Сигнатуры в 01_audio_modes нет — задаётся здесь.
pub trait PersistStore: Send {
    /// Содержимое файла; `None` — файла нет.
    fn load(&self, file: EngineFile) -> Result<Option<Vec<u8>>, ReadError>;
    /// Атомарная долговечная запись (ADR-4).
    fn save(&mut self, file: EngineFile, bytes: &[u8]) -> Result<(), WriteError>;
}

/// Файл хранилища движка (§2.8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineFile {
    BpTests,
    TrackMarks,
}

impl EngineFile {
    pub const fn file_name(self) -> &'static str {
        match self {
            EngineFile::BpTests => "bp_tests.toml",
            EngineFile::TrackMarks => "track_marks.toml",
        }
    }
}

/// `PersistStore` поверх модуля ФС: файлы — в каталоге настроек `dir`.
pub struct FsPersistStore {
    reader: Arc<dyn FileReader>,
    writer: Box<dyn FileWriter>,
    dir: PathBuf,
}

impl FsPersistStore {
    pub fn new(reader: Arc<dyn FileReader>, writer: Box<dyn FileWriter>, dir: PathBuf) -> FsPersistStore {
        FsPersistStore { reader, writer, dir }
    }

    fn path(&self, file: EngineFile) -> PathBuf {
        self.dir.join(file.file_name())
    }
}

impl PersistStore for FsPersistStore {
    fn load(&self, file: EngineFile) -> Result<Option<Vec<u8>>, ReadError> {
        match self.reader.read(&self.path(file)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.class == ReadErrorClass::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn save(&mut self, file: EngineFile, bytes: &[u8]) -> Result<(), WriteError> {
        let path = self.path(file);
        self.writer.write_atomic(&path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MemStore, WriteErrorClass, WriteStep};
    use super::*;
    use std::path::Path;

    #[test]
    fn engine_store_roundtrip_on_mem_store() {
        let mem = MemStore::new();
        let dir = PathBuf::from("/cfg");
        let mut store = FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), dir.clone());
        assert_eq!(store.load(EngineFile::BpTests).expect("missing is ok"), None);
        store.save(EngineFile::BpTests, b"[[test]]\n").expect("save");
        assert_eq!(store.load(EngineFile::BpTests).expect("load").as_deref(), Some(&b"[[test]]\n"[..]));
        assert_eq!(mem.counts(&dir.join("bp_tests.toml")).writes, 1);
        assert_eq!(mem.get(&dir.join("track_marks.toml")), None);
    }

    #[test]
    fn engine_store_reports_errors() {
        let mem = MemStore::new();
        let path = Path::new("/cfg/track_marks.toml");
        mem.fail_read(path, ReadErrorClass::NoAccess);
        mem.fail_write(path, WriteStep::SyncFile, WriteErrorClass::NoSpace, 1);
        let mut store = FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"));
        assert_eq!(store.load(EngineFile::TrackMarks).expect_err("read").class, ReadErrorClass::NoAccess);
        assert_eq!(store.save(EngineFile::TrackMarks, b"x").expect_err("write").class, WriteErrorClass::NoSpace);
    }
}
