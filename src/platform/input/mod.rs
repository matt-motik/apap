//! Модуль ввода колёсика (ADR-8, ТЗ-36, НФ-8, §2.8, §6.11): сырые события
//! winit, Slint и ksni → нормализованные щелчки `Clicks`. `cfg(target_os)` и
//! `winit_030` для этой задачи встречаются только внутри
//! `src/platform/input/` (ТЗ-54 п. 3).

use crate::audio::clock::ClockInstant;

pub mod fake;
pub mod matcher;
pub mod normalizer;
pub mod table;

pub use fake::FakeWheelInput;
pub use matcher::{MomentumMark, WheelMatcher};
pub use normalizer::WheelNormalizer;
pub use table::{
    parse_wheel_table, Outcome, SliderRow, SliderRule, TableError, TouchpadParams, TrayRow,
    WheelTable,
};

// Временный мост этапа С11: определения переносятся сюда отдельными шагами.
pub use crate::core::volume::Clicks;
pub use crate::platform::lifecycle::TrayScroll;

/// Событие колёсика winit в логических единицах, без пересчёта (§2.8, ADR-8).
/// `momentum`: `None` — платформа не сообщает инерцию; `Some(true)` —
/// инерционное событие macOS.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct WheelEvent {
    pub t: ClockInstant,
    pub delta: WheelDelta,
    pub phase: WheelPhase,
    pub momentum: Option<bool>,
}

/// Смещение колёсика (§2.8, ADR-8): `Lines` — строки (колесо), `Px` —
/// логические пиксели (тачпад). Положительное `y` — громче.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum WheelDelta {
    Lines { x: f32, y: f32 },
    Px { x: f64, y: f64 },
}

/// Фаза жеста прокрутки winit (§2.8, ADR-8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum WheelPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// Событие `scroll-event` ползунка Slint в логических пикселях (§2.8, ADR-8).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SlintScroll {
    pub t: ClockInstant,
    pub dx: f32,
    pub dy: f32,
}

/// Класс устройства прокрутки (§2.8, ADR-8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DeviceClass {
    Wheel,
    Touchpad,
}

/// Рабочий стол — ключ строки таблицы модуля ввода (§2.8, ADR-8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Desktop {
    Kde,
    Gnome,
    OtherLinux,
    Windows,
    Macos,
}

/// Тип сеанса окна — ключ строки таблицы модуля ввода (§2.8, ADR-8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Session {
    Wayland,
    X11,
    Win32,
    AppKit,
}

/// Хост значка трея (StatusNotifier) (§2.8, ADR-8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TrayHost {
    KdePlasma,
    GnomeAppIndicator,
    Unknown,
}

/// Модуль ввода колёсика (§2.8, §6.11, ADR-8): сырые события прокрутки →
/// щелчки `Clicks`. Реальная реализация держит фильтр окна, монитор AppKit,
/// нормализатор и сопоставитель; подмена `FakeWheelInput` подаёт события
/// напрямую.
pub trait WheelInput {
    /// Регистрирует единственный фильтр окна (ADR-8); на macOS — ещё и
    /// монитор AppKit (§6.11, ОВС-1 а).
    fn attach(&mut self, window: &slint::Window) -> AttachOutcome;
    /// `scroll-event` ползунка громкости → щелчки (§6.11 «Сопоставление»,
    /// В-2): с парным событием winit — по таблице, без пары — правило по
    /// умолчанию. Положительный результат — громче.
    fn slider_scroll(&mut self, ev: SlintScroll) -> Clicks;
    /// Прокрутка над значком трея → щелчки (§6.11 «Трей», В-5).
    fn tray_scroll(&mut self, ev: TrayScroll) -> Clicks;
    /// Конец тика: событие winit без пары отбрасывается (§6.11, В-2).
    fn end_tick(&mut self);
    /// Хост значка трея, определённый потоком трея (§6.11 «Хост трея», ADR-8).
    fn set_tray_host(&mut self, host: TrayHost);
}

/// Исход `WheelInput::attach` (§2.8, ADR-8): `Winit` — фильтр окна
/// зарегистрирован; `DefaultRuleOnly` — событий winit нет, ползунок работает
/// по правилу по умолчанию без пары (§6.11, В-2), `reason` — для журнала.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AttachOutcome {
    Winit,
    DefaultRuleOnly { reason: Box<str> },
}
