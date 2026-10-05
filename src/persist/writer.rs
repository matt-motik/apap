//! Писатель `apap-persist` (ADR-1, ТЗ-3, §2.7, §6.6): единственный поток,
//! выполняющий атомарную запись рабочих файлов и фоновые задачи (экспорт,
//! копия `*.bad`, карантин плейлиста). Снимки приходят с готовыми байтами —
//! поток не обращается к памяти UI (И-Т1).

use super::{ConfigFile, ConfigPaths, Snapshot, SnapshotId, WorkFile};
use crate::platform::fs::{FileWriter, WriteError, WriteErrorClass, WriteStep};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Команда писателю (ADR-1, §2.7). Байты в ней уже готовы к записи.
#[derive(Debug)]
pub enum WriterCmd {
    /// Записать снимок рабочего файла; более новый `Write` того же файла,
    /// пришедший до начала записи, заменяет его в слоте (§6.6).
    Write(Snapshot),
    /// Экспорт «Сохранить плейлист» (ТЗ-13 а, ADR-17).
    Export { attempt: u64, path: PathBuf, bytes: Arc<[u8]> },
    /// Копия неразбираемого файла (ТЗ-6).
    BadCopy { file: ConfigFile, bytes: Arc<[u8]> },
    /// `playlist.m3u` → `playlist.m3u.bad` с заменой (ТЗ-21 случай 2, ADR-5).
    QuarantinePlaylist,
    /// Завершить поток после уже принятых команд (§6.6).
    Stop,
}

/// Ответ писателя (§2.7).
#[derive(Debug)]
pub enum WriterReply {
    Written { file: WorkFile, id: SnapshotId },
    Failed { file: WorkFile, id: SnapshotId, err: WriteError },
    /// Снимок заменён в слоте более новым до начала записи (ADR-1, §6.6).
    Superseded { file: WorkFile, id: SnapshotId },
    Exported { attempt: u64, path: PathBuf },
    ExportFailed { attempt: u64, err: WriteError },
    BadCopySaved { file: ConfigFile, path: PathBuf },
    BadCopyFailed { file: ConfigFile, err: WriteError },
    Quarantined { path: PathBuf },
    QuarantineFailed { err: WriteError },
}

/// Канал к писателю (§2.7). Если поток не запустился (ТЗ-20), каждая
/// команда сразу получает ответ-ошибку без потока (`dead`).
pub struct WriterHandle {
    cmd_tx: Sender<WriterCmd>,
    reply_rx: Receiver<WriterReply>,
    reply_tx: Sender<WriterReply>,
    handle: Option<JoinHandle<()>>,
    dead: bool,
    paths: ConfigPaths,
}

impl WriterHandle {
    /// Отправить команду. Если писатель «мёртвый», синтезирует ответ-ошибку
    /// сразу (ТЗ-20); если получатель канала ответов исчез — ответ молча
    /// теряется.
    pub fn send(&self, cmd: WriterCmd) {
        if self.dead {
            if let Some(reply) = dead_reply(cmd, &self.paths) {
                let _ = self.reply_tx.send(reply);
            }
            return;
        }
        let _ = self.cmd_tx.send(cmd);
    }

    /// Неблокирующее чтение одного ответа.
    pub fn try_recv(&self) -> Option<WriterReply> {
        self.reply_rx.try_recv().ok()
    }

    /// Канал ответов для блокирующего ожидания (`recv_timeout` и т. п.).
    pub fn replies(&self) -> &Receiver<WriterReply> {
        &self.reply_rx
    }

    /// Дождаться завершения потока `apap-persist`, если он был запущен.
    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Писатель не запустился (ТЗ-20): каждая команда сразу получает
    /// ответ-ошибку, поток не создаётся.
    fn dead(paths: ConfigPaths) -> WriterHandle {
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        WriterHandle { cmd_tx, reply_rx, reply_tx, handle: None, dead: true, paths }
    }
}

