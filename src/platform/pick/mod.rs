//! Асинхронный выбор файлов (ADR-10): не блокирует цикл событий (ТЗ-53); на
//! время выбора шлюз держит причину `BlockReason::FilePicker` (ADR-12).
//! `cfg(target_os)` только внутри реализаций платформы (ADR-6).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// Запрос на выбор файла/каталога (ADR-10, §2.8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PickRequest {
    AddFiles { start: Option<PathBuf> },
    AddFolder { start: Option<PathBuf> },
    OpenPlaylist { start: Option<PathBuf> },
    /// Расширение по умолчанию .m3u8 (ОВ-17).
    SavePlaylist { start: Option<PathBuf> },
    ThemeFile { start: Option<PathBuf> },
}

impl PickRequest {
    /// Стартовый каталог запроса (ADR-10).
    pub fn start(&self) -> Option<&Path> {
        match self {
            PickRequest::AddFiles { start }
            | PickRequest::AddFolder { start }
            | PickRequest::OpenPlaylist { start }
            | PickRequest::SavePlaylist { start }
            | PickRequest::ThemeFile { start } => start.as_deref(),
        }
    }
}

/// Результат выбора (ADR-10, §2.8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PickResult {
    Paths(Vec<PathBuf>),
    Cancelled,
}

/// Асинхронный выбор файлов (ADR-10, ТЗ-53, ТЗ-54 п. 5).
pub trait FilePicker {
    /// Future выполняется в `slint::spawn_local` (ADR-10).
    fn pick(
        &self,
        req: PickRequest,
        parent: Option<&slint::Window>,
    ) -> Pin<Box<dyn Future<Output = PickResult>>>;
}

pub mod fake;
pub use fake::{FakeAnswer, FakePicker};
