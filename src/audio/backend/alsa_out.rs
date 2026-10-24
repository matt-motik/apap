//! Поток вывода ALSA (`apap-alsa-out`, §6.7). Часть 1 — чистая
//! классификация ошибок записи по `errno` в действие цикла потока
//! (ТЗ-51, ТЗ-76). Часть 2 — сам поток: `wait`/`avail_update`/`writei`
//! принадлежат циклу, а не `render` (И-Р17); без аллокаций внутри цикла
//! (И-Р7) и без возврата TID — ожидание TID для RT-приоритета (§6.8)
//! остаётся отдельным шагом.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::num::NonZeroU8;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// Получатель TID потока `apap-alsa-out` (§6.7, §6.8 п. 2, ТЗ-99): поток
/// отправляет свой TID в соответствующий `Sender` до входа в цикл; `recv_tid`
/// забирает его вне RT, не дольше 100 мс. Запрос RT-приоритета у rtkit по
/// этому TID — отдельный шаг (§6.8 п. 3).
pub(crate) type TidReceiver = mpsc::Receiver<rustix::thread::Pid>;

/// Таймаут ожидания TID потока вывода вне RT (§6.8 п. 2, ТЗ-99).
const RT_TID_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Ждёт TID потока `apap-alsa-out` не дольше 100 мс вне RT (§6.7, §6.8 п. 2,
/// ТЗ-99). Таймаут или отказ отправителя (поток не создался или уже упал до
/// отправки) — `None`, не паника.
pub(crate) fn recv_tid(rx: TidReceiver) -> Option<u32> {
    let pid = rx.recv_timeout(RT_TID_RECV_TIMEOUT).ok()?;
    u32::try_from(pid.as_raw_pid()).ok()
}

/// Имя потока вывода ALSA (§6.7).
const THREAD_NAME: &str = "apap-alsa-out";

/// Поток вывода (§2.3, §6.7): владеет флагом останова, `JoinHandle` и TID
/// потока (`rt_tid`, §6.8 п. 2, ТЗ-99), полученным через `recv_tid` до
/// постройки хендла. Запрос RT-приоритета у rtkit по этому TID — отдельный
/// шаг (§6.8 п. 3).
pub(crate) struct AlsaStreamHandle {
    stop: Arc<AtomicBool>,
    join: JoinHandle<PcmHandle>,
    tid: Option<u32>,
}

impl AlsaStreamHandle {
    pub(crate) fn new(stop: Arc<AtomicBool>, join: JoinHandle<PcmHandle>, tid: Option<u32>) -> AlsaStreamHandle {
        AlsaStreamHandle { stop, join, tid }
    }
}

impl StreamHandle for AlsaStreamHandle {
    /// Остановить поток и вернуть PCM значением (§2.3, §6.7): флаг `stop`
    /// заставляет цикл выйти не позднее `2 × period`, `join()` забирает PCM
    /// из потока. Падение потока не паникует здесь — PCM в этом случае
    /// просто не возвращается (`None`), вызывающая сторона переносит `Some`
    /// в `claim.pcm_mut()`.
    fn stop(self: Box<Self>) -> Option<PcmHandle> {
        self.stop.store(true, Ordering::Release);
        self.join.join().ok()
    }

    /// TID потока `apap-alsa-out`, полученный при `start` (§6.8 п. 2, ТЗ-99).
    fn rt_tid(&self) -> Option<u32> {
        self.tid
    }
}

/// Параметры цикла потока вывода, посчитанные до запуска (§6.7, ТЗ-112):
/// период в кадрах и в байтах (размер `period_buf`) и таймаут `pcm.wait`
/// (2×период в мс). Собраны в одну структуру, чтобы `spawn_output_thread`/
/// `run_output_loop` не превышали 7 параметров (`clippy::too_many_arguments`).
///
/// `start_threshold_frames`/`watchdog_deadline` — сторож запуска (§6.7 п. 2,
/// ТЗ-68, AM1.0-B34): посчитаны один раз в `AlsaExclusiveBackend::start`, не
/// в RT-цикле (И-Р7).
pub(crate) struct LoopTiming {
    pub(crate) period_frames: u32,
    pub(crate) period_bytes: usize,
    pub(crate) wait_timeout_ms: u32,
    /// Порог старта потока (`sw_params::start_threshold`, кадры) — тот же,
    /// что уже установлен в sw_params (`alsa.rs::start_threshold_frames`):
    /// цикл сравнивает с ним счётчик уже записанных кадров, не читая ALSA
    /// повторно.
    pub(crate) start_threshold_frames: i64,
    /// Дедлайн сторожа — `max(3×время буфера, 300 мс)` от момента, когда
    /// счётчик записанных кадров достиг `start_threshold_frames` (или от
    /// момента, когда декодер закончил, см. `run_output_loop`).
    pub(crate) watchdog_deadline: Duration,
}

/// Минимальный дедлайн сторожа запуска (§6.7 п. 2, ТЗ-68, AM1.0-B34) —
/// нижняя граница `max(3×буфер, 300 мс)` и запасное значение, если буфер
/// устройства не удалось прочитать при запуске потока.
pub(crate) const MIN_START_WATCHDOG_DEADLINE: Duration = Duration::from_millis(300);

/// Дедлайн сторожа запуска — `max(3×время буфера устройства, 300 мс)` (§6.7
/// п. 2, ТЗ-68, AM1.0-B34): время, которое даётся драйверу перейти в
/// `RUNNING` после того, как писатель заполнил буфер до порога старта.
/// `None` — переполнение или нулевая частота, а не паника; вызывающая
/// сторона подставляет `MIN_START_WATCHDOG_DEADLINE`.
pub(crate) fn start_watchdog_deadline(buffer_frames: u32, rate_hz: u32) -> Option<Duration> {
    let buffer_ms = u64::from(buffer_frames).checked_mul(1_000)?.checked_div(u64::from(rate_hz))?;
    let triple_ms = buffer_ms.checked_mul(3)?.max(300);
    u32::try_from(triple_ms).ok().map(|ms| Duration::from_millis(u64::from(ms)))
}

/// Решение сторожа запуска потока на текущей итерации цикла (§6.7 п. 2,
/// ТЗ-68, AM1.0-B34).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StartWatchdogDecision {
    /// Ждать дальше — порог ещё не достигнут, hw-указатель уже двинулся,
    /// либо дедлайн не истёк.
    Wait,
    /// Поток открыт, порог достигнут, hw-указатель не двинулся дольше
    /// дедлайна — честная ошибка вместо тишины (`StreamFault::Stalled`).
    Fault,
}

