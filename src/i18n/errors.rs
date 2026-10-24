//! Русский текст для ошибок (T1.0-B2, решение пользователя 2026-10-23).
//!
//! Два разных случая:
//! - **наши ошибки** — не системные, код + русский текст по уже
//!   существующим перечислениям движка ([`crate::audio::error::EngineFault`],
//!   [`crate::audio::error::CorruptKind`], ТЗ-86/87); код — стабильная
//!   метка для журнала/поддержки, не зависящая от текста;
//! - **системные ошибки** (ОС, драйвер) — [`system_error`] с известной
//!   причиной («наше описание (исходный текст)») и с неизвестной
//!   («Системная ошибка: <исходный>»); [`describe_io_error`] — готовая
//!   строка для `std::io::ErrorKind` (`FileError::Io`,
//!   `FileError::ReadDuringPlayback`, ТЗ-86/87).
//!
//! Подключение к местам вызова (`src/app/mod.rs::describe_open_error` и
//! `describe_file_error`, сейчас показывающим `{:?}` в обход перевода) —
//! отдельным шагом этой задачи.

use crate::audio::error::{CorruptKind, EngineFault};

/// Код + русский текст одной из наших ошибок движка (не системной,
/// [`EngineFault`], AM1.0 §2.4) — код для журнала, текст для пользователя.
pub fn engine_fault_text(fault: &EngineFault) -> (&'static str, String) {
    match fault {
        EngineFault::SpawnFailed { what } => {
            ("E_ENGINE_SPAWN", format!("не удалось запустить поток «{what}»"))
        }
        EngineFault::StoreFailed => ("E_ENGINE_STORE", "не удалось сохранить состояние движка".to_string()),
        EngineFault::DriverFormatUnstable => {
            ("E_ENGINE_FORMAT_UNSTABLE", "драйвер повторно изменил формат устройства".to_string())
        }
    }
}

/// Код + русский текст причины повреждения файла ([`CorruptKind`], ТЗ-86/87).
pub fn corrupt_kind_text(kind: &CorruptKind) -> (&'static str, &'static str) {
    match kind {
        CorruptKind::BadHeader => ("E_FILE_BAD_HEADER", "повреждён заголовок файла"),
        CorruptKind::ZeroChannels => ("E_FILE_ZERO_CHANNELS", "в файле не указаны каналы"),
        CorruptKind::BlockSizeOutOfRange => ("E_FILE_BLOCK_SIZE", "недопустимый размер блока"),
        CorruptKind::ChunkSizeOverflow => ("E_FILE_CHUNK_OVERFLOW", "переполнение размера блока"),
        CorruptKind::OffsetBeyondEof => ("E_FILE_OFFSET_EOF", "смещение выходит за конец файла"),
        CorruptKind::Truncated => ("E_FILE_TRUNCATED", "файл обрезан"),
        CorruptKind::DecodeFailed => ("E_FILE_DECODE_FAILED", "не удалось декодировать файл"),
    }
}

/// Системная ошибка (ОС/драйвер) с известной причиной — наше описание на
/// русском + исходный текст системы в скобках; с неизвестной (`known`
/// — `None`) — обобщённая фраза с исходным текстом (T1.0-B2, решение
/// пользователя 2026-10-23).
pub fn system_error(known: Option<&str>, original: &str) -> String {
    match known {
        Some(description) => format!("{description} ({original})"),
        None => format!("Системная ошибка: {original}"),
    }
}

