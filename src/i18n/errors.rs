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
//! `src/app/mod.rs::describe_open_error`/`describe_file_error`/
//! `describe_skip_reason` и подсказка кнопки «Сохранить» для недопустимого
//! цвета темы (`save_button_view`) подключены к этому модулю на T1.0-B8
//! шаг 7 (строки перенесены байт-в-байт, без изменения содержания).

use crate::audio::error::{CaptureFailure, CorruptKind, EngineFault};
use crate::theme::ThemeError;

use super::format_named;

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

/// Причина недопустимого цвета черновика темы для подсказки кнопки
/// «Сохранить» (`save_button_view`, ADR-11, §6.9, T1.0-B8): только
/// `InvalidHex` имеет русский текст — остальные варианты `ThemeError`
/// (`Io`/`Deserialize`) не показываются здесь (структура темы уже проверена
/// ядром) и используют `Display` как есть.
pub fn theme_error_reason(error: &ThemeError) -> String {
    match error {
        ThemeError::InvalidHex { field } => {
            format_named("недопустимый HEX-цвет в поле `{field}`", &[("field", field)])
        }
        other => other.to_string(),
    }
}

/// Подсказка кнопки «Сохранить» для недопустимого цвета темы (ADR-11,
/// §6.9, T1.0-B8): `reason` — вывод [`theme_error_reason`].
pub fn theme_tooltip(reason: &str) -> String {
    format_named("Тема: {reason}", &[("reason", reason)])
}

/// Короткая причина отказа захвата устройства ([`CaptureFailure`], ТЗ-46,
/// ТЗ-119, ТЗ-122, T1.0-B2) — текст замка и заголовок окна ошибки захвата
/// (`src/app/lock_view.rs::lock_view`, AM1.0-B2: вместо прежнего неверного
/// «устройство не найдено» показывается настоящая причина).
/// `ReservationDenied`/`BusyOutsideProtocol`/`DriverRefused` подставляют
/// владельца/errno (ТЗ-119, ТЗ-122).
pub fn capture_failure_label(f: &CaptureFailure) -> String {
    match f {
        CaptureFailure::ReservationDenied { owner: Some(owner) } => {
            format_named("резервирование отклонено ({owner})", &[("owner", owner)])
        }
        CaptureFailure::ReservationDenied { owner: None } => "резервирование отклонено".to_string(),
        CaptureFailure::OwnerNotResponding => "владелец резервирования не отвечает".to_string(),
        CaptureFailure::NoSessionBus => "нет сессионной шины D-Bus".to_string(),
        CaptureFailure::BusyOutsideProtocol { owner: Some(owner) } => {
            let pid = owner.pid.to_string();
            format_named(
                "устройство занято приложением {name} (PID {pid})",
                &[("name", &owner.name), ("pid", &pid)],
            )
        }
        CaptureFailure::BusyOutsideProtocol { owner: None } => {
            "устройство занято приложением вне протокола резервирования".to_string()
        }
        CaptureFailure::DriverRefused { errno } => {
            format_named("отказ драйвера (errno {errno})", &[("errno", &errno.to_string())])
        }
        CaptureFailure::LostOnReopen => "перехвачено при переоткрытии".to_string(),
        CaptureFailure::DeviceDisconnected => "устройство отключено".to_string(),
        CaptureFailure::ReservationLost => "резервирование перехвачено".to_string(),
        CaptureFailure::PlatformUnsupported => "платформа не поддерживается".to_string(),
        CaptureFailure::NoDeviceSelected => "устройство не выбрано".to_string(),
    }
}

/// Подсказка замка для отказа захвата (ТЗ-122, T1.0-B2): по умолчанию
/// совпадает с [`capture_failure_label`], но для «занято вне протокола» с
/// известным владельцем заменяется на указание закрыть приложение-владельца.
/// Тот же текст идёт в тело окна ошибки замка (`src/app/mod.rs`,
/// AM1.0-B14 шаг 5, `apply_lock_failure_message`/`show_lock_failure_message`).
pub fn capture_failure_hint(f: &CaptureFailure) -> String {
    match f {
        CaptureFailure::BusyOutsideProtocol { owner: Some(owner) } => format_named(
            "карту открыло напрямую приложение {name}, минуя резервирование PipeWire. \
Закройте его и нажмите «Повторить захват»",
            &[("name", &owner.name)],
        ),
        _ => capture_failure_label(f),
    }
}

