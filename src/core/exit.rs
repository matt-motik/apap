//! Примитивы пути выхода (ADR-7, ТЗ-14, ТЗ-32, НФ-9, §2.11, §6.10).
//!
//! Этот шаг даёт только типы и инжектируемое ожидание ответа писателя;
//! сборку в `exit()` (`AppCore`/`MusicApp`) делают следующие микро-шаги.

use crate::audio::clock::{Clock, ClockInstant};
use crate::audio::testing::ManualClock;
use crate::persist::writer::WriterReply;
use crate::persist::WorkFile;
use crate::platform::fs::{MemStore, WriteError};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// Срок пути выхода (НФ-9, §6.10).
pub const EXIT_BUDGET: Duration = Duration::from_secs(5);

/// Причина выхода (ТЗ-14).
///
/// ОТКЛОНЕНИЕ от §6.10: полный перечень причин спецификации — `WindowClose`,
/// `TrayQuit`, `Signal(TermSignal)`, `WindowsSessionEnd`, `MacosTerminate`.
/// На этом микро-шаге определены только `WindowClose` и `TrayQuit` — сигналы
/// (`apap-signals`) и завершение сессии ОС приходят вместе с жизненным
/// циклом платформы (`Lifecycle`) на этапе С5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitReason {
    WindowClose,
    /// Только при `PlatformCaps.tray` (В-1).
    TrayQuit,
}

/// Итог запроса на выход (§6.10).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitOutcome {
    Completed,
    /// Выход уже идёт: повторный запрос игнорируется (ТЗ-14, И-Т8).
    Ignored,
    /// Зарезервировано для обработчика ОС во вложенном цикле сообщений
    /// (ADR-7 п. 4, этап С5): приложение заимствовано, записи не было.
    Busy,
}

/// Фаза пути выхода (§6.10).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ExitPhase {
    #[default]
    Running,
    Exiting { started: ClockInstant, reason: ExitReason },
    Done,
}

/// Координатор пути выхода (ADR-7, §6.10): не даёт начать выход дважды
/// (ТЗ-14, И-Т8).
#[derive(Default)]
pub struct ExitCoordinator {
    phase: ExitPhase,
}

impl ExitCoordinator {
    /// Новый координатор в фазе `Running`.
    pub fn new() -> ExitCoordinator {
        ExitCoordinator::default()
    }

    pub fn phase(&self) -> ExitPhase {
        self.phase
    }

    /// Начинает выход по `reason`, если он ещё не идёт; возвращает срок
    /// `until = now + EXIT_BUDGET`. Повторный запрос во время `Exiting`/
    /// `Done` игнорируется — вызывающий код сам отображает `None` в
    /// `ExitOutcome::Ignored` (ТЗ-14, И-Т8).
    pub fn begin(&mut self, reason: ExitReason, now: ClockInstant) -> Option<ClockInstant> {
        if !matches!(self.phase, ExitPhase::Running) {
            return None;
        }
        self.phase = ExitPhase::Exiting { started: now, reason };
        Some(now.saturating_add(EXIT_BUDGET))
    }

    /// Завершает путь выхода: `Exiting` → `Done`. Вне `Exiting` — без эффекта.
    pub fn finish(&mut self) {
        if matches!(self.phase, ExitPhase::Exiting { .. }) {
            self.phase = ExitPhase::Done;
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.phase, ExitPhase::Running)
    }
}

/// Итог пути выхода — одна запись журнала (ТЗ-14, §6.10).
#[derive(Clone, PartialEq, Debug)]
pub struct ExitReport {
    pub reason: ExitReason,
    pub written: Vec<WorkFile>,
    /// Без отличий от эталона / флаг автозаписи не взведён — запись не нужна.
    pub unchanged: Vec<WorkFile>,
    /// Файл, на который наложен запрет автозаписи (случай 3 ОВ-8).
    pub forbidden: Vec<WorkFile>,
    pub failed: Vec<(WorkFile, WriteError)>,
    /// Ответа писателя не было до `started + EXIT_BUDGET`.
    pub timed_out: Vec<WorkFile>,
    pub engine_ack: bool,
}

