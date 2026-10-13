//! `WinitWheelInput` — реальный модуль ввода колёсика (ADR-8, ТЗ-36, НФ-8,
//! §2.8, §6.11): единственный фильтр окна `on_winit_window_event` кладёт
//! событие winit в `WheelMatcher`, `scroll-event` ползунка забирает пару,
//! `WheelNormalizer` считает щелчки. `winit_030` и `cfg(target_os)` для этой
//! задачи встречаются только в `src/platform/input/` (ТЗ-54 п. 3). На macOS
//! `attach` дополнительно ставит монитор AppKit (`macos.rs`, ОВС-1 а): он
//! кладёт метки инерции в очередь того же `WheelMatcher`.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::winit_030::winit::event::{MouseScrollDelta, TouchPhase, WindowEvent};
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::winit::window::Window as WinitWindow;
use slint::winit_030::{EventResult, WinitWindowAccessor};

use super::{
    parse_wheel_table, AttachOutcome, Clicks, Desktop, Session, SlintScroll, TouchpadDirection,
    TrayHost, TrayScroll, WheelDelta, WheelEvent, WheelInput, WheelMatcher, WheelNormalizer,
    WheelPhase, WheelTable,
};
use crate::audio::clock::Clock;
use crate::journal::{Journal, JournalRecord};

/// Таблица данных модуля ввода в составе исходников (ADR-8, НФ-8, §6.11).
const WHEEL_TABLE_TOML: &str = include_str!("wheel_table.toml");

/// Реальный модуль ввода (§2.8, §6.11, ADR-8). Живёт в UI-потоке: сопоставитель
/// разделяется с фильтром окна через `Rc<RefCell<…>>`, поэтому тип не `Send`.
pub struct WinitWheelInput {
    normalizer: WheelNormalizer,
    /// Слот «ожидает пары» — общий с фильтром окна (ADR-8 Б).
    matcher: Rc<RefCell<WheelMatcher>>,
    /// Сеанс, определённый фильтром по первому событию, когда окна winit при
    /// `attach` ещё не было; переносится в нормализатор в `slider_scroll`.
    detected: Rc<Cell<Option<Session>>>,
    /// Часы меток `WheelEvent::t`; те же, что у `SlintScroll::t` (§6.11).
    clock: Rc<dyn Clock>,
    /// Хост значка трея (§6.11 «Хост трея»); до `set_tray_host` — `Unknown`.
    tray_host: TrayHost,
    /// Причина, по которой бэкенд winit не выбран (ADR-8 «Подключение»).
    backend_error: Option<Box<str>>,
    /// Монитор прокрутки AppKit (ADR-8 «macOS-инерция», ОВС-1 а, §6.11):
    /// ставится в `attach`, снимается при уничтожении модуля. `None` — ещё не
    /// поставлен или AppKit его не создал: меток нет, события неинерционные.
    #[cfg(target_os = "macos")]
    monitor: Option<super::macos::ScrollMonitor>,
}

impl WinitWheelInput {
    /// Модуль ввода для текущей платформы (ADR-8, §6.11). Таблица —
    /// `wheel_table.toml`; при ошибке разбора действует `WheelTable::builtin`
    /// с записью журнала `WheelTable`. `backend_error` — ошибка выбора бэкенда
    /// winit: с ней `attach` фильтр не регистрирует. Системное направление
    /// прокрутки тачпада (ТЗ-36, §6.11 «Направление тачпада») читается здесь
    /// же: при ошибке — одна запись журнала `TouchpadDirection`, знак событий
    /// тачпада не исправляется.
    pub fn new(
        journal: &dyn Journal,
        clock: Box<dyn Clock>,
        backend_error: Option<Box<str>>,
    ) -> Self {
        let table = parse_wheel_table(WHEEL_TABLE_TOML).unwrap_or_else(|error| {
            journal.record(JournalRecord::WheelTable { error: error.0 });
            WheelTable::builtin()
        });
        let mut normalizer = WheelNormalizer::new(table, current_desktop(), initial_session());
        match system_touchpad_direction() {
            Ok(direction) => normalizer.set_touchpad_direction(direction),
            Err(error) => journal.record(JournalRecord::TouchpadDirection { error }),
        }
        WinitWheelInput {
            normalizer,
            matcher: Rc::new(RefCell::new(WheelMatcher::new())),
            detected: Rc::new(Cell::new(None)),
            clock: Rc::from(clock),
            tray_host: TrayHost::Unknown,
            backend_error,
            #[cfg(target_os = "macos")]
            monitor: None,
        }
    }
}

