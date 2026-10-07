//! Приёмник событий потока apap-engine (§2.8, ADR-02, ADR-20). Движок общается
//! с внешним миром только через `EventSink`: продакшен-реализация (прикладной
//! крейт) кладёт события в mpsc и будит Slint, тестовая (`VecSink`) копит их
//! в `Vec` для проверки в тестах. Компилируется всегда (не `cfg(test)`), как
//! остальные фейки в `src/audio/testing/mod.rs` — ими пользуются bin-тесты,
//! собирающие библиотеку без `cfg(test)`.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::sync::{Arc, Mutex};

use super::messages::EngineEvent;

/// Получатель событий движка. Вызывается только из потока движка (ADR-02);
/// реализация не должна надолго блокироваться, иначе задержит цикл движка.
pub trait EventSink: Send {
    fn emit(&self, ev: EngineEvent);
}

/// Фейк `EventSink` для тестов: копит события в `Vec` за `Mutex`. Клоны
/// делят одно и то же хранилище (`Arc`), поэтому тест держит один клон, а
/// движок — другой (как `Box<dyn EventSink>`).
#[derive(Clone, Default)]
pub struct VecSink {
    events: Arc<Mutex<Vec<EngineEvent>>>,
}

impl VecSink {
    pub fn new() -> VecSink {
        VecSink::default()
    }

    /// Забрать накопленные события, опустошив хранилище.
    pub fn take(&self) -> Vec<EngineEvent> {
        let mut guard = self.events.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    }

    pub fn len(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }
}

impl EventSink for VecSink {
    fn emit(&self, ev: EngineEvent) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(ev);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::messages::TransportState;

    #[test]
    fn vec_sink_collects_from_another_thread() {
        let sink = VecSink::new();
        let sink_clone = sink.clone();
        let handle = std::thread::spawn(move || {
            let boxed: Box<dyn EventSink> = Box::new(sink_clone);
            boxed.emit(EngineEvent::ShutdownComplete);
            boxed.emit(EngineEvent::Transport { state: TransportState::Playing });
        });
        handle.join().expect("thread panicked");

        let collected = sink.take();
        assert_eq!(collected.len(), 2);
        assert!(matches!(collected[0], EngineEvent::ShutdownComplete));
        assert!(matches!(
            collected[1],
            EngineEvent::Transport { state: TransportState::Playing }
        ));
        assert!(sink.is_empty());
    }
}
