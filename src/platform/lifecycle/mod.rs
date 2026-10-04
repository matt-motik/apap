//! Жизненный цикл приложения: возможности платформы, путь выхода, сигналы
//! (ADR-6, ADR-7). На этом этапе (С2 (02)) — только `PlatformCaps`;
//! `Lifecycle`, `ExitReason`, обработка сигналов и подмена `FakeLifecycle`
//! приходят на этапе С5 (ADR-6, ADR-7).

/// Возможности платформы во время работы: код приложения проверяет
/// возможность, а не ОС (ADR-6, §2 заход 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PlatformCaps {
    pub tray: bool,
    pub notifications: bool,
}