/// Чистая функция решения сторожа (§6.7 п. 2, ТЗ-68, AM1.0-B34): без ALSA,
/// тестируется без реального устройства. `threshold_reached` — писатель
/// заполнил буфер до порога старта, либо декодер уже закончил (короткий
/// трек/хвост — порог может быть недостижим в принципе, см.
/// `run_output_loop`); `pointer_moved` — PCM уже в `RUNNING` (hw-указатель
/// точно движется); `elapsed` — время с момента, когда `threshold_reached`
/// стало истинным впервые.
pub(crate) fn start_watchdog_decision(
    threshold_reached: bool,
    pointer_moved: bool,
    elapsed: Duration,
    deadline: Duration,
) -> StartWatchdogDecision {
    if !threshold_reached || pointer_moved {
        return StartWatchdogDecision::Wait;
    }
    if elapsed >= deadline {
        StartWatchdogDecision::Fault
    } else {
        StartWatchdogDecision::Wait
    }
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
///
/// Сторож запуска (§6.7 п. 2, ТЗ-68, AM1.0-B34): открытый поток, чей
/// hw-указатель никогда не двинулся, — честная ошибка (`StreamFault::
/// Stalled`), а не бесконечная тишина. `written_frames`/`watchdog_at`/
/// `watchdog_resolved`/`explicit_started` — состояние сторожа внутри одного
/// запуска потока, без аллокаций (`Instant::now()` — не системный вызов с
/// I/O). Короткий трек/хвост, где декодер заканчивает раньше, чем писатель
/// набирает порог старта, — порог в принципе недостижим обычным путём:
/// как только декодер закончил (`shared.ended`), поток один раз зовёт явный
/// `pcm.start()` (идиома самого крейта `alsa`, см. `pcm.rs` doc-пример), а
/// сторож остаётся даже после этой попытки — на случай, если и явный
/// `start()` не поднял поток (та же честная ошибка вместо тишины).
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

    let mut written_frames: i64 = 0;
    let mut watchdog_at: Option<Instant> = None;
    let mut watchdog_resolved = false;
    let mut explicit_started = false;

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
                Ok(_) => {
                    avail -= period_frames_i64;
                    written_frames = written_frames.saturating_add(period_frames_i64);
                }
                Err(e) => {
                    if handle_stream_error(pcm.pcm(), e.errno(), shared) {
                        break 'outer;
                    }
                    break;
                }
            }
        }

        if !watchdog_resolved {
            let running = pcm.pcm().state() == alsa::pcm::State::Running;
            if running {
                watchdog_resolved = true;
            } else {
                let ended = shared.ended.load(Ordering::Acquire);
                if ended && !explicit_started {
                    explicit_started = true;
                    let _ = pcm.pcm().start();
                }
                let threshold_reached = ended || written_frames >= timing.start_threshold_frames;
                if threshold_reached {
                    let t0 = watchdog_at.get_or_insert_with(Instant::now);
                    if start_watchdog_decision(true, running, t0.elapsed(), timing.watchdog_deadline)
                        == StartWatchdogDecision::Fault
                    {
                        shared.set_fault(StreamFault::Stalled);
                        break 'outer;
                    }
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

    #[test]
    fn recv_tid_without_sender_is_none() {
        let (tx, rx) = mpsc::channel::<rustix::thread::Pid>();
        drop(tx);
        assert_eq!(recv_tid(rx), None);
    }

    /// Сторож запуска (§6.7 п. 2, ТЗ-68, AM1.0-B34): порог не достигнут —
    /// ждём, даже если формальный «дедлайн» давно истёк (вырожденный
    /// `Duration::ZERO`, на практике недостижим, но функция не паникует).
    #[test]
    fn start_watchdog_waits_while_threshold_not_reached() {
        let decision =
            start_watchdog_decision(false, false, Duration::from_secs(10), Duration::ZERO);
        assert_eq!(decision, StartWatchdogDecision::Wait);
    }

    /// Порог достигнут, но hw-указатель уже двинулся (`RUNNING`) — не
    /// ошибка, даже если элапс формально больше дедлайна.
    #[test]
    fn start_watchdog_waits_when_pointer_already_moved() {
        let decision =
            start_watchdog_decision(true, true, Duration::from_secs(10), Duration::from_millis(300));
        assert_eq!(decision, StartWatchdogDecision::Wait);
    }

    /// Порог достигнут, указатель не двинулся, дедлайн не истёк — ждём дальше.
    #[test]
    fn start_watchdog_waits_before_deadline() {
        let decision =
            start_watchdog_decision(true, false, Duration::from_millis(100), Duration::from_millis(300));
        assert_eq!(decision, StartWatchdogDecision::Wait);
    }

    /// Порог достигнут, указатель не двинулся, дедлайн истёк — честная
    /// ошибка вместо бесконечной тишины (AM1.0-B34).
    #[test]
    fn start_watchdog_faults_after_deadline() {
        let decision =
            start_watchdog_decision(true, false, Duration::from_millis(300), Duration::from_millis(300));
        assert_eq!(decision, StartWatchdogDecision::Fault);
    }

    /// `max(3×буфер, 300 мс)` (§6.7 п. 2, ТЗ-68, AM1.0-B34): буфер короче
    /// 100 мс — дедлайн упирается в минимум 300 мс.
    #[test]
    fn start_watchdog_deadline_floors_at_300ms_for_short_buffers() {
        // 20 мс буфера при 44,1 кГц — 882 кадра; 3×20=60 мс < 300 мс.
        assert_eq!(start_watchdog_deadline(882, 44_100), Some(Duration::from_millis(300)));
    }

    /// Буфер 200 мс — дедлайн растёт вместе с ним (3×200=600 мс > 300 мс).
    #[test]
    fn start_watchdog_deadline_scales_with_large_buffers() {
        // 200 мс буфера при 48 кГц — 9 600 кадров.
        assert_eq!(start_watchdog_deadline(9_600, 48_000), Some(Duration::from_millis(600)));
    }

    #[test]
    fn start_watchdog_deadline_zero_rate_is_none() {
        assert_eq!(start_watchdog_deadline(882, 0), None);
    }
}
