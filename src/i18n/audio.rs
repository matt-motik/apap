//! Строки блока «Информация об устройстве» (§8 С11, AM1.0-B30) и окна «Путь
//! сигнала» (§3.6, ТЗ-71, ТЗ-81, ТЗ-84), перенесённые дословно из
//! `src/app/device_info_view.rs` и `src/app/path_view.rs` (T1.0-B2, решение:
//! все строки Rust в одном модуле i18n). Текст уже был русским — закреплён
//! решениями пользователя по AM1.0-B30 (ROADMAP.md), здесь только переехал.

use super::{decimal_with_comma, format_named};

/// Текст непрошенного устройства (AM1.0-B30, решение 2026-10-24): кэш
/// возможностей не содержит записи для устройства этого режима — оно ещё не
/// открывалось в текущей сессии движка, а не «устройство не умеет».
pub const DEVICE_NOT_CACHED: &str = "неизвестно — устройство ещё не открывалось";

/// «○ не открыто» — устройство фактически не открыто ни на одном режиме
/// (AM1.0-B30).
pub const DEVICE_NOW_CLOSED: &str = "○ не открыто";

/// Текст занятого устройства (AM1.0-B38, шаг 8/11): кэш возможностей не
/// содержит записи для устройства этого режима, потому что фоновый опрос
/// (`UiAudioState::device_busy`) получил `ProbeError::Busy` — устройство
/// открыто другим приложением, а не просто «ещё не открывалось в этой
/// сессии» (`DEVICE_NOT_CACHED`). Перенесена из локальной константы
/// `src/app/device_info_view.rs` сюда (шаг 11) — тот же модуль, что и
/// `DEVICE_NOT_CACHED`.
pub const DEVICE_BUSY: &str = "неизвестно — устройство занято";

/// «Сейчас: ● <устройство> — куда сейчас маршрутизирует сервер» —
/// Совместимый режим (AM1.0-B30).
pub fn device_now_compatible(label: &str) -> String {
    format_named("Сейчас: ● {label} — куда сейчас маршрутизирует сервер", &[("label", label)])
}

/// «Сейчас: ● <устройство> — монопольно/bit-perfect» — Оптимальный/Строгий
/// режим (AM1.0-B30).
pub fn device_now_exclusive(label: &str) -> String {
    format_named("Сейчас: ● {label} — монопольно/bit-perfect", &[("label", label)])
}

/// «Частоты: <DEVICE_NOT_CACHED>» (AM1.0-B30).
pub fn device_rates_not_cached() -> String {
    format!("Частоты: {DEVICE_NOT_CACHED}")
}

/// «Частоты: <DEVICE_BUSY>» (AM1.0-B38, шаг 11).
pub fn device_rates_busy() -> String {
    format!("Частоты: {DEVICE_BUSY}")
}

/// «Частоты: неизвестно» — пустой список частот либо кэшировано без данных
/// (AM1.0-B30, защитный случай).
pub const DEVICE_RATES_UNKNOWN: &str = "Частоты: неизвестно";

/// «44,1 кГц: <список через « / »>» — семья 44,1 кГц (AM1.0-B30).
pub fn device_rate_group_441(joined: &str) -> String {
    format_named("44,1 кГц: {joined}", &[("joined", joined)])
}

/// «48 кГц: <список через « / »>» — семья 48 кГц (AM1.0-B30).
pub fn device_rate_group_48(joined: &str) -> String {
    format_named("48 кГц: {joined}", &[("joined", joined)])
}

/// «другие: <список через « / »>» — частоты вне обеих семей (AM1.0-B30).
pub fn device_rate_group_other(joined: &str) -> String {
    format_named("другие: {joined}", &[("joined", joined)])
}

/// «Частоты: <группы через «; »>» (AM1.0-B30).
pub fn device_rates_line(groups: &str) -> String {
    format_named("Частоты: {groups}", &[("groups", groups)])
}

/// «Частоты: <min>–<max>» — сплошной диапазон без семей, `RateSet::Range`
/// (AM1.0-B30).
pub fn device_rates_range(min: &str, max: &str) -> String {
    format_named("Частоты: {min}–{max}", &[("min", min), ("max", max)])
}

