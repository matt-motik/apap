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

// ---------------------------------------------------------------------------
// Часть 2: названия режима/сервера, Display, рекомендации, советы (ТЗ-81,
// ТЗ-84, §6.25).
// ---------------------------------------------------------------------------

/// Название режима воспроизведения (ТЗ-84).
pub const MODE_COMPATIBLE: &str = "Совместимый режим";
pub const MODE_OPTIMAL: &str = "Оптимальный режим";
pub const MODE_STRICT: &str = "Строгий режим";

/// «Уровень ОС» (ТЗ-77): микшер WASAPI (единственное имя сервера с русским словом).
pub const SERVER_WASAPI_MIXER: &str = "микшер WASAPI";

/// `Display` отчёта (ТЗ-84, §6.25): подписи блоков строки этапа.
pub const CHANGES_LABEL: &str = "изменения";
pub const CAUSES_LABEL: &str = "причины";
pub const CONFIDENCE_LABEL: &str = "достоверность";
pub const NOTES_LABEL: &str = "заметки";

/// `Display` отчёта (§6.25): заголовок и пустой список рекомендаций.
pub const RECOMMENDATIONS_HEADER: &str = "Рекомендации:";
pub const NO_RECOMMENDATIONS: &str = "нет";

/// `Display` отчёта (§6.25): названия трёх групп рекомендаций.
pub const PLAYER_SETTINGS_GROUP: &str = "Настройки плеера";
pub const OS_SETTINGS_GROUP: &str = "Настройки ОС";
pub const HARDWARE_GROUP: &str = "Оборудование";

/// `Display` отчёта (§6.25): версия плеера и ОС.
pub const PLAYER_VERSION_LABEL: &str = "Версия плеера";
pub const OS_LABEL: &str = "ОС";

/// Текст рекомендации «смена режима» (ТЗ-81, ТЗ-84, §6.25).
pub fn switch_mode_text(mode: &str, why: &str) -> String {
    format_named("смена режима: {mode} ({why})", &[("mode", mode), ("why", why)])
}

/// Название этапа (ТЗ-71, ТЗ-84): для окна «Путь сигнала» и UI-презентера.
pub const STAGE_SOURCE: &str = "Источник";
pub const STAGE_DECODER: &str = "Декодер";
pub const STAGE_PROCESSING: &str = "Обработка";
pub const STAGE_OUTPUT: &str = "Вывод";
pub const STAGE_OS_LEVEL: &str = "Уровень ОС";
pub const STAGE_DEVICE: &str = "Устройство";

/// Текст статуса этапа (ТЗ-71, ТЗ-84): значок + текст, не только цвет.
pub const STATUS_UNCHANGED: &str = "✓ без изменений";
pub const STATUS_LOSSLESS: &str = "≈ изменено без потерь";
pub const STATUS_UNKNOWN: &str = "? неизвестно";
pub const STATUS_LOSSY: &str = "✗ с потерями / несовместимо";

/// Текст достоверности этапа (ТЗ-71, ТЗ-84): «подтверждено (…)» / «предположено».
pub fn confirmed_text(by: &str) -> String {
    format_named("подтверждено ({by})", &[("by", by)])
}
pub const ASSUMED_TEXT: &str = "предположено";

/// Как подтверждена достоверность (ТЗ-71, ТЗ-84).
pub const CONFIRMED_BY_DRIVER_HW_PARAMS: &str = "hw_params драйвера";
pub const CONFIRMED_BY_RUNTIME_TEST: &str = "тест тракта";
pub const CONFIRMED_BY_FILE_HEADER: &str = "заголовок файла";
pub const CONFIRMED_BY_OS_QUERY: &str = "запрос к ОС";

/// Текст действия рекомендации «Настройки плеера» (ТЗ-81, ТЗ-95, §6.25).
pub const ACTION_SET_VOLUME_FULL: &str = "громкость → 100 %";
pub const ACTION_UNMUTE: &str = "выключить mute";
pub const ACTION_TURN_OFF: &str = "выключить";
pub const ACTION_TURN_ON: &str = "включить";
pub const ACTION_CHOOSE_QUALITY_FILTER: &str = "выбрать фильтр SRC «Качество»";
pub const ACTION_INCREASE: &str = "увеличить";

/// Тема рекомендации «Настройки ОС» (ТЗ-81, §6.25): русские темы WASAPI/RT;
/// `PipeWire: allowed-rates`/`force-rate` без русских слов — не переводятся.
pub const OS_TOPIC_WASAPI_SHARED_FORMAT: &str = "формат общего режима WASAPI";
pub const OS_TOPIC_WASAPI_EXCLUSIVE_PERMISSION: &str = "разрешение на монопольный доступ WASAPI";
pub const OS_TOPIC_REALTIME_PRIORITY: &str = "приоритет реального времени (rtkit, группа audio)";