impl ExitReport {
    /// Пустой отчёт для `reason`: списки файлов пусты, движок пока не
    /// подтвердил остановку.
    pub fn new(reason: ExitReason) -> ExitReport {
        ExitReport {
            reason,
            written: Vec::new(),
            unchanged: Vec::new(),
            forbidden: Vec::new(),
            failed: Vec::new(),
            timed_out: Vec::new(),
            engine_ack: false,
        }
    }
}

/// Ожидание ответа писателя на пути выхода по инжектируемым часам (ADR-7,
/// НФ-9, §6.10). Подмена на `ManualWaiter` в тестах не спит реальное время
/// по оси часов движка (ADR-19).
pub trait ReplyWaiter {
    fn wait(&self, rx: &Receiver<WriterReply>, until: ClockInstant, clock: &dyn Clock) -> Option<WriterReply>;
}

/// Реальное ожидание: `recv_timeout(until − now)` (§6.10).
#[derive(Default)]
pub struct ChannelWaiter;

impl ReplyWaiter for ChannelWaiter {
    fn wait(&self, rx: &Receiver<WriterReply>, until: ClockInstant, clock: &dyn Clock) -> Option<WriterReply> {
        let now = clock.now();
        if now >= until {
            return rx.try_recv().ok();
        }
        rx.recv_timeout(until.saturating_since(now)).ok()
    }
}

/// Тестовая подмена (ADR-19, §7.1): никогда не спит по оси симулированного
/// времени. При пустом канале переводит `ManualClock` на
/// `min(until, MemStore::next_wake())` и повторяет — вместо ожидания срока
/// целиком. Опрос канала (`recv_timeout` на 5 мс реального времени) даёт
/// потоку писателя шанс ответить между продвижениями часов.
///
/// Параметр `clock: &dyn Clock` трейта `ReplyWaiter` здесь игнорируется в
/// пользу собственных часов `self.clock` — в тестах это те же часы, которыми
/// размечен срок `until` и задержки `MemStore`.
pub struct ManualWaiter {
    clock: ManualClock,
    store: MemStore,
}

impl ManualWaiter {
    pub fn new(clock: ManualClock, store: MemStore) -> ManualWaiter {
        ManualWaiter { clock, store }
    }
}

