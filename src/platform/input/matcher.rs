//! Сопоставитель колёсика (ADR-8, §2.8, §6.11 «Сопоставление», В-2, ОВС-1 а):
//! пара «событие winit ↔ `scroll-event` ползунка Slint» и очередь меток
//! инерции монитора AppKit. Модуль чистый: платформа выбирает метод
//! (`push_winit` или `push_winit_marked`), а не `cfg(target_os)`, поэтому
//! тестируется на любой ОС.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::collections::VecDeque;

use super::{SlintScroll, WheelDelta, WheelEvent};

/// Один щелчок колеса в `scroll-event` Slint: 60 логических px (§6.11, В-2).
const SLINT_WHEEL_CLICK_PX: f64 = 60.0;

/// Предел очереди меток инерции (ОВС-1 а): при переполнении отбрасывается
/// самая старая метка — память не растёт.
const MOMENTUM_QUEUE_CAP: usize = 64;

/// Относительный допуск сравнения значений, прошедших через `f32` (§6.11).
const F32_TOLERANCE: f64 = 1e-3;

/// Метка инерции от монитора AppKit (macOS, ОВС-1 а).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MomentumMark {
    pub precise: bool,
    pub dy: f64,
    pub momentum: bool,
}

/// Сопоставитель событий winit со `scroll-event` ползунка (ADR-8, §6.11,
/// В-2): хранит последнее непарное событие winit и очередь меток инерции
/// macOS (ОВС-1 а).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct WheelMatcher {
    /// Последнее событие winit, ещё не сопоставленное со `scroll-event`.
    pending: Option<WheelEvent>,
    /// Метки монитора AppKit в порядке прихода (FIFO), не больше
    /// `MOMENTUM_QUEUE_CAP`.
    momentum: VecDeque<MomentumMark>,
}

impl WheelMatcher {
    /// Пустой сопоставитель (§6.11); очередь меток выделена заранее.
    pub fn new() -> Self {
        WheelMatcher { pending: None, momentum: VecDeque::with_capacity(MOMENTUM_QUEUE_CAP) }
    }

    /// Событие winit без меток (все ОС, кроме macOS): `pending := Some(ev)`.
    /// Прежнее непарное событие отбрасывается (§6.11, В-2).
    pub fn push_winit(&mut self, ev: WheelEvent) {
        self.pending = Some(ev);
    }

    /// Событие winit на macOS (§6.11, ОВС-1 а): снимает метку из очереди и
    /// проставляет `ev.momentum`. Метка согласуется (точность и `|dy|`) —
    /// `Some(mark.momentum)`; не согласуется или очередь пуста —
    /// `Some(false)`, снятая метка отбрасывается. Затем `pending := Some(ev)`
    /// (В-2).
    pub fn push_winit_marked(&mut self, mut ev: WheelEvent) {
        let momentum = match self.momentum.pop_front() {
            Some(mark) if mark_agrees(mark, ev.delta) => mark.momentum,
            _ => false,
        };
        ev.momentum = Some(momentum);
        self.pending = Some(ev);
    }

    /// Монитор AppKit (ОВС-1 а): метка в конец очереди. При переполнении
    /// отбрасывается самая старая метка.
    pub fn push_mark(&mut self, mark: MomentumMark) {
        while self.momentum.len() >= MOMENTUM_QUEUE_CAP {
            self.momentum.pop_front();
        }
        self.momentum.push_back(mark);
    }

    /// `scroll-event` ползунка (§6.11, В-2): забирает `pending` всегда, даже
    /// при несовпадении. `Some(ev)` — пара найдена (Lines: `y · 60 == dy`;
    /// Px: `y == dy`, с точностью `f32`); иначе пары нет.
    pub fn take_matching(&mut self, scroll: SlintScroll) -> Option<WheelEvent> {
        let ev = self.pending.take()?;
        let expected = match ev.delta {
            WheelDelta::Lines { y, .. } => f64::from(y) * SLINT_WHEEL_CLICK_PX,
            WheelDelta::Px { y, .. } => y,
        };
        approx_eq(expected, f64::from(scroll.dy)).then_some(ev)
    }

    /// Конец тика UI (§6.11, В-2): `pending := None` — указатель вне
    /// ползунка, громкость не меняется.
    pub fn end_tick(&mut self) {
        self.pending = None;
    }
}

/// Согласие метки с событием winit (§6.11, ОВС-1 а): `Px ⇔ precise` и
/// совпадение `|dy|` с точностью `f32`.
fn mark_agrees(mark: MomentumMark, delta: WheelDelta) -> bool {
    let (precise, y) = match delta {
        WheelDelta::Lines { y, .. } => (false, f64::from(y)),
        WheelDelta::Px { y, .. } => (true, y),
    };
    mark.precise == precise && approx_eq(mark.dy.abs(), y.abs())
}