/// Полный текст значка замка для прочих отказов захвата (ТЗ-46, T1.0-B2):
/// `label` — вывод [`capture_failure_label`]. Отдельная функция, не часть
/// `capture_failure_label`, т.к. `NoDeviceSelected` (ТЗ-144) показывает
/// [`LOCK_TEXT_NO_DEVICE_SELECTED`] без этой обёртки.
pub fn capture_failure_text(label: &str) -> String {
    format_named("не удалось захватить: {reason}", &[("reason", label)])
}

/// Текст значка замка в состоянии `Open` (`LockState::Open`, ТЗ-118, §6.19,
/// T1.0-B2).
pub const LOCK_TEXT_OPEN: &str = "открыт";
/// Текст значка замка в состоянии `Acquiring` (ТЗ-119, §6.19, T1.0-B2).
pub const LOCK_TEXT_ACQUIRING: &str = "захват…";
/// Текст значка замка в состоянии `Held` (ТЗ-118, §6.19, T1.0-B2).
pub const LOCK_TEXT_HELD: &str = "закрыт";
/// Подсказка состояния `Held` (ТЗ-121, T1.0-B2): звук чужих приложений,
/// адресованный захваченному устройству, система перенаправляет на другое —
/// он может прозвучать из другого выхода.
pub const LOCK_HINT_HELD: &str = "пока устройство захвачено, звук других приложений, \
адресованный этому устройству, система перенаправляет на другое устройство — \
он может прозвучать из другого выхода";
/// Подсказка состояния `NotApplicable` (ТЗ-50, T1.0-B2): замок не нужен в
/// Совместимом режиме.
pub const LOCK_HINT_NOT_APPLICABLE: &str = "в Совместимом режиме устройство не захватывается";
/// Текст и подсказка отказа `NoDeviceSelected` (ТЗ-144, T1.0-B2): отдельный
/// текст, кнопка-значок остаётся «открыт» (`src/app/lock_view.rs::lock_view`).
pub const LOCK_TEXT_NO_DEVICE_SELECTED: &str = "устройство не выбрано";

/// Текст несовместимости трека с устройством (ТЗ-57, ТЗ-80, §2.4,
/// T1.0-B8): план остановлен на этапе `blocked.at` в Строгом режиме — общий
/// для отказа открытия (`OpenError::Incompatible`) и пропуска трека на лету
/// (`SkipReason::Incompatible`, `src/app/mod.rs`). `blocked` — вывод
/// `music_player_rs::audio::path::report::blocked_text`.
pub fn incompatible_track_text(blocked: &str) -> String {
    format_named("трек несовместим с устройством — {blocked}", &[("blocked", blocked)])
}

/// Короткий текст потери устройства вывода при отказе открытия трека
/// (`OpenError::DeviceLost`, ТЗ-86, T1.0-B8).
pub const DEVICE_LOST_SHORT: &str = "устройство вывода потеряно";

/// Текст внутренней ошибки движка при отказе открытия трека
/// (`OpenError::Internal`, ТЗ-86, T1.0-B8). `text` — вывод
/// [`engine_fault_text`].
pub fn internal_fault_text(text: &str) -> String {
    format_named("внутренняя ошибка движка: {text}", &[("text", text)])
}

/// Текст недоступного на платформе режима при отказе открытия трека
/// (`OpenError::ModeUnavailable`, ТЗ-86, ТЗ-111, §6.18 п. 2, T1.0-B8).
/// `mode` — `Debug` активного режима (`{mode:?}`).
pub fn mode_unavailable_text(mode: &str) -> String {
    format_named("режим {mode} недоступен на этой платформе", &[("mode", mode)])
}

/// Текст повреждённого файла при отказе открытия/пропуске трека (ТЗ-86,
/// ТЗ-87, T1.0-B8). `text` — вывод [`corrupt_kind_text`].
pub fn corrupt_file_text(text: &str) -> String {
    format_named("файл повреждён: {text}", &[("text", text)])
}

/// Текст неподдерживаемого формата файла при отказе открытия/пропуске
/// трека (ТЗ-86, ТЗ-87, T1.0-B8).
pub fn unsupported_format_text(codec: &str) -> String {
    format_named("неподдерживаемый формат: {codec}", &[("codec", codec)])
}

/// Тег контейнера для `FileError::Unsupported` при пробе формата файла с
/// расширением, не принадлежащим DSD-контейнерам (DSF/DFF) (§6.18 шаг 3,
/// ТЗ-103, T1.0-B8, `src/audio/dsd.rs::probe_dsd_format`) — попадает в
/// [`unsupported_format_text`] как `codec`.
pub const DSD_NOT_A_CONTAINER: &str = "не DSD-контейнер";

