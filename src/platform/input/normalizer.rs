//! Нормализатор колёсика (ADR-8, НФ-2, §6.11): сырое событие winit → щелчки
//! `Clicks` для ползунка громкости. Модуль чистый: платформа — значение поля
//! `desktop`, а не `cfg(target_os)`, поэтому тестируется на любой ОС.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::time::Duration;

use crate::audio::clock::ClockInstant;

use super::table::{SliderRule, TouchpadParams, WheelTable};
use super::{Clicks, Desktop, DeviceClass, Session, WheelDelta, WheelEvent};

/// Состояние накопителя тачпада (В-4, §6.11).
#[derive(Clone, Copy, PartialEq, Debug)]
struct TouchState {
    /// Остаток в долях шага, знак как пришёл; после шага — |acc| < 1.
    acc: f64,
    /// Шагов выдано за текущий свайп (со знаком).
    steps: i32,
    /// Время последнего события накопителя: граница свайпа.
    last_t: Option<ClockInstant>,
}

impl TouchState {
    const EMPTY: TouchState = TouchState { acc: 0.0, steps: 0, last_t: None };
}

/// Нормализатор колёсика (ADR-8, §6.11): колесо — k делений = k щелчков
/// (В-3), тачпад — накопитель с порогом и не больше K шагов на свайп (В-4).
#[derive(Clone, PartialEq, Debug)]
pub struct WheelNormalizer {
    table: WheelTable,
    desktop: Desktop,
    session: Option<Session>,
    /// Время последнего дробного `Lines` (ОВС-1 а): на Windows целое событие
    /// вскоре после дробного — тоже тачпад.
    last_fraction: Option<ClockInstant>,
    touch: TouchState,
}

impl WheelNormalizer {
    /// Нормализатор для таблицы и платформы (ADR-8, §6.11). `session` —
    /// `None`, пока окно не присоединено.
    pub fn new(table: WheelTable, desktop: Desktop, session: Option<Session>) -> Self {
        WheelNormalizer { table, desktop, session, last_fraction: None, touch: TouchState::EMPTY }
    }

    /// Сеанс окна становится известен при attach (§6.11); состояние
    /// накопителя не меняется.
    pub fn set_session(&mut self, session: Option<Session>) {
        self.session = session;
    }

    /// Событие колёсика над ползунком → щелчки (§6.11, В-3, В-4, ОВС-1 а).
    /// Положительное `y` — громче; `x` игнорируется. Инерционное событие
    /// macOS даёт 0 и не меняет ни накопитель, ни состояние (И-Р19). Не
    /// конечное `y` — 0 щелчков, состояние не меняется.
    pub fn slider(&mut self, ev: WheelEvent) -> Clicks {
        if ev.momentum == Some(true) {
            return Clicks(0);
        }
        let y = match ev.delta {
            WheelDelta::Lines { y, .. } => f64::from(y),
            WheelDelta::Px { y, .. } => y,
        };
        if !y.is_finite() {
            return Clicks(0);
        }
        let class = self.classify(ev.delta, y, ev.t);
        match self.table.slider_rule(self.desktop, self.session, class) {
            SliderRule::Division => {
                self.reset_touch();
                // Строка таблицы может назначить деление и для Px или дробных
                // Lines: тогда — ближайшее целое.
                Clicks(clicks_from_f64(y.round()))
            }
            SliderRule::Touchpad(p) => {
                let threshold = match ev.delta {
                    WheelDelta::Lines { .. } => f64::from(p.threshold_lines),
                    WheelDelta::Px { .. } => p.threshold_px,
                };
                self.touch_step(p, ev.t, y, threshold)
            }
        }
    }

    /// Класс устройства по виду события (§6.11, ОВС-1 а): `Px` и дробные
    /// `Lines` — тачпад; на Windows целое `Lines` в пределах
    /// `integer_after_fraction_ms` после дробного — тоже тачпад; иначе колесо.
    /// Дробное `Lines` запоминает своё время в `last_fraction`. `y` — конечное.
    fn classify(&mut self, delta: WheelDelta, y: f64, t: ClockInstant) -> DeviceClass {
        match delta {
            WheelDelta::Px { .. } => DeviceClass::Touchpad,
            WheelDelta::Lines { .. } if y.fract() != 0.0 => {
                self.last_fraction = Some(t);
                DeviceClass::Touchpad
            }
            WheelDelta::Lines { .. } => {
                let window = Duration::from_millis(u64::from(self.table.integer_after_fraction_ms));
                match self.last_fraction {
                    Some(last)
                        if self.desktop == Desktop::Windows
                            && t.saturating_since(last) <= window =>
                    {
                        DeviceClass::Touchpad
                    }
                    _ => DeviceClass::Wheel,
                }
            }
        }
    }

    /// Сброс накопителя тачпада (§6.11): событие колеса завершает свайп.
    fn reset_touch(&mut self) {
        self.touch = TouchState::EMPTY;
    }

