//! Хранение настроек, состояния и плейлиста (`docs/02_settings_persistence_v1.0/`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod keys;
pub mod settings_file;
pub mod state_file;
pub mod writer;

use crate::journal::JournalRecord;
use crate::platform::fs::{FileReader, FileWriter, ReadErrorClass, WriteError};
use keys::{FileRead, Parsed};
use settings_file::{parse_settings, Settings};
use state_file::{parse_state, SessionState};

/// Рабочий файл единственного писателя (ТЗ-3, §2.2). Порядок вариантов =
/// порядок записи на пути выхода (ТЗ-14).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum WorkFile {
    Playlist,
    State,
    Settings,
}

impl WorkFile {
    /// Имя файла в каталоге настроек.
    pub const fn file_name(self) -> &'static str {
        match self {
            WorkFile::Playlist => "playlist.m3u",
            WorkFile::State => "state.toml",
            WorkFile::Settings => "settings.toml",
        }
    }
}

/// Файл, разбираемый по ключам (ТЗ-5, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigFile {
    Settings,
    State,
}

impl ConfigFile {
    /// Тот же файл как рабочий файл писателя.
    pub const fn work(self) -> WorkFile {
        match self {
            ConfigFile::Settings => WorkFile::Settings,
            ConfigFile::State => WorkFile::State,
        }
    }
}

/// Пути файлов приложения (§2.2, ADR-19). Строится только в `main` из
/// `settings::config_dir()`; остальной код получает пути отсюда и сам каталог
/// пользователя не ищет, поэтому тесты не трогают его файлы (ТЗ-49).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    /// Каталог настроек.
    pub dir: PathBuf,
    /// `settings.toml`.
    pub settings: PathBuf,
    /// `state.toml`.
    pub state: PathBuf,
    /// `playlist.m3u`.
    pub playlist: PathBuf,
    /// Журнал `apap.log` (ОВС-4 б); ротация — `apap.1.log`, `apap.2.log` рядом.
    pub journal: PathBuf,
}

impl ConfigPaths {
    /// Пути всех файлов внутри каталога `dir`.
    pub fn in_dir(dir: PathBuf) -> ConfigPaths {
        ConfigPaths {
            settings: dir.join(WorkFile::Settings.file_name()),
            state: dir.join(WorkFile::State.file_name()),
            playlist: dir.join(WorkFile::Playlist.file_name()),
            journal: dir.join("apap.log"),
            dir,
        }
    }

    /// Путь рабочего файла.
    pub fn work(&self, f: WorkFile) -> &Path {
        match f {
            WorkFile::Playlist => &self.playlist,
            WorkFile::State => &self.state,
            WorkFile::Settings => &self.settings,
        }
    }

    /// «<имя>.bad» рядом с файлом: одна копия на файл (ТЗ-6, ТЗ-21, НФ-7).
    pub fn bad_copy(&self, f: WorkFile) -> PathBuf {
        self.dir.join(format!("{}.bad", f.file_name()))
    }
}

/// Идентификатор снимка (ТЗ-3, §2.2): растёт с каждым новым снимком,
/// используется писателем для ответов `Written`/`Superseded`/`Failed`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SnapshotId(u64);

impl SnapshotId {
    pub const fn new(n: u64) -> SnapshotId {
        SnapshotId(n)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Готовый к записи снимок рабочего файла (ТЗ-3, §2.2): байты уже
/// сериализованы, писатель не трогает память UI (И-Т1).
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub file: WorkFile,
    pub bytes: Arc<[u8]>,
}

/// Ошибка сериализации — класс «ошибка ввода-вывода» по ТЗ-20 (§2.6).
#[derive(Clone, PartialEq, Debug)]
pub struct SerializeError(pub Box<str>);

/// Эталонный текст файла (ОВ-2): текст последней успешной записи, до неё —
/// прочитанный при запуске. `None` — файла не было или он неразбираемый (§2.2).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ReferenceText(Option<Arc<[u8]>>);

impl ReferenceText {
    /// Эталон из прочитанных байт файла.
    pub fn of(bytes: Arc<[u8]>) -> ReferenceText {
        ReferenceText(Some(bytes))
    }

