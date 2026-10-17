//! Поток вывода ALSA (`apap-alsa-out`, §6.7). Часть 1 — чистая
//! классификация ошибок записи по `errno` в действие цикла потока
//! (ТЗ-51, ТЗ-76): сам цикл (`wait`/`avail_update`/`writei`/`prepare`,
//! §6.7 «Цикл потока») появится отдельным шагом.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

/// Действие цикла потока на ошибку ALSA по таблице §6.7:
///
/// | Ошибка ALSA | Действие |
/// |---|---|
/// | `EPIPE` (xrun устройства) | `Xrun` — счётчик xrun, `prepare()`, продолжить |
/// | `ESTRPIPE` (suspend) | `Resume` — `resume()` до успеха, при отказе `prepare()` |
/// | `ENODEV`, `EBADFD`, `EIO`, `ENXIO` | `DeviceUnavailable` — выход из цикла |
/// | прочие | `Other(errno)` — выход из цикла, `StreamFault::BackendError` |
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteErrorAction {
    Xrun,
    Resume,
    DeviceUnavailable,
    Other(i32),
}

/// Классификация `errno` ошибки записи/опроса PCM в действие цикла потока
/// (§6.7, ТЗ-51, ТЗ-76). `errno` — положительное значение, как возвращает
/// `alsa::Error::errno()` (крейт `alsa` 0.11 хранит `-r` для отрицательного
/// кода ALSA `r`, см. `acheck!`/`Error::new` в `alsa::error`), поэтому здесь
/// сравнение идёт с положительными константами `libc` (`libc::EPIPE` и т. д.),
/// а не с их отрицаниями.
pub fn classify_write_error(errno: i32) -> WriteErrorAction {
    match errno {
        libc::EPIPE => WriteErrorAction::Xrun,
        libc::ESTRPIPE => WriteErrorAction::Resume,
        libc::ENODEV | libc::EBADFD | libc::EIO | libc::ENXIO => WriteErrorAction::DeviceUnavailable,
        other => WriteErrorAction::Other(other),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    #[test]
    fn epipe_is_xrun() {
        assert_eq!(classify_write_error(libc::EPIPE), WriteErrorAction::Xrun);
    }

    #[test]
    fn estrpipe_is_resume() {
        assert_eq!(classify_write_error(libc::ESTRPIPE), WriteErrorAction::Resume);
    }

    #[test]
    fn device_gone_errnos_are_device_unavailable() {
        for errno in [libc::ENODEV, libc::EBADFD, libc::EIO, libc::ENXIO] {
            assert_eq!(classify_write_error(errno), WriteErrorAction::DeviceUnavailable);
        }
    }

    #[test]
    fn other_errno_is_passed_through() {
        assert_eq!(classify_write_error(libc::EINVAL), WriteErrorAction::Other(libc::EINVAL));
    }
}
