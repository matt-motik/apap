//! Журнал (ADR-21, §2.9): типизированные записи вместо `eprintln!` по месту.
//! Реализация по умолчанию — `FileJournal` (stderr + `apap.log`), подмена для
//! автотестов — `VecJournal`. В аудио-колбэке журнал не используется.

use crate::audio::backend::exclusive::ProbeError;
use crate::audio::backend::{BackendError, DeviceCaps, HwDeviceId, RateSet};
use crate::core::exit::{ExitReason, ExitReport, TermSignal};
use crate::persist::keys::{LoadNote, LoadNoteKind};
use crate::persist::{ConfigFile, WorkFile};
use crate::platform::fs::{ReadError, WriteError};
use crate::platform::input::TrayHost;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
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
    /// Стартовый `playlist.m3u` не прошёл проверку ОВ-8; причина —
    /// «строка N: …» (ADR-5, ТЗ-21, §6.12).
    PlaylistCorrupt { reason: Box<str> },
    /// Все заметки разбора одного файла — одной записью (ТЗ-5, §6.1).
    LoadNotes { file: ConfigFile, notes: Vec<LoadNote> },
    /// Неразбираемый файл: текст ошибки разбора и итог копии `*.bad`
    /// (ТЗ-6, ТЗ-7, §6.1).
    Unparsable { file: ConfigFile, error: Box<str>, copy: Result<PathBuf, WriteError> },
    /// ТЗ-20: вид ошибки, текст ОС, путь — в `WriteError`.
    WriteFailed { target: WriteTarget, err: WriteError },
    /// Временный файл после неудачной записи не удалён (ADR-4: ошибка
    /// удаления — только в журнал).
    TempRemoveFailed { err: WriteError },
    /// ТЗ-14: итог пути выхода.
    ExitSummary(ExitReport),
    /// Бэкенд winit не выбран, работа продолжается на бэкенде по умолчанию
    /// (§2.9, §6.11, ADR-8).
    BackendFallback { error: Box<str> },
    /// Ошибка таблицы данных модуля ввода `wheel_table.toml`; действует
    /// встроенное правило по умолчанию (§2.9, §6.11, ADR-8).
    WheelTable { error: Box<str> },
    /// Системная настройка направления прокрутки тачпада не прочитана: знак
    /// событий тачпада — как пришёл (ТЗ-36, §6.11 «Направление тачпада»).
    TouchpadDirection { error: Box<str> },
    /// Хост значка трея и способ, которым он определён (§2.9, §6.11, ADR-8).
    TrayHost { host: TrayHost, via: HostSource },
    /// Системное уведомление не доставлено (ошибка D-Bus); окно сообщения
    /// покажется при открытии плеера (ADR-9, ADR-13).
    Notify { error: Box<str> },
    /// `rtkit` отказал в приоритете реального времени потоку вывода;
    /// воспроизведение продолжается на обычном приоритете (§6.8 п. 3,
    /// ТЗ-76).
    RtPriorityDenied { tid: u32, prio: u32 },
    /// Перечисление Exclusive-устройств по сигналу `DeviceWatcher` или
    /// команде «Обновить» (§2.3, ADR-16, ТЗ-105, ТЗ-126,
    /// `DeviceCatalog::refresh_hw`, `src/engine/run.rs::refresh_devices`):
    /// `enumerate_error` — текст ошибки `ExclusiveBackend::enumerate`,
    /// `None` — перечисление прошло (список мог не измениться).
    DeviceEnumerate { enumerate_error: Option<Box<str>> },
    /// Поиск устройства сохранённой точки повторного захвата в обновлённом
    /// каталоге Exclusive-устройств (§6.20 п. 4, ТЗ-126,
    /// `DeviceCatalog::canonical_hw`, `src/engine/run.rs::
    /// try_reacquire_reappeared_device`): `found` — устройство нашлось, и
    /// движок запросил у `lock_step` повторный захват
    /// (`LockInput::DeviceReappeared`, `src/engine/lock.rs`); `false` — пока
    /// не нашлось, решение `lock_step` не запрашивалось.
    DeviceReacquireLookup { device: HwDeviceId, found: bool },
    /// Фоновый опрос caps устройства по выбору в списке вкладки «Аудио»
    /// (`EngineCmd::ProbeDeviceCaps`, §2.3, AM1.0-B38,
    /// `src/engine/run.rs::handle_internal`): устройство, исход
    /// (`ProbeOutcomeText::text` — текст готовится вызывающим через
    /// [`probe_outcome_text`], запись не владеет `DeviceCaps`) и
    /// длительность самого задания (open NONBLOCK → hw_params refine →
    /// close), засечённая служебным потоком `apap-probe` вне RT-пути.
    DeviceCapsProbe { device: HwDeviceId, outcome: ProbeOutcomeText, duration: Duration },
}

