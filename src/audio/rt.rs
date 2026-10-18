//! Приоритет реального времени потока вывода Exclusive (§6.8, ADR-08, ТЗ-99).
//!
//! `apap-alsa-out` конкурирует за процессор с UI, декодером и фоновой
//! сборкой на обычном планировщике `SCHED_OTHER`; задержка пробуждения
//! потока даёт xrun раньше, чем истощается бюджет колбэка (≤ 50 % периода,
//! ТЗ-99). [`init_rt`] один раз при запуске движка читает лимиты `rtkit`
//! (`org.freedesktop.RealtimeKit1`) на системной шине и ставит
//! `RLIMIT_RTTIME` процесса, чтобы последующий `MakeThreadRealtime` потока
//! вывода (§6.7) не упёрся в лимит ядра. Недоступность rtkit — не ошибка:
//! приоритет просто не запрашивается, воспроизведение продолжается на
//! обычном приоритете.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use rustix::process::{setrlimit, Resource, Rlimit};
use std::time::Duration;
use zbus::blocking::{connection, Connection};
use zbus::zvariant::OwnedValue;

/// Верхняя граница приоритета, который движок просит у `rtkit` (§6.8).
pub const RT_PRIORITY_CAP: u32 = 20;

const RTKIT_NAME: &str = "org.freedesktop.RealtimeKit1";
const RTKIT_PATH: &str = "/org/freedesktop/RealtimeKit1";
const RTKIT_IFACE: &str = "org.freedesktop.RealtimeKit1";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Лимиты `rtkit`, прочитанные один раз при запуске движка (§6.8).
pub struct RtLimits {
    pub max_priority: u32,
    pub rttime_usec_max: u64,
}

/// Приоритет, который движок запросит у `MakeThreadRealtime`: `min(max, 20)`
/// (§6.8). Неположительный `max_realtime_priority` означает, что rtkit RT не
/// даёт вообще — запрос не делается.
pub fn choose_priority(max_realtime_priority: i32) -> Option<u32> {
    let max = u32::try_from(max_realtime_priority).ok()?;
    choose_priority_from_limit(max)
}

/// То же, что [`choose_priority`], но принимает уже беззнаковый лимит
/// `RtLimits::max_priority` (§6.8 п. 1, 3, ТЗ-99). Ноль означает, что rtkit
/// RT не даёт вообще — запрос не делается.
fn choose_priority_from_limit(max_realtime_priority: u32) -> Option<u32> {
    if max_realtime_priority == 0 {
        return None;
    }
    Some(max_realtime_priority.min(RT_PRIORITY_CAP))
}

/// Свойство интерфейса `rtkit` через `org.freedesktop.DBus.Properties.Get`.
fn get_property(conn: &Connection, name: &str) -> Option<OwnedValue> {
    conn.call_method(Some(RTKIT_NAME), RTKIT_PATH, Some(PROPERTIES_IFACE), "Get", &(RTKIT_IFACE, name))
        .ok()?
        .body()
        .deserialize()
        .ok()
}

/// Читает `MaxRealtimePriority` и `RTTimeUSecMax` у `rtkit` на системной
/// шине (§6.8). Любая неудача — нет системной шины, нет `rtkit`, таймаут,
/// неожидаемый тип свойства — молча даёт `None`: это штатный случай, а не
/// ошибка движка.
pub fn probe_rtkit() -> Option<RtLimits> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    let _ctx = rt.enter();
    let conn = connection::Builder::system().ok()?.method_timeout(PROBE_TIMEOUT).build().ok()?;
    let max_priority = get_property(&conn, "MaxRealtimePriority")?;
    let max_priority = i32::try_from(max_priority).ok()?;
    let rttime_usec_max = get_property(&conn, "RTTimeUSecMax")?;
    let rttime_usec_max = i64::try_from(rttime_usec_max).ok()?;
    let max_priority = u32::try_from(max_priority).ok()?;
    let rttime_usec_max = u64::try_from(rttime_usec_max).ok()?;
    Some(RtLimits { max_priority, rttime_usec_max })
}

/// Ставит `RLIMIT_RTTIME` процесса равным `rttime_usec_max` (soft = hard),
/// чтобы `apap-alsa-out`, заблокированный в `wait` на каждом периоде
/// (≤ 10 мс), не упирался в лимит непрерывного RT-времени ядра (§6.8).
pub fn apply_rttime_limit(limits: &RtLimits) -> bool {
    let limit = Rlimit { current: Some(limits.rttime_usec_max), maximum: Some(limits.rttime_usec_max) };
    setrlimit(Resource::Rttime, limit).is_ok()
}

/// Один раз при запуске движка (Linux): читает лимиты `rtkit` и ставит
/// `RLIMIT_RTTIME`. Недоступность шины/rtkit или отказ `setrlimit` — не
/// ошибка движка, а повод не запрашивать RT-приоритет далее (§6.8).
pub fn init_rt() -> Option<RtLimits> {
    let limits = probe_rtkit()?;
    apply_rttime_limit(&limits).then_some(limits)
}

