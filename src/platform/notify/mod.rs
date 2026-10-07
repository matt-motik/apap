//! Системные уведомления (ADR-9, §2.8). Этап С5: трейт `Notifier`.
//! Реализации: `none` — пустая, для ОС без системных уведомлений (Windows,
//! macOS); `fake` — подмена для автотестов; D-Bus (Linux) приходит следующим
//! шагом (ADR-9).

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
