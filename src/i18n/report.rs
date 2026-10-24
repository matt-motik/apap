//! Русский текст отчёта окна «Путь сигнала» (T1.0-B8; AM1.0 §3.6, §6.25,
//! ТЗ-70…ТЗ-81, ТЗ-129, ТЗ-131, ТЗ-145, ADR-25, ОВС-6, ОВС-8, ОВС-11) —
//! перенесено из `src/audio/path/report.rs` байт-в-байт (решение
//! пользователя 2026-10-24), без изменения содержания.
//!
//! Часть 1 (этот файл): заголовки/заметки этапов «Источник»…«Устройство»,
//! причины несовместимости (ТЗ-80), причины преобразований (ТЗ-81),
//! MD5 (ТЗ-69), буфер устройства (ТЗ-131), сводки форматов. Названия
//! режима (`ModeKind`), API/аудиосервера (`OsServer`) и `Display` отчёта —
//! часть 2.
//!
//! Коды форматов (FLAC, ALAC, float32…) и названия API/серверов без
//! русских слов этим модулем не переводятся (см. `super::super`).

use super::format_named;

/// Граница гарантии (ТЗ-78): показывается во всех режимах.
pub const GUARANTEE_BOUNDARY: &str = "Плеер отвечает за данные до входа драйвера устройства. \
Что делает с сигналом сам ЦАП (внутренний ресемплинг, цифровая громкость, DSP), плеер не контролирует.";

/// Заголовок отчёта, когда поток не открыт (ТЗ-56).
pub const NO_STREAM_TITLE: &str = "Поток не открыт";

/// Заметка этапа-заглушки, когда поток не открыт (ТЗ-56).
pub const STREAM_NOT_OPEN_NOTE: &str = "поток не открыт";

/// «Источник» (ТЗ-74): формат из открытого файла.
pub fn opened_from_file_note(fmt: &str) -> String {
    format_named("{fmt} (из открытого файла)", &[("fmt", fmt)])
}

/// «Источник» (ТЗ-74): пометка lossy-кодека с округлением до 24 бит.
pub fn lossy_source_note(codec: &str) -> String {
    format_named("источник с потерями ({codec}): 24 бит (выход декодера)", &[("codec", codec)])
}

/// «Декодер» (ТЗ-75): изменение при lossy-кодеке без дизеринга.
pub const LOSSY_DECODE_CHANGE: &str = "с потерями, округление до 24 бит без дизеринга";

/// «Декодер» (ТЗ-75): причина округления — lossy-кодек источника.
pub fn lossy_codec_cause(codec: &str) -> String {
    format_named("кодек с потерями ({codec})", &[("codec", codec)])
}

/// «Декодер» (ТЗ-75): число ошибок декодирования за время воспроизведения трека.
pub fn decode_errors_note(count: u32) -> String {
    format!("ошибок декодирования: {count}")
}

/// «Декодер»/«Обработка» (ТЗ-75, ТЗ-129, ОВС-11): «ограничено N сэмплов».
pub fn clipped_samples_note(count: u64) -> String {
    format!("ограничено {count} сэмплов")
}

/// «Обработка» (ТЗ-16): фактическая граница фильтра DSD→PCM меньше выбранной.
pub fn filter_boundary_note(actual: &str, chosen: &str) -> String {
    format_named(
        "фактическая граница фильтра {actual} (выбрано {chosen})",
        &[("actual", actual), ("chosen", chosen)],
    )
}

/// «Вывод» (ТЗ-76): API/доступ и имя устройства.
pub fn api_device_note(api: &str, device: &str) -> String {
    format_named("{api}, «{device}»", &[("api", api), ("device", device)])
}

/// «Вывод» (ТЗ-76): запрошенный формат потока.
pub fn requested_note(stream: &str) -> String {
    format_named("запрошено: {stream}", &[("stream", stream)])
}

/// «Вывод» (ТЗ-76): драйвер принял отличный от запрошенного формат.
pub fn driver_accepted_change(stream: &str) -> String {
    format_named("драйвер принял {stream}", &[("stream", stream)])
}

/// «Вывод» (ТЗ-76): формат, прочитанный у драйвера.
pub fn actual_driver_note(stream: &str) -> String {
    format_named("фактически: {stream} (прочитано у драйвера)", &[("stream", stream)])
}

/// «Вывод» (ТЗ-76): формат драйвера не прочитан.
pub const REQUESTED_UNCONFIRMED_NOTE: &str = "запрошено, не подтверждено";

/// «Вывод» (ОВ-35): устройство открыто без сервера резервирования.
pub const NO_RESERVATION_SERVER_NOTE: &str = "устройство открыто без резервирования: сервера резервирования нет";

