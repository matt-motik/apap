//! Системные вызовы записи для Linux (и прочих Unix, кроме macOS) —
//! столбец «Linux» таблицы ADR-4 (ТЗ-19).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Шаг 3: `fsync(fd)` временного файла до замены.
pub(super) fn sync_file(f: &File) -> io::Result<()> {
    f.sync_all()
}

/// Шаг 4: `rename(tmp, target)` — атомарная замена.
pub(super) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Шаг 5: `open(dir, O_RDONLY | O_DIRECTORY)` + `fsync` — запись в каталоге
/// переживает пропадание питания.
pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)?
        .sync_all()
}