/// Совет «Оборудование» (ТЗ-80, ТЗ-81, §6.25, AM1.0-B29): требуемая частота/
/// разрядность/каналы/DoP устройства.
pub fn hw_need_rate(khz: &str) -> String {
    format_named("ЦАП с поддержкой {khz}", &[("khz", khz)])
}
pub fn hw_need_bit_depth(bits: u8) -> String {
    format!("ЦАП с поддержкой {bits} бит")
}
pub fn hw_need_channels(n: u8) -> String {
    format!("ЦАП с поддержкой {n} каналов")
}
pub fn hw_need_dop_rate(dsd: &str, pcm_khz: &str) -> String {
    format_named("ЦАП с поддержкой DoP {dsd} (PCM {pcm_khz})", &[("dsd", dsd), ("pcm_khz", pcm_khz)])
}

/// Совет «Оборудование» (§6.25, AM1.0-B29): дописывание «этот ЦАП: …» для
/// DoP (скобка `base` уже открыта — «; ») и для прочих требований (новая скобка).
pub fn hw_have_dop_suffix(base_without_paren: &str, have_text: &str) -> String {
    format!("{base_without_paren}; этот ЦАП: {have_text})")
}
pub fn hw_have_suffix(base: &str, have_text: &str) -> String {
    format!("{base} (этот ЦАП: {have_text})")
}

/// «этот ЦАП» (§6.25, AM1.0-B29): «до M бит, float» без ограничения по разрядности.
pub const DAC_FLOAT_ONLY: &str = "до 24 бит, float";

/// «этот ЦАП» (§6.25, AM1.0-B29): «до M бит».
pub fn dac_max_bits(max_bits: u8) -> String {
    format!("до {max_bits} бит")
}

/// «этот ЦАП» (§6.25, AM1.0-B29): список частот через « / », суффикс «кГц».
pub fn rates_list_khz(nums_joined: &str) -> String {
    format!("{nums_joined} кГц")
}

/// «этот ЦАП» (§6.25, AM1.0-B29): DoP до DSDm / без DoP.
pub fn dac_dop_up_to(dsd: &str) -> String {
    format_named("DoP до {dsd}", &[("dsd", dsd)])
}
pub const DAC_NO_DOP: &str = "без DoP";
pub fn dac_dop_combined(dop: &str, max_khz: &str) -> String {
    format_named("{dop}, до {max_khz}", &[("dop", dop), ("max_khz", max_khz)])
}

/// Причина смены режима (ТЗ-81): «ресемплинг»/«усечение разрядности»/
/// «сведение каналов» — русские слова; `DSD→PCM`/`Exclusive` без русских
/// слов остаются литералом в месте вызова.
pub const SWITCH_WHY_RESAMPLE: &str = "ресемплинг";
pub const SWITCH_WHY_TRUNCATE: &str = "усечение разрядности";
pub const SWITCH_WHY_DOWNMIX: &str = "сведение каналов";

// ---------------------------------------------------------------------------
// Часть 3: тексты преобразований пути сигнала (§3.3, ТЗ-72,
// `TransformKind::describe`/`src_filter_name`, T1.0-B8) — перенесено из
// `src/audio/path/transform.rs` байт-в-байт, без изменения содержания.
// ---------------------------------------------------------------------------

/// Текст преобразования `MonoToStereo` — моно → 2 канала дублированием (ОВ-2).
pub const TRANSFORM_MONO_TO_STEREO: &str = "Моно → 2 канала, дублирование";

/// Текст преобразования `Volume` — программная громкость стадии AtomicGain
/// (ОВ-5); `pct` — значение `Volume::get()` (0..=100).
pub fn transform_volume_note(pct: u8) -> String {
    format!("Громкость {pct} %")
}

/// Текст преобразования `Mute` — стадия AtomicGain, усиление 0 (T1.0-B9;
/// перенесено из `src/audio/path/transform.rs::describe` байт-в-байт,
/// пропущено шагом 10 T1.0-B8).
pub const TRANSFORM_MUTE: &str = "Mute: усиление 0";

/// Текст преобразования `SilenceGate` — подмена тишиной без пересчёта
/// сэмплов (NoGain/DopRender; ОВС-18), без потерь, см. [`TRANSFORM_MUTE`].
pub const TRANSFORM_SILENCE_GATE: &str = "Mute: подмена тишиной, без пересчёта сэмплов";

/// Тексты преобразования `Dither`: TPDF-дизеринг и переквантование без
/// дизеринга, см. [`TRANSFORM_MUTE`].
pub const TRANSFORM_DITHER_TPDF: &str = "Дизеринг TPDF";
pub const TRANSFORM_DITHER_OFF: &str = "Переквантование без дизеринга";

#[cfg(test)]
mod tests {
    use super::*;

    /// T1.0-B9: байт-в-байт перенос из `transform.rs::describe` (шаг 10
    /// T1.0-B8 перенёс только соседние варианты).
    #[test]
    fn transform_mute_silence_dither_are_russian() {
        assert_eq!(TRANSFORM_MUTE, "Mute: усиление 0");
        assert_eq!(TRANSFORM_SILENCE_GATE, "Mute: подмена тишиной, без пересчёта сэмплов");
        assert_eq!(TRANSFORM_DITHER_TPDF, "Дизеринг TPDF");
        assert_eq!(TRANSFORM_DITHER_OFF, "Переквантование без дизеринга");
    }
}
