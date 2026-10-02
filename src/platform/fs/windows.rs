//! Системные вызовы записи для Windows — столбец «Windows» таблицы ADR-4
//! (ТЗ-19). Отдельного сброса каталога нет: `MOVEFILE_WRITE_THROUGH`
//! возвращает управление после фактического перемещения на диске.

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

/// Шаг 3: `FlushFileBuffers` временного файла (`File::sync_all`).
pub(super) fn sync_file(f: &File) -> io::Result<()> {
    f.sync_all()
}

/// Шаг 4: `MoveFileExW(tmp, target, MOVEFILE_REPLACE_EXISTING |
/// MOVEFILE_WRITE_THROUGH)` — замена с записью на диск до возврата.
pub(super) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let from_w = wide(from);
    let to_w = wide(to);
    // SAFETY: обе строки — UTF-16 с завершающим нулём, живут до конца вызова.
    let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Шаг 5: на Windows нет — гарантию даёт `MOVEFILE_WRITE_THROUGH` шага 4.
pub(super) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Путь как UTF-16 с завершающим нулём для `*W`-функций.
fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}
