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
//! Перечисление своих ошибок и системные ошибки — [`errors`].

pub mod errors;

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

/// Склонение существительного под число по правилам русского языка
/// (T1.0-B2): единственная форма для 1, 21, 31… («1 трек»), счётная для
/// 2–4, 22–24… («3 трека»), множественная для 0, 5–20, 25–30… включая
/// 11–14 («0 треков», «11 треков», «25 треков»). Возвращает только форму
/// слова — число форматирует вызывающий код: `format!("{n} {}", plural_ru(n,
/// "трек", "трека", "треков"))`.
pub fn plural_ru<'a>(n: u64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let rem100 = n % 100;
    let rem10 = n % 10;
    if (11..=14).contains(&rem100) {
        many
    } else if rem10 == 1 {
        one
    } else if (2..=4).contains(&rem10) {
        few
    } else {
        many
    }
}

/// Общая часть числовых форматтеров (T1.0-B2): десятичная точка меняется на
/// запятую, знак «минус» — на типографский «−» (U+2212), ноль (в том числе
/// «-0,0» после округления) всегда без знака. `trim_trailing_zeros`
/// управляет тем, остаётся ли дробная часть, состоящая только из нулей
/// (`format_khz`/`format_ms` её убирают, `format_decimal`/`format_db` — нет,
/// у них фиксированное число знаков после запятой).
fn decimal_with_comma(value: f64, decimals: usize, trim_trailing_zeros: bool) -> String {
    let raw = format!("{value:.decimals$}");
    let unsigned = raw.strip_prefix('-').unwrap_or(raw.as_str());
    let is_zero = unsigned.chars().all(|c| c == '0' || c == '.');
    let sign = if raw.starts_with('-') && !is_zero { "\u{2212}" } else { "" };
    let digits = if trim_trailing_zeros && unsigned.contains('.') {
        unsigned.trim_end_matches('0').trim_end_matches('.')
    } else {
        unsigned
    };
    format!("{sign}{}", digits.replace('.', ","))
}

/// Вещественное число с десятичной запятой и фиксированным числом знаков
/// после неё (T1.0-B2): `format_decimal(-3.0, 1)` → `"−3,0"`. Для единиц с
/// обрезкой лишних нулей — `format_khz`/`format_ms`.
pub fn format_decimal(value: f64, decimals: usize) -> String {
    decimal_with_comma(value, decimals, false)
}

/// Частота в кГц с десятичной запятой без лишних нулей (T1.0-B2): принимает
/// значение уже в кГц (не Гц). `format_khz(44.1)` → `"44,1 кГц"`,
/// `format_khz(48.0)` → `"48 кГц"`.
pub fn format_khz(value_khz: f64) -> String {
    format!("{} кГц", decimal_with_comma(value_khz, 2, true))
}

/// Длительность в миллисекундах без лишних нулей в дробной части
/// (T1.0-B2): `format_ms(12.0)` → `"12 мс"`, `format_ms(0.5)` → `"0,5 мс"`.
pub fn format_ms(value_ms: f64) -> String {
    format!("{} мс", decimal_with_comma(value_ms, 2, true))
}

/// Уровень в децибелах с фиксированным одним знаком после запятой
/// (T1.0-B2): `format_db(-3.0)` → `"−3,0 дБ"`.
pub fn format_db(value_db: f64) -> String {
    format!("{} дБ", decimal_with_comma(value_db, 1, false))
}

/// Глубина сигнала в битах (T1.0-B2): `format_bits(24)` → `"24 бит"` —
/// единица не склоняется (устоявшийся технический оборот, решение
/// пользователя 2026-10-23, ROADMAP.md T1.0-B2).
pub fn format_bits(bits: u32) -> String {
    format!("{bits} бит")
}

/// Пункт меню трея «Play / Pause» (T1.0-B2, ADR-6).
pub const TRAY_MENU_PLAY_PAUSE: &str = "Воспроизведение / Пауза";
/// Пункт меню трея «Stop» (T1.0-B2, ADR-6).
pub const TRAY_MENU_STOP: &str = "Стоп";
/// Пункт меню трея «Previous» (T1.0-B2, ADR-6).
pub const TRAY_MENU_PREVIOUS: &str = "Предыдущий";
/// Пункт меню трея «Next» (T1.0-B2, ADR-6).
pub const TRAY_MENU_NEXT: &str = "Следующий";
/// Пункт меню трея «Show / Hide window» (T1.0-B2, ADR-6).
pub const TRAY_MENU_SHOW_HIDE: &str = "Показать / Скрыть окно";
/// Пункт меню трея «Quit» (T1.0-B2, ADR-6).
pub const TRAY_MENU_QUIT: &str = "Выход";