/// Текст ошибки чтения файла при отказе открытия/пропуске трека (ТЗ-86,
/// ТЗ-87, T1.0-B8). `text` — вывод [`describe_io_error`].
pub fn read_error_text(text: &str) -> String {
    format_named("ошибка чтения: {text}", &[("text", text)])
}

/// Текст ошибки чтения на известном кадре во время воспроизведения
/// (ТЗ-86, ТЗ-87, T1.0-B8). `text` — вывод [`describe_io_error`].
pub fn read_error_at_frame_text(frame: &str, text: &str) -> String {
    format_named("ошибка чтения на кадре {frame}: {text}", &[("frame", frame), ("text", text)])
}

/// Отказ `configure`/`start` ALSA Exclusive без открытого PCM (вызван до
/// `acquire`, §6.7, T1.0-B8): внутренняя проверка инварианта, до пользователя
/// доходит через `OpenError` (`src/audio/backend/alsa.rs`).
pub const ALSA_CONFIGURE_NO_PCM: &str = "configure вызван без открытого PCM (acquire не выполнен, §6.7)";
/// Отказ `reconfigure` ALSA Exclusive без открытого PCM (§6.7, T1.0-B8).
pub const ALSA_RECONFIGURE_NO_PCM: &str = "reconfigure вызван без открытого PCM (acquire не выполнен, §6.7)";
/// Отказ `start` ALSA Exclusive без открытого PCM (§6.7, T1.0-B8).
pub const ALSA_START_NO_PCM: &str = "start вызван без открытого PCM (acquire не выполнен, §6.7)";

/// Текст отказа переоткрытия PCM перед `configure` после предыдущего
/// использования (§6.6, §6.7, ТЗ-49, ТЗ-109, AM1.0-B17, T1.0-B8). `cause` —
/// `Debug` исходной ошибки (`{e:?}`).
pub fn alsa_reopen_before_configure_failed_text(cause: &str) -> String {
    format_named(
        "не удалось переоткрыть PCM перед configure после предыдущего использования \
(§6.6, §6.7, ТЗ-49, ТЗ-109, AM1.0-B17): {cause}",
        &[("cause", cause)],
    )
}

/// Текст отказа размера периода, переполняющего буфер (§6.7, ТЗ-112,
/// T1.0-B8): общий для Exclusive-потока и Shared-потока без резервирования.
pub const ALSA_PERIOD_OVERFLOWS_BUFFER: &str = "размер периода переполняет буфер (§6.7, ТЗ-112)";
/// Текст отказа таймаута `wait`, переполненного или нулевой частотой (§6.7,
/// T1.0-B8): общий для Exclusive-потока и Shared-потока без резервирования.
pub const ALSA_WAIT_TIMEOUT_OVERFLOW: &str = "таймаут wait переполнен или частота равна нулю (§6.7)";
/// Текст отказа периода, переполняющего `u32` (§6.7, ТЗ-112, T1.0-B8).
pub const ALSA_PERIOD_OVERFLOWS_U32: &str = "период переполняет u32 (§6.7, ТЗ-112)";

/// Текст отказа `prepare()` над PCM в состоянии `SETUP` после повторного
/// использования предыдущего потока (§6.7, AM1.0-B17, T1.0-B8). `cause` —
/// исходная ошибка ALSA (`{e}`).
pub fn alsa_prepare_after_reuse_failed_text(cause: &str) -> String {
    format_named(
        "не удалось выполнить prepare() над PCM в состоянии SETUP после повторного \
использования предыдущего потока (§6.7, AM1.0-B17): {cause}",
        &[("cause", cause)],
    )
}

/// Текст отказа `prepare()` над Shared PCM после hw_params/sw_params (§6.7,
/// T1.0-B8, ADR-07 «Уточнение»). `cause` — исходная ошибка ALSA (`{e}`).
pub fn alsa_prepare_shared_failed_text(cause: &str) -> String {
    format_named(
        "не удалось выполнить prepare() над Shared PCM после hw_params/sw_params (§6.7): {cause}",
        &[("cause", cause)],
    )
}

/// Текст отказа запуска потока вывода `apap-alsa-out` (§6.7, ТЗ-51, T1.0-B8):
/// общий для Exclusive-потока и Shared-потока без резервирования. `cause` —
/// исходная ошибка ОС (`{e}`).
pub fn alsa_spawn_output_thread_failed_text(cause: &str) -> String {
    format_named("не удалось создать поток apap-alsa-out: {cause}", &[("cause", cause)])
}