/// «Вывод» (ОВС-8): приоритет реального времени получен.
pub fn rt_priority_granted_note(prio: u32) -> String {
    format!("приоритет реального времени: получен ({prio})")
}

/// «Вывод» (ОВС-8): приоритет реального времени не получен.
pub const RT_PRIORITY_DENIED_NOTE: &str = "приоритет реального времени: не получен";

/// «Вывод» (ТЗ-68): счётчик Underrun.
pub fn underrun_note(n: u32) -> String {
    format!("Underrun: {n}")
}

/// «Вывод» (ТЗ-68): счётчик Xrun (только Exclusive).
pub fn xrun_note(n: u32) -> String {
    format!("Xrun: {n}")
}

/// «Уровень ОС» (ТЗ-77): монопольный доступ, сервер не участвует.
pub const OS_NOT_INVOLVED_NOTE: &str = "не участвует: монопольный доступ к устройству";

/// «Уровень ОС» (ТЗ-77): частота сервера не определена.
pub const RATE_UNKNOWN: &str = "частота неизвестна";

/// «Уровень ОС» (ТЗ-77): частота сервера известна.
pub fn rate_known_note(khz: &str) -> String {
    format_named("частота {khz}", &[("khz", khz)])
}

/// «Уровень ОС» (ТЗ-77): формат микшера сервера не определён.
pub const FORMAT_UNKNOWN: &str = "формат неизвестен";

/// «Уровень ОС» (ТЗ-77): сервер, частота и формат микшера.
pub fn os_known_note(server: &str, rate: &str, format: &str) -> String {
    format_named(
        "{server}: {rate}, {format}; не контролируется плеером",
        &[("server", server), ("rate", rate), ("format", format)],
    )
}

/// «Уровень ОС» (ТЗ-77): сервер не определён.
pub const OS_UNKNOWN_NOTE: &str = "не контролируется плеером, возможно преобразование";

/// «Устройство» (ТЗ-78): принятый формат подтверждён.
pub fn accepted_format_note(stream: &str) -> String {
    format_named("принят формат {stream}", &[("stream", stream)])
}

/// «Устройство» (ТЗ-78): принятый формат не подтверждён.
pub const ACCEPTED_FORMAT_UNCONFIRMED_NOTE: &str = "принятый формат не подтверждён";

/// «Устройство» (ТЗ-145, ADR-25, §6.25): подсказка про тишину на ЦАП без DoP.
pub const DOP_SILENCE_HINT: &str = "если ЦАП молчит — он не понимает DoP: снимите «ЦАП понимает DoP»";

/// Заголовок отчёта для пропущенного трека (ТЗ-80).
pub fn skipped_title(source: &str, mode: &str) -> String {
    format_named("{source} — {mode}: трек пропущен", &[("source", source), ("mode", mode)])
}

/// Заметка этапа-заглушки после первого несовместимого этапа (ТЗ-80).
pub const NOT_BUILT_NOTE: &str = "не построен";

/// Изменение этапа, на котором план остановлен несовместимостью (ТЗ-80).
pub const INCOMPATIBLE_CHANGE: &str = "несовместимо";

/// Причина несовместимости: частота не поддерживается устройством (ТЗ-80).
pub fn rate_unsupported(rate: &str) -> String {
    format_named("частота {rate} не поддерживается устройством", &[("rate", rate)])
}

/// Причина несовместимости: усечение разрядности запрещено режимом (ТЗ-80).
pub fn truncation_forbidden(from_bits: u8, max: u8) -> String {
    format!("{from_bits} бит: устройство принимает не более {max} бит, усечение запрещено режимом")
}

/// Причина несовместимости: число каналов не поддерживается устройством (ТЗ-80).
pub fn channels_unsupported(src: u8) -> String {
    format!("{src} кан. не поддерживается устройством")
}

/// Причина несовместимости: DoP не поддерживается, известна граница DoP устройства (ТЗ-80).
pub fn dop_unsupported_max(dsd: &str, max: &str) -> String {
    format_named(
        "DoP {dsd} не поддерживается устройством (не выше {max})",
        &[("dsd", dsd), ("max", max)],
    )
}

/// Причина несовместимости: DoP не поддерживается, граница DoP устройства неизвестна (ТЗ-80).
pub fn dop_unsupported(dsd: &str) -> String {
    format_named("DoP {dsd} не поддерживается устройством", &[("dsd", dsd)])
}

/// Причина несовместимости: DoP выключен настройкой «ЦАП понимает DoP» (ТЗ-145, ADR-25).
pub fn dop_disabled_for_device(device: &str) -> String {
    format_named("DoP выключен для устройства {device}", &[("device", device)])
}

