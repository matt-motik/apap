//! `FakeNotifier` — подмена `Notifier` для автотестов (ADR-6, ADR-9).
//! Компилируется всегда, без `cfg(test)`: тесты бинарника собирают библиотеку
//! без `cfg(test)`. Записывает все вызовы `notify`; клоны делят один журнал.

use super::{Notification, Notifier};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct FakeNotifier {
    calls: Arc<Mutex<Vec<Notification>>>,
}

impl FakeNotifier {
    pub fn new() -> FakeNotifier {
        FakeNotifier::default()
    }

    /// Все записанные уведомления в порядке вызовов.
    pub fn calls(&self) -> Vec<Notification> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn count(&self) -> usize {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Notifier for FakeNotifier {
    fn notify(&self, n: Notification) {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(n);
    }
}
