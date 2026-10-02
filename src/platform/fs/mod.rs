//! Модуль файловой системы (ADR-4, ADR-6, §2.8, ТЗ-54 п. 2): чтение,
//! атомарная долговечная запись и классификация ошибок записи (ТЗ-18,
//! ТЗ-19, ТЗ-20). Трейты и типы — здесь без `cfg`; системные вызовы по ОС —
//! в `linux.rs`, `macos.rs`, `windows.rs`.

use std::io;
use std::path::{Path, PathBuf};

/// Чтение файлов: загрузка при запуске и поток загрузки плейлиста (§2.8).
pub trait FileReader: Send + Sync {
    fn read(&self, path: &Path) -> Result<Vec<u8>, ReadError>;
}

/// Запись (§2.8). Экземпляр рабочих файлов существует только у их
/// единственного писателя (ADR-1, И-Т1).
pub trait FileWriter: Send {
    /// Атомарная долговечная замена (ТЗ-18, ТЗ-19): последовательность ADR-4
    /// для ОС (§6.7).
    fn write_atomic(&mut self, path: &Path, bytes: &[u8]) -> Result<(), WriteError>;
    /// Переименование с заменой и сбросом каталога (ADR-5): шаги `Replace` и
    /// `SyncDir` последовательности ADR-4.
    fn rename_replace(&mut self, from: &Path, to: &Path) -> Result<(), WriteError>;
}

/// Ошибка чтения: класс, код и текст ОС, путь (§2.8).
#[derive(Clone, PartialEq, Debug)]
pub struct ReadError {
    pub class: ReadErrorClass,
    pub os_code: Option<i32>,
    pub os_text: Box<str>,
    pub path: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadErrorClass {
    NotFound,
    NoAccess,
    Io,
}

impl ReadError {
    /// Ошибка чтения `path` по ошибке ОС.
    pub fn from_io(err: &io::Error, path: &Path) -> ReadError {
        let class = match err.kind() {
            io::ErrorKind::NotFound => ReadErrorClass::NotFound,
            io::ErrorKind::PermissionDenied => ReadErrorClass::NoAccess,
            _ => ReadErrorClass::Io,
        };
        ReadError {
            class,
            os_code: err.raw_os_error(),
            os_text: err.to_string().into_boxed_str(),
            path: path.to_path_buf(),
        }
    }
}

/// Ошибка записи (ТЗ-20): класс — для окна; шаг, код и текст ОС, путь — для
/// журнала (§2.8).
#[derive(Clone, PartialEq, Debug)]
pub struct WriteError {
    pub class: WriteErrorClass,
    pub step: WriteStep,
    pub os_code: Option<i32>,
    pub os_text: Box<str>,
    pub path: PathBuf,
}

impl WriteError {
    /// Ошибка шага `step` записи `path` по ошибке ОС; класс — `classify` (ADR-4).
    pub fn from_io(err: &io::Error, step: WriteStep, path: &Path) -> WriteError {
        WriteError {
            class: classify(err),
            step,
            os_code: err.raw_os_error(),
            os_text: err.to_string().into_boxed_str(),
            path: path.to_path_buf(),
        }
    }

    /// Ошибка сериализации: класс «ошибка ввода-вывода» (таблица ADR-4).
    pub fn serialize(text: &str, path: &Path) -> WriteError {
        WriteError {
            class: WriteErrorClass::Io,
            step: WriteStep::Serialize,
            os_code: None,
            os_text: text.into(),
            path: path.to_path_buf(),
        }
    }
}

/// Класс ошибки записи (ТЗ-20, таблица ADR-4).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum WriteErrorClass {
    NoSpace,
    NoAccess,
    ReadOnlyFs,
    Io,
}

impl WriteErrorClass {
    /// Текст класса для окна Error (ТЗ-20).
    pub const fn text(self) -> &'static str {
        match self {
            WriteErrorClass::NoSpace => "нет места на диске",
            WriteErrorClass::NoAccess => "нет доступа",
            WriteErrorClass::ReadOnlyFs => "файловая система только для чтения",
            WriteErrorClass::Io => "ошибка ввода-вывода",
        }
    }
}

/// Шаг последовательности записи ADR-4 (§6.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteStep {
    Serialize,
    CreateDir,
    OpenTemp,
    WriteData,
    SyncFile,
    Replace,
    SyncDir,
    RemoveTemp,
}

/// Коды ОС Linux → класс (таблица ADR-4): `EPERM`, `EACCES`, `ENOSPC`,
/// `EROFS`, `EDQUOT`.
pub const LINUX_CODES: &[(i32, WriteErrorClass)] = &[
    (1, WriteErrorClass::NoAccess),
    (13, WriteErrorClass::NoAccess),
    (28, WriteErrorClass::NoSpace),
    (30, WriteErrorClass::ReadOnlyFs),
    (122, WriteErrorClass::NoSpace),
];

/// Коды ОС macOS → класс (таблица ADR-4): `EPERM`, `EACCES`, `ENOSPC`,
/// `EROFS`, `EDQUOT`.
pub const MACOS_CODES: &[(i32, WriteErrorClass)] = &[
    (1, WriteErrorClass::NoAccess),
    (13, WriteErrorClass::NoAccess),
    (28, WriteErrorClass::NoSpace),
    (30, WriteErrorClass::ReadOnlyFs),
    (69, WriteErrorClass::NoSpace),
];

