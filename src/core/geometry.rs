//! Геометрия окна и правило эха (ADR-22, ОВС-5 а, ТЗ-11, §6.17).
//!
//! Программное восстановление геометрии при старте эхом возвращается от
//! оконного менеджера: трекер должен отличить это эхо от настоящего
//! действия пользователя, чтобы не взводить дедлайн записи (срок N) на
//! значение, которое приложение само же установило.

use crate::persist::state_file::{Origin, WindowGeometry};

/// Отслеживает последнюю выставленную геометрию и различает эхо
/// программной установки от изменения пользователем (ADR-22, §6.17).
///
/// Сигнатура `observe` по спецификации (§2.11) не принимает `state`: решение
/// «применять ли первое показание после `window_shown`» — на стороне
/// вызывающего (ОВС-5 а). Этот тип только сообщает источник показания;
/// сравнение с `state.window` и собственно `change_state` делает вызывающий
/// код (`src/main.rs`), которому доступен `SessionState`.
#[derive(Default)]
pub struct GeometryTracker {
    remembered: Option<WindowGeometry>,
    awaiting_first: bool,
}

impl GeometryTracker {
    /// Новый трекер без запомненного значения и без ожидания первого показа.
    pub fn new() -> Self {
        GeometryTracker::default()
    }

    /// Программа выставила геометрию (восстановление при старте или
    /// повторное применение после `show()`): запомнить как эхо (§6.17).
    pub fn program_set(&mut self, g: WindowGeometry) {
        self.remembered = Some(g);
    }

    /// Первый показ окна: следующее показание на тике — ожидаемое эхо
    /// повторного применения геометрии после `show()`, а не действие
    /// пользователя (ОВС-5 а, ТЗ-11).
    pub fn window_shown(&mut self) {
        self.awaiting_first = true;
    }

    /// Показание геометрии на тике. `None` — изменения нет (эхо); `Some` —
    /// новое значение и его источник (§6.17).
    ///
    /// Вызывающий код уже подставил в `g.position` запомненное значение,
    /// если платформа (Wayland) не отдаёт позицию (ADR-22) — трекер этим
    /// не занимается.
    pub fn observe(&mut self, g: WindowGeometry) -> Option<(WindowGeometry, Origin)> {
        if self.awaiting_first {
            self.awaiting_first = false;
            self.remembered = Some(g);
            return Some((g, Origin::Program));
        }
        if self.remembered == Some(g) {
            return None;
        }
        self.remembered = Some(g);
        Some((g, Origin::User))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::state_file::{PhysPos, PhysSize};

    fn geom(x: i32) -> WindowGeometry {
        WindowGeometry {
            position: Some(PhysPos { x, y: 0 }),
            size: Some(PhysSize { width: 800, height: 600 }),
            maximized: false,
            fullscreen: false,
        }
    }

    #[test]
    fn program_set_echo_is_ignored() {
        let mut t = GeometryTracker::new();
        let g = geom(1);
        t.program_set(g);

        assert_eq!(t.observe(g), None);

        let g2 = geom(2);
        assert_eq!(t.observe(g2), Some((g2, Origin::User)));
    }

    #[test]
    fn first_reading_after_show_is_program() {
        let mut t = GeometryTracker::new();
        let g = geom(1);
        t.program_set(g);
        t.window_shown();

        let g_other = geom(2);
        assert_eq!(t.observe(g_other), Some((g_other, Origin::Program)));
        assert_eq!(t.observe(g_other), None);

        let g_another = geom(3);
        assert_eq!(t.observe(g_another), Some((g_another, Origin::User)));
    }

    #[test]
    fn user_change_reported_once() {
        let mut t = GeometryTracker::new();
        let g = geom(1);
        t.program_set(g);

        let g2 = geom(2);
        assert_eq!(t.observe(g2), Some((g2, Origin::User)));
        assert_eq!(t.observe(g2), None);
    }

    /// Восстановленная геометрия (program_set → window_shown → эхо на
    /// тике) не взводит дедлайн записи — эхо не должно быть `User`
    /// (ТЗ-11, ОВС-5 а).
    #[test]
    fn restored_geometry_does_not_start_deadline() {
        let mut t = GeometryTracker::new();
        let restored = geom(1);
        t.program_set(restored);
        t.window_shown();

        assert_eq!(t.observe(restored), Some((restored, Origin::Program)));
        assert_eq!(t.observe(restored), None);
    }
}