impl ReplyWaiter for ManualWaiter {
    fn wait(&self, rx: &Receiver<WriterReply>, until: ClockInstant, _clock: &dyn Clock) -> Option<WriterReply> {
        loop {
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(reply) => return Some(reply),
                Err(RecvTimeoutError::Disconnected) => return None,
                Err(RecvTimeoutError::Timeout) => {}
            }

            let now = self.clock.now();
            if now >= until {
                return rx.try_recv().ok();
            }

            match self.store.next_wake() {
                Some(w) => {
                    let target = if w < until { w } else { until };
                    if target > now {
                        self.clock.advance(target.saturating_since(now));
                        continue;
                    }
                    // `target` не продвигает часы (задержка уже наступила, но
                    // её некому снять без реального писателя): срок исчерпан
                    // сразу, без бесконечного опроса (§7.1).
                    let gap = until.saturating_since(self.clock.now());
                    if gap > Duration::ZERO {
                        self.clock.advance(gap);
                    }
                    return rx.try_recv().ok();
                }
                None => match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(reply) => return Some(reply),
                    Err(RecvTimeoutError::Disconnected) => return None,
                    Err(RecvTimeoutError::Timeout) => {
                        let gap = until.saturating_since(self.clock.now());
                        if gap > Duration::ZERO {
                            self.clock.advance(gap);
                        }
                        return None;
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::SnapshotId;
    use std::sync::mpsc;

    #[test]
    fn until_is_now_plus_budget() {
        let now = ClockInstant::from_start(Duration::from_secs(10));
        let mut c = ExitCoordinator::new();
        let until = c.begin(ExitReason::WindowClose, now).expect("first begin");
        assert_eq!(until, now.saturating_add(EXIT_BUDGET));
    }

    /// Повторный запрос во время `Exiting` игнорируется (ТЗ-14, И-Т8); после
    /// `finish` путь — `Done`, повторный запрос всё ещё `None`.
    #[test]
    fn begin_once_then_ignored() {
        let mut c = ExitCoordinator::new();
        assert!(c.is_running());
        let t0 = ClockInstant::from_start(Duration::from_secs(1));
        assert!(c.begin(ExitReason::WindowClose, t0).is_some());
        assert!(!c.is_running());
        assert!(matches!(
            c.phase(),
            ExitPhase::Exiting { reason: ExitReason::WindowClose, started } if started == t0
        ));

        let t1 = ClockInstant::from_start(Duration::from_secs(2));
        assert!(c.begin(ExitReason::TrayQuit, t1).is_none());
        assert!(matches!(c.phase(), ExitPhase::Exiting { reason: ExitReason::WindowClose, .. }));

        c.finish();
        assert!(matches!(c.phase(), ExitPhase::Done));
        assert!(c.begin(ExitReason::WindowClose, t1).is_none());
    }

    fn reply() -> WriterReply {
        WriterReply::Written { file: WorkFile::Settings, id: SnapshotId::new(1) }
    }

    #[test]
    fn channel_waiter_returns_queued_reply() {
        let (tx, rx) = mpsc::channel();
        tx.send(reply()).expect("send reply");
        let clock = crate::audio::clock::MonotonicClock::new();
        let until = clock.now().saturating_add(Duration::from_secs(1));
        let got = ChannelWaiter.wait(&rx, until, &clock);
        assert!(matches!(got, Some(WriterReply::Written { file: WorkFile::Settings, .. })));
    }

    #[test]
    fn channel_waiter_times_out() {
        let (_tx, rx) = mpsc::channel::<WriterReply>();
        let clock = crate::audio::clock::MonotonicClock::new();
        let until = clock.now().saturating_add(Duration::from_millis(20));
        let got = ChannelWaiter.wait(&rx, until, &clock);
        assert!(got.is_none());
    }

    /// §7.1, ADR-19: пустой канал с живым отправителем — `ManualWaiter`
    /// продвигает часы к `next_wake`, затем (раз задержку некому снять) к
    /// `until`, и возвращает `None`; весь тест укладывается в несколько
    /// опросов по 5 мс реального времени.
    #[test]
    fn manual_waiter_advances_to_next_wake() {
        let clock = ManualClock::new();
        let store = MemStore::new();
        let path = std::path::Path::new("/cfg/settings.toml");
        let wake_at = ClockInstant::START.saturating_add(Duration::from_secs(3));
        store.delay_write(path, wake_at, clock.clone());

        let (_tx, rx) = mpsc::channel::<WriterReply>();
        let waiter = ManualWaiter::new(clock.clone(), store);
        let until = ClockInstant::START.saturating_add(Duration::from_secs(5));

        let got = waiter.wait(&rx, until, &clock);

        assert!(got.is_none());
        assert_eq!(clock.now(), until);
    }

    #[test]
    fn manual_waiter_returns_reply() {
        let clock = ManualClock::new();
        let store = MemStore::new();
        let (tx, rx) = mpsc::channel();
        tx.send(reply()).expect("send reply");

        let waiter = ManualWaiter::new(clock.clone(), store);
        let until = ClockInstant::START.saturating_add(Duration::from_secs(5));

        let got = waiter.wait(&rx, until, &clock);
        assert!(matches!(got, Some(WriterReply::Written { file: WorkFile::Settings, .. })));
    }
}
