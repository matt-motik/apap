//! Обнаружение подключения/отключения устройств (ADR-16, ТЗ-105, §2.3).
//! Тестируемое ядро [`PollWatcher`] не зависит от ОС; системный снимок —
//! `/proc/asound/cards` на Linux, на прочих ОС — [`NullDeviceWatcher`]
//! (обновление только по команде «Обновить», ADR-16).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::backend::catalog::DeviceWatcher;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Шаг сна цикла опроса: `drop` должен завершиться быстро, поэтому ждём
/// снимками не длиннее этого значения (ADR-16).
const PARK_SLICE: Duration = Duration::from_millis(50);

/// Опрос снимка устройств в отдельном потоке `apap-devwatch` (ADR-16,
/// ТЗ-105). `F` — функция снимка: `None` означает, что снимок сейчас
/// недоступен (например, файл не читается); такой снимок не считается
/// изменением относительно предыдущего `None`.
pub struct PollWatcher<F> {
    snapshot: Option<F>,
    interval: Duration,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl<F> PollWatcher<F>
where
    F: FnMut() -> Option<String> + Send + 'static,
{
    pub fn new(snapshot: F, interval: Duration) -> Self {
        Self {
            snapshot: Some(snapshot),
            interval,
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl<F> DeviceWatcher for PollWatcher<F>
where
    F: FnMut() -> Option<String> + Send + 'static,
{
    fn start(&mut self, notify: Box<dyn Fn() + Send>) {
        if self.handle.is_some() {
            return;
        }
        let Some(mut snapshot) = self.snapshot.take() else {
            return;
        };
        let stop = Arc::clone(&self.stop);
        let interval = self.interval;
        let job = move || {
            let mut last = snapshot();
            while !stop.load(Ordering::Relaxed) {
                let mut remaining = interval;
                while remaining > Duration::ZERO && !stop.load(Ordering::Relaxed) {
                    let slice = remaining.min(PARK_SLICE);
                    thread::park_timeout(slice);
                    remaining = remaining.saturating_sub(slice);
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let current = snapshot();
                if current != last {
                    notify();
                    last = current;
                }
            }
        };
        if let Ok(h) = thread::Builder::new().name("apap-devwatch".into()).spawn(job) {
            self.handle = Some(h);
        }
    }
}

impl<F> Drop for PollWatcher<F> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.thread().unpark();
            let _ = h.join();
        }
    }
}

/// Реализация `DeviceWatcher` для ОС без опроса (ADR-16): `start` ничего не
/// делает, события никогда не приходят, обновление — по команде «Обновить».
#[derive(Default)]
pub struct NullDeviceWatcher;

impl DeviceWatcher for NullDeviceWatcher {
    fn start(&mut self, _notify: Box<dyn Fn() + Send>) {}
}

/// Снимок `/proc/asound/cards` (ADR-16): несколько сотен байт, `None` если
/// файл недоступен (например, нет звуковых карт или нет `/proc`).
#[cfg(target_os = "linux")]
fn asound_cards_snapshot() -> Option<String> {
    std::fs::read_to_string("/proc/asound/cards").ok()
}

/// `DeviceWatcher` для текущей ОС (ADR-16, ТЗ-105): на Linux — опрос
/// `/proc/asound/cards` раз в 1 с, на прочих ОС — [`NullDeviceWatcher`].
pub fn system_device_watcher() -> Box<dyn DeviceWatcher> {
    #[cfg(target_os = "linux")]
    {
        Box::new(PollWatcher::new(asound_cards_snapshot, Duration::from_secs(1)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Box::new(NullDeviceWatcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    fn poll_until(deadline: Duration, mut pred: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        pred()
    }

    #[test]
    fn devwatch_fires_only_on_change() {
        let value = Arc::new(AtomicUsize::new(0));
        let reader = Arc::clone(&value);
        let snapshot = move || Some(reader.load(Ordering::Relaxed).to_string());
        let mut watcher = PollWatcher::new(snapshot, Duration::from_millis(10));
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        watcher.start(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));

        thread::sleep(Duration::from_millis(100));
        assert_eq!(count.load(Ordering::Relaxed), 0);

        value.store(1, Ordering::Relaxed);
        let fired = poll_until(Duration::from_millis(500), || count.load(Ordering::Relaxed) >= 1);
        assert!(fired);
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn devwatch_start_twice_is_ignored() {
        let mut watcher = PollWatcher::new(|| Some("x".to_string()), Duration::from_millis(10));
        let count = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&count);
        watcher.start(Box::new(move || {
            c1.fetch_add(1, Ordering::Relaxed);
        }));
        let c2 = Arc::clone(&count);
        watcher.start(Box::new(move || {
            c2.fetch_add(100, Ordering::Relaxed);
        }));
        thread::sleep(Duration::from_millis(50));
        assert!(count.load(Ordering::Relaxed) < 100);
    }

    #[test]
    fn devwatch_drop_stops_thread() {
        let mut watcher = PollWatcher::new(|| Some("x".to_string()), Duration::from_secs(10));
        watcher.start(Box::new(|| {}));
        let start = Instant::now();
        drop(watcher);
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn devwatch_null_never_fires() {
        let mut watcher = NullDeviceWatcher;
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        watcher.start(Box::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
        }));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn devwatch_system_watcher_starts() {
        let mut watcher = system_device_watcher();
        watcher.start(Box::new(|| {}));
        drop(watcher);
    }
}
