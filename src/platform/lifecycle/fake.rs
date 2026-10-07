//! `FakeLifecycle` — подмена жизненного цикла для автотестов (ADR-6, §2.8):
//! возможности задаёт тест, `fire` вызывает установленный `ExitEntry`,
//! `tray` шлёт событие трея как поток трея.

use super::{ExitEntry, Lifecycle, PlatformCaps, PlatformError, TrayEvent};
use crate::core::exit::{ExitOutcome, ExitReason};
use std::sync::mpsc::Sender;

/// Подмена `Lifecycle` (ADR-6). По умолчанию — без трея и уведомлений.
#[derive(Default)]
pub struct FakeLifecycle {
    pub caps: PlatformCaps,
    entry: Option<ExitEntry>,
    tray_tx: Option<Sender<TrayEvent>>,
}

impl FakeLifecycle {
    /// Без трея и системных уведомлений (подмена по умолчанию в `Harness`).
    pub fn new() -> FakeLifecycle {
        FakeLifecycle::default()
    }

    /// «С треем»: `caps.tray` и `caps.notifications` включены (ADR-6).
    pub fn with_tray() -> FakeLifecycle {
        FakeLifecycle {
            caps: PlatformCaps { tray: true, notifications: true },
            ..FakeLifecycle::default()
        }
    }

    /// Запрос выхода от «ОС»: вызывает `ExitEntry`; `None` — `install` не был вызван.
    pub fn fire(&self, reason: ExitReason) -> Option<ExitOutcome> {
        self.entry.as_ref().map(|entry| entry(reason))
    }

    /// Событие «потока трея»; `false` — канал не установлен или закрыт.
    pub fn tray(&self, ev: TrayEvent) -> bool {
        self.tray_tx.as_ref().is_some_and(|tx| tx.send(ev).is_ok())
    }
}

impl Lifecycle for FakeLifecycle {
    fn caps(&self) -> PlatformCaps {
        self.caps
    }

    fn install(
        &mut self,
        _window: Option<&slint::Window>,
        entry: ExitEntry,
        tray_tx: Sender<TrayEvent>,
    ) -> Result<(), PlatformError> {
        self.entry = Some(entry);
        self.tray_tx = Some(tray_tx);
        Ok(())
    }
}