/// Коды ОС Windows → класс (таблица ADR-4): `ERROR_ACCESS_DENIED`,
/// `ERROR_WRITE_PROTECT`, `ERROR_SHARING_VIOLATION`, `ERROR_LOCK_VIOLATION`,
/// `ERROR_HANDLE_DISK_FULL`, `ERROR_DISK_FULL`.
pub const WINDOWS_CODES: &[(i32, WriteErrorClass)] = &[
    (5, WriteErrorClass::NoAccess),
    (19, WriteErrorClass::ReadOnlyFs),
    (32, WriteErrorClass::NoAccess),
    (33, WriteErrorClass::NoAccess),
    (39, WriteErrorClass::NoSpace),
    (112, WriteErrorClass::NoSpace),
];

/// Таблица кодов ОС, на которой собран плеер.
#[cfg(target_os = "linux")]
const OS_CODES: &[(i32, WriteErrorClass)] = LINUX_CODES;
#[cfg(target_os = "macos")]
const OS_CODES: &[(i32, WriteErrorClass)] = MACOS_CODES;
#[cfg(target_os = "windows")]
const OS_CODES: &[(i32, WriteErrorClass)] = WINDOWS_CODES;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const OS_CODES: &[(i32, WriteErrorClass)] = &[];

/// Класс по коду ОС из таблицы `table`; `None` — кода в таблице нет.
pub fn class_of_code(table: &[(i32, WriteErrorClass)], code: i32) -> Option<WriteErrorClass> {
    table.iter().find(|(c, _)| *c == code).map(|(_, class)| *class)
}

/// Класс по `io::ErrorKind` (таблица ADR-4); остальное — «ошибка ввода-вывода».
pub fn class_of_kind(kind: io::ErrorKind) -> WriteErrorClass {
    match kind {
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => WriteErrorClass::NoSpace,
        io::ErrorKind::PermissionDenied => WriteErrorClass::NoAccess,
        io::ErrorKind::ReadOnlyFilesystem => WriteErrorClass::ReadOnlyFs,
        _ => WriteErrorClass::Io,
    }
}

/// Классификация по коду ОС, затем по `io::ErrorKind` (таблица ADR-4, ТЗ-20).
pub fn classify(err: &io::Error) -> WriteErrorClass {
    err.raw_os_error()
        .and_then(|code| class_of_code(OS_CODES, code))
        .unwrap_or_else(|| class_of_kind(err.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_os_errors() {
        use WriteErrorClass::*;
        let cases: [(&[(i32, WriteErrorClass)], &[(i32, WriteErrorClass)]); 3] = [
            (LINUX_CODES, &[(28, NoSpace), (122, NoSpace), (13, NoAccess), (1, NoAccess), (30, ReadOnlyFs), (5, Io)]),
            (MACOS_CODES, &[(28, NoSpace), (69, NoSpace), (13, NoAccess), (1, NoAccess), (30, ReadOnlyFs), (122, Io)]),
            (
                WINDOWS_CODES,
                &[(112, NoSpace), (39, NoSpace), (5, NoAccess), (32, NoAccess), (33, NoAccess), (19, ReadOnlyFs), (2, Io)],
            ),
        ];
        for (table, expected) in cases {
            for &(code, class) in expected {
                assert_eq!(class_of_code(table, code).unwrap_or(Io), class, "code {code}");
            }
        }
        assert_eq!(class_of_kind(io::ErrorKind::StorageFull), NoSpace);
        assert_eq!(class_of_kind(io::ErrorKind::QuotaExceeded), NoSpace);
        assert_eq!(class_of_kind(io::ErrorKind::PermissionDenied), NoAccess);
        assert_eq!(class_of_kind(io::ErrorKind::ReadOnlyFilesystem), ReadOnlyFs);
        assert_eq!(class_of_kind(io::ErrorKind::InvalidData), Io);
        // Ошибка без кода ОС классифицируется по виду.
        assert_eq!(classify(&io::Error::from(io::ErrorKind::PermissionDenied)), NoAccess);
        assert_eq!(classify(&io::Error::other("x")), Io);
    }

    #[test]
    fn write_error_class_texts() {
        assert_eq!(WriteErrorClass::NoSpace.text(), "нет места на диске");
        assert_eq!(WriteErrorClass::NoAccess.text(), "нет доступа");
        assert_eq!(WriteErrorClass::ReadOnlyFs.text(), "файловая система только для чтения");
        assert_eq!(WriteErrorClass::Io.text(), "ошибка ввода-вывода");
    }

    #[test]
    fn read_error_classes() {
        let p = Path::new("/x");
        assert_eq!(ReadError::from_io(&io::Error::from(io::ErrorKind::NotFound), p).class, ReadErrorClass::NotFound);
        assert_eq!(
            ReadError::from_io(&io::Error::from(io::ErrorKind::PermissionDenied), p).class,
            ReadErrorClass::NoAccess
        );
        assert_eq!(ReadError::from_io(&io::Error::other("e"), p).class, ReadErrorClass::Io);
    }
}
