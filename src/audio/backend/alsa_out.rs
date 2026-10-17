//! Поток вывода ALSA (`apap-alsa-out`, §6.7). Часть 1 — чистая
//! классификация ошибок записи по `errno` в действие цикла потока
//! (ТЗ-51, ТЗ-76). Часть 2 — сам поток: `wait`/`avail_update`/`writei`
//! принадлежат циклу, а не `render` (И-Р17); без аллокаций внутри цикла
//! (И-Р7) и без возврата TID — ожидание TID для RT-приоритета (§6.8)
//! остаётся отдельным шагом.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::num::NonZeroU8;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use super::exclusive::{ExclusiveRender, PcmHandle, StreamHandle};
use crate::audio::error::StreamFault;
use crate::audio::session::SessionShared;

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

/// Пауза между повторами `resume()` при `EAGAIN` (§6.7).
const RESUME_RETRY_STEP: std::time::Duration = std::time::Duration::from_millis(10);
/// Предел повторов `resume()`: ~1 с, затем `prepare()` (§6.7).
const RESUME_MAX_TRIES: u32 = 100;

/// Действие на ошибку ALSA по таблице §6.7 (ТЗ-51, ТЗ-76): `true` — выход
/// из цикла (фатально), `false` — обработано, цикл продолжается. `Xrun` и
/// `Resume` восстанавливают PCM (`prepare`/`resume`) без выхода; остальные
/// пишут `StreamFault` в `SessionShared` (§2.9) и останавливают поток.
fn handle_stream_error(pcm: &alsa::PCM, errno: i32, shared: &SessionShared) -> bool {
    match classify_write_error(errno) {
        WriteErrorAction::Xrun => {
            shared.xruns.fetch_add(1, Ordering::Relaxed);
            let _ = pcm.prepare();
            false
        }
        WriteErrorAction::Resume => {
            // EAGAIN — устройство ещё просыпается: повтор с паузой, но не
            // бесконечно; по исчерпании попыток — prepare() (§6.7).
            for _ in 0..RESUME_MAX_TRIES {
                match pcm.resume() {
                    Ok(()) => return false,
                    Err(e) if e.errno() == libc::EAGAIN => std::thread::sleep(RESUME_RETRY_STEP),
                    Err(_) => break,
                }
            }
            let _ = pcm.prepare();
            false
        }
        WriteErrorAction::DeviceUnavailable => {
            shared.set_fault(StreamFault::DeviceUnavailable);
            true
        }
        WriteErrorAction::Other(_) => {
            shared.set_fault(StreamFault::BackendError);
            true
        }
    }
}

/// Размер буфера одного периода в байтах: период в кадрах × байт на кадр,
/// где байт на кадр = байт на сэмпл × число каналов (§6.7, ТЗ-112). Буфер
/// аллоцируется один раз при старте потока, до цикла (И-Р7). `None` —
/// нереалистичное переполнение `u32`/`usize`, а не паника.
pub(crate) fn period_buffer_bytes(period_frames: u32, bytes_per_sample: u32, channels: NonZeroU8) -> Option<usize> {
    let bytes_per_frame = bytes_per_sample.checked_mul(u32::from(channels.get()))?;
    let total = period_frames.checked_mul(bytes_per_frame)?;
    usize::try_from(total).ok()
}

/// Таймаут `pcm.wait` — 2 периода в миллисекундах (§6.7): поток выходит не
/// позднее `2 × period` после `stop.store(true)`. `None` — переполнение или
/// нулевая частота дискретизации, а не паника.
pub(crate) fn wait_timeout_ms(period_frames: u32, rate_hz: u32) -> Option<u32> {
    let period_ms = u64::from(period_frames).checked_mul(1000)?.checked_div(u64::from(rate_hz))?;
    let timeout_ms = period_ms.checked_mul(2)?;
    u32::try_from(timeout_ms).ok()
}

/// Возврат PCM из потока вывода в `DeviceClaim` без закрытия (§2.3, §6.7):
/// `StreamHandle::stop(self: Box<Self>)` (трейт `exclusive.rs`) не несёт
/// параметра `claim`, поэтому PCM возвращается через этот общий слот —
/// `AlsaStreamHandle::stop` кладёт его сюда после `JoinHandle::join()`, а
/// `AlsaExclusiveBackend::start` отдаёт второй клон вызывающей стороне,
/// которая переносит PCM в `claim.pcm_mut()` после вызова `stop()`.
pub(crate) type PcmReturnSlot = Arc<Mutex<Option<PcmHandle>>>;

/// Получатель TID потока `apap-alsa-out` (§6.7, §6.8, ТЗ-99): этот шаг
/// только создаёт канал и отправляет TID из потока до входа в цикл, но не
/// потребляет его — ожидание (не дольше 100 мс, вне RT) и вызов повышения
/// приоритета потока появятся отдельным шагом (§6.8).
pub(crate) type TidReceiver = mpsc::Receiver<rustix::thread::Pid>;

/// Имя потока вывода ALSA (§6.7).
const THREAD_NAME: &str = "apap-alsa-out";

/// Поток вывода (§2.3, §6.7): владеет флагом останова, `JoinHandle` и общим
/// с `AlsaExclusiveBackend::start` слотом возврата PCM. `tid_rx` хранится
/// до появления шага RT-приоритета (§6.8, см. `TidReceiver`).
pub(crate) struct AlsaStreamHandle {
    stop: Arc<AtomicBool>,
    join: JoinHandle<PcmHandle>,
    pcm_slot: PcmReturnSlot,
    #[allow(dead_code)] // потребитель — шаг RT-приоритета потока (§6.8, ТЗ-99)
    tid_rx: TidReceiver,
}