/// Исход опроса caps для строки журнала (ADR-21, §2.9, AM1.0-B38):
/// подготавливается [`probe_outcome_text`] из `&Result<DeviceCaps,
/// ProbeError>` до того, как вызывающий (`src/engine/run.rs`) передаст сам
/// результат дальше в `EngineEvent::DeviceCapsProbed` — запись журнала не
/// владеет `DeviceCaps` и не дублирует его клоном.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProbeOutcomeText {
    /// Сводка возможностей: частоты, число форматов, максимум каналов.
    Caps(Box<str>),
    /// Устройство занято другим приложением (`ProbeError::Busy`).
    Busy,
    /// Прочий отказ ALSA при опросе (`ProbeError::Driver`), текст хоста.
    Driver(Box<str>),
}

/// Что записывалось (§2.9).
#[derive(Clone, PartialEq, Debug)]
pub enum WriteTarget {
    Work(WorkFile),
    BadCopy(ConfigFile),
    Quarantine,
    Export(PathBuf),
}

/// Способ определения хоста трея (§2.9, §6.11, ADR-8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostSource {
    /// По владельцу имени `org.kde.StatusNotifierWatcher` на D-Bus.
    DbusOwner,
    /// По переменной окружения `XDG_CURRENT_DESKTOP`.
    XdgCurrentDesktop,
    /// Хост не определён ни одним способом.
    None,
}

impl JournalRecord {
    /// Вид записи — второе поле строки журнала (ADR-21).
    pub fn kind(&self) -> &'static str {
        match self {
            JournalRecord::ReadFailed { .. } => "ошибка чтения",
            JournalRecord::LoadNotes { .. } => "заметки разбора",
            JournalRecord::PlaylistCorrupt { .. } => "повреждённый плейлист",
            JournalRecord::Unparsable { .. } => "неразбираемый файл",
            JournalRecord::WriteFailed { .. } => "ошибка записи",
            JournalRecord::TempRemoveFailed { .. } => "временный файл не удалён",
            JournalRecord::ExitSummary(_) => "итог выхода",
            JournalRecord::BackendFallback { .. } => "бэкенд по умолчанию",
            JournalRecord::WheelTable { .. } => "таблица колёсика",
            JournalRecord::TouchpadDirection { .. } => "направление тачпада",
            JournalRecord::TrayHost { .. } => "хост трея",
            JournalRecord::Notify { .. } => "уведомление не доставлено",
            JournalRecord::RtPriorityDenied { .. } => "приоритет реального времени не получен",
            JournalRecord::DeviceEnumerate { .. } => "перечисление устройств",
            JournalRecord::DeviceReacquireLookup { .. } => "поиск устройства повторного захвата",
            JournalRecord::DeviceCapsProbe { .. } => "опрос caps устройства",
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
            JournalRecord::PlaylistCorrupt { reason } => {
                format!("{}: {reason}", WorkFile::Playlist.file_name())
            }
            JournalRecord::LoadNotes { file, notes } => {
                let body = notes.iter().map(load_note_text).collect::<Vec<_>>().join("; ");
                format!("{}: {body}", file.work().file_name())
            }
            JournalRecord::Unparsable { file, error, copy } => {
                let copy_text = match copy {
                    Ok(path) => format!("копия {}", path.display()),
                    Err(err) => format!("копия не записана: {}", write_error_text(err)),
                };
                format!("{}: {error}; {copy_text}", file.work().file_name())
            }
            JournalRecord::WriteFailed { target, err } => {
                format!("{}: {}", target_name(target), write_error_text(err))
            }
            JournalRecord::TempRemoveFailed { err } => write_error_text(err),
            JournalRecord::ExitSummary(report) => {
                let failed = report
                    .failed
                    .iter()
                    .map(|(f, err)| format!("{} ({})", f.file_name(), write_error_text(err)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "причина {}; записаны: {}; без изменений: {}; запрещены: {}; ошибки: {}; не успели: {}; движок остановлен: {}",
                    exit_reason_text(report.reason),
                    file_list(&report.written),
                    file_list(&report.unchanged),
                    file_list(&report.forbidden),
                    if failed.is_empty() { "—".to_string() } else { failed },
                    file_list(&report.timed_out),
                    if report.engine_ack { "да" } else { "нет" }
                )
            }
            JournalRecord::BackendFallback { error } => format!("winit не выбран: {error}"),
            JournalRecord::WheelTable { error } => {
                format!("wheel_table.toml: {error}; действует встроенное правило по умолчанию")
            }
            JournalRecord::TouchpadDirection { error } => {
                format!("настройка не прочитана, знак как пришёл: {error}")
            }
            JournalRecord::TrayHost { host, via } => {
                format!("{}; способ определения: {}", tray_host_text(*host), host_source_text(*via))
            }
            JournalRecord::Notify { error } => format!("D-Bus: {error}"),
            JournalRecord::RtPriorityDenied { tid, prio } => {
                format!("rtkit MakeThreadRealtime: поток {tid}, приоритет {prio}")
            }
            JournalRecord::DeviceEnumerate { enumerate_error } => match enumerate_error {
                Some(err) => format!("ошибка: {err}"),
                None => "без ошибки".to_string(),
            },
            JournalRecord::DeviceReacquireLookup { device, found } => {
                if *found {
                    format!("{device}: найдено, запрошен повторный захват lock_step")
                } else {
                    format!("{device}: не найдено в перечислении, lock_step не запрошен")
                }
            }
            JournalRecord::DeviceCapsProbe { device, outcome, duration } => {
                let outcome = match outcome {
                    ProbeOutcomeText::Caps(summary) => format!("получены ({summary})"),
                    ProbeOutcomeText::Busy => "занято другим приложением".to_string(),
                    ProbeOutcomeText::Driver(err) => format!("ошибка драйвера: {err}"),
                };
                format!("{device}: {outcome}; {} мс", duration.as_millis())
            }
        }
    }
}

