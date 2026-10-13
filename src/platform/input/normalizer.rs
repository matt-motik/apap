//! Нормализатор колёсика (ADR-8, НФ-2, §6.11): сырое событие winit → щелчки
//! `Clicks` для ползунка громкости. Модуль чистый: платформа — значение поля
//! `desktop`, а не `cfg(target_os)`, поэтому тестируется на любой ОС.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::time::Duration;

use crate::audio::clock::ClockInstant;

use super::table::{SliderRule, TouchpadParams, WheelTable};
use super::{
    Clicks, Desktop, DeviceClass, Session, SlintScroll, TouchpadDirection, TrayHost, TrayScroll,
    WheelDelta, WheelEvent,
};

/// Один щелчок колеса в `scroll-event` Slint: 60 логических px (§6.11, В-2).
const SLINT_WHEEL_CLICK_PX: f64 = 60.0;

/// Состояние накопителя тачпада (В-4, §6.11).
#[derive(Clone, Copy, PartialEq, Debug)]
struct TouchState {
    /// Остаток в долях шага, знак — после поправки направления тачпада; после
    /// шага — |acc| < 1.
    acc: f64,
    /// Шагов выдано за текущий свайп (со знаком).
    steps: i32,
    /// Время последнего события накопителя: граница свайпа.
    last_t: Option<ClockInstant>,
}

impl TouchState {
    const EMPTY: TouchState = TouchState { acc: 0.0, steps: 0, last_t: None };
}

/// Состояние накопителя трея (В-5, §6.11): независимо от накопителя тачпада
/// ползунка.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TrayState {
    /// Остаток в единицах события хоста, знак уже приведён к «вверх = громче».
    acc: i64,
    /// Модуль первого ненулевого события: деление неизвестного хоста (В-5).
    first_abs: Option<u32>,
}

impl TrayState {
    const EMPTY: TrayState = TrayState { acc: 0, first_abs: None };
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
    /// Поправка направления тачпада (§6.11 «Направление тачпада», ТЗ-36):
    /// `None` — системная настройка не прочитана, знак как пришёл.
    touch_direction: Option<TouchpadDirection>,
    touch: TouchState,
    tray: TrayState,
}

impl WheelNormalizer {
    /// Нормализатор для таблицы и платформы (ADR-8, §6.11). `session` —
    /// `None`, пока окно не присоединено.
    pub fn new(table: WheelTable, desktop: Desktop, session: Option<Session>) -> Self {
        WheelNormalizer {
            table,
            desktop,
            session,
            last_fraction: None,
            touch_direction: None,
            touch: TouchState::EMPTY,
            tray: TrayState::EMPTY,
        }
    }

    /// Сеанс окна становится известен при attach (§6.11); состояние
    /// накопителя не меняется.
    pub fn set_session(&mut self, session: Option<Session>) {
        self.session = session;
    }

    /// Поправка направления тачпада по системной настройке прокрутки (§6.11
    /// «Направление тачпада», ТЗ-36); `None` — настройка не прочитана, знак
    /// как пришёл. Состояние накопителя не меняется: вызывающий задаёт
    /// поправку только когда свайп не идёт (`touch_idle`).
    pub fn set_touchpad_direction(&mut self, direction: Option<TouchpadDirection>) {
        self.touch_direction = direction;
    }

    /// Свайп тачпада не идёт (§6.11 «Направление тачпада», ТЗ-36): событий
    /// тачпада ещё не было или пауза с последнего больше `swipe_gap_ms`
    /// параметров `default_touchpad`.
    pub fn touch_idle(&self, t: ClockInstant) -> bool {
        let gap = Duration::from_millis(u64::from(self.table.default_touchpad.swipe_gap_ms));
        match self.touch.last_t {
            Some(last) => t.saturating_since(last) > gap,
            None => true,
        }
    }

