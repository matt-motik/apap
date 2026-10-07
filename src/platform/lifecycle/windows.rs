//! Жизненный цикл Windows (ADR-7 п. 4, ТЗ-15): сабклассинг HWND окна Slint
//! через `SetWindowSubclass` (comctl32). winit 0.30 не обрабатывает
//! `WM_QUERYENDSESSION`/`WM_ENDSESSION`, а `with_msg_hook` не видит посланных
//! сообщений, поэтому завершение сеанса ловит процедура сабкласса.

use super::{ExitEntry, Lifecycle, PlatformCaps, PlatformError, TrayEvent};
use crate::core::exit::ExitReason;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;
use std::sync::mpsc::Sender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{WM_ENDSESSION, WM_QUERYENDSESSION};

/// Идентификатор сабкласса окна плеера (ADR-7 п. 4).
const SUBCLASS_ID: usize = 1;

thread_local! {
    /// `ExitEntry` UI-потока: процедура сабкласса вызывается в UI-потоке
    /// во время диспетчеризации (ADR-7 п. 4).
    static EXIT_ENTRY: RefCell<Option<ExitEntry>> = const { RefCell::new(None) };
}

/// Жизненный цикл Windows (ADR-7): трея нет (В-1), сигналов нет.
pub struct WindowsLifecycle {
    caps: PlatformCaps,
}

impl WindowsLifecycle {
    pub fn new() -> WindowsLifecycle {
        WindowsLifecycle { caps: PlatformCaps::default() }
    }
}

impl Default for WindowsLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle for WindowsLifecycle {
    fn caps(&self) -> PlatformCaps {
        self.caps
    }

    /// Шаг 8 порядка запуска (ADR-23): после первого показа окна — HWND через
    /// `window_handle()` и `SetWindowSubclass` (ADR-7 п. 4, ТЗ-15).
    fn install(
        &mut self,
        window: Option<&slint::Window>,
        entry: ExitEntry,
        _tray_tx: Sender<TrayEvent>,
    ) -> Result<(), PlatformError> {
        let Some(window) = window else { return Ok(()) };
        let slint_handle = window.window_handle();
        let handle = slint_handle.window_handle().map_err(|e| PlatformError {
            what: "сабкласс окна",
            detail: e.to_string().into(),
        })?;
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return Err(PlatformError { what: "сабкласс окна", detail: "окно не Win32".into() });
        };
        let hwnd = win32.hwnd.get() as HWND;
        EXIT_ENTRY.with(|e| *e.borrow_mut() = Some(entry));
        // SAFETY: `hwnd` — живое окно этого (UI) потока; процедура сабкласса
        // не использует `dwrefdata`, сабкласс снимается вместе с окном.
        let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) };
        if ok == 0 {
            EXIT_ENTRY.with(|e| *e.borrow_mut() = None);
            return Err(PlatformError {
                what: "сабкласс окна",
                detail: std::io::Error::last_os_error().to_string().into(),
            });
        }
        Ok(())
    }
}

/// Процедура сабкласса (ADR-7 п. 4, ТЗ-15): `WM_QUERYENDSESSION` → `TRUE`
/// без пути выхода; `WM_ENDSESSION` с `wParam = TRUE` → синхронный путь
/// выхода (≤ 5 с), затем 0; остальное — `DefSubclassProc`.
unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            let entry = EXIT_ENTRY.with(|e| e.borrow().clone());
            if let Some(entry) = entry {
                entry(ExitReason::WindowsSessionEnd);
            }
            0
        }
        // SAFETY: параметры пришли от системы в эту же процедуру.
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}
