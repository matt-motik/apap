//! Системные уведомления (ADR-9, §2.8). Этап С5: трейт `Notifier`.
//! Реализации: `none` — пустая, для ОС без системных уведомлений (Windows,
//! macOS); `fake` — подмена для автотестов; `linux` — D-Bus `Notify` по
//! сессионной шине (ADR-9, ТЗ-52 п.2).

use crate::core::messages::MessageLevel;

/// Содержимое системного уведомления (ADR-9).
#[derive(Clone, PartialEq, Debug)]
pub struct Notification {
    pub level: MessageLevel,
    pub title: Box<str>,
    pub body: Box<str>,
}

/// Системные уведомления (ADR-9, §2.8). Реализация не блокирует вызывающий
/// поток; ошибка доставки — только в журнал внутри реализации (ADR-13).
pub trait Notifier: Send {
    fn notify(&self, n: Notification);
}

pub mod fake;
pub mod none;
pub use fake::FakeNotifier;
pub use none::NoneNotifier;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxNotifier;

/// Слот рантайма tokio потока трея (ADR-9): заполняет поток трея после
/// регистрации значка, читает `LinuxNotifier`.
pub type RtSlot = std::sync::Arc<std::sync::OnceLock<tokio::runtime::Handle>>;

/// Notifier платформы (ADR-6, ADR-9, ТЗ-54 п.4): на Linux — `LinuxNotifier`
/// на рантайме трея из `rt`, на прочих ОС — `NoneNotifier`.
pub fn platform_notifier(
    rt: RtSlot,
    journal: std::sync::Arc<dyn crate::journal::Journal>,
) -> Box<dyn Notifier> {
    #[cfg(target_os = "linux")]
    return Box::new(LinuxNotifier::new(rt, journal));
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rt, journal);
        Box::new(NoneNotifier)
    }
}