    /// Шаг накопителя тачпада (В-4, §6.11): `value` — смещение в единицах
    /// события, `threshold` — уже выбранный порог шага в тех же единицах.
    /// Пауза больше `swipe_gap_ms` — граница свайпа: остаток и счётчик шагов
    /// сбрасываются. Остаток переносится; за свайп — не больше `max_steps`
    /// шагов. Не конечное значение или порог ≤ 0 — 0 щелчков, состояние не
    /// меняется.
    fn touch_step(
        &mut self,
        p: TouchpadParams,
        t: ClockInstant,
        value: f64,
        threshold: f64,
    ) -> Clicks {
        if !(threshold.is_finite() && threshold > 0.0) {
            return Clicks(0);
        }
        let gap = Duration::from_millis(u64::from(p.swipe_gap_ms));
        let new_swipe = match self.touch.last_t {
            Some(last) => t.saturating_since(last) > gap,
            None => false,
        };
        let (acc, steps) = if new_swipe { (0.0, 0) } else { (self.touch.acc, self.touch.steps) };
        let sum = acc + value / threshold;
        if !sum.is_finite() {
            return Clicks(0);
        }
        let whole = sum.trunc();
        let limit = i32::from(p.max_steps).saturating_sub(steps.saturating_abs()).max(0);
        let k = clicks_from_f64(whole).clamp(-limit, limit);
        self.touch = TouchState { acc: sum - whole, steps: steps.saturating_add(k), last_t: Some(t) };
        Clicks(k)
    }
}

/// Целое число щелчков из `f64` без переполнения: не конечное → 0, остальное
/// ограничивается симметричным диапазоном `±i32::MAX` (отрицание результата
/// безопасно). После `clamp` значение точно представимо в `i32`, поэтому
/// приведение `as` здесь только отбрасывает дробную часть.
fn clicks_from_f64(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let max = f64::from(i32::MAX);
    v.clamp(-max, max) as i32
}

#[cfg(test)]
mod tests {
    use super::super::{parse_wheel_table, WheelPhase};
    use super::*;

    fn lines_ev(ms: u64, y: f32, momentum: Option<bool>) -> WheelEvent {
        WheelEvent {
            t: ClockInstant::from_start(Duration::from_millis(ms)),
            delta: WheelDelta::Lines { x: 0.0, y },
            phase: WheelPhase::Moved,
            momentum,
        }
    }

    fn lines(ms: u64, y: f32) -> WheelEvent {
        lines_ev(ms, y, None)
    }

    fn windows() -> WheelNormalizer {
        WheelNormalizer::new(WheelTable::builtin(), Desktop::Windows, Some(Session::Win32))
    }

    /// В-3: колесо — k делений = k щелчков (KDE Wayland, таблица из файла).
    #[test]
    fn wheel_division_is_clicks() {
        let table = match parse_wheel_table(include_str!("wheel_table.toml")) {
            Ok(table) => table,
            Err(e) => panic!("wheel_table.toml не разобран: {e}"),
        };
        let mut n = WheelNormalizer::new(table, Desktop::Kde, None);
        n.set_session(Some(Session::Wayland));
        for i in 0..3 {
            assert_eq!(n.slider(lines(i * 10, 1.0)), Clicks(1));
        }
        assert_eq!(n.slider(lines(40, -2.0)), Clicks(-2));
    }

    /// В-4: порог 0,5 строки, перенос остатка, не больше 5 шагов на свайп.
    #[test]
    fn touchpad_accumulates_and_limits_swipe() {
        let mut n = windows();
        assert_eq!(n.slider(lines(0, 0.3)), Clicks(0));
        assert_eq!(n.slider(lines(10, 0.3)), Clicks(1));
        // 5 шагов сразу: с учётом уже выданного доступно только 4.
        assert_eq!(n.slider(lines(20, 2.5)), Clicks(4));
        assert_eq!(n.slider(lines(30, 0.5)), Clicks(0));
        assert_eq!(n.slider(lines(40, -0.5)), Clicks(0));
    }

    /// В-4: пауза больше 300 мс — граница свайпа, остаток и счётчик сброшены.
    #[test]
    fn touchpad_swipe_gap_resets() {
        let mut n = windows();
        assert_eq!(n.slider(lines(0, 2.5)), Clicks(5));
        assert_eq!(n.slider(lines(10, 0.3)), Clicks(0));
        // Ровно 300 мс — ещё тот же свайп: остаток 0,6 + 0,6 даёт шаг, но лимит исчерпан.
        assert_eq!(n.slider(lines(310, 0.3)), Clicks(0));
        // 301 мс: остаток сброшен (0,3 строки — снова меньше шага).
        assert_eq!(n.slider(lines(611, 0.3)), Clicks(0));
        assert_eq!(n.slider(lines(620, 2.7)), Clicks(5));
    }

    /// ОВС-1 а: на Windows целое после дробного в пределах 200 мс — тачпад.
    #[test]
    fn windows_integer_after_fraction_is_touchpad() {
        let mut n = windows();
        assert_eq!(n.slider(lines(0, 0.5)), Clicks(1));
        assert_eq!(n.slider(lines(100, 1.0)), Clicks(2));
        assert_eq!(n.slider(lines(200, 1.0)), Clicks(2));
        assert_eq!(n.slider(lines(201, 1.0)), Clicks(1));

        let mut kde = WheelNormalizer::new(WheelTable::builtin(), Desktop::Kde, Some(Session::X11));
        assert_eq!(kde.slider(lines(0, 0.5)), Clicks(1));
        assert_eq!(kde.slider(lines(100, 1.0)), Clicks(1));
    }

    /// И-Р19: инерционное событие — 0 щелчков, накопитель и состояние не тронуты.
    #[test]
    fn momentum_is_ignored() {
        let mut with = windows();
        let mut without = windows();
        assert_eq!(with.slider(lines(0, 0.3)), Clicks(0));
        assert_eq!(without.slider(lines(0, 0.3)), Clicks(0));
        assert_eq!(with.slider(lines_ev(50, 2.5, Some(true))), Clicks(0));
        assert_eq!(with, without);
        assert_eq!(with.slider(lines(100, 0.3)), Clicks(1));
        assert_eq!(without.slider(lines(100, 0.3)), Clicks(1));
    }
}