impl WheelInput for WinitWheelInput {
    /// Регистрирует единственный фильтр окна (ADR-8, §6.11 «Сопоставление»).
    /// Фильтр ставится на адаптер окна, даже если окна winit ещё нет; сеанс
    /// уточняется по raw-window-handle сразу или по первому событию колёсика.
    /// Бэкенд не winit — `DefaultRuleOnly` без регистрации (В-2). На macOS
    /// после фильтра ставится монитор AppKit (ADR-8 «macOS-инерция», ОВС-1 а,
    /// §6.11) — один на модуль: повторный `attach` второй монитор не ставит.
    fn attach(&mut self, window: &slint::Window) -> AttachOutcome {
        if let Some(reason) = &self.backend_error {
            return AttachOutcome::DefaultRuleOnly { reason: reason.clone() };
        }
        let mut resolved = false;
        if let Some(found) = window.with_winit_window(window_session) {
            resolved = true;
            if found.is_some() {
                self.normalizer.set_session(found);
            }
        }
        let matcher = Rc::clone(&self.matcher);
        let detected = Rc::clone(&self.detected);
        let clock = Rc::clone(&self.clock);
        window.on_winit_window_event(move |window, event| {
            let WindowEvent::MouseWheel { delta, phase, .. } = event else {
                return EventResult::Propagate;
            };
            if !resolved {
                if let Some(found) = window.with_winit_window(window_session) {
                    resolved = true;
                    if found.is_some() {
                        detected.set(found);
                    }
                }
            }
            let Some(delta) = logical_delta(*delta, f64::from(window.scale_factor())) else {
                return EventResult::Propagate;
            };
            let ev = WheelEvent { t: clock.now(), delta, phase: wheel_phase(*phase), momentum: None };
            if let Ok(mut matcher) = matcher.try_borrow_mut() {
                // macOS (ОВС-1 а): `momentum` проставляется по метке очереди.
                if cfg!(target_os = "macos") {
                    matcher.push_winit_marked(ev);
                } else {
                    matcher.push_winit(ev);
                }
            }
            EventResult::Propagate
        });
        // macOS (ОВС-1 а, §6.11): метки инерции для `push_winit_marked`. Без
        // монитора очередь пуста, и событие считается неинерционным.
        #[cfg(target_os = "macos")]
        if self.monitor.is_none() {
            self.monitor = super::macos::ScrollMonitor::install(Rc::clone(&self.matcher));
        }
        AttachOutcome::Winit
    }

    /// `scroll-event` ползунка (§6.11 «Сопоставление», В-2): пара с событием
    /// winit → `slider`, иначе правило по умолчанию без пары. В начале нового
    /// свайпа (`touch_idle`) системное направление тачпада перечитывается
    /// (ТЗ-36, §6.11 «Направление тачпада»): смена настройки действует без
    /// перезапуска; при ошибке чтения знак не исправляется, журнал не пишется.
    fn slider_scroll(&mut self, ev: SlintScroll) -> Clicks {
        if let Some(session) = self.detected.take() {
            self.normalizer.set_session(Some(session));
        }
        if self.normalizer.touch_idle(ev.t) {
            self.normalizer.set_touchpad_direction(system_touchpad_direction().unwrap_or(None));
        }
        let paired = self.matcher.try_borrow_mut().ok().and_then(|mut m| m.take_matching(ev));
        match paired {
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
        if let Ok(mut matcher) = self.matcher.try_borrow_mut() {
            matcher.end_tick();
        }
    }

    fn set_tray_host(&mut self, host: TrayHost) {
        self.tray_host = host;
    }
}

/// Дельта winit в единицах `WheelDelta` (§2.8, §6.11): строки — как есть,
/// физические пиксели — в логические делением на масштаб окна, как делает
/// Slint. Недопустимый масштаб — `None`: пары со `scroll-event` не будет.
fn logical_delta(delta: MouseScrollDelta, scale: f64) -> Option<WheelDelta> {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => Some(WheelDelta::Lines { x, y }),
        MouseScrollDelta::PixelDelta(p) => (scale.is_normal() && scale > 0.0)
            .then(|| WheelDelta::Px { x: p.x / scale, y: p.y / scale }),
    }
}

/// Фаза жеста winit → `WheelPhase` (§2.8).
fn wheel_phase(phase: TouchPhase) -> WheelPhase {
    match phase {
        TouchPhase::Started => WheelPhase::Started,
        TouchPhase::Moved => WheelPhase::Moved,
        TouchPhase::Ended => WheelPhase::Ended,
        TouchPhase::Cancelled => WheelPhase::Cancelled,
    }
}