/// «<частота> · float — сервер ресемплирует и микширует» — Совместимый
/// режим (ОВС-4, AM1.0-B30).
pub fn device_mixer_line(khz: &str) -> String {
    format_named("{khz} · float — сервер ресемплирует и микширует", &[("khz", khz)])
}

/// «неизвестно» — `OsStage` не `Known` или частота не узнана (AM1.0-B30).
pub const DEVICE_MIXER_UNKNOWN: &str = "неизвестно";

/// «Разрядность: <DEVICE_NOT_CACHED>» (AM1.0-B30).
pub fn device_bits_not_cached() -> String {
    format!("Разрядность: {DEVICE_NOT_CACHED}")
}

/// «Разрядность: <DEVICE_BUSY>» (AM1.0-B38, шаг 11).
pub fn device_bits_busy() -> String {
    format!("Разрядность: {DEVICE_BUSY}")
}

/// «Разрядность: неизвестно» — кэшировано, но данные пусты (AM1.0-B30,
/// защитный случай).
pub const DEVICE_BITS_UNKNOWN: &str = "Разрядность: неизвестно";

/// «Разрядность: до 24 бит, float» — только `F32` среди форматов поля
/// (AM1.0-B30).
pub const DEVICE_BITS_FLOAT_ONLY: &str = "Разрядность: до 24 бит, float";

/// «Разрядность: <биты через « / »> бит (<коды через «, »>)» (AM1.0-B30).
pub fn device_bits_line(bits: &str, codes: &str) -> String {
    format_named("Разрядность: {bits} бит ({codes})", &[("bits", bits), ("codes", codes)])
}

/// «DSD: <DEVICE_NOT_CACHED>» (AM1.0-B30).
pub fn device_dsd_not_cached() -> String {
    format!("DSD: {DEVICE_NOT_CACHED}")
}

/// «DSD: <DEVICE_BUSY>» (AM1.0-B38, шаг 11).
pub fn device_dsd_busy() -> String {
    format!("DSD: {DEVICE_BUSY}")
}

/// «DSD: до <DSDxxx> (DoP)» (AM1.0-B30).
pub fn device_dsd_line(name: &str) -> String {
    format_named("DSD: до {name} (DoP)", &[("name", name)])
}

/// «DSD: DoP не поддерживается» (AM1.0-B30).
pub const DEVICE_DSD_NO_DOP: &str = "DSD: DoP не поддерживается";

/// Частота в кГц без единицы измерения — для списков через « / »
/// (AM1.0-B30): то же числовое представление, что [`super::format_khz`],
/// без суффикса «кГц» (`device_info_view::khz_num`).
pub fn format_khz_number(value_khz: f64) -> String {
    decimal_with_comma(value_khz, 2, true)
}

/// Суффикс лейбла программного узла звукового сервера (PipeWire/Pulse) в
/// списке устройств вкладки «Аудио» (T1.0-B6, найдено на T1.0-B2 шаг 21):
/// такие узлы проксируют поток через сервер с неявным ресемплингом и не
/// могут отдать bit-exact (DoP) вывод, поэтому диалог настроек явно это
/// помечает. Используется только при построении лейбла
/// (`audio::output::label_device_names`) и при обратном разборе лейбла в
/// сырое имя устройства (`app::ui_manager`) — сырое (персистентное) имя
/// устройства этим суффиксом не затрагивается.
pub const SERVER_NODE_SUFFIX: &str = " — (программный, с ресемплингом)";

/// Заголовок одноразового уведомления «устройство изменено» (AM1.0-B32,
/// шаг 7; AM1.0-B37, шаг 13): общий текст для миграции устаревшего
/// `hw:`/`plughw:`-значения Совместимого режима на старте
/// (`migrate_legacy_compatible_device`) и пропавшего узла PipeWire во время
/// работы движка (`migrate_vanished_compatible_device`, `src/engine/run.rs`,
/// `Notice::CompatibleDeviceMigrated`) — обе ветки показывают пользователю
/// один и тот же текст (`src/app/mod.rs`).
pub const COMPATIBLE_DEVICE_MIGRATED_TITLE: &str = "Устройство изменено";

/// Текст того же уведомления, см. [`COMPATIBLE_DEVICE_MIGRATED_TITLE`].
pub const COMPATIBLE_DEVICE_MIGRATED_BODY: &str =
    "Устройство Совместимого режима было выбрано в обход аудиосервера — переключено на «Системное по умолчанию»";

