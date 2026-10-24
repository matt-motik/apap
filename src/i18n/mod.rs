//! Локализация пользовательских строк интерфейса (T1.0-B2): русский текст
//! с заделом под многоязычность. Rust-строки собираются здесь через
//! функции-конструкторы с именованными подстановками, а не разбросанными
//! литералами по вызывающему коду; Slint-строки переводятся на месте через
//! `@tr(...)` (`docs/spec_theme_v1.0.md`, решение пользователя 2026-10-23).
//!
//! Технические термины (PCM, DSD, DoP, ASIO, WASAPI, ALSA, PipeWire,
//! bit-perfect, форматы `S24_3LE`…), имена палитр/окон и ключи настроек
//! этим модулем не переводятся — они приходят в строки как есть.
//!
//! Перечисление своих ошибок — `errors` (подмодуль добавляется шагом,
//! который его создаёт).

/// Имя приложения — единая точка правки, если рабочее название сменится.
pub const APP_NAME: &str = "APAP";

/// Заголовок окна (T1.0-B2): `"APAP — <трек>"`, когда известно имя текущего
/// трека, иначе просто [`APP_NAME`].
pub fn window_title(track: Option<&str>) -> String {
    match track {
        Some(name) if !name.is_empty() => format!("{APP_NAME} — {name}"),
        _ => APP_NAME.to_string(),
    }
}

/// Подставляет именованные плейсхолдеры `{name}` в шаблон строки (T1.0-B2):
/// основа для конструкторов пользовательских сообщений в этом модуле.
///
/// Плейсхолдер без соответствующего ключа в `pairs` остаётся в тексте как
/// есть — отображаемая (пусть и неполная) строка важнее паники в UI.
pub fn format_named(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut result = template.to_string();
    for (key, value) in pairs {
        let placeholder = format!("{{{key}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_title_with_track() {
        assert_eq!(window_title(Some("Клавир — Партита №2")), "APAP — Клавир — Партита №2");
    }

    #[test]
    fn window_title_without_track() {
        assert_eq!(window_title(None), "APAP");
    }

    #[test]
    fn window_title_empty_track_falls_back_to_app_name() {
        assert_eq!(window_title(Some("")), "APAP");
    }

    #[test]
    fn format_named_substitutes_single_placeholder() {
        assert_eq!(format_named("Устройство: {name}", &[("name", "Cayin RU3")]), "Устройство: Cayin RU3");
    }

    #[test]
    fn format_named_substitutes_multiple_placeholders() {
        let result = format_named(
            "{device}: {rate} кГц, {bits} бит",
            &[("device", "Motu M2"), ("rate", "44,1"), ("bits", "24")],
        );
        assert_eq!(result, "Motu M2: 44,1 кГц, 24 бит");
    }

    #[test]
    fn format_named_repeated_placeholder_is_substituted_everywhere() {
        let result = format_named("{name} ({name})", &[("name", "ALSA")]);
        assert_eq!(result, "ALSA (ALSA)");
    }

    #[test]
    fn format_named_missing_key_leaves_placeholder_untouched() {
        let result = format_named("Устройство: {name}", &[]);
        assert_eq!(result, "Устройство: {name}");
    }

    #[test]
    fn format_named_no_pairs_returns_template_unchanged() {
        assert_eq!(format_named("Статика без плейсхолдеров", &[]), "Статика без плейсхолдеров");
    }
}
