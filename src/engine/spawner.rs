//! Запуск потоков движка (§2.10, ТЗ-88, ADR-20). Отказ `spawn` сообщением
//! вместо тишины: `EngineFault::SpawnFailed`, у движка — `OpenFailed(Internal(..))`.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::error::EngineFault;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

/// Запуск именованного потока движка (ТЗ-88, ADR-20).
pub trait ThreadSpawner: Send + Sync {
    fn spawn(
        &self,
        name: &'static str,
        job: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<JoinHandle<()>, EngineFault>;
}

/// Боевая реализация поверх `std::thread::Builder` (ТЗ-88, ADR-20).
#[derive(Debug, Default, Clone, Copy)]
pub struct StdSpawner;

impl ThreadSpawner for StdSpawner {
    fn spawn(
        &self,
        name: &'static str,
        job: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<JoinHandle<()>, EngineFault> {
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(job)
            .map_err(|_| EngineFault::SpawnFailed { what: name })
    }
}

/// Фейк для тестов: падает ровно на N-м (1-based) вызове `spawn`, остальные
/// делегирует `StdSpawner` (§2.10, ADR-20).
#[derive(Debug, Default)]
pub struct FailingSpawner {
    fail_on: u64,
    calls: AtomicU64,
}

impl FailingSpawner {
    /// `fail_on` — номер вызова (считая с 1), на котором `spawn` вернёт ошибку.
    pub fn new(fail_on: u64) -> FailingSpawner {
        FailingSpawner { fail_on, calls: AtomicU64::new(0) }
    }

    /// Сколько раз был вызван `spawn`.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Acquire)
    }
}

impl ThreadSpawner for FailingSpawner {
    fn spawn(
        &self,
        name: &'static str,
        job: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<JoinHandle<()>, EngineFault> {
        let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        if call == self.fail_on {
            return Err(EngineFault::SpawnFailed { what: name });
        }
        StdSpawner.spawn(name, job)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::sync::Arc;

    #[test]
    fn std_spawner_runs_job_and_names_thread() {
        let (tx, rx) = mpsc::channel();
        let handle = StdSpawner
            .spawn(
                "apap-decode",
                Box::new(move || {
                    let name = std::thread::current().name().map(str::to_owned);
                    tx.send(name).expect("send thread name");
                }),
            )
            .expect("spawn ok");
        let name = rx.recv().expect("recv thread name");
        assert_eq!(name.as_deref(), Some("apap-decode"));
        handle.join().expect("join ok");
    }

    #[test]
    fn failing_spawner_fails_only_on_nth_call() {
        let spawner = FailingSpawner::new(2);
        let ran = Arc::new(AtomicBool::new(false));

        let handle1 = spawner.spawn("apap-decode", Box::new(|| {})).expect("call1 ok");
        handle1.join().expect("join1 ok");

        let ran2 = Arc::clone(&ran);
        let result2 = spawner.spawn(
            "apap-decode",
            Box::new(move || {
                ran2.store(true, Ordering::Release);
            }),
        );
        match result2 {
            Err(EngineFault::SpawnFailed { what: "apap-decode" }) => {}
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
        assert!(!ran.load(Ordering::Acquire));

        let handle3 = spawner.spawn("apap-decode", Box::new(|| {})).expect("call3 ok");
        handle3.join().expect("join3 ok");

        assert_eq!(spawner.calls(), 3);
    }
}
