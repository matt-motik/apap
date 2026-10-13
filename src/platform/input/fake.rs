//! `FakeWheelInput` — подмена `WheelInput` для автотестов (ADR-6, ADR-8,
//! ТЗ-36, §2.8, §6.11, §7.1). Компилируется всегда, без `cfg(test)`: тесты
//! бинарника собирают библиотеку без `cfg(test)`. Оконной системы нет:
//! события «фильтра winit» и метки монитора AppKit подаются методами
//! `push_winit`, `push_winit_marked` и `push_mark`, а нормализация и
//! сопоставление — настоящие `WheelNormalizer` и `WheelMatcher`.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use super::{
    AttachOutcome, Clicks, Desktop, MomentumMark, Session, SlintScroll, TrayHost, TrayScroll,
    WheelEvent, WheelInput, WheelMatcher, WheelNormalizer, WheelTable,
};

/// Подмена модуля ввода (§7.1, ADR-8): подаёт `WheelEvent`, `SlintScroll` и
/// `TrayScroll` в `WheelNormalizer` с заданными `Desktop`/`Session`/`TrayHost`
/// по алгоритму сопоставления §6.11.
#[derive(Clone, PartialEq, Debug)]
pub struct FakeWheelInput {
    normalizer: WheelNormalizer,
    matcher: WheelMatcher,
    /// Хост значка трея (§6.11 «Хост трея»); до `set_tray_host` — `Unknown`.
    tray_host: TrayHost,
    /// Исход, который возвращает `attach` (§2.8).
    attach_outcome: AttachOutcome,
}

impl FakeWheelInput {
    /// Подмена для таблицы и платформы (§7.1, ADR-8). Хост трея —
    /// `TrayHost::Unknown`, исход `attach` — `AttachOutcome::Winit`.
    pub fn new(table: WheelTable, desktop: Desktop, session: Option<Session>) -> Self {
        FakeWheelInput {
            normalizer: WheelNormalizer::new(table, desktop, session),
            matcher: WheelMatcher::new(),
            tray_host: TrayHost::Unknown,
            attach_outcome: AttachOutcome::Winit,
        }
    }

    /// Подмена со встроенным правилом по умолчанию `WheelTable::builtin`
    /// (§6.11, ОВС-2 а).
    pub fn builtin(desktop: Desktop, session: Option<Session>) -> Self {
        FakeWheelInput::new(WheelTable::builtin(), desktop, session)
    }

    /// Задаёт исход, который вернёт `attach` (§2.8): например,
    /// `DefaultRuleOnly` — окно без фильтра winit.
    pub fn with_attach_outcome(mut self, outcome: AttachOutcome) -> Self {
        self.attach_outcome = outcome;
        self
    }

    /// Событие «фильтра winit» без меток инерции — все ОС, кроме macOS
    /// (§6.11 «Сопоставление», В-2).
    pub fn push_winit(&mut self, ev: WheelEvent) {
        self.matcher.push_winit(ev);
    }

    /// Событие «фильтра winit» на macOS (§6.11, ОВС-1 а): `momentum`
    /// проставляется по очередной метке монитора AppKit.
    pub fn push_winit_marked(&mut self, ev: WheelEvent) {
        self.matcher.push_winit_marked(ev);
    }

    /// Метка «монитора AppKit» (macOS, §6.11, ОВС-1 а).
    pub fn push_mark(&mut self, mark: MomentumMark) {
        self.matcher.push_mark(mark);
    }
}

impl WheelInput for FakeWheelInput {
    /// Окно не используется: возвращается исход, заданный при создании.
    fn attach(&mut self, _window: &slint::Window) -> AttachOutcome {
        self.attach_outcome.clone()
    }

    /// `scroll-event` ползунка (§6.11 «Сопоставление», В-2): пара с событием
    /// winit → `slider`, иначе правило по умолчанию без пары.
    fn slider_scroll(&mut self, ev: SlintScroll) -> Clicks {
        match self.matcher.take_matching(ev) {
            Some(wheel) => self.normalizer.slider(wheel),
            None => self.normalizer.slider_unpaired(ev),
        }
    }

    /// Прокрутка над значком трея (§6.11 «Трей», В-5) для запомненного хоста.
    fn tray_scroll(&mut self, ev: TrayScroll) -> Clicks {
        self.normalizer.tray(ev, self.tray_host)
    }

    /// Конец тика (§6.11, В-2): событие winit без пары отбрасывается.
    fn end_tick(&mut self) {
        self.matcher.end_tick();
    }

