//! Монитор прокрутки AppKit (ADR-8 «macOS-инерция», ОВС-1 а, §6.11, ТЗ-36):
//! winit 0.30.13 не отличает инерционные события прокрутки от движения
//! пальцев, поэтому локальный монитор `NSEvent` читает `momentumPhase` раньше
//! winit и кладёт метку `MomentumMark` в очередь `WheelMatcher`. Фильтр окна
//! (`winit.rs`) снимает метки в порядке диспетчеризации. Файл собирается
//! только на macOS. Не проверено запуском: ревью кода.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask};

use super::{MomentumMark, WheelMatcher};

/// Локальный монитор событий прокрутки AppKit (ADR-8, ОВС-1 а, §6.11).
/// Владеет объектом-монитором; при уничтожении снимает его
/// (`+[NSEvent removeMonitor:]`). Живёт в UI-потоке (§4.1): тип не `Send`.
pub(super) struct ScrollMonitor {
    /// Объект, возвращённый `addLocalMonitorForEventsMatchingMask:handler:`.
    token: Retained<AnyObject>,
}

impl ScrollMonitor {
    /// Ставит монитор на `NSEventMask::ScrollWheel` (ADR-8, ОВС-1 а, §6.11).
    /// Вызывается в главном (UI) потоке. `None` — AppKit монитор не создал:
    /// меток нет, и каждое событие winit считается неинерционным (§6.11).
    ///
    /// Блок вызывается в `-[NSApplication sendEvent:]` до `scrollWheel:`
    /// представления winit, то есть раньше фильтра окна. На каждое событие он
    /// кладёт одну метку и возвращает то же событие: оно не поглощается и не
    /// изменяется.
    ///
    /// Единицы `dy` совпадают с событием winit после пересчёта в `winit.rs`.
    /// При `hasPreciseScrollingDeltas` winit отдаёт `PixelDelta` =
    /// `scrollingDelta` × масштаб окна (физические px,
    /// `winit-0.30.13/src/platform_impl/macos/view.rs:668-676`), а фильтр делит
    /// его обратно на масштаб — получаются логические px, равные
    /// `scrollingDeltaY`. Иначе winit отдаёт `LineDelta` = `scrollingDeltaY`,
    /// приведённое к `f32`. Сопоставитель сравнивает `|dy|` с точностью `f32`,
    /// поэтому знак метки не важен.
    pub(super) fn install(matcher: Rc<RefCell<WheelMatcher>>) -> Option<ScrollMonitor> {
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit передаёт блоку действительное событие, живое на
            // время вызова; ссылка не переживает блок. Блок вызывается в
            // главном потоке — том же UI-потоке, что владеет `Rc`.
            let ev = unsafe { event.as_ref() };
            // SAFETY: свойства только читаются; маска монитора пропускает
            // лишь события прокрутки, для которых они определены.
            let (precise, dy, phase) = unsafe {
                (ev.hasPreciseScrollingDeltas(), ev.scrollingDeltaY(), ev.momentumPhase())
            };
            // Сопоставитель занят — метка пропускается: фильтр окна получит
            // пустую очередь или несогласованную метку и сочтёт событие
            // неинерционным (§6.11).
            if let Ok(mut matcher) = matcher.try_borrow_mut() {
                matcher.push_mark(MomentumMark { precise, dy, momentum: !phase.is_empty() });
            }
            event.as_ptr()
        });
        // SAFETY: вызов в главном потоке (`attach` выполняется в UI-потоке);
        // сигнатура блока совпадает с ожидаемой AppKit
        // (`NSEvent * _Nullable (^)(NSEvent *)`); AppKit копирует блок и
        // удерживает его до `removeMonitor:`, локальный `block` можно отпустить.
        let token = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::ScrollWheel, &block)
        }?;
        Some(ScrollMonitor { token })
    }
}

impl Drop for ScrollMonitor {
    /// Снимает монитор (ADR-8, ОВС-1 а): AppKit отпускает блок вместе с его
    /// `Rc` на сопоставитель.
    fn drop(&mut self) {
        // SAFETY: `token` — объект, возвращённый
        // `addLocalMonitorForEventsMatchingMask:handler:`, снимается один раз;
        // владелец (`WinitWheelInput`) живёт и уничтожается в UI-потоке.
        unsafe { NSEvent::removeMonitor(&self.token) };
    }
}