/// Текст отказа Exclusive hw_params для формата, которого нет в Exclusive
/// (ТЗ-112, T1.0-B8): только `OutputSampleFormat::F32`. `sample` — `Debug`
/// формата (`{sample:?}`).
pub fn alsa_format_unsupported_exclusive_text(sample: &str) -> String {
    format_named("формат {sample} не поддерживается Exclusive (ТЗ-112)", &[("sample", sample)])
}

/// Текст отказа поиска формата в `FORMAT_MAP` (внутренний инвариант, ТЗ-112,
/// T1.0-B8). `sample` — `Debug` формата (`{sample:?}`).
pub fn alsa_format_not_in_map_text(sample: &str) -> String {
    format_named("формат {sample} не найден в FORMAT_MAP", &[("sample", sample)])
}

/// Текст отказа отрицательной ширины формата (внутренний инвариант, §6.7,
/// T1.0-B8). `sample` — `Debug` формата (`{sample:?}`).
pub fn alsa_format_width_negative_text(sample: &str) -> String {
    format_named("ширина формата {sample} отрицательна (§6.7)", &[("sample", sample)])
}

/// Текст отказа открытия Shared PCM по имени узла PipeWire (ADR-07
/// «Уточнение», AM1.0-B37, T1.0-B8, `src/audio/backend/shared.rs`).
/// `pcm_name` — ALSA-имя PCM (`pipewire:NODE=<node_name>`), `cause` —
/// исходная ошибка ALSA (`{e}`).
pub fn alsa_shared_pcm_open_failed_text(pcm_name: &str, cause: &str) -> String {
    format_named(
        "не удалось открыть Shared PCM {pcm_name}: {cause}",
        &[("pcm_name", pcm_name), ("cause", cause)],
    )
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

    /// AM1.0-B2, ТЗ-119, T1.0-B2: владелец резервирования называется в тексте
    /// вместо прежнего неверного «устройство не найдено».
    #[test]
    fn capture_failure_label_reservation_denied_with_owner_names_owner() {
        let f = CaptureFailure::ReservationDenied { owner: Some("Spotify".to_string()) };
        let text = capture_failure_label(&f);
        assert_eq!(text, "резервирование отклонено (Spotify)");
    }

    #[test]
    fn capture_failure_label_reservation_denied_without_owner() {
        let text = capture_failure_label(&CaptureFailure::ReservationDenied { owner: None });
        assert_eq!(text, "резервирование отклонено");
    }

    /// ТЗ-122, T1.0-B2: «занято вне протокола» с известным владельцем
    /// называет процесс и PID.
    #[test]
    fn capture_failure_label_busy_outside_protocol_with_owner() {
        use crate::audio::error::BusyOwner;

        let owner = BusyOwner { name: "aplay".to_string(), pid: 4242 };
        let text = capture_failure_label(&CaptureFailure::BusyOutsideProtocol { owner: Some(owner) });
        assert_eq!(text, "устройство занято приложением aplay (PID 4242)");
    }

    #[test]
    fn capture_failure_label_busy_outside_protocol_without_owner() {
        let text = capture_failure_label(&CaptureFailure::BusyOutsideProtocol { owner: None });
        assert_eq!(text, "устройство занято приложением вне протокола резервирования");
    }

    #[test]
    fn capture_failure_label_driver_refused_includes_errno() {
        let text = capture_failure_label(&CaptureFailure::DriverRefused { errno: 5 });
        assert!(text.contains('5'), "text={text}");
    }

    #[test]
    fn capture_failure_label_no_device_selected() {
        assert_eq!(capture_failure_label(&CaptureFailure::NoDeviceSelected), "устройство не выбрано");
    }

    /// ТЗ-122, T1.0-B2: подсказка с известным владельцем советует закрыть
    /// приложение-владельца, подсказка без владельца совпадает с текстом.
    #[test]
    fn capture_failure_hint_busy_owner_suggests_closing_app() {
        use crate::audio::error::BusyOwner;

        let owner = BusyOwner { name: "aplay".to_string(), pid: 4242 };
        let hint = capture_failure_hint(&CaptureFailure::BusyOutsideProtocol { owner: Some(owner) });
        assert_eq!(
            hint,
            "карту открыло напрямую приложение aplay, минуя резервирование PipeWire. \
Закройте его и нажмите «Повторить захват»"
        );
    }

    #[test]
    fn capture_failure_hint_falls_back_to_label() {
        let hint = capture_failure_hint(&CaptureFailure::DeviceDisconnected);
        assert_eq!(hint, "устройство отключено");
    }

    #[test]
    fn capture_failure_text_wraps_label() {
        assert_eq!(
            capture_failure_text("устройство занято приложением aplay (PID 4242)"),
            "не удалось захватить: устройство занято приложением aplay (PID 4242)"
        );
    }

    #[test]
    fn incompatible_track_text_substitutes_blocked() {
        assert_eq!(
            incompatible_track_text("ЦАП не поддерживает 32 бит"),
            "трек несовместим с устройством — ЦАП не поддерживает 32 бит"
        );
    }

    #[test]
    fn internal_fault_text_substitutes_text() {
        assert_eq!(internal_fault_text("не удалось сохранить состояние движка"), "внутренняя ошибка движка: не удалось сохранить состояние движка");
    }

    #[test]
    fn mode_unavailable_text_substitutes_mode() {
        assert_eq!(mode_unavailable_text("Strict"), "режим Strict недоступен на этой платформе");
    }

    #[test]
    fn corrupt_file_text_substitutes_text() {
        assert_eq!(corrupt_file_text("повреждён заголовок файла"), "файл повреждён: повреждён заголовок файла");
    }

    #[test]
    fn unsupported_format_text_substitutes_codec() {
        assert_eq!(unsupported_format_text("APE"), "неподдерживаемый формат: APE");
    }

    #[test]
    fn read_error_text_substitutes_text() {
        assert_eq!(read_error_text("файл не найден"), "ошибка чтения: файл не найден");
    }

    #[test]
    fn read_error_at_frame_text_substitutes_frame_and_text() {
        assert_eq!(
            read_error_at_frame_text("123456", "файл закончился раньше ожидаемого"),
            "ошибка чтения на кадре 123456: файл закончился раньше ожидаемого"
        );
    }

    #[test]
    fn theme_error_reason_invalid_hex_names_field() {
        let reason = theme_error_reason(&ThemeError::InvalidHex { field: "accent".to_string() });
        assert_eq!(reason, "недопустимый HEX-цвет в поле `accent`");
    }

    #[test]
    fn theme_tooltip_wraps_reason() {
        assert_eq!(theme_tooltip("недопустимый HEX-цвет в поле `accent`"), "Тема: недопустимый HEX-цвет в поле `accent`");
    }

    #[test]
    fn alsa_reopen_before_configure_failed_text_substitutes_cause() {
        let text = alsa_reopen_before_configure_failed_text("EBUSY");
        assert!(text.starts_with("не удалось переоткрыть PCM перед configure"), "text={text}");
        assert!(text.ends_with("EBUSY"), "text={text}");
    }

    #[test]
    fn alsa_prepare_after_reuse_failed_text_substitutes_cause() {
        let text = alsa_prepare_after_reuse_failed_text("EIO");
        assert!(text.starts_with("не удалось выполнить prepare() над PCM в состоянии SETUP"), "text={text}");
        assert!(text.ends_with("EIO"), "text={text}");
    }

    #[test]
    fn alsa_prepare_shared_failed_text_substitutes_cause() {
        assert_eq!(
            alsa_prepare_shared_failed_text("EIO"),
            "не удалось выполнить prepare() над Shared PCM после hw_params/sw_params (§6.7): EIO"
        );
    }

    #[test]
    fn alsa_spawn_output_thread_failed_text_substitutes_cause() {
        assert_eq!(
            alsa_spawn_output_thread_failed_text("os error 11"),
            "не удалось создать поток apap-alsa-out: os error 11"
        );
    }

    #[test]
    fn alsa_format_unsupported_exclusive_text_substitutes_sample() {
        assert_eq!(alsa_format_unsupported_exclusive_text("F32"), "формат F32 не поддерживается Exclusive (ТЗ-112)");
    }

    #[test]
    fn alsa_format_not_in_map_text_substitutes_sample() {
        assert_eq!(alsa_format_not_in_map_text("F32"), "формат F32 не найден в FORMAT_MAP");
    }

    #[test]
    fn alsa_format_width_negative_text_substitutes_sample() {
        assert_eq!(alsa_format_width_negative_text("S16LE"), "ширина формата S16LE отрицательна (§6.7)");
    }

    #[test]
    fn alsa_shared_pcm_open_failed_text_substitutes_both() {
        assert_eq!(
            alsa_shared_pcm_open_failed_text("pipewire:NODE=foo", "EBUSY"),
            "не удалось открыть Shared PCM pipewire:NODE=foo: EBUSY"
        );
    }
}