    fn set_tray_host(&mut self, host: TrayHost) {
        self.tray_host = host;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::{WheelDelta, WheelPhase};
    use super::*;
    use crate::audio::clock::ClockInstant;

    /// Событие winit `Px` и парный ему `scroll-event` Slint (те же `t` и `dy`).
    fn px(ms: u64, y: f32, phase: WheelPhase) -> (WheelEvent, SlintScroll) {
        let t = ClockInstant::from_start(Duration::from_millis(ms));
        let wheel = WheelEvent {
            t,
            delta: WheelDelta::Px { x: 0.0, y: f64::from(y) },
            phase,
            momentum: None,
        };
        (wheel, SlintScroll { t, dx: 0.0, dy: y })
    }

    /// ТЗ-36, В-2, ОВС-2 а: `SlintScroll` без события winit — 60 px = один
    /// щелчок; 30 px тачпада — один шаг.
    #[test]
    fn unpaired_slint_scroll_uses_default_rule() {
        let mut input = FakeWheelInput::builtin(Desktop::Kde, Some(Session::X11));
        assert_eq!(input.slider_scroll(px(0, 60.0, WheelPhase::Moved).1), Clicks(1));
        assert_eq!(input.slider_scroll(px(10, -120.0, WheelPhase::Moved).1), Clicks(-2));

        assert_eq!(input.slider_scroll(px(20, 30.0, WheelPhase::Moved).1), Clicks(1));
        // Остаток переносится: 15 + 15 px — ещё один шаг.
        assert_eq!(input.slider_scroll(px(30, 15.0, WheelPhase::Moved).1), Clicks(0));
        assert_eq!(input.slider_scroll(px(40, 15.0, WheelPhase::Moved).1), Clicks(1));
    }

    /// ТЗ-36, В-2: событие winit, `end_tick` без `scroll-event` — щелчков нет;
    /// следующий `scroll-event` старое событие не использует.
    #[test]
    fn unpaired_winit_event_ignored() {
        let (wheel, scroll) = px(0, 60.0, WheelPhase::Moved);

        // Контроль: с парой 60 px тачпада — два шага по 30 px.
        let mut paired = FakeWheelInput::builtin(Desktop::Windows, Some(Session::Win32));
        paired.push_winit(wheel);
        assert_eq!(paired.slider_scroll(scroll), Clicks(2));

        let mut input = FakeWheelInput::builtin(Desktop::Windows, Some(Session::Win32));
        let untouched = input.clone();
        input.push_winit(wheel);
        input.end_tick();
        // Ни один метод, возвращающий щелчки, не вызван; состояние как до события.
        assert_eq!(input, untouched);
        // Пары нет: правило по умолчанию — 60 px Slint = один щелчок колеса.
        assert_eq!(input.slider_scroll(scroll), Clicks(1));
    }

    /// ТЗ-36, ОВС-1 а, И-Р19: жест (`Started…Ended`, метки «не инерция»),
    /// затем инерция (метки «инерция») — шаги только от жеста; инерция не
    /// меняет ни результат, ни накопитель.
    #[test]
    fn macos_momentum_ignored() {
        let mut input = FakeWheelInput::builtin(Desktop::Macos, Some(Session::AppKit));
        let gesture = [
            (0, WheelPhase::Started, Clicks(0)),
            (10, WheelPhase::Moved, Clicks(1)),
            (20, WheelPhase::Ended, Clicks(0)),
        ];
        for (ms, phase, expected) in gesture {
            let (wheel, scroll) = px(ms, 15.0, phase);
            input.push_mark(MomentumMark { precise: true, dy: 15.0, momentum: false });
            input.push_winit_marked(wheel);
            assert_eq!(input.slider_scroll(scroll), expected);
            input.end_tick();
        }

        let before_momentum = input.clone();
        for ms in [30, 40, 50] {
            let (wheel, scroll) = px(ms, 30.0, WheelPhase::Moved);
            input.push_mark(MomentumMark { precise: true, dy: 30.0, momentum: true });
            input.push_winit_marked(wheel);
            assert_eq!(input.slider_scroll(scroll), Clicks(0));
            input.end_tick();
        }
        assert_eq!(input, before_momentum);

        // Остаток жеста (15 px) сохранён: ещё 15 px — шаг.
        let (wheel, scroll) = px(60, 15.0, WheelPhase::Started);
        input.push_mark(MomentumMark { precise: true, dy: 15.0, momentum: false });
        input.push_winit_marked(wheel);
        assert_eq!(input.slider_scroll(scroll), Clicks(1));
    }

    /// ОВС-1 а: метка с другим значением отброшена; событие — не инерция и
    /// даёт шаг по правилу.
    #[test]
    fn macos_mark_mismatch_treated_as_gesture() {
        let mut input = FakeWheelInput::builtin(Desktop::Macos, Some(Session::AppKit));
        let (wheel, scroll) = px(0, 30.0, WheelPhase::Moved);

        // Другое |dy|.
        input.push_mark(MomentumMark { precise: true, dy: 99.0, momentum: true });
        input.push_winit_marked(wheel);
        assert_eq!(input.slider_scroll(scroll), Clicks(1));

        // Другая точность (метка «строки» при событии Px).
        let (wheel, scroll) = px(10, 30.0, WheelPhase::Moved);
        input.push_mark(MomentumMark { precise: false, dy: 30.0, momentum: true });
        input.push_winit_marked(wheel);
        assert_eq!(input.slider_scroll(scroll), Clicks(1));

        // Метки отброшены, а не оставлены в очереди: пустая очередь — не инерция.
        let (wheel, scroll) = px(20, 30.0, WheelPhase::Moved);
        input.push_winit_marked(wheel);
        assert_eq!(input.slider_scroll(scroll), Clicks(1));
    }
}
