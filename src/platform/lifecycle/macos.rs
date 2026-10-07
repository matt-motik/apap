//! Запрос завершения macOS (ADR-7 п. 5, ТЗ-15): метод
//! `applicationShouldTerminate:` добавляется в класс `WinitApplicationDelegate`
//! через `class_addMethod` (делегат winit заменять нельзя). Если метод уже есть,
//! делегат подписывается на `NSApplicationWillTerminateNotification` с тем же
//! синхронным путём выхода (вариант Д). Не проверено запуском: ревью кода.

use super::{ExitEntry, PlatformError};
use crate::core::exit::ExitReason;
use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{class, ffi, msg_send, sel};
use std::cell::RefCell;
use std::ffi::CStr;

/// `NSTerminateNow` (`NSApplicationTerminateReply`).
const NS_TERMINATE_NOW: usize = 1;

/// Класс делегата приложения winit 0.30 (ADR-7, контекст).
const DELEGATE_CLASS: &str = "WinitApplicationDelegate";

thread_local! {
    /// `ExitEntry` главного (UI) потока: AppKit вызывает делегат в нём.
    static EXIT_ENTRY: RefCell<Option<ExitEntry>> = const { RefCell::new(None) };
}

/// Синхронный путь выхода (ADR-7 п. 2) по запросу завершения ОС.
fn run_exit() {
    let entry = EXIT_ENTRY.with(|e| e.borrow().clone());
    if let Some(entry) = entry {
        entry(ExitReason::MacosTerminate);
    }
}

/// `-[WinitApplicationDelegate applicationShouldTerminate:]` (ADR-7 п. 5):
/// путь выхода в пределах 5 с, затем `NSTerminateNow`.
extern "C" fn should_terminate(_this: *mut AnyObject, _cmd: Sel, _sender: *mut AnyObject) -> usize {
    run_exit();
    NS_TERMINATE_NOW
}

/// Наблюдатель `NSApplicationWillTerminateNotification` (ADR-7 п. 5, вариант Д).
extern "C" fn will_terminate(_this: *mut AnyObject, _cmd: Sel, _note: *mut AnyObject) {
    run_exit();
}

fn err(detail: &str) -> PlatformError {
    PlatformError { what: "завершение macOS", detail: detail.into() }
}

/// Добавляет метод в класс; `false` — метод с этим именем уже есть.
///
/// # Safety
/// `imp` — функция с сигнатурой, соответствующей `types`.
unsafe fn add_method(cls: &AnyClass, name: Sel, imp: ffi::IMP, types: &CStr) -> bool {
    let cls = cls as *const AnyClass as *mut ffi::objc_class;
    // SAFETY: класс зарегистрирован в рантайме; сигнатура `imp` совпадает с `types`.
    let added = unsafe { ffi::class_addMethod(cls, name.as_ptr(), imp, types.as_ptr()) };
    Bool::from_raw(added).as_bool()
}

/// Ставит перехват запроса завершения (ADR-7 п. 5, ТЗ-15). Вызывается в
/// главном потоке после инициализации бэкенда Slint (ADR-23 шаг 8).
pub fn install_terminate(entry: ExitEntry) -> Result<(), PlatformError> {
    let cls = AnyClass::get(DELEGATE_CLASS).ok_or_else(|| err("класс делегата winit не найден"))?;
    EXIT_ENTRY.with(|e| *e.borrow_mut() = Some(entry));

    type Handler = extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize;
    type Observer = extern "C" fn(*mut AnyObject, Sel, *mut AnyObject);
    // SAFETY: IMP — непрозрачный указатель на функцию; рантайм вызывает её
    // с сигнатурой из строки типов (`Q@:@` — NSUInteger, self, _cmd, sender).
    let imp: ffi::IMP = Some(unsafe { std::mem::transmute::<Handler, _>(should_terminate) });
    // SAFETY: `should_terminate` соответствует `Q@:@`.
    if unsafe { add_method(cls, sel!(applicationShouldTerminate:), imp, c"Q@:@") } {
        return Ok(());
    }

    eprintln!("applicationShouldTerminate: уже есть у делегата winit; подписка на NSApplicationWillTerminateNotification");
    // SAFETY: см. выше; `will_terminate` соответствует `v@:@`.
    let imp: ffi::IMP = Some(unsafe { std::mem::transmute::<Observer, _>(will_terminate) });
    // SAFETY: `will_terminate` соответствует `v@:@`.
    if !unsafe { add_method(cls, sel!(apapWillTerminate:), imp, c"v@:@") } {
        return Err(err("метод apapWillTerminate: не добавлен"));
    }
    // SAFETY: стандартные сообщения AppKit/Foundation в главном потоке;
    // делегат живёт всё время работы приложения.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let delegate: *mut AnyObject = msg_send![app, delegate];
        if delegate.is_null() {
            return Err(err("у NSApplication нет делегата"));
        }
        let name: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: c"NSApplicationWillTerminateNotification".as_ptr()
        ];
        let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        let _: () = msg_send![
            center,
            addObserver: delegate,
            selector: sel!(apapWillTerminate:),
            name: name,
            object: std::ptr::null_mut::<AnyObject>()
        ];
    }
    Ok(())
}
