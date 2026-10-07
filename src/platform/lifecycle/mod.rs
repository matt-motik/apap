//! Жизненный цикл приложения: возможности платформы, путь выхода, сигналы,
//! трей (ADR-6, ADR-7, §2.8). Трейт `Lifecycle` ставит обработчики ОС на
//! шаге 8 порядка запуска (ADR-23); путь выхода входит через `ExitEntry`.

use crate::audio::clock::ClockInstant;
use crate::core::exit::{ExitOutcome, ExitReason};
use std::rc::Rc;
use std::sync::mpsc::Sender;

/// Возможности платформы во время работы: код приложения проверяет
/// возможность, а не ОС (ADR-6, §2 заход 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PlatformCaps {
    pub tray: bool,
    pub notifications: bool,
}

/// Точка входа пути выхода в UI-потоке (ADR-7, §6.10). Возвращает управление
/// после записи или по сроку 5 с.
pub type ExitEntry = Rc<dyn Fn(ExitReason) -> ExitOutcome>;

/// Установка обработчиков жизненного цикла ОС (ADR-7, ТЗ-54 п.1).
pub trait Lifecycle {
    fn caps(&self) -> PlatformCaps;
    /// Шаг 8 порядка запуска (ADR-23): сигналы, сабкласс HWND, метод делегата
    /// macOS, трей. `window` — `None` у подмены.
    fn install(
        &mut self,
        window: Option<&slint::Window>,
        entry: ExitEntry,
        tray_tx: Sender<TrayEvent>,
    ) -> Result<(), PlatformError>;
}

/// Событие трея (Linux, §2.8). `Quit` → путь выхода; `Scroll` → модуль ввода;
/// остальное — команды плеера.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TrayEvent {
    TogglePlay,
    Stop,
    Prev,
    Next,
    ShowHide,
    Quit,
    Scroll(TrayScroll),
}

/// Прокрутка над значком трея ksni (Linux, ADR-8).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TrayScroll {
    pub t: ClockInstant,
    pub delta: i32,
    pub vertical: bool,
}

/// Завершение процесса при втором сигнале; подменяется в тестах
/// `apap-signals` (ADR-7 п. 3, ТЗ-14).
pub type ProcessExit = Box<dyn Fn(i32) + Send>;

/// Ошибка установки обработчика платформы; пишется в журнал (ADR-13).
#[derive(Clone, PartialEq, Debug)]
pub struct PlatformError {
    pub what: &'static str,
    pub detail: Box<str>,
}

pub mod fake;
pub use fake::FakeLifecycle;

#[cfg(unix)]
pub mod unix;