/// Равенство с относительным допуском `f32` (§6.11); не конечные значения
/// не совпадают ни с чем.
fn approx_eq(a: f64, b: f64) -> bool {
    a.is_finite() && b.is_finite() && (a - b).abs() <= F32_TOLERANCE * b.abs().max(1.0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::WheelPhase;
    use super::*;
    use crate::audio::clock::ClockInstant;

    fn at(ms: u64) -> ClockInstant {
        ClockInstant::from_start(Duration::from_millis(ms))
    }

    fn lines(ms: u64, y: f32) -> WheelEvent {
        WheelEvent {
            t: at(ms),
            delta: WheelDelta::Lines { x: 0.0, y },
            phase: WheelPhase::Moved,
            momentum: None,
        }
    }

    fn px(ms: u64, y: f64) -> WheelEvent {
        WheelEvent {
            t: at(ms),
            delta: WheelDelta::Px { x: 0.0, y },
            phase: WheelPhase::Moved,
            momentum: None,
        }
    }

    fn scroll(ms: u64, dy: f32) -> SlintScroll {
        SlintScroll { t: at(ms), dx: 0.0, dy }
    }

    fn mark(precise: bool, dy: f64, momentum: bool) -> MomentumMark {
        MomentumMark { precise, dy, momentum }
    }

    /// §6.11, В-2: Lines — `y · 60 == dy`; несовпадение всё равно забирает
    /// pending.
    #[test]
    fn lines_pair_matches_by_sixty_px() {
        let mut m = WheelMatcher::new();
        m.push_winit(lines(0, 1.0));
        assert_eq!(m.take_matching(scroll(0, 60.0)), Some(lines(0, 1.0)));

        m.push_winit(lines(10, 1.0));
        assert_eq!(m.take_matching(scroll(10, 120.0)), None);
        assert_eq!(m.pending, None);
        assert_eq!(m.take_matching(scroll(10, 60.0)), None);
    }

    /// §6.11, В-2: Px — `y == dy` с точностью f32; не конечное — не пара.
    #[test]
    fn px_pair_matches_by_value() {
        let mut m = WheelMatcher::new();
        m.push_winit(px(0, 12.5));
        assert_eq!(m.take_matching(scroll(0, 12.5)), Some(px(0, 12.5)));

        m.push_winit(px(10, -0.1));
        assert_eq!(m.take_matching(scroll(10, -0.1)), Some(px(10, -0.1)));

        m.push_winit(px(20, f64::NAN));
        assert_eq!(m.take_matching(scroll(20, f32::NAN)), None);
    }

    /// В-2: новое событие winit вытесняет непарное; `end_tick` очищает.
    #[test]
    fn newer_event_replaces_pending_and_end_tick_clears() {
        let mut m = WheelMatcher::new();
        m.push_winit(lines(0, 1.0));
        m.push_winit(lines(5, 2.0));
        assert_eq!(m.pending, Some(lines(5, 2.0)));
        assert_eq!(m.take_matching(scroll(5, 120.0)), Some(lines(5, 2.0)));

        m.push_winit(lines(10, 1.0));
        m.end_tick();
        assert_eq!(m.take_matching(scroll(10, 60.0)), None);
    }

    /// ОВС-1 а: согласованная метка задаёт momentum; очередь — FIFO.
    #[test]
    fn agreeing_marks_set_momentum_in_fifo_order() {
        let mut m = WheelMatcher::new();
        m.push_mark(mark(true, -4.0, true));
        m.push_mark(mark(false, 1.0, false));

        m.push_winit_marked(px(0, 4.0));
        assert_eq!(m.pending.map(|ev| ev.momentum), Some(Some(true)));
        m.push_winit_marked(lines(5, -1.0));
        assert_eq!(m.pending.map(|ev| ev.momentum), Some(Some(false)));
        assert!(m.momentum.is_empty());

        // Момент события сохраняется, пара со scroll-event находится.
        let mut expected = lines(5, -1.0);
        expected.momentum = Some(false);
        assert_eq!(m.take_matching(scroll(5, -60.0)), Some(expected));
    }

    /// ОВС-1 а: несогласованная метка отбрасывается → `Some(false)`; пустая
    /// очередь → `Some(false)`; очередь ограничена.
    #[test]
    fn disagreeing_or_missing_mark_gives_no_momentum() {
        let mut m = WheelMatcher::new();
        m.push_mark(mark(false, 4.0, true));
        m.push_mark(mark(true, 9.0, true));
        m.push_mark(mark(true, 2.0, true));

        m.push_winit_marked(px(0, 4.0)); // precise не тот
        assert_eq!(m.pending.map(|ev| ev.momentum), Some(Some(false)));
        assert_eq!(m.momentum.len(), 2);
        m.push_winit_marked(px(5, 4.0)); // dy другой
        assert_eq!(m.pending.map(|ev| ev.momentum), Some(Some(false)));
        assert_eq!(m.momentum.len(), 1);
        m.push_winit_marked(px(10, 2.0));
        assert_eq!(m.pending.map(|ev| ev.momentum), Some(Some(true)));

        m.push_winit_marked(px(15, 2.0)); // очередь пуста
        assert_eq!(m.pending.map(|ev| ev.momentum), Some(Some(false)));

        for i in 0..(MOMENTUM_QUEUE_CAP + 3) {
            m.push_mark(mark(true, 1.0, i >= 3));
        }
        assert_eq!(m.momentum.len(), MOMENTUM_QUEUE_CAP);
        assert_eq!(m.momentum.front().map(|k| k.momentum), Some(true));
    }
}