/// Ответ-ошибка «мёртвого» писателя для команды `cmd` (ТЗ-20); `Stop` ответа
/// не получает.
fn dead_reply(cmd: WriterCmd, paths: &ConfigPaths) -> Option<WriterReply> {
    match cmd {
        WriterCmd::Write(s) => Some(WriterReply::Failed { file: s.file, id: s.id, err: dead_error(paths.work(s.file)) }),
        WriterCmd::Export { attempt, path, .. } => Some(WriterReply::ExportFailed { attempt, err: dead_error(&path) }),
        WriterCmd::BadCopy { file, .. } => {
            Some(WriterReply::BadCopyFailed { file, err: dead_error(&paths.bad_copy(file.work())) })
        }
        WriterCmd::QuarantinePlaylist => {
            Some(WriterReply::QuarantineFailed { err: dead_error(&paths.bad_copy(WorkFile::Playlist)) })
        }
        WriterCmd::Stop => None,
    }
}

/// Ошибка «поток писателя не запустился» (ТЗ-20): класс «ошибка
/// ввода-вывода», шаг — открытие временного файла.
fn dead_error(path: &Path) -> WriteError {
    WriteError {
        class: WriteErrorClass::Io,
        step: WriteStep::OpenTemp,
        os_code: None,
        os_text: "writer thread failed to start".into(),
        path: path.to_path_buf(),
    }
}

/// Запускает поток `apap-persist`. `writer` целиком переходит в поток —
/// кроме него, писатель ничего не читает из общей памяти (И-Т1, ADR-1).
/// Если поток не запустился, писатель переходит в «мёртвый» режим (ТЗ-20).
pub fn spawn_writer(writer: Box<dyn FileWriter>, paths: ConfigPaths) -> WriterHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCmd>();
    let (reply_tx, reply_rx) = mpsc::channel::<WriterReply>();
    let thread_paths = paths.clone();
    let thread_reply_tx = reply_tx.clone();
    let spawned = thread::Builder::new()
        .name("apap-persist".into())
        .spawn(move || run_writer(writer, thread_paths, cmd_rx, thread_reply_tx));
    match spawned {
        Ok(handle) => WriterHandle { cmd_tx, reply_rx, reply_tx, handle: Some(handle), dead: false, paths },
        Err(_) => WriterHandle::dead(paths),
    }
}

/// Задача в очереди `order` (§6.6): `Work` ссылается на слот рабочего
/// файла, прочие несут данные команды целиком.
enum Job {
    Work(WorkFile),
    Export { attempt: u64, path: PathBuf, bytes: Arc<[u8]> },
    BadCopy { file: ConfigFile, bytes: Arc<[u8]> },
    Quarantine,
}

/// Индекс слота рабочего файла (§6.6: `slots: [Option<Snapshot>; 3]`).
fn slot_index(f: WorkFile) -> usize {
    match f {
        WorkFile::Playlist => 0,
        WorkFile::State => 1,
        WorkFile::Settings => 2,
    }
}

/// Принять одну команду: обновить слоты и очередь `order` (§6.6). Слот уже
/// занят ⟺ для файла есть задача в `order` — это и подменяемый снимок, и
/// признак «не дублировать очередь».
fn accept(
    cmd: WriterCmd,
    slots: &mut [Option<Snapshot>; 3],
    order: &mut VecDeque<Job>,
    stopping: &mut bool,
    reply_tx: &Sender<WriterReply>,
) {
    match cmd {
        WriterCmd::Write(snap) => {
            let idx = slot_index(snap.file);
            match slots[idx].take() {
                Some(old) => {
                    let _ = reply_tx.send(WriterReply::Superseded { file: old.file, id: old.id });
                }
                None => order.push_back(Job::Work(snap.file)),
            }
            slots[idx] = Some(snap);
        }
        WriterCmd::Export { attempt, path, bytes } => order.push_back(Job::Export { attempt, path, bytes }),
        WriterCmd::BadCopy { file, bytes } => order.push_back(Job::BadCopy { file, bytes }),
        WriterCmd::QuarantinePlaylist => order.push_back(Job::Quarantine),
        WriterCmd::Stop => *stopping = true,
    }
}