    /// `true`, если `text` отличается от эталона; пустой эталон отличается от любого текста.
    pub fn differs(&self, text: &[u8]) -> bool {
        match &self.0 {
            Some(bytes) => bytes.as_ref() != text,
            None => true,
        }
    }
}

/// Прочитать файл один раз и классифицировать результат без разбора
/// (ADR-23 шаг 2, §6.1): `NotFound` — файла нет, иная ошибка — `Failed`.
pub fn read_config(reader: &dyn FileReader, path: &Path) -> FileRead {
    match reader.read(path) {
        Ok(bytes) => FileRead::Bytes(Arc::from(bytes)),
        Err(e) if e.class == ReadErrorClass::NotFound => FileRead::Absent,
        Err(e) => FileRead::Failed(e),
    }
}

/// Результат чтения и разбора обоих файлов до создания окна (ADR-23 шаг 2).
pub struct Boot {
    pub settings: Parsed<Settings>,
    pub state: Parsed<SessionState>,
}

/// Читает `settings.toml` и `state.toml` ровно по одному разу каждый (ТЗ-1) и
/// разбирает их; чистая функция, записи не производит (ТЗ-4, ADR-23 шаг 2).
pub fn boot(reader: &dyn FileReader, paths: &ConfigPaths) -> Boot {
    Boot {
        settings: parse_settings(read_config(reader, &paths.settings)),
        state: parse_state(read_config(reader, &paths.state)),
    }
}

/// Итог попытки записать копию `*.bad` одного файла (§2.13, И-Р12, И-Р18).
pub struct BadCopyOutcome {
    pub file: ConfigFile,
    pub result: Result<PathBuf, WriteError>,
}

/// Для каждого неразбираемого файла из `boot` записывает его исходные байты в
/// `<файл>.bad` (не более одной копии на файл — И-Р12) до первой записи этого
/// файла писателем (И-Р18; на этапе С3 копия пишется синхронно в `main` до
/// появления писателя `apap-persist`, §8 С3). Файлы, разобранные успешно или
/// отсутствующие, не порождают записи.
pub fn write_bad_copies(boot: &Boot, writer: &mut dyn FileWriter, paths: &ConfigPaths) -> Vec<BadCopyOutcome> {
    let mut out = Vec::new();
    if let Parsed::Unparsable { original, .. } = &boot.settings {
        out.push(write_bad_copy(writer, paths, ConfigFile::Settings, original));
    }
    if let Parsed::Unparsable { original, .. } = &boot.state {
        out.push(write_bad_copy(writer, paths, ConfigFile::State, original));
    }
    out
}

fn write_bad_copy(
    writer: &mut dyn FileWriter,
    paths: &ConfigPaths,
    file: ConfigFile,
    original: &[u8],
) -> BadCopyOutcome {
    let path = paths.bad_copy(file.work());
    let result = writer.write_atomic(&path, original).map(|()| path);
    BadCopyOutcome { file, result }
}

/// Записи журнала для шага 2 ADR-23 (§6.1): ошибка чтения файла —
/// `JournalRecord::ReadFailed`; успешный разбор с заметками — `LoadNotes`
/// (ТЗ-5). Неразбираемый файл в журнал здесь не попадает — его запись
/// `Unparsable` объединяет текст ошибки разбора с итогом копии `*.bad` и
/// строится из результата `write_bad_copies` (`journal_records_for_bad_copies`).
pub fn journal_records_for_boot(boot: &Boot) -> Vec<JournalRecord> {
    let mut out = Vec::new();
    push_boot_file_records(&mut out, ConfigFile::Settings, &boot.settings);
    push_boot_file_records(&mut out, ConfigFile::State, &boot.state);
    out
}

fn push_boot_file_records<T>(out: &mut Vec<JournalRecord>, file: ConfigFile, parsed: &Parsed<T>) {
    match parsed {
        Parsed::Parsed { notes, .. } if !notes.is_empty() => {
            out.push(JournalRecord::LoadNotes { file, notes: notes.clone() });
        }
        Parsed::ReadFailed { err, .. } => {
            out.push(JournalRecord::ReadFailed { file: file.work(), err: err.clone() });
        }
        _ => {}
    }
}

/// Записи журнала «неразбираемый файл» (ТЗ-6, ТЗ-7, §6.1): текст ошибки
/// разбора из `boot` объединён с итогом записи копии `*.bad` из `outcomes`
/// (одна запись на файл, независимо от того, удалась копия или нет).
pub fn journal_records_for_bad_copies(boot: &Boot, outcomes: &[BadCopyOutcome]) -> Vec<JournalRecord> {
    outcomes
        .iter()
        .map(|o| JournalRecord::Unparsable { file: o.file, error: parse_error_text(o.file, boot), copy: o.result.clone() })
        .collect()
}

fn parse_error_text(file: ConfigFile, boot: &Boot) -> Box<str> {
    match file {
        ConfigFile::Settings => unparsable_error(&boot.settings),
        ConfigFile::State => unparsable_error(&boot.state),
    }
}

fn unparsable_error<T>(parsed: &Parsed<T>) -> Box<str> {
    match parsed {
        Parsed::Unparsable { error, .. } => error.clone(),
        _ => Box::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_paths_in_dir_places_all_files_in_dir() {
        let dir = PathBuf::from("/cfg/music_player");
        let p = ConfigPaths::in_dir(dir.clone());
        assert_eq!(p.dir, dir);
        assert_eq!(p.settings, dir.join("settings.toml"));
        assert_eq!(p.state, dir.join("state.toml"));
        assert_eq!(p.playlist, dir.join("playlist.m3u"));
        assert_eq!(p.journal, dir.join("apap.log"));
    }

    #[test]
    fn config_paths_work_and_bad_copy() {
        let dir = PathBuf::from("/cfg/music_player");
        let p = ConfigPaths::in_dir(dir.clone());
        assert_eq!(p.work(WorkFile::Settings), p.settings.as_path());
        assert_eq!(p.work(WorkFile::State), p.state.as_path());
        assert_eq!(p.work(WorkFile::Playlist), p.playlist.as_path());
        assert_eq!(p.bad_copy(WorkFile::Settings), dir.join("settings.toml.bad"));
        assert_eq!(p.bad_copy(ConfigFile::State.work()), dir.join("state.toml.bad"));
        assert_eq!(p.bad_copy(WorkFile::Playlist), dir.join("playlist.m3u.bad"));
    }

    use crate::platform::fs::MemStore;

    fn paths() -> ConfigPaths {
        ConfigPaths::in_dir(PathBuf::from("/cfg"))
    }

    #[test]
    fn boot_reads_each_file_exactly_once() {
        let fs = MemStore::new();
        let p = paths();
        fs.put(&p.settings, b"theme = \"dark\"\n");
        fs.put(&p.state, b"");
        let _ = boot(&fs, &p);
        assert_eq!(fs.counts(&p.settings).reads, 1);
        assert_eq!(fs.counts(&p.state).reads, 1);
    }

    #[test]
    fn boot_absent_files_use_defaults() {
        let fs = MemStore::new();
        let p = paths();
        let b = boot(&fs, &p);
        assert!(matches!(b.settings, Parsed::Absent { .. }));
        assert!(matches!(b.state, Parsed::Absent { .. }));
    }

    #[test]
    fn boot_read_failure_reports_read_failed() {
        use crate::platform::fs::ReadErrorClass;
        let fs = MemStore::new();
        let p = paths();
        fs.fail_read(&p.settings, ReadErrorClass::NoAccess);
        let b = boot(&fs, &p);
        assert!(matches!(b.settings, Parsed::ReadFailed { .. }));
        let records = journal_records_for_boot(&b);
        assert_eq!(records.len(), 1);
        assert!(matches!(&records[0], JournalRecord::ReadFailed { file: WorkFile::Settings, .. }));
    }

    #[test]
    fn boot_parsed_notes_reports_load_notes() {
        let fs = MemStore::new();
        let p = paths();
        fs.put(&p.state, b"bogus = 1\n");
        let b = boot(&fs, &p);
        assert!(matches!(&b.state, Parsed::Parsed { notes, .. } if !notes.is_empty()));
        let records = journal_records_for_boot(&b);
        assert_eq!(records.len(), 1);
        assert!(matches!(&records[0], JournalRecord::LoadNotes { file: ConfigFile::State, .. }));
    }

    #[test]
    fn journal_records_for_bad_copies_reports_unparsable_with_copy_path() {
        let fs = MemStore::new();
        let p = paths();
        fs.put(&p.settings, b"[[\n");
        let b = boot(&fs, &p);
        let mut writer = fs.clone();
        let outcomes = write_bad_copies(&b, &mut writer, &p);
        let records = journal_records_for_bad_copies(&b, &outcomes);
        assert_eq!(records.len(), 1);
        match &records[0] {
            JournalRecord::Unparsable { file, error, copy } => {
                assert_eq!(*file, ConfigFile::Settings);
                assert!(!error.is_empty());
                assert_eq!(copy.as_deref(), Ok(p.bad_copy(WorkFile::Settings).as_path()));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn write_bad_copies_writes_original_bytes_once_each() {
        let fs = MemStore::new();
        let p = paths();
        fs.put(&p.settings, b"[[\n");
        fs.put(&p.state, b"[[\n");
        let b = boot(&fs, &p);
        assert!(matches!(b.settings, Parsed::Unparsable { .. }));
        assert!(matches!(b.state, Parsed::Unparsable { .. }));
        let mut writer = fs.clone();
        let outcomes = write_bad_copies(&b, &mut writer, &p);
        assert_eq!(outcomes.len(), 2);
        assert_eq!(fs.get(&p.bad_copy(WorkFile::Settings)).as_deref(), Some(&b"[[\n"[..]));
        assert_eq!(fs.get(&p.bad_copy(WorkFile::State)).as_deref(), Some(&b"[[\n"[..]));
        assert_eq!(fs.counts(&p.bad_copy(WorkFile::Settings)).writes, 1);
        assert_eq!(fs.counts(&p.bad_copy(WorkFile::State)).writes, 1);
    }

    #[test]
    fn write_bad_copies_reports_write_failure() {
        use crate::platform::fs::{WriteErrorClass, WriteStep};
        let fs = MemStore::new();
        let p = paths();
        fs.put(&p.settings, b"[[\n");
        let bad = p.bad_copy(WorkFile::Settings);
        fs.fail_write(&bad, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        let b = boot(&fs, &p);
        let mut writer = fs.clone();
        let outcomes = write_bad_copies(&b, &mut writer, &p);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_err());
        let records = journal_records_for_bad_copies(&b, &outcomes);
        assert_eq!(records.len(), 1);
        assert!(matches!(
            &records[0],
            JournalRecord::Unparsable { file: ConfigFile::Settings, copy: Err(_), .. }
        ));
    }

    #[test]
    fn write_bad_copies_no_writes_when_files_are_fine() {
        let fs = MemStore::new();
        let p = paths();
        fs.put(&p.settings, b"theme = \"dark\"\n");
        fs.put(&p.state, b"");
        let b = boot(&fs, &p);
        let mut writer = fs.clone();
        let outcomes = write_bad_copies(&b, &mut writer, &p);
        assert!(outcomes.is_empty());
        assert_eq!(fs.counts(&p.bad_copy(WorkFile::Settings)).writes, 0);
        assert_eq!(fs.counts(&p.bad_copy(WorkFile::State)).writes, 0);
    }
}
