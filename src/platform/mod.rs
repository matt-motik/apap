//! Платформенные модули (ADR-6, ТЗ-54): трейты и типы без `cfg`, реализации
//! по ОС и подмены для автотестов. `cfg(target_os)` для этих задач
//! встречается только внутри `src/platform/`.

pub mod fs;
pub mod lifecycle;
pub mod notify;