impl AlsaStreamHandle {
    pub(crate) fn new(stop: Arc<AtomicBool>, join: JoinHandle<PcmHandle>, pcm_slot: PcmReturnSlot, tid_rx: TidReceiver) -> AlsaStreamHandle {
        AlsaStreamHandle { stop, join, pcm_slot, tid_rx }
    }
}

impl StreamHandle for AlsaStreamHandle {
    /// Остановить поток и вернуть PCM в `PcmReturnSlot` (§2.3, §6.7): флаг
    /// `stop` заставляет цикл выйти не позднее `2 × period`, `join()`
    /// забирает PCM из потока, слот отдаёт его вызывающей стороне (см.
    /// `PcmReturnSlot`). Падение потока или отравленный `Mutex` не паникуют
    /// здесь — PCM в этом случае просто не возвращается в слот.
    fn stop(self: Box<Self>) {
        self.stop.store(true, Ordering::Release);
        if let Ok(pcm) = self.join.join() {
            if let Ok(mut slot) = self.pcm_slot.lock() {
                *slot = Some(pcm);
            }
        }
    }
}

/// Параметры цикла потока вывода, посчитанные до запуска (§6.7, ТЗ-112):
/// период в кадрах и в байтах (размер `period_buf`) и таймаут `pcm.wait`
/// (2×период в мс). Собраны в одну структуру, чтобы `spawn_output_thread`/
/// `run_output_loop` не превышали 7 параметров (`clippy::too_many_arguments`).
pub(crate) struct LoopTiming {
    pub(crate) period_frames: u32,
    pub(crate) period_bytes: usize,
    pub(crate) wait_timeout_ms: u32,
}

/// Запуск потока `apap-alsa-out` (§6.7): PCM перемещается в поток целиком и
/// возвращается через `JoinHandle` при выходе из цикла. Если ОС отказывает в
/// создании потока, `thread::Builder::spawn` роняет захваченное замыкание
/// вместе с перемещённым в него `pcm` (API `std::thread` не отдаёт захваченные
/// значения обратно при отказе) — вызывающая сторона получает `Err` и работает
/// с уже закрытым PCM (повторный `acquire`), что приемлемо для этого
/// практически недостижимого случая нехватки ресурсов ОС.
pub(crate) fn spawn_output_thread(
    pcm: PcmHandle,
    mut render: ExclusiveRender,
    timing: LoopTiming,
    shared: Arc<SessionShared>,
    stop: Arc<AtomicBool>,
    tid_tx: mpsc::Sender<rustix::thread::Pid>,
) -> std::io::Result<JoinHandle<PcmHandle>> {
    thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(move || run_output_loop(pcm, &mut render, &timing, &shared, &stop, &tid_tx))
}

/// Цикл потока вывода (§6.7 «Цикл потока»): `wait`/`avail_update`/`writei`
/// принадлежат циклу, а не `render` (ТЗ-98, И-Р17). `period_buf` аллоцирован
/// один раз до входа в цикл — внутри цикла нет аллокаций (И-Р7, ТЗ-112).
/// TID потока отправляется в `tid_tx` до входа в цикл (§6.8); отказ
/// отправки (получатель уже отброшен) не паникует и не прерывает поток.
fn run_output_loop(
    pcm: PcmHandle,
    render: &mut ExclusiveRender,
    timing: &LoopTiming,
    shared: &SessionShared,
    stop: &AtomicBool,
    tid_tx: &mpsc::Sender<rustix::thread::Pid>,
) -> PcmHandle {
    let _ = tid_tx.send(rustix::thread::gettid());

    let mut period_buf = vec![0u8; timing.period_bytes];
    let io = pcm.pcm().io_bytes();
    let period_frames_i64 = i64::from(timing.period_frames);

    'outer: while !stop.load(Ordering::Acquire) {
        if let Err(e) = pcm.pcm().wait(Some(timing.wait_timeout_ms)) {
            if handle_stream_error(pcm.pcm(), e.errno(), shared) {
                break 'outer;
            }
        }

        let mut avail = match pcm.pcm().avail_update() {
            Ok(a) => a,
            Err(e) => {
                if handle_stream_error(pcm.pcm(), e.errno(), shared) {
                    break 'outer;
                }
                continue 'outer;
            }
        };

        while avail >= period_frames_i64 {
            render.render(&mut period_buf);
            match io.writei(&period_buf) {
                Ok(_) => avail -= period_frames_i64,
                Err(e) => {
                    if handle_stream_error(pcm.pcm(), e.errno(), shared) {
                        break 'outer;
                    }
                    break;
                }
            }
        }
    }

    drop(io);
    let _ = pcm.pcm().drop();
    pcm
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

    #[test]
    fn period_buffer_bytes_computes_frames_times_bytes_per_frame() {
        let channels = NonZeroU8::new(2).expect("nonzero");
        assert_eq!(period_buffer_bytes(1_024, 3, channels), Some(1_024 * 3 * 2));
    }

    #[test]
    fn period_buffer_bytes_overflow_is_none() {
        let channels = NonZeroU8::new(8).expect("nonzero");
        assert_eq!(period_buffer_bytes(u32::MAX, 4, channels), None);
    }

    #[test]
    fn wait_timeout_ms_is_twice_period_in_milliseconds() {
        assert_eq!(wait_timeout_ms(1_920, 48_000), Some(80));
    }

    #[test]
    fn wait_timeout_ms_zero_rate_is_none() {
        assert_eq!(wait_timeout_ms(1_920, 0), None);
    }
}