/// Известная на русском причина для `std::io::ErrorKind` — `None`, если код
/// не распознан; тогда [`system_error`] берёт общую фразу.
pub fn io_error_reason(kind: std::io::ErrorKind) -> Option<&'static str> {
    use std::io::ErrorKind;
    match kind {
        ErrorKind::NotFound => Some("файл не найден"),
        ErrorKind::PermissionDenied => Some("нет доступа"),
        ErrorKind::AlreadyExists => Some("файл уже существует"),
        ErrorKind::TimedOut => Some("превышено время ожидания"),
        ErrorKind::Interrupted => Some("операция прервана"),
        ErrorKind::UnexpectedEof => Some("файл закончился раньше ожидаемого"),
        ErrorKind::OutOfMemory => Some("недостаточно памяти"),
        ErrorKind::WouldBlock => Some("ресурс временно недоступен"),
        ErrorKind::ConnectionRefused => Some("соединение отклонено"),
        _ => None,
    }
}

/// Готовая русская строка для `std::io::ErrorKind` (`FileError::Io`,
/// `FileError::ReadDuringPlayback`, ТЗ-86/87) — сочетает [`io_error_reason`]
/// и [`system_error`]; исходный текст — `Display` стандартной библиотеки.
pub fn describe_io_error(kind: std::io::ErrorKind) -> String {
    system_error(io_error_reason(kind), &kind.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_fault_spawn_failed_has_code_and_substitutes_what() {
        let (code, text) = engine_fault_text(&EngineFault::SpawnFailed { what: "декодер" });
        assert_eq!(code, "E_ENGINE_SPAWN");
        assert_eq!(text, "не удалось запустить поток «декодер»");
    }

    #[test]
    fn engine_fault_store_failed_text() {
        let (code, text) = engine_fault_text(&EngineFault::StoreFailed);
        assert_eq!(code, "E_ENGINE_STORE");
        assert_eq!(text, "не удалось сохранить состояние движка");
    }

    #[test]
    fn engine_fault_driver_format_unstable_text() {
        let (code, text) = engine_fault_text(&EngineFault::DriverFormatUnstable);
        assert_eq!(code, "E_ENGINE_FORMAT_UNSTABLE");
        assert_eq!(text, "драйвер повторно изменил формат устройства");
    }

    #[test]
    fn corrupt_kind_all_variants_have_distinct_codes() {
        let kinds = [
            CorruptKind::BadHeader,
            CorruptKind::ZeroChannels,
            CorruptKind::BlockSizeOutOfRange,
            CorruptKind::ChunkSizeOverflow,
            CorruptKind::OffsetBeyondEof,
            CorruptKind::Truncated,
            CorruptKind::DecodeFailed,
        ];
        let mut codes: Vec<&str> = kinds.iter().map(|k| corrupt_kind_text(k).0).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), kinds.len(), "коды не должны повторяться");
    }

    #[test]
    fn corrupt_kind_bad_header_text() {
        assert_eq!(corrupt_kind_text(&CorruptKind::BadHeader), ("E_FILE_BAD_HEADER", "повреждён заголовок файла"));
    }

    #[test]
    fn system_error_known_cause_wraps_original_in_parens() {
        assert_eq!(system_error(Some("нет доступа"), "permission denied"), "нет доступа (permission denied)");
    }

    #[test]
    fn system_error_unknown_cause_uses_generic_phrase() {
        assert_eq!(system_error(None, "weird errno 42"), "Системная ошибка: weird errno 42");
    }

    #[test]
    fn io_error_reason_known_kind_not_found() {
        assert_eq!(io_error_reason(std::io::ErrorKind::NotFound), Some("файл не найден"));
    }

    #[test]
    fn io_error_reason_unknown_kind_is_none() {
        assert_eq!(io_error_reason(std::io::ErrorKind::Other), None);
    }

    #[test]
    fn describe_io_error_known_kind_includes_original_text() {
        let text = describe_io_error(std::io::ErrorKind::PermissionDenied);
        assert!(text.starts_with("нет доступа ("), "text={text}");
        assert!(text.contains("permission"), "text={text}");
    }

    #[test]
    fn describe_io_error_unknown_kind_uses_generic_prefix() {
        let text = describe_io_error(std::io::ErrorKind::Other);
        assert!(text.starts_with("Системная ошибка: "), "text={text}");
    }
}
