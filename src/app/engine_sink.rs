//! Продакшен-реализация `EventSink` для Slint (ADR-02, вариант В; ТЗ-60,
//! ТЗ-106, ОВ-24).
//!
//! Движок кладёт события в `std::sync::mpsc` и, если флаг `wake_pending`
//! был снят, будит UI-поток через `slint::invoke_from_event_loop` пустым
//! `Send`-замыканием. Замыкание в UI-потоке запускает слив очереди через
//! thread-local хук (`install_drain_hook`); существующий тик 100 мс —
//! резервный слив на случай пропущенного будильника. Опустошённая очередь
//! стоит одного `try_recv`. Доступа к `Rc` из чужого потока здесь нет —
//! хук работает строго в UI-потоке.
//!
//! Подключение (установка хука, обновление `UiAudioState`, Slint-свойств)
//! делается в шаге 28; этот модуль — только мост-инфраструктура, поэтому
//! `#![allow(dead_code)]`.
#![allow(dead_code)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use music_player_rs::engine::messages::EngineEvent;
use music_player_rs::engine::sink::EventSink;

/// `EventSink`, пишущий события в mpsc-канал и будящий UI-поток не чаще,
/// чем раз на цикл «эмиты — слив» (ADR-02).
pub(crate) struct SlintEventSink {
    tx: mpsc::Sender<EngineEvent>,
    wake_pending: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSink for SlintEventSink {
    fn emit(&self, ev: EngineEvent) {
        // UI-поток мог уже завершиться (закрытие окна/шатдаун) — очередь
        // получателя не читает, `send` возвращает `SendError`. Это не
        // ошибка движка: событие просто некому доставить, паниковать или
        // логировать из потока движка нельзя (§3 правил, ADR-02).
        let _ = self.tx.send(ev);

        // Будим UI-поток только на переходе «очередь была пуста из
        // перспективы UI» → «есть что слить», иначе каждый `emit` на
        // активном треке порождал бы лишний `invoke_from_event_loop`.
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }
}

/// Приёмная сторона канала: живёт в UI-потоке, сливает накопленные
/// события в порядке отправки.
pub(crate) struct EngineEventQueue {
    rx: mpsc::Receiver<EngineEvent>,
    wake_pending: Arc<AtomicBool>,
}

impl EngineEventQueue {
    /// Слить все накопленные события по порядку, вызвав `f` на каждом.
    ///
    /// Флаг `wake_pending` сбрасывается ПЕРВЫМ, до чтения `rx` — иначе
    /// между последним `try_recv` и сбросом флага возможна гонка: движок
    /// кладёт новое событие и видит `wake_pending == true` (ещё не
    /// сброшен), не будит UI, а `drain` уже вычитал канал до этого
    /// события и больше не вернётся за ним сам. Сброс до чтения гарантирует,
    /// что любое событие, не попавшее в текущий `try_recv`-цикл, вызовет
    /// будильник при следующем `emit` (И-Р13, И-Р14).
    pub(crate) fn drain(&self, mut f: impl FnMut(EngineEvent)) {
        self.wake_pending.store(false, Ordering::Release);
        while let Ok(ev) = self.rx.try_recv() {
            f(ev);
        }
    }
}

/// Завести канал движок → UI. `wake` вызывается из потока движка при
/// переходе очереди «пуста для UI» → «есть событие»; продакшен передаёт
/// `slint_wake()`, тесты — считающее замыкание.
pub(crate) fn event_channel(
    wake: Arc<dyn Fn() + Send + Sync>,
) -> (SlintEventSink, EngineEventQueue) {
    let (tx, rx) = mpsc::channel();
    let wake_pending = Arc::new(AtomicBool::new(false));
    let sink = SlintEventSink { tx, wake_pending: wake_pending.clone(), wake };
    let queue = EngineEventQueue { rx, wake_pending };
    (sink, queue)
}

/// Будильник для продакшена: просит event loop Slint выполнить
/// `run_drain_hook` в UI-потоке.
pub(crate) fn slint_wake() -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(|| {
        // `Err` означает, что event loop уже остановлен (шатдаун/окно
        // закрыто) — будильник просто теряется, тик 100 мс как резерв
        // здесь не нужен, т.к. приложение в этот момент завершается.
        let _ = slint::invoke_from_event_loop(run_drain_hook);
    })
}

thread_local! {
    /// Хук слива очереди, выставляется в UI-потоке из `main` (там же —
    /// `Weak<RefCell<MusicApp>>`, `try_borrow_mut`: если занято, слив
    /// довершит тик 100 мс).
    static DRAIN_HOOK: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
}

/// Установить хук слива очереди. Вызывается один раз в `main` после
/// `MusicApp::init`.
pub(crate) fn install_drain_hook(f: Box<dyn Fn()>) {
    DRAIN_HOOK.with(|cell| {
        *cell.borrow_mut() = Some(f);
    });
}

/// Выполнить установленный хук слива. Не паникует при реентрантности
/// (`try_borrow`): если хук сам провоцирует повторный вызов, внутренний
/// вызов просто ничего не делает.
fn run_drain_hook() {
    DRAIN_HOOK.with(|cell| {
        let Ok(hook) = cell.try_borrow() else {
            return;
        };
        if let Some(f) = hook.as_ref() {
            f();
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    fn counting_wake() -> (Arc<dyn Fn() + Send + Sync>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });
        (wake, count)
    }

    #[test]
    fn many_emits_before_drain_wake_once() {
        let (wake, count) = counting_wake();
        let (sink, queue) = event_channel(wake);

        sink.emit(EngineEvent::ShutdownComplete);
        sink.emit(EngineEvent::Position { secs: 1.0 });
        sink.emit(EngineEvent::Position { secs: 2.0 });

        assert_eq!(count.load(Ordering::SeqCst), 1);

        let mut received = Vec::new();
        queue.drain(|ev| received.push(ev));
        assert_eq!(received.len(), 3);
    }

    #[test]
    fn emit_after_drain_wakes_again() {
        let (wake, count) = counting_wake();
        let (sink, queue) = event_channel(wake);

        sink.emit(EngineEvent::ShutdownComplete);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        queue.drain(|_| {});

        sink.emit(EngineEvent::ShutdownComplete);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn drain_delivers_events_in_order() {
        let (wake, _count) = counting_wake();
        let (sink, queue) = event_channel(wake);

        sink.emit(EngineEvent::Position { secs: 1.0 });
        sink.emit(EngineEvent::Position { secs: 2.0 });
        sink.emit(EngineEvent::Position { secs: 3.0 });

        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        queue.drain(move |ev| {
            if let EngineEvent::Position { secs } = ev {
                received_clone.lock().unwrap().push(secs);
            }
        });

        assert_eq!(*received.lock().unwrap(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn emit_from_another_thread_is_received() {
        let (wake, count) = counting_wake();
        let (sink, queue) = event_channel(wake);

        let handle = std::thread::spawn(move || {
            let boxed: Box<dyn EventSink> = Box::new(sink);
            boxed.emit(EngineEvent::ShutdownComplete);
        });
        handle.join().expect("thread panicked");

        assert_eq!(count.load(Ordering::SeqCst), 1);

        let mut received = Vec::new();
        queue.drain(|ev| received.push(ev));
        assert_eq!(received.len(), 1);
        assert!(matches!(received[0], EngineEvent::ShutdownComplete));
    }

    #[test]
    fn emit_after_queue_dropped_does_not_panic() {
        let (wake, _count) = counting_wake();
        let (sink, queue) = event_channel(wake);
        drop(queue);

        sink.emit(EngineEvent::ShutdownComplete);
    }
}