/// Причина несовместимости: драйвер принял другую частоту, чем запрошено (ТЗ-80).
pub fn driver_rejected_rate(actual: &str, requested: &str) -> String {
    format_named(
        "драйвер принял {actual} вместо запрошенной {requested}",
        &[("actual", actual), ("requested", requested)],
    )
}

/// Причина несовместимости: драйвер принял другой формат, чем запрошено (ТЗ-80).
pub fn driver_rejected_format(actual: &str, requested: &str) -> String {
    format_named(
        "драйвер принял формат {actual} вместо {requested}",
        &[("actual", actual), ("requested", requested)],
    )
}

/// Причина несовместимости: PCM с плавающей точкой в Строгом режиме (ОВС-1, ТЗ-132).
pub const FLOAT_UNSUPPORTED_STRICT: &str = "PCM с плавающей точкой не воспроизводится в Строгом режиме";

/// Причина преобразования: настройка «ЦАП понимает DoP» выключена (ТЗ-145, ADR-25) —
/// точнее, чем общая фраза «настройка плеера: <ярлык>».
pub const DOP_DISABLED_FOR_DEVICE_CAUSE: &str = "DoP выключен для устройства";

/// Причина преобразования: настройка плеера с русским ярлыком параметра (ТЗ-81, ТЗ-95).
pub fn player_setting_cause(label: &str) -> String {
    format_named("настройка плеера: {label}", &[("label", label)])
}

/// Причина преобразования: ограничение устройства (ТЗ-81).
pub const DEVICE_LIMIT_CAUSE: &str = "ограничение устройства";

/// Причина преобразования: драйвер принял другой формат (ТЗ-81).
pub const DRIVER_ACCEPTED_CAUSE: &str = "драйвер принял другой формат";

/// Причина преобразования: аудиосервер не принимает формат (ТЗ-81).
pub const SERVER_LIMIT_CAUSE: &str = "аудиосервер не принимает формат";

/// Причина преобразования: правило режима (ТЗ-81).
pub const MODE_RULE_CAUSE: &str = "правило режима";

/// Причина преобразования: формат контейнера устройства (ТЗ-81).
pub const FORMAT_CONTAINER_CAUSE: &str = "формат контейнера устройства";

/// MD5 (ТЗ-69): проверка ожидается после окончания трека.
pub const MD5_PENDING: &str = "MD5: проверка после окончания трека";

/// MD5 (ТЗ-69): контрольная сумма совпала.
pub const MD5_MATCHED: &str = "MD5 совпал";

/// MD5 (ТЗ-69): контрольная сумма не совпала.
pub const MD5_MISMATCHED: &str = "MD5 не совпал";

/// MD5 (ТЗ-69): в файле нет контрольной суммы.
pub const MD5_ABSENT: &str = "MD5 в файле нет";

/// MD5 (ТЗ-69, ОВ-15): проверка не выполнялась из-за перемотки.
pub const MD5_NOT_CHECKED_SEEKED: &str = "MD5 не проверен: была перемотка";

/// «Вывод» (ТЗ-131): буфер устройства в миллисекундах и период в кадрах.
pub fn device_buffer_note(ms: u64, period_frames: u32) -> String {
    format!("буфер устройства {ms} мс (период {period_frames} кадров)")
}

/// Разрядность PCM-источника (ТЗ-74).
pub fn bits_depth(bits: u8) -> String {
    format!("{bits} бит")
}

/// Сводка формата источника: кодек, разрядность, частота, каналы (ТЗ-74).
pub fn source_summary(codec: &str, depth: &str, khz: &str, channels: u8) -> String {
    format_named(
        "{codec} {depth} / {khz}, {channels} кан.",
        &[("codec", codec), ("depth", depth), ("khz", khz), ("channels", &channels.to_string())],
    )
}

/// Сводка формата потока: частота, разрядность, каналы (ТЗ-76).
pub fn stream_summary(khz: &str, sample: &str, channels: u8) -> String {
    format_named(
        "{khz}, {sample}, {channels} кан.",
        &[("khz", khz), ("sample", sample), ("channels", &channels.to_string())],
    )
}

/// Разрядность выходного формата S16 (ТЗ-76).
pub const SAMPLE_S16: &str = "16 бит";

/// Разрядность выходного формата S24 в 3 байтах (ТЗ-76).
pub const SAMPLE_S24_3: &str = "24 бит (3 байта)";

/// Разрядность выходного формата S24, упакованного в 32 бита (ТЗ-76).
pub const SAMPLE_S24: &str = "24 бит (в 32)";

/// Разрядность выходного формата S32 (ТЗ-76).
pub const SAMPLE_S32: &str = "32 бит";
