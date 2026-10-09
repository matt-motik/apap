//! `FakePicker` — подмена `FilePicker` для автотестов (§7.1, ADR-10).
//! Компилируется всегда, без `cfg(test)`: тесты бинарника собирают библиотеку
//! без `cfg(test)`. Отвечает заданными путями, `Cancelled` или «никогда»
//! (`std::future::pending`, ТЗ-53); записывает все полученные запросы.

use super::{FilePicker, PickRequest, PickResult};
use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;

/// Ответ `FakePicker` на каждый запрос (§7.1, ADR-10).
#[derive(Clone)]
pub enum FakeAnswer {
    Paths(Vec<PathBuf>),
    Cancelled,
    /// Future никогда не завершается (диалог «висит открытым», ТЗ-53).
    Never,
}

/// Подмена `FilePicker` для автотестов (§7.1, ADR-10). Клоны делят один
/// журнал запросов через `Rc<RefCell<..>>`.
#[derive(Clone)]
pub struct FakePicker {
    answer: FakeAnswer,
    requests: Rc<RefCell<Vec<PickRequest>>>,
}

impl FakePicker {
    pub fn paths(paths: Vec<PathBuf>) -> FakePicker {
        FakePicker {
            answer: FakeAnswer::Paths(paths),
            requests: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn cancelled() -> FakePicker {
        FakePicker { answer: FakeAnswer::Cancelled, requests: Rc::new(RefCell::new(Vec::new())) }
    }

    pub fn never() -> FakePicker {
        FakePicker { answer: FakeAnswer::Never, requests: Rc::new(RefCell::new(Vec::new())) }
    }

    /// Журнал запросов в порядке вызовов; держит силу после `Box<dyn FilePicker>` (§7.1).
    pub fn requests(&self) -> Rc<RefCell<Vec<PickRequest>>> {
        self.requests.clone()
    }

    /// Число полученных запросов («второй выбор не открыт», §7.1).
    pub fn calls(&self) -> usize {
        self.requests.borrow().len()
    }
}

impl FilePicker for FakePicker {
    fn pick(
        &self,
        req: PickRequest,
        _parent: Option<&slint::Window>,
    ) -> Pin<Box<dyn Future<Output = PickResult>>> {
        self.requests.borrow_mut().push(req);
        match self.answer.clone() {
            FakeAnswer::Paths(paths) => Box::pin(async move { PickResult::Paths(paths) }),
            FakeAnswer::Cancelled => Box::pin(async move { PickResult::Cancelled }),
            FakeAnswer::Never => Box::pin(std::future::pending::<PickResult>()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll, Waker};

    fn poll_once<F: Future + ?Sized>(fut: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        fut.as_mut().poll(&mut cx)
    }

    #[test]
    fn platform_fakes_picker_paths_ready() {
        let picker = FakePicker::paths(vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        let mut fut = picker.pick(PickRequest::AddFiles { start: None }, None);
        match poll_once(&mut fut) {
            Poll::Ready(PickResult::Paths(paths)) => {
                assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
            }
            other => panic!("expected Ready(Paths(..)), got {other:?}"),
        }
    }

    #[test]
    fn platform_fakes_picker_cancelled_ready() {
        let picker = FakePicker::cancelled();
        let mut fut = picker.pick(PickRequest::AddFolder { start: None }, None);
        match poll_once(&mut fut) {
            Poll::Ready(PickResult::Cancelled) => {}
            other => panic!("expected Ready(Cancelled), got {other:?}"),
        }
    }

    #[test]
    fn platform_fakes_picker_never_pending() {
        let picker = FakePicker::never();
        let mut fut = picker.pick(PickRequest::OpenPlaylist { start: None }, None);
        for _ in 0..5 {
            assert!(matches!(poll_once(&mut fut), Poll::Pending));
        }
    }

    #[test]
    fn platform_fakes_picker_requests_recorded_in_order() {
        let picker = FakePicker::cancelled();
        let requests = picker.requests();

        let start_a = Some(PathBuf::from("/music"));
        let start_b = Some(PathBuf::from("/themes"));
        drop(picker.pick(PickRequest::AddFiles { start: start_a.clone() }, None));
        drop(picker.pick(PickRequest::ThemeFile { start: start_b.clone() }, None));

        assert_eq!(picker.calls(), 2);
        assert_eq!(
            requests.borrow().as_slice(),
            &[
                PickRequest::AddFiles { start: start_a },
                PickRequest::ThemeFile { start: start_b },
            ]
        );
    }
}