    /// Событие колёсика над ползунком → щелчки (§6.11, В-3, В-4, ОВС-1 а).
    /// Положительное `y` — громче; у тачпада знак — после поправки направления
    /// (ТЗ-36), у колеса — как пришёл; `x` игнорируется. Инерционное событие
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

    /// Событие `scroll-event` ползунка Slint без парного события winit →
    /// щелчки: правило по умолчанию без пары (§6.11, В-2, ОВС-2 а, ADR-8).
    /// `dy`, кратное 60 логическим px Slint, — колесо: `dy / 60` щелчков,
    /// накопитель тачпада сбрасывается. Иначе — тачпад: накопитель с
    /// параметрами `default_touchpad` и порогом `threshold_px`. Положительное
    /// `dy` — громче; у тачпада знак — после поправки направления (ТЗ-36);
    /// `dx` игнорируется. `dy == 0` или не конечное `dy` —
    /// 0 щелчков, состояние не меняется.
    pub fn slider_unpaired(&mut self, ev: SlintScroll) -> Clicks {
        if !ev.dy.is_finite() || ev.dy == 0.0 {
            return Clicks(0);
        }
        let dy = f64::from(ev.dy);
        if dy % SLINT_WHEEL_CLICK_PX == 0.0 {
            self.reset_touch();
            return Clicks(clicks_from_f64(dy / SLINT_WHEEL_CLICK_PX));
        }
        let p = self.table.default_touchpad;
        self.touch_step(p, ev.t, dy, p.threshold_px)
    }