/// Текст ошибки перечисления Exclusive-устройств для строки журнала
/// (§2.9, ADR-24) — используется движком при формировании
/// `JournalRecord::DeviceEnumerate` (`src/engine/run.rs`).
pub fn backend_error_text(err: &BackendError) -> Box<str> {
    match err {
        BackendError::Unavailable(msg) => msg.as_str().into(),
    }
}

/// Исход опроса caps для `JournalRecord::DeviceCapsProbe` (§2.9, AM1.0-B38)
/// — используется движком (`src/engine/run.rs::handle_internal`) из
/// `&Result<DeviceCaps, ProbeError>` до передачи самого результата в
/// `EngineEvent::DeviceCapsProbed`, чтобы запись журнала не владела
/// `DeviceCaps` и не требовала его клона.
pub fn probe_outcome_text(result: &Result<DeviceCaps, ProbeError>) -> ProbeOutcomeText {
    match result {
        Ok(caps) => ProbeOutcomeText::Caps(caps_summary_text(caps).into()),
        Err(ProbeError::Busy) => ProbeOutcomeText::Busy,
        Err(ProbeError::Driver(err)) => ProbeOutcomeText::Driver(err.as_str().into()),
    }
}

/// Короткая сводка возможностей устройства для строки журнала (§2.9,
/// AM1.0-B38): частоты (диапазон или число дискретных значений), число
/// форматов, максимум каналов — не полный список, как в UI
/// (`src/app/device_info_view.rs`), а именно сводка для одной строки.
fn caps_summary_text(caps: &DeviceCaps) -> String {
    let rates = match &caps.rates {
        RateSet::Discrete(rates) => format!("{} частот", rates.len()),
        RateSet::Range { min, max } => format!("{}..{} Гц", min.hz(), max.hz()),
    };
    let channels = caps.channels.iter().copied().max().unwrap_or(0);
    format!("{rates}, форматов {}, каналов до {channels}", caps.formats.len())
}

/// Название хоста трея для строки журнала (§2.9, ADR-8).
fn tray_host_text(host: TrayHost) -> &'static str {
    match host {
        TrayHost::KdePlasma => "KDE Plasma",
        TrayHost::GnomeAppIndicator => "GNOME AppIndicator",
        TrayHost::Unknown => "неизвестен",
    }
}

