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
    if max_realtime_priority <= 0 {
        return None;
    }
    let max = u32::try_from(max_realtime_priority).ok()?;
    Some(max.min(RT_PRIORITY_CAP))
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
}
