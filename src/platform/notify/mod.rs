//! Системные уведомления (ADR-9, §2.8). На этом этапе (С2 (02)) — только
//! тип `Notification`; трейт `Notifier` и реализация D-Bus (Linux) приходят
//! на этапе С5 (ADR-9).

use crate::core::messages::MessageLevel;

/// Содержимое системного уведомления (ADR-9).
#[derive(Clone, PartialEq, Debug)]
pub struct Notification {
    pub level: MessageLevel,
    pub title: Box<str>,
    pub body: Box<str>,
}