/// Способ определения хоста трея для строки журнала (§2.9, §6.11, ADR-8).
fn host_source_text(via: HostSource) -> &'static str {
    match via {
        HostSource::DbusOwner => "владелец org.kde.StatusNotifierWatcher (D-Bus)",
        HostSource::XdgCurrentDesktop => "XDG_CURRENT_DESKTOP",
        HostSource::None => "не определён",
    }
}

/// Текст причины выхода для строки журнала (ТЗ-14).
fn exit_reason_text(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::WindowClose => "закрытие окна",
        ExitReason::TrayQuit => "«Выход» в трее",
        ExitReason::Signal(TermSignal::Term) => "сигнал SIGTERM",
        ExitReason::Signal(TermSignal::Int) => "сигнал SIGINT",
        ExitReason::Signal(TermSignal::Hup) => "сигнал SIGHUP",
        ExitReason::WindowsSessionEnd => "завершение сеанса Windows",
        ExitReason::MacosTerminate => "завершение macOS",
    }
}

/// Список рабочих файлов через запятую; пустой список — «—» (ТЗ-14).
fn file_list(files: &[WorkFile]) -> String {
    if files.is_empty() {
        return "—".to_string();
    }
    files.iter().map(|f| f.file_name()).collect::<Vec<_>>().join(", ")
}

/// Текст одной заметки разбора для строки `LoadNotes` (ТЗ-5).
fn load_note_text(note: &LoadNote) -> String {
    match &note.kind {
        LoadNoteKind::Missing => format!("{}: нет значения, подставлено по умолчанию", note.key),
        LoadNoteKind::Invalid { found, allowed } => {
            format!("{}: недопустимое значение {found} (ожидается {allowed}), подставлено по умолчанию", note.key)
        }
        LoadNoteKind::Unknown => format!("{}: неизвестный ключ, исчезнет при записи", note.key),
        LoadNoteKind::Adjusted { reason } => format!("{}: значение изменено ({reason})", note.key),
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

/// Лимит файла журнала: 1 МиБ = 1 048 576 байт (ОВС-4 б; Т-журнал, §10).
pub const JOURNAL_FILE_LIMIT: u64 = 1 << 20;

/// Сообщение потоку `apap-log` (§6.16).
enum LogMsg {
    Line(String),
    Flush(Sender<()>),
}

/// Приёмник журнала по умолчанию (ADR-21, ОВС-4 б): каждая запись — строка в
/// stderr сразу в потоке вызывающего и дозапись в `apap.log` фоновым потоком
/// `apap-log`. Вызывающий только кладёт строку в канал (ТЗ-22).
pub struct FileJournal {
    tx: Sender<LogMsg>,
}

impl FileJournal {
    /// Запускает `apap-log`; ротация — в потоке до первой строки (§6.16).
    /// Если поток не запустился, журнал пишет только в stderr.
    pub fn start(path: PathBuf) -> FileJournal {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("apap-log".into())
            .spawn(move || log_thread(&path, &rx));
        if let Err(e) = spawned {
            eprintln!("журнал: поток apap-log не запущен, файл журнала отключён: {e}");
        }
        FileJournal { tx }
    }
}

impl Journal for FileJournal {
    fn record(&self, rec: JournalRecord) {
        let line = format_line(&rec, SystemTime::now());
        eprintln!("{line}");
        let _ = self.tx.send(LogMsg::Line(line));
    }

    fn flush(&self, budget: Duration) -> bool {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(LogMsg::Flush(ack_tx)).is_err() {
            return false;
        }
        ack_rx.recv_timeout(budget).is_ok()
    }
}

/// Путь ротации `apap.<n>.log` рядом с `apap.log` (ADR-21).
fn rotated_path(path: &Path, n: u32) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.{n}.{}", ext.to_string_lossy()),
        None => format!("{stem}.{n}"),
    };
    path.with_file_name(name)
}

