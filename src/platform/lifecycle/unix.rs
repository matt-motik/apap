//! Поток `apap-signals`: ловит SIGTERM/SIGINT/SIGHUP на отдельном
//! однопоточном рантайме tokio, не дожидаясь UI (ADR-7 п. 3, ТЗ-14, ТЗ-15).
//! Первый сигнал любого вида запускает путь выхода через `on_first`; второй
//! сигнал (любого из трёх видов) завершает процесс напрямую из этого потока
//! кодом `128 + номер_сигнала`.

use super::{PlatformError, ProcessExit};
use crate::core::exit::TermSignal;
use tokio::signal::unix::{signal, Signal, SignalKind};

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
