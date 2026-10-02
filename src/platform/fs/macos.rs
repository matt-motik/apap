//! Системные вызовы записи для macOS — столбец «macOS» таблицы ADR-4 (ТЗ-19).
//! `fsync(2)` на macOS не сбрасывает кэш накопителя; это делает
//! `fcntl(F_FULLFSYNC)`. Если ФС его не поддерживает — `fsync`.

use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// `fcntl(fd, F_FULLFSYNC)`; при ошибке — `fsync(fd)` (ADR-4, шаги 3 и 5).
fn full_sync(f: &File) -> io::Result<()> {
    // SAFETY: дескриптор принадлежит открытому `f` и живёт до конца вызова;
    // F_FULLFSYNC не принимает указателей.
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
    if rc == -1 {
        f.sync_all()
    } else {
        Ok(())
    }
}

/// Шаг 3: сброс временного файла до замены.
pub(super) fn sync_file(f: &File) -> io::Result<()> {
    full_sync(f)
}

/// Шаг 4: `rename(tmp, target)` — атомарная замена.
pub(super) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Шаг 5: `open(dir, O_RDONLY)` + `F_FULLFSYNC`, при ошибке — `fsync`.
pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
    full_sync(&File::open(dir)?)
}