    /// Прокрутка над значком трея → щелчки (§6.11 «Трей», В-5, ADR-8, НФ-2).
    /// Знак и единица деления — из строки таблицы для хоста; у неизвестного
    /// хоста (деление 0) единицей становится модуль первого ненулевого
    /// события. Дробные деления накапливаются в собственном накопителе трея,
    /// не связанном с накопителем тачпада ползунка. Положительный результат —
    /// громче. Горизонтальная прокрутка и `delta == 0` — 0 щелчков, состояние
    /// не меняется.
    pub fn tray(&mut self, ev: TrayScroll, host: TrayHost) -> Clicks {
        if !ev.vertical || ev.delta == 0 {
            return Clicks(0);
        }
        let row = self.table.tray_row(host);
        let div = if row.division == 0 {
            *self.tray.first_abs.get_or_insert(ev.delta.unsigned_abs())
        } else {
            row.division
        };
        if div == 0 {
            return Clicks(0);
        }
        let div = i64::from(div);
        let step = i64::from(row.sign).saturating_mul(i64::from(ev.delta));
        let acc = self.tray.acc.saturating_add(step);
        // Деление целых — к нулю; остаток сохраняет знак накопителя.
        let k = acc / div;
        self.tray.acc = acc % div;
        let max = i64::from(i32::MAX);
        Clicks(i32::try_from(k.clamp(-max, max)).unwrap_or(0))
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
    /// Смещение умножается на поправку направления тачпада (§6.11
    /// «Направление тачпада», ТЗ-36): `UpNegative` меняет знак, `UpPositive`
    /// и `None` оставляют как пришёл. Пауза больше `swipe_gap_ms` — граница свайпа: остаток и счётчик шагов
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
        let value = match self.touch_direction {
            Some(TouchpadDirection::UpNegative) => -value,
            Some(TouchpadDirection::UpPositive) | None => value,
        };
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

    fn slint(ms: u64, dy: f32) -> SlintScroll {
        SlintScroll { t: ClockInstant::from_start(Duration::from_millis(ms)), dx: 0.0, dy }
    }

    /// В-2, ОВС-2 а: без пары 60 px Slint — один щелчок колеса.
    #[test]
    fn unpaired_wheel_60px_is_one_click() {
        let mut n = windows();
        assert_eq!(n.slider_unpaired(slint(0, 60.0)), Clicks(1));
        assert_eq!(n.slider_unpaired(slint(10, -120.0)), Clicks(-2));
    }

    /// В-2, ОВС-2 а: без пары тачпад — порог 30 px, остаток переносится,
    /// не больше 5 шагов на свайп.
    #[test]
    fn unpaired_touchpad_30px_is_one_step() {
        let mut n = windows();
        assert_eq!(n.slider_unpaired(slint(0, 30.0)), Clicks(1));
        assert_eq!(n.slider_unpaired(slint(10, 15.0)), Clicks(0));
        assert_eq!(n.slider_unpaired(slint(20, 15.0)), Clicks(1));

        let mut swipe = windows();
        // 170 px = 5,67 шага: лимит свайпа — 5.
        assert_eq!(swipe.slider_unpaired(slint(0, 170.0)), Clicks(5));
        assert_eq!(swipe.slider_unpaired(slint(10, 45.0)), Clicks(0));
    }

    /// §6.11: `dy == 0` — 0 щелчков, накопитель и состояние не тронуты.
    #[test]
    fn unpaired_zero_dy_keeps_state() {
        let mut with = windows();
        let mut without = windows();
        assert_eq!(with.slider_unpaired(slint(0, 15.0)), Clicks(0));
        assert_eq!(without.slider_unpaired(slint(0, 15.0)), Clicks(0));
        assert_eq!(with.slider_unpaired(slint(50, 0.0)), Clicks(0));
        assert_eq!(with, without);
        assert_eq!(with.slider_unpaired(slint(100, 15.0)), Clicks(1));
        assert_eq!(without.slider_unpaired(slint(100, 15.0)), Clicks(1));
    }

    fn tray_ev(delta: i32, vertical: bool) -> TrayScroll {
        TrayScroll { t: ClockInstant::from_start(Duration::ZERO), delta, vertical }
    }

    fn tray_normalizer() -> WheelNormalizer {
        let table = match parse_wheel_table(include_str!("wheel_table.toml")) {
            Ok(table) => table,
            Err(e) => panic!("wheel_table.toml не разобран: {e}"),
        };
        WheelNormalizer::new(table, Desktop::Kde, None)
    }

    /// В-5: KDE Plasma — 120 единиц = щелчок, дробные деления накапливаются.
    #[test]
    fn tray_kde_division_120() {
        let mut n = tray_normalizer();
        assert_eq!(n.tray(tray_ev(120, true), TrayHost::KdePlasma), Clicks(1));
        assert_eq!(n.tray(tray_ev(-240, true), TrayHost::KdePlasma), Clicks(-2));
        assert_eq!(n.tray(tray_ev(60, true), TrayHost::KdePlasma), Clicks(0));
        assert_eq!(n.tray(tray_ev(60, true), TrayHost::KdePlasma), Clicks(1));
    }

    /// В-5: GNOME AppIndicator — единица 1, знак обратный.
    #[test]
    fn tray_gnome_sign_is_inverted() {
        let mut n = tray_normalizer();
        assert_eq!(n.tray(tray_ev(-1, true), TrayHost::GnomeAppIndicator), Clicks(1));
        assert_eq!(n.tray(tray_ev(1, true), TrayHost::GnomeAppIndicator), Clicks(-1));
    }

    /// В-5: неизвестный хост — деление по модулю первого события.
    #[test]
    fn tray_unknown_first_event_sets_division() {
        let mut n = tray_normalizer();
        assert_eq!(n.tray(tray_ev(15, true), TrayHost::Unknown), Clicks(1));
        assert_eq!(n.tray(tray_ev(15, true), TrayHost::Unknown), Clicks(1));
        assert_eq!(n.tray(tray_ev(-30, true), TrayHost::Unknown), Clicks(-2));
    }

    /// §6.11: горизонтальная прокрутка — 0 щелчков, накопитель не тронут.
    #[test]
    fn tray_horizontal_is_ignored() {
        let mut with = tray_normalizer();
        let mut without = tray_normalizer();
        assert_eq!(with.tray(tray_ev(60, true), TrayHost::KdePlasma), Clicks(0));
        assert_eq!(without.tray(tray_ev(60, true), TrayHost::KdePlasma), Clicks(0));
        assert_eq!(with.tray(tray_ev(120, false), TrayHost::KdePlasma), Clicks(0));
        assert_eq!(with, without);
        assert_eq!(with.tray(tray_ev(60, true), TrayHost::KdePlasma), Clicks(1));
    }

    /// Свайп тачпада Windows из дробных `Lines` → щелчки по событиям.
    fn touch_swipe(direction: Option<TouchpadDirection>) -> Vec<i32> {
        let mut n = windows();
        n.set_touchpad_direction(direction);
        [(0, -0.3), (10, -0.3), (20, -1.1)].iter().map(|&(ms, y)| n.slider(lines(ms, y)).0).collect()
    }

    /// ТЗ-36, §6.11 «Направление тачпада»: `UpNegative` меняет знак смещения
    /// тачпада, `UpPositive` и `None` — знак как пришёл.
    #[test]
    fn touchpad_direction_up_negative_inverts_sign() {
        let inverted = touch_swipe(Some(TouchpadDirection::UpNegative));
        let direct = touch_swipe(Some(TouchpadDirection::UpPositive));
        assert_eq!(inverted, vec![0, 1, 2]);
        assert_eq!(direct, vec![0, -1, -2]);
        assert_eq!(touch_swipe(None), direct);
        assert_eq!(inverted.iter().sum::<i32>(), -direct.iter().sum::<i32>());
    }

    /// ТЗ-36, §6.11: поправка направления не касается колеса (правило `Division`).
    #[test]
    fn touchpad_direction_does_not_affect_wheel() {
        let mut n = windows();
        n.set_touchpad_direction(Some(TouchpadDirection::UpNegative));
        assert_eq!(n.slider(lines(0, 1.0)), Clicks(1));
        assert_eq!(n.slider(lines(10, -2.0)), Clicks(-2));
    }

    /// ТЗ-36, §6.11: поправка направления не касается трея.
    #[test]
    fn touchpad_direction_does_not_affect_tray() {
        let mut n = tray_normalizer();
        n.set_touchpad_direction(Some(TouchpadDirection::UpNegative));
        assert_eq!(n.tray(tray_ev(120, true), TrayHost::KdePlasma), Clicks(1));
        assert_eq!(n.tray(tray_ev(-240, true), TrayHost::KdePlasma), Clicks(-2));
        assert_eq!(n.tray(tray_ev(-1, true), TrayHost::GnomeAppIndicator), Clicks(1));
    }

    /// ТЗ-36, §6.11: без пары поправка действует на тачпад (`dy` не кратно
    /// 60 px) и не действует на колесо (`dy` кратно 60 px).
    #[test]
    fn touchpad_direction_applies_to_unpaired() {
        let mut n = windows();
        n.set_touchpad_direction(Some(TouchpadDirection::UpNegative));
        assert_eq!(n.slider_unpaired(slint(0, 30.0)), Clicks(-1));
        assert_eq!(n.slider_unpaired(slint(10, -45.0)), Clicks(1));
        assert_eq!(n.slider_unpaired(slint(20, 60.0)), Clicks(1));
        assert_eq!(n.slider_unpaired(slint(30, -120.0)), Clicks(-2));
    }

    /// ТЗ-36, §6.11: свайп не идёт до первого события тачпада и после паузы
    /// больше `swipe_gap_ms` (300 мс).
    #[test]
    fn touch_idle_tracks_swipe_gap() {
        let at = |ms| ClockInstant::from_start(Duration::from_millis(ms));
        let mut n = windows();
        assert!(n.touch_idle(at(0)));
        assert_eq!(n.slider(lines(100, 0.3)), Clicks(0));
        assert!(!n.touch_idle(at(100)));
        // Ровно 300 мс — ещё тот же свайп.
        assert!(!n.touch_idle(at(400)));
        assert!(n.touch_idle(at(401)));
    }
}
