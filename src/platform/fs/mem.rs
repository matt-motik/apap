//! `MemStore` — подмена модуля ФС для автотестов (ADR-19, §2.8, §6.7).
//! Компилируется всегда (ADR-6): тесты бинарника собирают библиотеку без
//! `cfg(test)`. Моделирует последовательность ADR-4: «содержимое на диске»
//! целевого файла меняется только на шаге `Replace`; ошибка или «сбой» на
//! любом шаге оставляют прежнее содержимое и, возможно, `<имя>.tmp`.
//! Задержки записи по `ManualClock` (`delay_write`, `next_wake`, §7.1,
//! ADR-19): `delay_write` откладывает ровно следующую `write_atomic` пути до
//! момента `until` по переданным часам, не удерживая мьютекс хранилища на
//! время ожидания (И-Р11, §2.11) — поток-писатель ждёт поллингом `now()`, а
//! другие потоки свободно читают/пишут и опрашивают `next_wake`.

use super::{temp_path, FileReader, FileWriter, ReadError, ReadErrorClass, WriteError, WriteErrorClass, WriteStep};
use crate::audio::clock::{Clock, ClockInstant};
use crate::audio::testing::ManualClock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Файлы в памяти. Реализует `FileReader` и `FileWriter`; клоны делят одно
/// содержимое (§2.8).
#[derive(Clone, Default)]
pub struct MemStore {
    inner: Arc<Mutex<MemInner>>,
}

/// Счётчики операций по пути (§2.8). `writes` — только успешные
/// `write_atomic` («запись файла» матрицы §5.1 ТЗ); неудачи — `failures`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct OpCounts {
    pub reads: u32,
    pub writes: u32,
    pub renames: u32,
    pub failures: u32,
}

/// Вызов модуля ФС с именем потока — проверка «нет ввода-вывода в
/// UI-потоке» (И-Р11).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FsCall {
    pub thread: Box<str>,
    pub op: FsOp,
    pub path: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsOp {
    Read,
    WriteAtomic,
    RenameReplace,
}

#[derive(Default)]
struct MemInner {
    files: HashMap<PathBuf, Vec<u8>>,
    counts: HashMap<PathBuf, OpCounts>,
    write_faults: HashMap<PathBuf, Fault>,
    read_faults: HashMap<PathBuf, ReadErrorClass>,
    calls: Vec<FsCall>,
    delays: HashMap<PathBuf, Delay>,
}

/// Отложенная (одноразовая) следующая запись пути (§2.11, ADR-19).
#[derive(Clone)]
struct Delay {
    until: ClockInstant,
    clock: ManualClock,
}