/// Отсутствие файла при ротации — не ошибка.
fn ignore_not_found(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Ротация «один файл на запуск, всего 3 файла» и открытие `apap.log` (ADR-21).
fn rotate_and_open(path: &Path) -> io::Result<File> {
    let first = rotated_path(path, 1);
    let second = rotated_path(path, 2);
    ignore_not_found(fs::remove_file(&second))?;
    ignore_not_found(fs::rename(&first, &second))?;
    ignore_not_found(fs::rename(path, &first))?;
    OpenOptions::new().create(true).append(true).open(path)
}

/// Тело потока `apap-log` (§6.16). Ротация и запись — без `fsync` и без
/// атомарной записи (ОВС-4 б); любая ошибка отключает файл до конца сеанса,
/// stderr продолжает работать.
fn log_thread(path: &Path, rx: &Receiver<LogMsg>) {
    let mut file = match rotate_and_open(path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("журнал: файл {} отключён: {e}", path.display());
            None
        }
    };
    let mut size: u64 = 0;
    for msg in rx {
        match msg {
            LogMsg::Line(line) => {
                let Some(f) = file.as_mut() else { continue };
                let len = u64::try_from(line.len()).unwrap_or(u64::MAX).saturating_add(1);
                if size.saturating_add(len) > JOURNAL_FILE_LIMIT {
                    let note = format!("{} журнал обрезан: достигнут лимит 1 МБ\n", utc_timestamp(SystemTime::now()));
                    let _ = f.write_all(note.as_bytes());
                    file = None;
                    continue;
                }
                match f.write_all(line.as_bytes()).and_then(|()| f.write_all(b"\n")) {
                    Ok(()) => size += len,
                    Err(e) => {
                        eprintln!("журнал: запись в {} отключена: {e}", path.display());
                        file = None;
                    }
                }
            }
            LogMsg::Flush(ack) => {
                // Строки уже записаны: запись без буфера в пространстве пользователя.
                let _ = ack.send(());
            }
        }
    }
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
    fn journal_load_notes_line_lists_each_note() {
        use crate::persist::keys::KeyPath;
        let notes = vec![
            LoadNote { key: KeyPath::new("volume"), kind: LoadNoteKind::Invalid { found: "200".into(), allowed: "целое 0..=100" } },
            LoadNote { key: KeyPath::new("muted"), kind: LoadNoteKind::Missing },
            LoadNote { key: KeyPath::new("bogus"), kind: LoadNoteKind::Unknown },
        ];
        let rec = JournalRecord::LoadNotes { file: ConfigFile::State, notes };
        let line = format_line(&rec, UNIX_EPOCH);
        assert_eq!(
            line,
            "1970-01-01T00:00:00.000Z заметки разбора: state.toml: volume: недопустимое значение 200 \
             (ожидается целое 0..=100), подставлено по умолчанию; muted: нет значения, подставлено по \
             умолчанию; bogus: неизвестный ключ, исчезнет при записи"
        );
    }

    #[test]
    fn journal_playlist_corrupt_line_has_reason() {
        let rec = JournalRecord::PlaylistCorrupt { reason: "строка 3: не абсолютный путь".into() };
        assert_eq!(
            format_line(&rec, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z повреждённый плейлист: playlist.m3u: строка 3: не абсолютный путь"
        );
    }

    #[test]
    fn journal_unparsable_line_has_error_and_copy_path() {
        let rec = JournalRecord::Unparsable {
            file: ConfigFile::Settings,
            error: "ожидался символ `=`".into(),
            copy: Ok(PathBuf::from("/cfg/settings.toml.bad")),
        };
        let line = format_line(&rec, UNIX_EPOCH);
        assert_eq!(
            line,
            "1970-01-01T00:00:00.000Z неразбираемый файл: settings.toml: ожидался символ `=`; \
             копия /cfg/settings.toml.bad"
        );
    }

    #[test]
    fn journal_unparsable_line_reports_failed_copy() {
        let err = WriteError {
            class: WriteErrorClass::NoSpace,
            step: WriteStep::WriteData,
            os_code: Some(28),
            os_text: "No space left on device".into(),
            path: Path::new("/cfg/settings.toml.bad.tmp").to_path_buf(),
        };
        let rec = JournalRecord::Unparsable { file: ConfigFile::Settings, error: "ожидался символ `=`".into(), copy: Err(err) };
        let line = format_line(&rec, UNIX_EPOCH);
        assert_eq!(
            line,
            "1970-01-01T00:00:00.000Z неразбираемый файл: settings.toml: ожидался символ `=`; копия не \
             записана: нет места на диске, шаг WriteData: No space left on device (код ОС 28); путь \
             /cfg/settings.toml.bad.tmp"
        );
    }

    #[test]
    fn journal_exit_summary_line_lists_files_by_outcome() {
        let err = WriteError {
            class: WriteErrorClass::NoSpace,
            step: WriteStep::WriteData,
            os_code: Some(28),
            os_text: "No space left on device".into(),
            path: Path::new("/cfg/settings.toml.tmp").to_path_buf(),
        };
        let mut report = crate::core::exit::ExitReport::new(crate::core::exit::ExitReason::WindowClose);
        report.written = vec![WorkFile::State];
        report.forbidden = vec![WorkFile::Playlist];
        report.failed = vec![(WorkFile::Settings, err)];
        report.engine_ack = true;

        let rec = JournalRecord::ExitSummary(report);
        assert_eq!(rec.kind(), "итог выхода");
        let text = rec.text();
        assert!(text.contains("причина закрытие окна"));
        assert!(text.contains("записаны: state.toml"));
        assert!(text.contains("без изменений: —"));
        assert!(text.contains("запрещены: playlist.m3u"));
        assert!(text.contains("ошибки: settings.toml (нет места на диске"));
        assert!(text.contains("не успели: —"));
        assert!(text.contains("движок остановлен: да"));
    }

    #[test]
    fn journal_backend_fallback_line_has_error() {
        let rec = JournalRecord::BackendFallback { error: "backend not available".into() };
        assert_eq!(
            format_line(&rec, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z бэкенд по умолчанию: winit не выбран: backend not available"
        );
    }

    #[test]
    fn journal_wheel_table_line_has_error() {
        let rec = JournalRecord::WheelTable { error: "строка 4: нет ключа division".into() };
        assert_eq!(
            format_line(&rec, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z таблица колёсика: wheel_table.toml: строка 4: нет ключа division; \
             действует встроенное правило по умолчанию"
        );
    }

    #[test]
    fn journal_touchpad_direction_line_has_error() {
        let rec = JournalRecord::TouchpadDirection { error: "нет значения ScrollDirection".into() };
        assert_eq!(rec.kind(), "направление тачпада");
        assert_eq!(
            format_line(&rec, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z направление тачпада: настройка не прочитана, знак как пришёл: \
             нет значения ScrollDirection"
        );
    }

    #[test]
    fn journal_tray_host_line_has_host_and_source() {
        let cases = [
            (TrayHost::KdePlasma, HostSource::DbusOwner, "KDE Plasma; способ определения: владелец org.kde.StatusNotifierWatcher (D-Bus)"),
            (TrayHost::GnomeAppIndicator, HostSource::XdgCurrentDesktop, "GNOME AppIndicator; способ определения: XDG_CURRENT_DESKTOP"),
            (TrayHost::Unknown, HostSource::None, "неизвестен; способ определения: не определён"),
        ];
        for (host, via, text) in cases {
            let rec = JournalRecord::TrayHost { host, via };
            assert_eq!(rec.kind(), "хост трея");
            assert_eq!(format_line(&rec, UNIX_EPOCH), format!("1970-01-01T00:00:00.000Z хост трея: {text}"));
        }
    }

    #[test]
    fn journal_device_enumerate_line_has_error_or_not() {
        let ok = JournalRecord::DeviceEnumerate { enumerate_error: None };
        assert_eq!(ok.kind(), "перечисление устройств");
        assert_eq!(format_line(&ok, UNIX_EPOCH), "1970-01-01T00:00:00.000Z перечисление устройств: без ошибки");

        let err = JournalRecord::DeviceEnumerate { enumerate_error: Some("no host".into()) };
        assert_eq!(format_line(&err, UNIX_EPOCH), "1970-01-01T00:00:00.000Z перечисление устройств: ошибка: no host");
    }

    #[test]
    fn journal_device_reacquire_lookup_line_has_device_and_outcome() {
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");

        let found = JournalRecord::DeviceReacquireLookup { device: device.clone(), found: true };
        assert_eq!(found.kind(), "поиск устройства повторного захвата");
        assert_eq!(
            format_line(&found, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z поиск устройства повторного захвата: hw:CARD=RU3,DEV=0: найдено, запрошен повторный захват lock_step"
        );

        let missing = JournalRecord::DeviceReacquireLookup { device, found: false };
        assert_eq!(
            format_line(&missing, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z поиск устройства повторного захвата: hw:CARD=RU3,DEV=0: не найдено в перечислении, lock_step не запрошен"
        );
    }

    #[test]
    fn journal_device_caps_probe_line_has_outcome_and_duration() {
        use crate::audio::backend::{AccessKind, OutputApi};
        use crate::audio::format::OutputSampleFormat;
        use smallvec::SmallVec;

        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("hw id");
        let caps = DeviceCaps {
            api: OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            name: "Cayin RU3".into(),
            rates: RateSet::Range {
                min: crate::audio::format::SampleRate::new(44_100).expect("rate"),
                max: crate::audio::format::SampleRate::new(192_000).expect("rate"),
            },
            formats: SmallVec::from_slice(&[OutputSampleFormat::S24Le, OutputSampleFormat::F32]),
            channels: SmallVec::from_slice(&[2]),
        };

        let ok = JournalRecord::DeviceCapsProbe {
            device: device.clone(),
            outcome: probe_outcome_text(&Ok(caps)),
            duration: Duration::from_millis(37),
        };
        assert_eq!(ok.kind(), "опрос caps устройства");
        assert_eq!(
            format_line(&ok, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z опрос caps устройства: hw:CARD=RU3,DEV=0: получены (44100..192000 Гц, \
             форматов 2, каналов до 2); 37 мс"
        );

        let busy = JournalRecord::DeviceCapsProbe {
            device: device.clone(),
            outcome: probe_outcome_text(&Err(ProbeError::Busy)),
            duration: Duration::from_millis(5),
        };
        assert_eq!(
            format_line(&busy, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z опрос caps устройства: hw:CARD=RU3,DEV=0: занято другим приложением; 5 мс"
        );

        let driver = JournalRecord::DeviceCapsProbe {
            device,
            outcome: probe_outcome_text(&Err(ProbeError::Driver("no such device".into()))),
            duration: Duration::from_millis(12),
        };
        assert_eq!(
            format_line(&driver, UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z опрос caps устройства: hw:CARD=RU3,DEV=0: ошибка драйвера: no such \
             device; 12 мс"
        );
    }

    /// Временный каталог теста (не каталог пользователя, ТЗ-49).
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apap-journal-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("test dir");
        dir
    }

    fn note(text: &str) -> JournalRecord {
        JournalRecord::TempRemoveFailed { err: WriteError::serialize(text, Path::new("/x")) }
    }

    fn run_once(path: &Path, text: &str) {
        let j = FileJournal::start(path.to_path_buf());
        j.record(note(text));
        assert!(j.flush(Duration::from_secs(5)));
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn file_journal_rotation_three_files() {
        let dir = test_dir("rotation");
        let log = dir.join("apap.log");
        run_once(&log, "run-1");
        run_once(&log, "run-2");
        run_once(&log, "run-3");
        assert!(read(&log).contains("run-3"));
        assert!(read(&dir.join("apap.1.log")).contains("run-2"));
        assert!(read(&dir.join("apap.2.log")).contains("run-1"));
        run_once(&log, "run-4");
        assert!(read(&log).contains("run-4"));
        assert!(read(&dir.join("apap.1.log")).contains("run-3"));
        assert!(read(&dir.join("apap.2.log")).contains("run-2"));
        assert_eq!(fs::read_dir(&dir).expect("dir").count(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_journal_limit_truncates() {
        let dir = test_dir("limit");
        let log = dir.join("apap.log");
        let j = FileJournal::start(log.clone());
        let chunk = "x".repeat(4096);
        for _ in 0..300 {
            j.record(note(&chunk));
        }
        assert!(j.flush(Duration::from_secs(5)));
        let text = read(&log);
        let last = text.lines().last().unwrap_or_default();
        assert!(last.ends_with(" журнал обрезан: достигнут лимит 1 МБ"), "{last}");
        let before = text.lines().count();
        let body = u64::try_from(text.len() - last.len() - 1).unwrap_or(u64::MAX);
        assert!(body <= JOURNAL_FILE_LIMIT);
        j.record(note("after limit"));
        assert!(j.flush(Duration::from_secs(5)));
        assert_eq!(read(&log).lines().count(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_journal_error_no_window() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_dir("noperm");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).expect("chmod");
        let log = dir.join("apap.log");
        let j = FileJournal::start(log.clone());
        j.record(note("lost"));
        // Ошибка открытия не роняет журнал: flush отвечает, файла нет.
        assert!(j.flush(Duration::from_secs(5)));
        assert!(!log.exists());
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&dir);
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