/// Тултип трея во время воспроизведения в bit-perfect (T1.0-B2, ADR-9,
/// §8.1): «bit-perfect» не переводится — технический термин (AGENTS.md).
pub const TRAY_TOOLTIP_BIT_PERFECT: &str =
    "Воспроизведение — Bit-perfect (прямой вывод, громкость на ЦАП)";
/// Тултип трея во время обычного воспроизведения (T1.0-B2, ADR-9, §8.1).
pub const TRAY_TOOLTIP_PLAYING: &str = "Воспроизведение";
/// Тултип трея на паузе (T1.0-B2, ADR-9, §8.1).
pub const TRAY_TOOLTIP_PAUSED: &str = "Пауза";

/// Тултип трея, когда звук недоступен (T1.0-B2, ADR-9, §8.1): `error` —
/// причина, пришедшая из аудио-подсистемы как есть (не переводится).
pub fn tray_tooltip_unavailable(error: &str) -> String {
    format_named("Воспроизведение недоступно: {error}", &[("error", error)])
}

/// Фильтр диалога выбора аудиофайлов (`PickRequest::AddFiles`) (T1.0-B2,
/// ADR-10, ТЗ-53).
pub const PICKER_FILTER_AUDIO: &str = "Аудио";
/// Фильтр диалога открытия плейлиста (`PickRequest::OpenPlaylist`) (T1.0-B2,
/// ADR-10, ТЗ-53).
pub const PICKER_FILTER_PLAYLIST: &str = "Плейлист";
/// Фильтр диалога выбора файла темы (`PickRequest::ThemeFile`) (T1.0-B2,
/// ADR-10, ТЗ-53).
pub const PICKER_FILTER_THEME: &str = "Тема";
/// Фильтр M3U8 диалога сохранения плейлиста (`PickRequest::SavePlaylist`),
/// первый в списке и задающий расширение-подсказку (T1.0-B2, ADR-10, ТЗ-53,
/// ОВ-17).
pub const PICKER_FILTER_PLAYLIST_M3U8: &str = "Плейлист (M3U8)";
/// Фильтр M3U диалога сохранения плейлиста (`PickRequest::SavePlaylist`)
/// (T1.0-B2, ADR-10, ТЗ-53).
pub const PICKER_FILTER_PLAYLIST_M3U: &str = "Плейлист (M3U)";

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

    #[test]
    fn plural_ru_one_for_1_21_31() {
        for n in [1, 21, 31, 101] {
            assert_eq!(plural_ru(n, "трек", "трека", "треков"), "трек", "n={n}");
        }
    }

    #[test]
    fn plural_ru_few_for_2_3_4() {
        for n in [2, 3, 4, 22, 24, 102] {
            assert_eq!(plural_ru(n, "трек", "трека", "треков"), "трека", "n={n}");
        }
    }

    #[test]
    fn plural_ru_many_for_0_5_11_14_25() {
        for n in [0, 5, 11, 12, 13, 14, 20, 25, 100, 111, 114] {
            assert_eq!(plural_ru(n, "трек", "трека", "треков"), "треков", "n={n}");
        }
    }

    #[test]
    fn format_decimal_keeps_trailing_zero() {
        assert_eq!(format_decimal(3.0, 1), "3,0");
    }

    #[test]
    fn format_decimal_negative_uses_typographic_minus() {
        assert_eq!(format_decimal(-3.0, 1), "\u{2212}3,0");
    }

    #[test]
    fn format_decimal_rounds_to_zero_drops_sign() {
        assert_eq!(format_decimal(-0.04, 1), "0,0");
    }

    #[test]
    fn format_decimal_fractional_rounds() {
        assert_eq!(format_decimal(3.14728, 2), "3,15");
    }

    #[test]
    fn format_khz_fractional_value() {
        assert_eq!(format_khz(44.1), "44,1 кГц");
    }

    #[test]
    fn format_khz_whole_value_has_no_trailing_zero() {
        assert_eq!(format_khz(48.0), "48 кГц");
    }

    #[test]
    fn format_khz_two_decimal_digits() {
        assert_eq!(format_khz(22.05), "22,05 кГц");
    }

    #[test]
    fn format_ms_whole_value_has_no_trailing_zero() {
        assert_eq!(format_ms(12.0), "12 мс");
    }

    #[test]
    fn format_ms_fractional_value() {
        assert_eq!(format_ms(0.5), "0,5 мс");
    }

    #[test]
    fn format_db_keeps_fixed_one_decimal() {
        assert_eq!(format_db(-3.0), "\u{2212}3,0 дБ");
        assert_eq!(format_db(0.0), "0,0 дБ");
    }

    #[test]
    fn format_bits_is_not_declined() {
        assert_eq!(format_bits(24), "24 бит");
        assert_eq!(format_bits(1), "1 бит");
    }

    #[test]
    fn tray_tooltip_unavailable_substitutes_error() {
        assert_eq!(
            tray_tooltip_unavailable("device missing"),
            "Воспроизведение недоступно: device missing"
        );
    }
}
