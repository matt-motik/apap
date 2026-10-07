//! `LinuxNotifier` — D-Bus `Notify` по сессионной шине freedesktop
//! (`org.freedesktop.Notifications`, ADR-9, ТЗ-52 п.2). Вызов не блокирует
//! поток трея: задача порождается на рантайме tokio, ошибка D-Bus — только в
//! журнал (ADR-13). Только Linux.

use super::{Notification, Notifier, RtSlot};
use crate::core::messages::MessageLevel;
use crate::journal::{Journal, JournalRecord};
use std::collections::HashMap;
use std::sync::Arc;
use zbus::zvariant::Value;

const NOTIFICATIONS_DEST: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_IFACE: &str = "org.freedesktop.Notifications";

/// `Notifier` для Linux: `Notify` на сессионной шине (ADR-9, ТЗ-52 п.2).
/// Рантайм — потока трея (ADR-9): слот заполняется после регистрации значка.
pub struct LinuxNotifier {
    rt: RtSlot,
    journal: Arc<dyn Journal>,
}

impl LinuxNotifier {
    pub fn new(rt: RtSlot, journal: Arc<dyn Journal>) -> LinuxNotifier {
        LinuxNotifier { rt, journal }
    }
}

impl Notifier for LinuxNotifier {
    /// Слот пуст — трея нет, окно в трей не скрывается и уведомление не
    /// нужно (ADR-9, `PlatformCaps.notifications = false`).
    fn notify(&self, n: Notification) {
        let Some(rt) = self.rt.get() else {
            return;
        };
        let journal = Arc::clone(&self.journal);
        rt.spawn(async move {
            if let Err(e) = send(&n).await {
                journal.record(JournalRecord::Notify { error: e.to_string().into() });
            }
        });
    }
}

/// Уровень важности для хинта `urgency` спецификации freedesktop.
fn urgency(level: MessageLevel) -> u8 {
    match level {
        MessageLevel::Info => 0,
        MessageLevel::Warning => 1,
        MessageLevel::Error => 2,
    }
}

/// Один вызов `Notify` по сессионной шине (ADR-9, ТЗ-52 п.2).
async fn send(n: &Notification) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("urgency", Value::U8(urgency(n.level)));
    let body = (
        "apap",
        0u32,
        "",
        n.title.as_ref(),
        n.body.as_ref(),
        Vec::<&str>::new(),
        hints,
        -1i32,
    );
    conn.call_method(
        Some(NOTIFICATIONS_DEST),
        NOTIFICATIONS_PATH,
        Some(NOTIFICATIONS_IFACE),
        "Notify",
        &body,
    )
    .await?;
    Ok(())
}
