//! Поток `apap-signals`: ловит SIGTERM/SIGINT/SIGHUP на отдельном
//! однопоточном рантайме tokio, не дожидаясь UI (ADR-7 п. 3, ТЗ-14, ТЗ-15).
//! Первый сигнал любого вида запускает путь выхода через `on_first`; второй
//! сигнал (любого из трёх видов) завершает процесс напрямую из этого потока
//! кодом `128 + номер_сигнала`.

use super::{ExitEntry, Lifecycle, PlatformCaps, PlatformError, ProcessExit, TrayEvent};
use crate::audio::clock::Clock;
use crate::core::exit::{ExitReason, TermSignal};
use crate::platform::tray::{self, TrayPort};
use std::cell::RefCell;
use std::sync::mpsc::Sender;
use tokio::signal::unix::{signal, Signal, SignalKind};

thread_local! {
    /// `ExitEntry` UI-потока: сигнал доставляется в него через
    /// `invoke_from_event_loop` (`Rc` не пересекает границу потоков, ADR-7 п. 1).
    static EXIT_ENTRY: RefCell<Option<ExitEntry>> = const { RefCell::new(None) };
}

/// Жизненный цикл Linux/macOS (ADR-7): поток `apap-signals`, перехват
/// завершения macOS, затем трей.
pub struct UnixLifecycle {
    caps: PlatformCaps,
    signals: Option<std::thread::JoinHandle<()>>,
    /// Трей запускается в `install` (ADR-23 шаг 8); `clock` — метки `TrayScroll`.
    tray: Option<(TrayPort, Box<dyn Clock>)>,
}

impl UnixLifecycle {
    pub fn new(tray: Option<(TrayPort, Box<dyn Clock>)>) -> UnixLifecycle {
        UnixLifecycle { caps: PlatformCaps::default(), signals: None, tray }
    }
}

impl Lifecycle for UnixLifecycle {
    fn caps(&self) -> PlatformCaps {
        self.caps
    }

    /// Шаг 8 порядка запуска (ADR-23): `ExitEntry` — в UI-поток, поток
    /// `apap-signals` (первый сигнал → `ExitEntry(Signal)`, ТЗ-14), на macOS —
    /// `applicationShouldTerminate:` (ADR-7 п. 5), трей.
    /// Готовность трея приходит в `TrayPort::ready` (`PlatformCaps.tray`).
    fn install(
        &mut self,
        _window: Option<&slint::Window>,
        entry: ExitEntry,
        tray_tx: Sender<TrayEvent>,
    ) -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        let mac_entry = entry.clone();
        EXIT_ENTRY.with(|e| *e.borrow_mut() = Some(entry));
        let on_first: Box<dyn Fn(TermSignal) + Send> = Box::new(|sig| {
            let _ = slint::invoke_from_event_loop(move || {
                let entry = EXIT_ENTRY.with(|e| e.borrow().clone());
                if let Some(entry) = entry {
                    entry(ExitReason::Signal(sig));
                }
            });
        });
        self.signals = Some(spawn_signals(on_first, process_exit())?);
        // Метод делегата `applicationShouldTerminate:` (ADR-7 п. 5, ТЗ-15).
        #[cfg(target_os = "macos")]
        super::macos::install_terminate(mac_entry)?;
        if let Some((port, clock)) = self.tray.take() {
            tray::run(port, tray_tx, clock);
        }
        Ok(())
    }
}

/// Код завершения процесса по второму сигналу (ТЗ-14): `128 + номер сигнала`.
fn exit_code(sig: TermSignal) -> i32 {
    128 + match sig {
        TermSignal::Term => libc::SIGTERM,
        TermSignal::Int => libc::SIGINT,
        TermSignal::Hup => libc::SIGHUP,
    }
}

/// Настоящий `ProcessExit`: завершает процесс немедленно (ADR-7 п. 3, ТЗ-14).
pub fn process_exit() -> ProcessExit {
    Box::new(|code| std::process::exit(code))
}

/// Запускает поток `apap-signals` (ADR-7 п. 3, ТЗ-14, ТЗ-15). Регистрирует
/// обработчики сигналов на вызывающем потоке — чтобы сигналы уже перехватывались
/// к моменту возврата функции — и переносит рантайм вместе с ними в новый
/// поток. `on_first` вызывается на первый сигнал (ожидается планирование
/// `invoke_from_event_loop` путём выхода вызывающей стороны); `exit`
/// вызывается прямо из потока `apap-signals` на второй сигнал, без ожидания UI.
pub fn spawn_signals(
    on_first: Box<dyn Fn(TermSignal) + Send>,
    exit: ProcessExit,
) -> Result<std::thread::JoinHandle<()>, PlatformError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PlatformError { what: "сигналы", detail: e.to_string().into() })?;

    let guard = rt.enter();
    let term = signal(SignalKind::terminate())
        .map_err(|e| PlatformError { what: "сигналы", detail: e.to_string().into() })?;
    let int = signal(SignalKind::interrupt())
        .map_err(|e| PlatformError { what: "сигналы", detail: e.to_string().into() })?;
    let hup = signal(SignalKind::hangup())
        .map_err(|e| PlatformError { what: "сигналы", detail: e.to_string().into() })?;
    drop(guard);

    std::thread::Builder::new()
        .name("apap-signals".into())
        .spawn(move || rt.block_on(run_loop(term, int, hup, on_first, exit)))
        .map_err(|e| PlatformError { what: "сигналы", detail: e.to_string().into() })
}

/// Цикл обработки сигналов на потоке `apap-signals` (ТЗ-14).
async fn run_loop(
    mut term: Signal,
    mut int: Signal,
    mut hup: Signal,
    on_first: Box<dyn Fn(TermSignal) + Send>,
    exit: ProcessExit,
) {
    let mut first_done = false;
    loop {
        let sig = tokio::select! {
            s = term.recv() => s.map(|()| TermSignal::Term),
            s = int.recv() => s.map(|()| TermSignal::Int),
            s = hup.recv() => s.map(|()| TermSignal::Hup),
        };
        let Some(sig) = sig else { return };
        if first_done {
            exit(exit_code(sig));
            return;
        }
        on_first(sig);
        first_done = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Двойной сигнал → немедленное завершение (§7.2, ТЗ-14): UI «завис» на
    /// записи (первый запрос выхода не обработан), второй SIGTERM вызывает
    /// подменённый `ProcessExit(128 + 15)` прямо из потока `apap-signals`.
    #[test]
    fn double_signal_exits_immediately() {
        let (first_tx, first_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let handle = spawn_signals(
            Box::new(move |sig| {
                let _ = first_tx.send(sig);
            }),
            Box::new(move |code| {
                let _ = exit_tx.send(code);
            }),
        )
        .expect("регистрация сигналов");

        // SAFETY: kill(getpid(), SIGTERM) — сигнал самому процессу; обработчик
        // tokio уже установлен spawn_signals, процесс не завершается.
        let send_term = || unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };

        assert_eq!(send_term(), 0);
        assert_eq!(first_rx.recv_timeout(Duration::from_secs(5)), Ok(TermSignal::Term));
        // Запрос выхода «завис» в UI (запись 10 с): ответа нет, ждём только apap-signals.
        assert!(exit_rx.try_recv().is_err());

        assert_eq!(send_term(), 0);
        assert_eq!(exit_rx.recv_timeout(Duration::from_secs(1)), Ok(143));
        handle.join().expect("поток apap-signals");
    }
}