/// Сеанс окна по raw-window-handle (§6.11, ADR-8); `None` — вид окна вне
/// таблицы, действует сеанс, определённый при запуске.
fn window_session(window: &WinitWindow) -> Option<Session> {
    match window.window_handle().map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Wayland(_)) => Some(Session::Wayland),
        Ok(RawWindowHandle::Xlib(_)) | Ok(RawWindowHandle::Xcb(_)) => Some(Session::X11),
        Ok(RawWindowHandle::Win32(_)) => Some(Session::Win32),
        Ok(RawWindowHandle::AppKit(_)) => Some(Session::AppKit),
        _ => None,
    }
}

/// Рабочий стол — ключ строки таблицы (§2.8, ADR-8).
fn current_desktop() -> Desktop {
    if cfg!(target_os = "windows") {
        Desktop::Windows
    } else if cfg!(target_os = "macos") {
        Desktop::Macos
    } else {
        desktop_from_xdg(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
    }
}

/// Сеанс до `attach` (§2.8, ADR-8); уточняется по raw-window-handle окна.
fn initial_session() -> Option<Session> {
    if cfg!(target_os = "windows") {
        Some(Session::Win32)
    } else if cfg!(target_os = "macos") {
        Some(Session::AppKit)
    } else {
        session_from_env(
            &std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
            &std::env::var("WAYLAND_DISPLAY").unwrap_or_default(),
        )
    }
}

/// Системное направление прокрутки тачпада (ТЗ-36, ТЗ-54 п. 3, §6.11
/// «Направление тачпада»). Windows — настройка из реестра (`windows.rs`);
/// ошибка — значение не прочитано или не распознано. На прочих системах
/// настройка не читается: `None` — знак как пришёл, без записи журнала.
fn system_touchpad_direction() -> Result<Option<TouchpadDirection>, Box<str>> {
    #[cfg(windows)]
    {
        super::windows::touchpad_direction().map(Some)
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

/// Рабочий стол Linux по `XDG_CURRENT_DESKTOP` (§2.8, ADR-8): значение —
/// список через `:` (например, `ubuntu:GNOME`), регистр не учитывается.
fn desktop_from_xdg(value: &str) -> Desktop {
    let value = value.to_ascii_uppercase();
    if value.contains("KDE") {
        Desktop::Kde
    } else if value.contains("GNOME") {
        Desktop::Gnome
    } else {
        Desktop::OtherLinux
    }
}

/// Сеанс Linux по `XDG_SESSION_TYPE`, затем по `WAYLAND_DISPLAY` (§2.8);
/// `None` — не определён до появления окна.
fn session_from_env(session_type: &str, wayland_display: &str) -> Option<Session> {
    if session_type.eq_ignore_ascii_case("wayland") {
        Some(Session::Wayland)
    } else if session_type.eq_ignore_ascii_case("x11") {
        Some(Session::X11)
    } else if !wayland_display.is_empty() {
        Some(Session::Wayland)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::winit_030::winit::dpi::PhysicalPosition;

    /// §2.8, ADR-8: `XDG_CURRENT_DESKTOP` → ключ таблицы.
    #[test]
    fn desktop_from_xdg_recognizes_kde_and_gnome() {
        assert_eq!(desktop_from_xdg("KDE"), Desktop::Kde);
        assert_eq!(desktop_from_xdg("ubuntu:GNOME"), Desktop::Gnome);
        assert_eq!(desktop_from_xdg("gnome-classic:gnome"), Desktop::Gnome);
        assert_eq!(desktop_from_xdg("XFCE"), Desktop::OtherLinux);
        assert_eq!(desktop_from_xdg(""), Desktop::OtherLinux);
    }

    /// §2.8: сеанс Linux до появления окна.
    #[test]
    fn session_from_env_prefers_session_type() {
        assert_eq!(session_from_env("wayland", ""), Some(Session::Wayland));
        assert_eq!(session_from_env("x11", "wayland-0"), Some(Session::X11));
        assert_eq!(session_from_env("", "wayland-0"), Some(Session::Wayland));
        assert_eq!(session_from_env("tty", ""), None);
    }

    /// §6.11: `PixelDelta` — в логические px; строки — без пересчёта;
    /// недопустимый масштаб события не даёт.
    #[test]
    fn logical_delta_divides_pixels_by_scale() {
        let px = MouseScrollDelta::PixelDelta(PhysicalPosition::new(3.0, -45.0));
        assert_eq!(logical_delta(px, 1.5), Some(WheelDelta::Px { x: 2.0, y: -30.0 }));
        assert_eq!(logical_delta(px, 0.0), None);
        assert_eq!(logical_delta(px, f64::NAN), None);
        let lines = MouseScrollDelta::LineDelta(0.0, -2.0);
        assert_eq!(logical_delta(lines, 0.0), Some(WheelDelta::Lines { x: 0.0, y: -2.0 }));
    }
}