/// Один блокирующий вызов `MakeThreadRealtime(tid, prio)` у `rtkit` на
/// системной шине (§6.8 п. 3, ТЗ-99, ТЗ-76). Без повторных попыток — одна
/// попытка на сессию. `false` — любая неудача (нет шины, нет rtkit, отказ
/// `PolicyKit`, таймаут): поток продолжает работать на обычном приоритете.
fn make_thread_realtime(tid: u64, prio: u32) -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return false;
    };
    let _ctx = rt.enter();
    let Ok(builder) = connection::Builder::system() else {
        return false;
    };
    let Ok(conn) = builder.method_timeout(PROBE_TIMEOUT).build() else {
        return false;
    };
    conn.call_method(Some(RTKIT_NAME), RTKIT_PATH, Some(RTKIT_IFACE), "MakeThreadRealtime", &(tid, prio)).is_ok()
}

/// Запрашивает у `rtkit` RT-приоритет `prio` для потока `tid` в отдельном
/// служебном потоке `apap-rtkit`, чтобы блокирующий D-Bus-вызов не задержал
/// вызывающего (§6.8 п. 3, ТЗ-99, ТЗ-76). По завершении вызывает `done`
/// ровно один раз: `Some(prio)` при успехе, `None` при любой неудаче. Без
/// повторных попыток — одна попытка на сессию.
pub fn request_realtime<F>(tid: u32, prio: u32, done: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce(Option<u32>) + Send + 'static,
{
    let tid = u64::from(tid);
    std::thread::Builder::new().name("apap-rtkit".to_string()).spawn(move || {
        let ok = make_thread_realtime(tid, prio);
        done(ok.then_some(prio));
    })
}

/// Запрос RT-приоритета у `rtkit` как внешняя зависимость движка (§6.8,
/// ADR-20): в тестах подменяется фейком, чтобы не трогать системную шину.
/// `done` вызывается ровно один раз — `Some(prio)` при успехе, `None` при
/// любой неудаче (§6.8 п. 3, ТЗ-99, ТЗ-76).
pub trait RtPriorityRequester: Send + Sync {
    /// Однократная инициализация при запуске движка (Linux, §6.8 п. 1, 3,
    /// ТЗ-99): читает лимиты `rtkit` и ставит `RLIMIT_RTTIME` процесса,
    /// возвращая приоритет, который движок будет запрашивать для потока
    /// вывода в последующих `request`. `None` — `rtkit`/шина недоступны или
    /// `MaxRealtimePriority` равен нулю: далее `request` не вызывается,
    /// RT-приоритет за эту сессию движка не запрашивается вовсе.
    fn init(&self) -> Option<u32>;

    fn request(&self, tid: u32, prio: u32, done: Box<dyn FnOnce(Option<u32>) + Send>);
}

/// Боевая реализация [`RtPriorityRequester`]: запрос `rtkit` в служебном
/// потоке `apap-rtkit` через [`request_realtime`] (§6.8 п. 3, ТЗ-99, ТЗ-76).
/// Отказ запустить сам служебный поток (крайне редкий исход — нехватка
/// ресурсов ОС) оставляет `done` невызванным: приоритет остаётся `Pending`
/// до конца сессии, как и было бы при недоступности `rtkit`/шины.
pub struct RtKitRequester;

impl RtPriorityRequester for RtKitRequester {
    fn init(&self) -> Option<u32> {
        let limits = init_rt()?;
        choose_priority_from_limit(limits.max_priority)
    }

    fn request(&self, tid: u32, prio: u32, done: Box<dyn FnOnce(Option<u32>) + Send>) {
        let _ = request_realtime(tid, prio, done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt_choose_priority_caps_at_20() {
        assert_eq!(choose_priority(95), Some(20));
    }

    #[test]
    fn rt_choose_priority_below_cap() {
        assert_eq!(choose_priority(20), Some(20));
        assert_eq!(choose_priority(5), Some(5));
    }

    #[test]
    fn rt_choose_priority_zero_or_negative_is_none() {
        assert_eq!(choose_priority(0), None);
        assert_eq!(choose_priority(-15), None);
    }

    #[test]
    /// TID u32::MAX не существует: rtkit (если есть) откажет, приоритет
    /// процесса теста не меняется.
    fn rt_request_realtime_calls_done_once() {
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        let spawn_result = request_realtime(u32::MAX, 1, move |result| {
            let _ = tx.send(result);
        });
        let Ok(handle) = spawn_result else {
            panic!("не удалось создать поток apap-rtkit");
        };
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(received) => assert_eq!(received, None),
            Err(_) => panic!("done не был вызван за отведённое время"),
        }
        let _ = handle.join();
    }
}