/// Выполнить задачу (§6.6) и отправить соответствующий ответ.
fn run_job(job: Job, writer: &mut dyn FileWriter, paths: &ConfigPaths, slots: &mut [Option<Snapshot>; 3], reply_tx: &Sender<WriterReply>) {
    let reply = match job {
        Job::Work(file) => {
            let idx = slot_index(file);
            let Some(snap) = slots[idx].take() else { return };
            match writer.write_atomic(paths.work(file), &snap.bytes) {
                Ok(()) => WriterReply::Written { file, id: snap.id },
                Err(err) => WriterReply::Failed { file, id: snap.id, err },
            }
        }
        Job::BadCopy { file, bytes } => {
            let path = paths.bad_copy(file.work());
            match writer.write_atomic(&path, &bytes) {
                Ok(()) => WriterReply::BadCopySaved { file, path },
                Err(err) => WriterReply::BadCopyFailed { file, err },
            }
        }
        Job::Quarantine => {
            let path = paths.bad_copy(WorkFile::Playlist);
            match writer.rename_replace(&paths.playlist, &path) {
                Ok(()) => WriterReply::Quarantined { path },
                Err(err) => WriterReply::QuarantineFailed { err },
            }
        }
        Job::Export { attempt, path, bytes } => match writer.write_atomic(&path, &bytes) {
            Ok(()) => WriterReply::Exported { attempt, path },
            Err(err) => WriterReply::ExportFailed { attempt, err },
        },
    };
    let _ = reply_tx.send(reply);
}