/// Подвал окна «Путь сигнала»: граница гарантии, версия плеера, ОС (§3.6,
/// ТЗ-71, ТЗ-81, ТЗ-84; перенесено из `src/app/path_view.rs::path_view`).
pub fn path_footer(boundary: &str, version: &str, os: &str) -> String {
    format_named(
        "{boundary}\n\nВерсия плеера: {version}\nОС: {os}",
        &[("boundary", boundary), ("version", version), ("os", os)],
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    #[test]
    fn device_now_compatible_substitutes_label() {
        assert_eq!(
            device_now_compatible("Системное по умолчанию (PipeWire)"),
            "Сейчас: ● Системное по умолчанию (PipeWire) — куда сейчас маршрутизирует сервер"
        );
    }

    #[test]
    fn device_now_exclusive_substitutes_label() {
        assert_eq!(
            device_now_exclusive("Motu M2 (USB Audio)"),
            "Сейчас: ● Motu M2 (USB Audio) — монопольно/bit-perfect"
        );
    }

    #[test]
    fn device_rates_not_cached_includes_hint() {
        assert_eq!(device_rates_not_cached(), "Частоты: неизвестно — устройство ещё не открывалось");
    }

    #[test]
    fn device_rates_busy_includes_hint() {
        assert_eq!(device_rates_busy(), "Частоты: неизвестно — устройство занято");
    }

    #[test]
    fn device_rate_groups_substitute_joined_list() {
        assert_eq!(device_rate_group_441("44,1 / 88,2"), "44,1 кГц: 44,1 / 88,2");
        assert_eq!(device_rate_group_48("48 / 96"), "48 кГц: 48 / 96");
        assert_eq!(device_rate_group_other("705,6"), "другие: 705,6");
    }

    #[test]
    fn device_rates_line_substitutes_groups() {
        assert_eq!(device_rates_line("44,1 кГц: 44,1; 48 кГц: 48"), "Частоты: 44,1 кГц: 44,1; 48 кГц: 48");
    }

    #[test]
    fn device_rates_range_substitutes_min_and_max() {
        assert_eq!(device_rates_range("8 кГц", "192 кГц"), "Частоты: 8 кГц–192 кГц");
    }

    #[test]
    fn device_mixer_line_substitutes_khz() {
        assert_eq!(device_mixer_line("48 кГц"), "48 кГц · float — сервер ресемплирует и микширует");
    }

    #[test]
    fn device_bits_not_cached_includes_hint() {
        assert_eq!(device_bits_not_cached(), "Разрядность: неизвестно — устройство ещё не открывалось");
    }

    #[test]
    fn device_bits_busy_includes_hint() {
        assert_eq!(device_bits_busy(), "Разрядность: неизвестно — устройство занято");
    }

    #[test]
    fn device_bits_line_substitutes_bits_and_codes() {
        assert_eq!(
            device_bits_line("16 / 24 / 32", "S16_LE, S24_3LE, S32_LE"),
            "Разрядность: 16 / 24 / 32 бит (S16_LE, S24_3LE, S32_LE)"
        );
    }

    #[test]
    fn device_dsd_not_cached_includes_hint() {
        assert_eq!(device_dsd_not_cached(), "DSD: неизвестно — устройство ещё не открывалось");
    }

    #[test]
    fn device_dsd_busy_includes_hint() {
        assert_eq!(device_dsd_busy(), "DSD: неизвестно — устройство занято");
    }

    #[test]
    fn device_dsd_line_substitutes_name() {
        assert_eq!(device_dsd_line("DSD256"), "DSD: до DSD256 (DoP)");
    }

    #[test]
    fn format_khz_number_matches_format_khz_without_unit() {
        assert_eq!(format_khz_number(44.1), "44,1");
        assert_eq!(format_khz_number(96.0), "96");
        assert_eq!(format_khz_number(22.05), "22,05");
    }

    #[test]
    fn path_footer_substitutes_boundary_version_and_os() {
        assert_eq!(
            path_footer("Граница гарантии: …", "1.2.3", "Linux"),
            "Граница гарантии: …\n\nВерсия плеера: 1.2.3\nОС: Linux"
        );
    }

    #[test]
    fn server_node_suffix_is_russian() {
        assert_eq!(SERVER_NODE_SUFFIX, " — (программный, с ресемплингом)");
    }
}