/// Внедрённая неудача шага записи пути.
#[derive(Clone, Copy)]
struct Fault {
    step: WriteStep,
    class: WriteErrorClass,
    times: u32,
    /// «Сбой» (процесс прерван): временный файл не удаляется.
    crash: bool,
}

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::default()
    }

    /// Положить файл «на диск» без учёта в счётчиках.
    pub fn put(&self, path: &Path, bytes: &[u8]) {
        self.lock().files.insert(path.to_path_buf(), bytes.to_vec());
    }

    /// «Содержимое на диске»: то, что пережило бы сбой (ТЗ-16, ТЗ-18).
    pub fn get(&self, path: &Path) -> Option<Vec<u8>> {
        self.lock().files.get(path).cloned()
    }

    pub fn counts(&self, path: &Path) -> OpCounts {
        self.lock().counts.get(path).copied().unwrap_or_default()
    }

    /// Внедрить ошибку на шаге последовательности ADR-4 для пути; `times` —
    /// сколько раз подряд.
    pub fn fail_write(&self, path: &Path, step: WriteStep, class: WriteErrorClass, times: u32) {
        let fault = Fault { step, class, times, crash: false };
        self.lock().write_faults.insert(path.to_path_buf(), fault);
    }

    /// Один «сбой» (процесс прерван) перед шагом `step` записи пути: шаги до
    /// него выполнены, временный файл остаётся (ТЗ-18).
    pub fn crash_write(&self, path: &Path, step: WriteStep) {
        let fault = Fault { step, class: WriteErrorClass::Io, times: 1, crash: true };
        self.lock().write_faults.insert(path.to_path_buf(), fault);
    }

    pub fn fail_read(&self, path: &Path, class: ReadErrorClass) {
        self.lock().read_faults.insert(path.to_path_buf(), class);
    }

    /// Все вызовы с именем потока (И-Р11).
    pub fn calls(&self) -> Vec<FsCall> {
        self.lock().calls.clone()
    }

    /// Задержать ровно следующую `write_atomic` пути (одноразово) до
    /// момента `until` по часам `clock` (§2.11, ADR-19). Ожидание — в самой
    /// `write_atomic`, без удержания мьютекса хранилища.
    pub fn delay_write(&self, path: &Path, until: ClockInstant, clock: ManualClock) {
        self.lock().delays.insert(path.to_path_buf(), Delay { until, clock });
    }

    /// Ближайший момент среди ещё не отработанных задержек (§2.11): отсчёт
    /// идёт от регистрации `delay_write`, даже если сама запись не началась.
    pub fn next_wake(&self) -> Option<ClockInstant> {
        self.lock().delays.values().map(|d| d.until).min()
    }

    /// Подождать (без мьютекса хранилища) зарегистрированную задержку пути,
    /// если она есть; снять её по завершении ожидания (одноразово).
    fn wait_for_delay(&self, path: &Path) {
        let Some(delay) = self.lock().delays.get(path).cloned() else {
            return;
        };
        while delay.clock.now() < delay.until {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.lock().delays.remove(path);
    }

    fn lock(&self) -> MutexGuard<'_, MemInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl MemInner {
    fn call(&mut self, op: FsOp, path: &Path) {
        let thread = std::thread::current().name().unwrap_or("<unnamed>").into();
        self.calls.push(FsCall { thread, op, path: path.to_path_buf() });
    }

    fn count(&mut self, path: &Path) -> &mut OpCounts {
        self.counts.entry(path.to_path_buf()).or_default()
    }

    /// Внедрённая неудача шага `step` записи `path`, если она назначена.
    fn take_fault(&mut self, path: &Path, step: WriteStep) -> Option<Fault> {
        let fault = *self.write_faults.get(path).filter(|f| f.step == step)?;
        if fault.times <= 1 {
            self.write_faults.remove(path);
        } else if let Some(f) = self.write_faults.get_mut(path) {
            f.times -= 1;
        }
        Some(fault)
    }

    /// Выполнить шаг: при внедрённой неудаче — ошибка; при обычной ошибке
    /// временный файл удаляется (ADR-4), при «сбое» — остаётся.
    fn step(&mut self, path: &Path, tmp: Option<&Path>, step: WriteStep) -> Result<(), WriteError> {
        let Some(fault) = self.take_fault(path, step) else {
            return Ok(());
        };
        if !fault.crash {
            if let Some(tmp) = tmp {
                self.files.remove(tmp);
            }
        }
        self.count(path).failures += 1;
        Err(WriteError {
            class: fault.class,
            step,
            os_code: None,
            os_text: if fault.crash { "сбой (MemStore)" } else { "внедрённая ошибка (MemStore)" }.into(),
            path: path.to_path_buf(),
        })
    }
}

impl FileReader for MemStore {
    fn read(&self, path: &Path) -> Result<Vec<u8>, ReadError> {
        let mut inner = self.lock();
        inner.call(FsOp::Read, path);
        inner.count(path).reads += 1;
        let class = match inner.read_faults.get(path) {
            Some(class) => *class,
            None => match inner.files.get(path) {
                Some(bytes) => return Ok(bytes.clone()),
                None => ReadErrorClass::NotFound,
            },
        };
        Err(ReadError { class, os_code: None, os_text: "MemStore".into(), path: path.to_path_buf() })
    }
}

impl FileWriter for MemStore {
    fn write_atomic(&mut self, path: &Path, bytes: &[u8]) -> Result<(), WriteError> {
        self.wait_for_delay(path);
        let tmp = temp_path(path);
        let mut inner = self.lock();
        inner.call(FsOp::WriteAtomic, path);
        inner.step(path, None, WriteStep::CreateDir)?;
        inner.step(path, None, WriteStep::OpenTemp)?;
        inner.files.insert(tmp.clone(), Vec::new());
        inner.step(path, Some(&tmp), WriteStep::WriteData)?;
        inner.files.insert(tmp.clone(), bytes.to_vec());
        inner.step(path, Some(&tmp), WriteStep::SyncFile)?;
        inner.step(path, Some(&tmp), WriteStep::Replace)?;
        let data = inner.files.remove(&tmp).unwrap_or_default();
        inner.files.insert(path.to_path_buf(), data);
        inner.step(path, None, WriteStep::SyncDir)?;
        inner.count(path).writes += 1;
        Ok(())
    }

