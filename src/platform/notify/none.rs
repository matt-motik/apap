//! Пустая реализация `Notifier` для ОС без системных уведомлений (Windows,
//! macOS: `PlatformCaps.notifications = false`, ADR-9, ТЗ-54 п.4). Компилируется
//! всегда, без `cfg` (ADR-6).

use super::{Notification, Notifier};

#[derive(Default, Debug, Clone, Copy)]
pub struct NoneNotifier;

impl Notifier for NoneNotifier {
    fn notify(&self, _n: Notification) {}
}
