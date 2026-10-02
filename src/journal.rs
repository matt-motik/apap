//! Журнал (ADR-21, §2.9): типизированные записи вместо `eprintln!` по месту.
//! Реализация по умолчанию — `FileJournal` (stderr + `apap.log`), подмена для
//! автотестов — `VecJournal`. В аудио-колбэке журнал не используется.

use crate::persist::{ConfigFile, WorkFile};
use crate::platform::fs::{ReadError, WriteError};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Журнал приложения (ADR-21).
pub trait Journal: Send + Sync {
    fn record(&self, rec: JournalRecord);
    /// Дождаться дозаписи принятых строк не дольше `budget`; `false` — не
    /// успели (ADR-21).
    fn flush(&self, budget: Duration) -> bool;
}

/// Типизированная запись журнала (ADR-21, §2.9); одна запись = одна строка
/// файла. Варианты остальных этапов добавляются вместе с их типами.
#[derive(Clone, PartialEq, Debug)]
pub enum JournalRecord {
    /// Рабочий файл не прочитан.
    ReadFailed { file: WorkFile, err: ReadError },
    /// ТЗ-20: вид ошибки, текст ОС, путь — в `WriteError`.
    WriteFailed { target: WriteTarget, err: WriteError },
    /// Временный файл после неудачной записи не удалён (ADR-4: ошибка
    /// удаления — только в журнал).
    TempRemoveFailed { err: WriteError },
}

/// Что записывалось (§2.9).
#[derive(Clone, PartialEq, Debug)]
pub enum WriteTarget {
    Work(WorkFile),
    BadCopy(ConfigFile),
    Quarantine,
    Export(PathBuf),
}

impl JournalRecord {
    /// Вид записи — второе поле строки журнала (ADR-21).
    pub fn kind(&self) -> &'static str {
        match self {
            JournalRecord::ReadFailed { .. } => "ошибка чтения",
            JournalRecord::WriteFailed { .. } => "ошибка записи",
            JournalRecord::TempRemoveFailed { .. } => "временный файл не удалён",
        }
    }

    /// Текст записи: для ошибок — класс, шаг, текст и код ОС, путь (ТЗ-20).
    pub fn text(&self) -> String {
        match self {
            JournalRecord::ReadFailed { file, err } => format!(
                "{} ({:?}): {}{}; путь {}",
                file.file_name(),
                err.class,
                err.os_text,
                os_code_suffix(err.os_code),
                err.path.display()
            ),
            JournalRecord::WriteFailed { target, err } => {
                format!("{}: {}", target_name(target), write_error_text(err))
            }
            JournalRecord::TempRemoveFailed { err } => write_error_text(err),
        }
    }
}

fn target_name(target: &WriteTarget) -> String {
    match target {
        WriteTarget::Work(f) => f.file_name().to_string(),
        WriteTarget::BadCopy(f) => format!("копия {}.bad", f.work().file_name()),
        WriteTarget::Quarantine => "playlist.m3u.bad".to_string(),
        WriteTarget::Export(p) => format!("экспорт {}", p.display()),
    }
}

fn write_error_text(err: &WriteError) -> String {
    format!(
        "{}, шаг {:?}: {}{}; путь {}",
        err.class.text(),
        err.step,
        err.os_text,
        os_code_suffix(err.os_code),
        err.path.display()
    )
}

fn os_code_suffix(code: Option<i32>) -> String {
    code.map(|c| format!(" (код ОС {c})")).unwrap_or_default()
}

/// Строка журнала: `<UTC ISO 8601 с миллисекундами> <вид записи>: <текст>`
/// (ADR-21).
pub fn format_line(rec: &JournalRecord, now: SystemTime) -> String {
    format!("{} {}: {}", utc_timestamp(now), rec.kind(), rec.text())
}

/// UTC-время ISO 8601 с миллисекундами (`2026-10-02T12:34:56.789Z`) без
/// новых крейтов: дата — `civil_from_days` (ADR-21).
pub fn utc_timestamp(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    let millis = since.subsec_millis();
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// Дни от 1970-01-01 → (год, месяц, день) по пролептическому григорианскому
/// календарю (http://howardhinnant.github.io/date_algorithms.html#civil_from_days).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Подмена для автотестов (ADR-21): накапливает записи.
#[derive(Default)]
pub struct VecJournal {
    records: Mutex<Vec<JournalRecord>>,
}

impl VecJournal {
    pub fn records(&self) -> Vec<JournalRecord> {
        self.records.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Journal for VecJournal {
    fn record(&self, rec: JournalRecord) {
        self.records.lock().unwrap_or_else(|e| e.into_inner()).push(rec);
    }

    fn flush(&self, _budget: Duration) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::{WriteErrorClass, WriteStep};
    use std::path::Path;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_728), (2026, 10, 2));
    }

    #[test]
    fn utc_timestamp_has_millis() {
        let t = UNIX_EPOCH + Duration::from_millis(1_790_944_496_789);
        assert_eq!(utc_timestamp(t), "2026-10-02T12:34:56.789Z");
    }

    #[test]
    fn journal_write_failed_line_has_class_os_text_and_path() {
        let err = WriteError {
            class: WriteErrorClass::NoSpace,
            step: WriteStep::WriteData,
            os_code: Some(28),
            os_text: "No space left on device".into(),
            path: Path::new("/cfg/state.toml.tmp").to_path_buf(),
        };
        let rec = JournalRecord::WriteFailed { target: WriteTarget::Work(WorkFile::State), err };
        let line = format_line(&rec, UNIX_EPOCH);
        assert_eq!(
            line,
            "1970-01-01T00:00:00.000Z ошибка записи: state.toml: нет места на диске, шаг WriteData: \
             No space left on device (код ОС 28); путь /cfg/state.toml.tmp"
        );
    }

    #[test]
    fn journal_vec_collects_records() {
        let j = VecJournal::default();
        let err = WriteError::serialize("bad", Path::new("/x"));
        j.record(JournalRecord::TempRemoveFailed { err: err.clone() });
        assert!(j.flush(Duration::ZERO));
        assert_eq!(j.records(), vec![JournalRecord::TempRemoveFailed { err }]);
    }
}