/// Цикл потока `apap-persist` (§6.6): блокирующий `recv`, только если нет
/// работы, затем неблокирующее добавление всего, что уже пришло; `Stop` и
/// разрыв канала команд без оставшейся работы завершают поток.
fn run_writer(mut writer: Box<dyn FileWriter>, paths: ConfigPaths, cmd_rx: Receiver<WriterCmd>, reply_tx: Sender<WriterReply>) {
    let mut slots: [Option<Snapshot>; 3] = [None, None, None];
    let mut order: VecDeque<Job> = VecDeque::new();
    let mut stopping = false;
    let mut disconnected = false;

    loop {
        if order.is_empty() && !disconnected && !stopping {
            match cmd_rx.recv() {
                Ok(cmd) => accept(cmd, &mut slots, &mut order, &mut stopping, &reply_tx),
                Err(_) => disconnected = true,
            }
        }
        while let Ok(cmd) = cmd_rx.try_recv() {
            accept(cmd, &mut slots, &mut order, &mut stopping, &reply_tx);
        }

        let Some(job) = order.pop_front() else {
            if stopping || disconnected {
                return;
            }
            continue;
        };

        run_job(job, writer.as_mut(), &paths, &mut slots, &reply_tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::clock::ClockInstant;
    use crate::audio::testing::ManualClock;
    use crate::platform::fs::MemStore;
    use std::time::Duration;

    fn paths() -> ConfigPaths {
        ConfigPaths::in_dir(PathBuf::from("/cfg"))
    }

    fn snap(id: u64, file: WorkFile, bytes: &[u8]) -> Snapshot {
        Snapshot { id: SnapshotId::new(id), file, bytes: Arc::from(bytes) }
    }

    const TIMEOUT: Duration = Duration::from_secs(2);

    /// Более новый снимок того же файла, пришедший до начала его записи,
    /// заменяет прежний в слоте (ADR-1, §6.6): прежний получает `Superseded`,
    /// на диск попадают только первая и последняя версии.
    ///
    /// Отклонение от буквального порядка шагов в прозе задания: реалистичный
    /// порядок ответов — `Written(id1)`, затем `Superseded(id2)`, затем
    /// `Written(id3)`, а не `Superseded` до продвижения часов. Причина:
    /// `MemStore::write_atomic` ждёт задержку, блокируя весь вызывающий
    /// поток (единственный поток писателя) до продвижения `ManualClock`;
    /// до этого момента писатель физически не может вернуться в цикл и
    /// принять id2/id3, то есть `Superseded` для id2 не может быть отправлен
    /// раньше, чем завершится запись id1.
    #[test]
    fn writes_serialized_last_snapshot_wins() {
        let p = paths();
        let fs = MemStore::new();
        let clock = ManualClock::new();
        let until = ClockInstant::START.saturating_add(Duration::from_millis(50));
        fs.delay_write(&p.settings, until, clock.clone());

        let handle = spawn_writer(Box::new(fs.clone()), p.clone());

        handle.send(WriterCmd::Write(snap(1, WorkFile::Settings, b"one")));
        // Дать писателю время забрать задачу id1 и войти в блокирующее
        // ожидание задержки внутри write_atomic (тот же приём, что и в
        // delay_write_blocks_until_clock_advances в src/platform/fs/mem.rs).
        thread::sleep(Duration::from_millis(50));

        handle.send(WriterCmd::Write(snap(2, WorkFile::Settings, b"two")));
        handle.send(WriterCmd::Write(snap(3, WorkFile::Settings, b"three")));

        clock.advance(Duration::from_millis(60));

        let r1 = handle.replies().recv_timeout(TIMEOUT).expect("written id1");
        assert!(matches!(r1, WriterReply::Written { file: WorkFile::Settings, id } if id == SnapshotId::new(1)));

        let r2 = handle.replies().recv_timeout(TIMEOUT).expect("superseded id2");
        assert!(matches!(r2, WriterReply::Superseded { file: WorkFile::Settings, id } if id == SnapshotId::new(2)));

        let r3 = handle.replies().recv_timeout(TIMEOUT).expect("written id3");
        assert!(matches!(r3, WriterReply::Written { file: WorkFile::Settings, id } if id == SnapshotId::new(3)));

        assert_eq!(fs.get(&p.settings).as_deref(), Some(&b"three"[..]));
        assert_eq!(fs.counts(&p.settings).writes, 2);

        handle.send(WriterCmd::Stop);
        handle.join();
    }

    /// Задачи разных файлов исполняются в порядке поступления (§6.6).
    #[test]
    fn fifo_across_files() {
        let p = paths();
        let fs = MemStore::new();
        let handle = spawn_writer(Box::new(fs.clone()), p.clone());

        handle.send(WriterCmd::Write(snap(1, WorkFile::Playlist, b"pl")));
        handle.send(WriterCmd::Export { attempt: 1, path: PathBuf::from("/export/x.m3u"), bytes: Arc::from(&b"ex"[..]) });
        handle.send(WriterCmd::Write(snap(2, WorkFile::Settings, b"se")));

        let r1 = handle.replies().recv_timeout(TIMEOUT).expect("first");
        assert!(matches!(r1, WriterReply::Written { file: WorkFile::Playlist, id } if id == SnapshotId::new(1)));

        let r2 = handle.replies().recv_timeout(TIMEOUT).expect("second");
        assert!(matches!(r2, WriterReply::Exported { attempt: 1, .. }));

        let r3 = handle.replies().recv_timeout(TIMEOUT).expect("third");
        assert!(matches!(r3, WriterReply::Written { file: WorkFile::Settings, id } if id == SnapshotId::new(2)));

        handle.send(WriterCmd::Stop);
        handle.join();
    }

    /// Ошибка записи возвращается в `Failed`, а не теряется и не паникует.
    #[test]
    fn failed_write_replies_failed() {
        let p = paths();
        let fs = MemStore::new();
        fs.fail_write(&p.settings, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        let handle = spawn_writer(Box::new(fs.clone()), p.clone());

        handle.send(WriterCmd::Write(snap(1, WorkFile::Settings, b"x")));
        let r = handle.replies().recv_timeout(TIMEOUT).expect("reply");
        match r {
            WriterReply::Failed { file: WorkFile::Settings, id, err } => {
                assert_eq!(id, SnapshotId::new(1));
                assert_eq!(err.class, WriteErrorClass::NoSpace);
            }
            other => panic!("unexpected: {other:?}"),
        }

        handle.send(WriterCmd::Stop);
        handle.join();
    }

    /// `BadCopy` пишет `<файл>.bad`; `QuarantinePlaylist` переносит плейлист
    /// туда же с заменой (ТЗ-6, ТЗ-21 случай 2).
    #[test]
    fn bad_copy_and_quarantine_paths() {
        let p = paths();
        let fs = MemStore::new();
        fs.put(&p.playlist, b"old playlist");
        let handle = spawn_writer(Box::new(fs.clone()), p.clone());

        handle.send(WriterCmd::BadCopy { file: ConfigFile::Settings, bytes: Arc::from(&b"bad settings"[..]) });
        let r1 = handle.replies().recv_timeout(TIMEOUT).expect("bad copy reply");
        match r1 {
            WriterReply::BadCopySaved { file: ConfigFile::Settings, path } => {
                assert_eq!(path, p.bad_copy(WorkFile::Settings));
                assert_eq!(fs.get(&path).as_deref(), Some(&b"bad settings"[..]));
            }
            other => panic!("unexpected: {other:?}"),
        }

        handle.send(WriterCmd::QuarantinePlaylist);
        let r2 = handle.replies().recv_timeout(TIMEOUT).expect("quarantine reply");
        match r2 {
            WriterReply::Quarantined { path } => {
                assert_eq!(path, p.bad_copy(WorkFile::Playlist));
                assert_eq!(fs.get(&p.playlist), None);
                assert_eq!(fs.get(&path).as_deref(), Some(&b"old playlist"[..]));
            }
            other => panic!("unexpected: {other:?}"),
        }

        handle.send(WriterCmd::Stop);
        handle.join();
    }

    /// `Stop` завершает уже принятые задачи перед выходом (§6.6).
    #[test]
    fn stop_finishes_accepted_jobs_then_exits() {
        let p = paths();
        let fs = MemStore::new();
        let handle = spawn_writer(Box::new(fs.clone()), p.clone());

        handle.send(WriterCmd::Write(snap(1, WorkFile::Settings, b"settings body")));
        handle.send(WriterCmd::Stop);

        let r = handle.replies().recv_timeout(TIMEOUT).expect("written");
        assert!(matches!(r, WriterReply::Written { file: WorkFile::Settings, .. }));

        handle.join();
        assert_eq!(fs.get(&p.settings).as_deref(), Some(&b"settings body"[..]));
    }

    /// Писатель, не запустившийся (ТЗ-20), отвечает ошибкой ввода-вывода на
    /// каждую команду, кроме `Stop`.
    #[test]
    fn dead_writer_replies_io_errors() {
        let p = paths();
        let handle = WriterHandle::dead(p.clone());

        handle.send(WriterCmd::Write(snap(1, WorkFile::State, b"x")));
        match handle.try_recv().expect("reply") {
            WriterReply::Failed { file: WorkFile::State, id, err } => {
                assert_eq!(id, SnapshotId::new(1));
                assert_eq!(err.class, WriteErrorClass::Io);
                assert_eq!(err.path, p.state);
            }
            other => panic!("unexpected: {other:?}"),
        }

        handle.send(WriterCmd::Export { attempt: 2, path: PathBuf::from("/exports/x.m3u"), bytes: Arc::from(&b"e"[..]) });
        assert!(matches!(handle.try_recv(), Some(WriterReply::ExportFailed { attempt: 2, .. })));

        handle.send(WriterCmd::BadCopy { file: ConfigFile::Settings, bytes: Arc::from(&b"b"[..]) });
        assert!(matches!(handle.try_recv(), Some(WriterReply::BadCopyFailed { file: ConfigFile::Settings, .. })));

        handle.send(WriterCmd::QuarantinePlaylist);
        assert!(matches!(handle.try_recv(), Some(WriterReply::QuarantineFailed { .. })));

        handle.send(WriterCmd::Stop);
        assert!(handle.try_recv().is_none());

        handle.join();
    }
}
