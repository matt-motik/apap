//! Модуль ввода колёсика (ADR-8, ТЗ-36, НФ-8, §2.8, §6.11): сырые события
//! winit, Slint и ksni → нормализованные щелчки `Clicks`. `cfg(target_os)` и
//! `winit_030` для этой задачи встречаются только внутри
//! `src/platform/input/` (ТЗ-54 п. 3).

use crate::audio::clock::ClockInstant;

pub mod normalizer;
pub mod table;

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