    fn rename_replace(&mut self, from: &Path, to: &Path) -> Result<(), WriteError> {
        let mut inner = self.lock();
        inner.call(FsOp::RenameReplace, from);
        inner.step(to, None, WriteStep::Replace)?;
        let Some(data) = inner.files.remove(from) else {
            inner.count(to).failures += 1;
            return Err(WriteError {
                class: WriteErrorClass::Io,
                step: WriteStep::Replace,
                os_code: None,
                os_text: "нет исходного файла (MemStore)".into(),
                path: to.to_path_buf(),
            });
        };
        inner.files.insert(to.to_path_buf(), data);
        inner.step(to, None, WriteStep::SyncDir)?;
        inner.count(from).renames += 1;
        inner.count(to).renames += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEPS: [WriteStep; 6] = [
        WriteStep::CreateDir,
        WriteStep::OpenTemp,
        WriteStep::WriteData,
        WriteStep::SyncFile,
        WriteStep::Replace,
        WriteStep::SyncDir,
    ];

    /// ТЗ-18: прерывание на каждом шаге → файл целиком равен прежней или
    /// новой версии; оставшийся `.tmp` не читается и не мешает следующей
    /// записи.
    #[test]
    fn atomic_write_interrupted_at_each_step() {
        let path = Path::new("/cfg/state.toml");
        let tmp = temp_path(path);
        for crash in [false, true] {
            for step in STEPS {
                let mut fs = MemStore::new();
                fs.put(path, b"old = 1\n");
                if crash {
                    fs.crash_write(path, step);
                } else {
                    fs.fail_write(path, step, WriteErrorClass::NoSpace, 1);
                }
                let err = fs.write_atomic(path, b"new = 2\n").expect_err("injected");
                assert_eq!(err.step, step);
                let on_disk = fs.read(path).expect("target readable");
                let replaced = matches!(step, WriteStep::SyncDir);
                let expected: &[u8] = if replaced { b"new = 2\n" } else { b"old = 1\n" };
                assert_eq!(on_disk, expected, "step {step:?} crash {crash}");
                if !crash {
                    assert_eq!(fs.get(&tmp), None, "tmp removed after error at {step:?}");
                }
                assert_eq!(fs.counts(path).writes, 0);
                assert_eq!(fs.counts(path).failures, 1);
                // Следующая запись проходит поверх оставшегося .tmp.
                fs.write_atomic(path, b"next = 3\n").expect("next write");
                assert_eq!(fs.get(path).as_deref(), Some(&b"next = 3\n"[..]));
                assert_eq!(fs.get(&tmp), None);
                assert_eq!(fs.counts(path).writes, 1);
            }
        }
    }

    #[test]
    fn mem_store_fail_write_times_and_counts() {
        let path = Path::new("/cfg/settings.toml");
        let mut fs = MemStore::new();
        fs.fail_write(path, WriteStep::WriteData, WriteErrorClass::ReadOnlyFs, 2);
        assert_eq!(fs.write_atomic(path, b"a").expect_err("1").class, WriteErrorClass::ReadOnlyFs);
        assert!(fs.write_atomic(path, b"a").is_err());
        fs.write_atomic(path, b"a").expect("third succeeds");
        assert_eq!(fs.counts(path), OpCounts { reads: 0, writes: 1, renames: 0, failures: 2 });
        assert_eq!(fs.get(path).as_deref(), Some(&b"a"[..]));
    }

    #[test]
    fn mem_store_read_rename_and_calls() {
        let a = Path::new("/cfg/playlist.m3u");
        let b = Path::new("/cfg/playlist.m3u.bad");
        let mut fs = MemStore::new();
        assert_eq!(fs.read(a).expect_err("missing").class, ReadErrorClass::NotFound);
        fs.put(a, b"x");
        fs.fail_read(a, ReadErrorClass::NoAccess);
        assert_eq!(fs.read(a).expect_err("fault").class, ReadErrorClass::NoAccess);
        fs.rename_replace(a, b).expect("rename");
        assert_eq!(fs.get(a), None);
        assert_eq!(fs.get(b).as_deref(), Some(&b"x"[..]));
        assert_eq!(fs.counts(a).reads, 2);
        assert_eq!(fs.counts(a).renames, 1);
        // Клон делит содержимое.
        let clone = fs.clone();
        assert_eq!(clone.get(b).as_deref(), Some(&b"x"[..]));
        let ops: Vec<FsOp> = fs.calls().iter().map(|c| c.op).collect();
        assert_eq!(ops, vec![FsOp::Read, FsOp::Read, FsOp::RenameReplace]);
    }

    /// §2.11, ADR-19: отложенная запись не завершается, пока `ManualClock`
    /// не дойдёт до `until`, и не удерживает мьютекс хранилища на время
    /// ожидания (поток-писатель ждёт поллингом, параллельно доступен `get`).
    #[test]
    fn delay_write_blocks_until_clock_advances() {
        let path = Path::new("/cfg/delayed.toml");
        let clock = ManualClock::new();
        let fs = MemStore::new();
        let until = ClockInstant::START.saturating_add(Duration::from_millis(50));
        fs.delay_write(path, until, clock.clone());

        let (tx, rx) = std::sync::mpsc::channel();
        let mut writer = fs.clone();
        let handle = std::thread::spawn(move || {
            writer.write_atomic(path, b"value").expect("delayed write");
            tx.send(()).expect("send done");
        });

        std::thread::sleep(Duration::from_millis(20));
        assert!(rx.try_recv().is_err(), "write must not complete before delay elapses");
        assert_eq!(fs.get(path), None, "fs mutex is free; write just hasn't finished yet");

        clock.advance(Duration::from_millis(60));
        rx.recv_timeout(Duration::from_secs(2)).expect("write completes after delay");
        handle.join().expect("writer thread");
        assert_eq!(fs.get(path).as_deref(), Some(&b"value"[..]));
    }

    /// §2.11: `next_wake` — минимум среди ещё не отработанных задержек; по
    /// мере их освобождения выбывают по одной, в конце — `None`.
    #[test]
    fn next_wake_returns_earliest_pending_delay() {
        let a = Path::new("/cfg/a.toml");
        let b = Path::new("/cfg/b.toml");
        let clock = ManualClock::new();
        let fs = MemStore::new();
        let until_a = ClockInstant::START.saturating_add(Duration::from_millis(100));
        let until_b = ClockInstant::START.saturating_add(Duration::from_millis(30));
        fs.delay_write(a, until_a, clock.clone());
        fs.delay_write(b, until_b, clock.clone());
        assert_eq!(fs.next_wake(), Some(until_b));

        clock.advance(Duration::from_millis(30));
        fs.clone().write_atomic(b, b"b").expect("write b");
        assert_eq!(fs.next_wake(), Some(until_a));

        clock.advance(Duration::from_millis(70));
        fs.clone().write_atomic(a, b"a").expect("write a");
        assert_eq!(fs.next_wake(), None);
    }

    /// §2.11: `delay_write` откладывает ровно одну, следующую запись пути;
    /// после её завершения задержка снята, и повторная запись того же пути
    /// не ждёт заново.
    #[test]
    fn delay_write_is_one_shot() {
        let path = Path::new("/cfg/once.toml");
        let clock = ManualClock::new();
        let fs = MemStore::new();
        let until = ClockInstant::START.saturating_add(Duration::from_millis(20));
        fs.delay_write(path, until, clock.clone());

        clock.advance(Duration::from_millis(20));
        fs.clone().write_atomic(path, b"first").expect("first write");
        assert_eq!(fs.next_wake(), None, "delay removed once released");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut writer = fs.clone();
        std::thread::spawn(move || {
            writer.write_atomic(path, b"second").expect("second write");
            tx.send(()).expect("send done");
        });
        rx.recv_timeout(Duration::from_millis(200)).expect("second write is not delayed again");
        assert_eq!(fs.get(path).as_deref(), Some(&b"second"[..]));
    }
}
