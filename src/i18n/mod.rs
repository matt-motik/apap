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

pub mod audio;
pub mod errors;
pub mod messages;

use crate::audio::test_run::{IncompleteWhy, TestBlocker};
use crate::audio::verdict::NbpReason;
use crate::settings::CoverSource;

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

/// Битрейт в кбит/с (T1.0-B2): `format_kbps(1411)` → `"1411 кбит/с"` —
/// единица не склоняется, как и в [`format_bits`] (устоявшийся технический
/// оборот).
pub fn format_kbps(kbps: u32) -> String {
    format!("{kbps} кбит/с")
}

/// Число каналов с русским склонением (T1.0-B2): `format_channels(1)` →
/// `"1 канал"`, `format_channels(2)` → `"2 канала"`, `format_channels(5)` →
/// `"5 каналов"` ([`plural_ru`]).
pub fn format_channels(n: u32) -> String {
    format!("{n} {}", plural_ru(u64::from(n), "канал", "канала", "каналов"))
}

/// Число треков с русским склонением (T1.0-B2): `format_track_count(1)` →
/// `"1 трек"`, `format_track_count(3)` → `"3 трека"`, `format_track_count(0)`
/// → `"0 треков"` ([`plural_ru`]).
pub fn format_track_count(n: u64) -> String {
    format!("{n} {}", plural_ru(n, "трек", "трека", "треков"))
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

/// Статус окна при запуске, до первого ответа движка об устройстве
/// (T1.0-B2, ТЗ-104): первое подтверждение устройства заменяет его на
/// [`status_audio_ready`].
pub const STATUS_AUDIO_CHECKING: &str = "Аудио: проверка устройства\u{2026}";

/// Статус окна после первого подтверждения устройства движком (T1.0-B2,
/// ТЗ-104): `name` — имя устройства как есть, не переводится.
pub fn status_audio_ready(name: &str) -> String {
    format_named("Аудио: {name} готово", &[("name", name)])
}

/// Статус окна на старте фонового сканирования/импорта («Add Files»/«Add
/// Folder») (T1.0-B2): `items` — число выбранных путей (файлов и/или папок),
/// не треков — их число пока не известно.
pub fn status_adding_tracks(items: usize) -> String {
    format!(
        "Добавление треков\u{2026} {items} {}",
        plural_ru(items as u64, "элемент", "элемента", "элементов")
    )
}

/// Статус окна во время сканирования, на каждую пачку найденных треков
/// (T1.0-B2): `count` — число треков, накопленных сканированием на данный
/// момент.
pub fn status_scanning(count: usize) -> String {
    format!("Сканирование\u{2026} {}", format_track_count(count as u64))
}

/// Статус окна по завершении сканирования без новых треков (T1.0-B2):
/// `total` — всего найдено сканированием (уже известных путей).
pub fn status_scan_finished_nothing_new(total: usize) -> String {
    format!(
        "Сканирование завершено: новых нет (найдено {})",
        format_track_count(total as u64)
    )
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

/// Короткая подпись причины «Не bit-perfect» для бейджа тракта (ТЗ-53,
/// T1.0-B2; перенесено из `src/app/badge_view.rs::nbp_reason_label`).
pub fn nbp_reason_label(r: NbpReason) -> &'static str {
    match r {
        NbpReason::TestFailed => "тест не пройден",
        NbpReason::DriverParamsUnconfirmed => "параметры драйвера не подтверждены",
        NbpReason::DsdToPcm => "DSD→PCM",
        NbpReason::Resample => "ресемплинг",
        NbpReason::Downmix => "downmix",
        NbpReason::Truncation => "усечение разрядности",
        NbpReason::FloatToInt => "квантование float-источника",
        NbpReason::Int32ToFloat => "int32 → float",
        NbpReason::Mute => "mute",
        NbpReason::Volume => "громкость",
        NbpReason::Dither => "дизеринг",
    }
}

/// Текст бейджа тракта «Не bit-perfect: <причина> [+N]» (ТЗ-53, T1.0-B2):
/// `more` — число прочих причин сверх основной, суффикс `+N` опускается при
/// `more == 0`.
pub fn badge_not_bit_perfect(primary: NbpReason, more: u8) -> String {
    let mut text = format_named("Не bit-perfect: {reason}", &[("reason", nbp_reason_label(primary))]);
    if more > 0 {
        text.push_str(&format!(" +{more}"));
    }
    text
}

/// Текст бейджа тракта «Нет потока» (ТЗ-56, ОВС-18, ADR-06, T1.0-B2).
pub const BADGE_NO_STREAM: &str = "Нет потока";
/// Текст бейджа тракта Shared без изменений до системы (ТЗ-54, ADR-06, T1.0-B2).
pub const BADGE_SHARED_CLEAN: &str = "Shared: без изменений до системы";
/// Текст бейджа тракта Exclusive bit-perfect (ТЗ-52, ADR-06, T1.0-B2).
pub const BADGE_BIT_PERFECT: &str = "Bit-perfect";
/// Текст бейджа тракта на mute (ОВС-18, ADR-06, T1.0-B2): «Mute» не
/// переводится — устоявшееся обозначение рядом с «Bit-perfect».
pub const BADGE_MUTED: &str = "Mute";

/// Короткая подпись причины незавершённого теста (ТЗ-67, T1.0-B2; перенесено
/// из `src/app/badge_view.rs::incomplete_why_label`).
pub fn incomplete_why_label(why: IncompleteWhy) -> &'static str {
    match why {
        IncompleteWhy::Underrun => "underrun",
        IncompleteWhy::Xrun => "xrun",
        IncompleteWhy::MirrorOverflow => "переполнение зеркала",
        IncompleteWhy::Seek => "перемотка",
        IncompleteWhy::TrackChanged => "смена трека",
        IncompleteWhy::ModeChanged => "смена режима",
        IncompleteWhy::DeviceChanged => "смена устройства",
        IncompleteWhy::LockOpened => "открыт замок",
    }
}

/// Подсказка блокировки кнопки «Тест» (ТЗ-61, ТЗ-62, ТЗ-65, ОВ-14, ОВ-20,
/// T1.0-B2; перенесено из `src/app/badge_view.rs::blocker_hint`).
pub fn test_blocker_hint(blocker: &TestBlocker) -> String {
    match blocker {
        TestBlocker::SharedMode => BLOCKER_HINT_SHARED_MODE.to_string(),
        TestBlocker::NoStream => BLOCKER_HINT_NO_STREAM.to_string(),
        TestBlocker::Lossy(r) => {
            format_named("Мешает тесту: {reason} — см. «Путь сигнала»", &[("reason", nbp_reason_label(*r))])
        }
        TestBlocker::DriverParamsUnconfirmed => BLOCKER_HINT_DRIVER_PARAMS_UNCONFIRMED.to_string(),
        TestBlocker::Muted => BLOCKER_HINT_MUTED.to_string(),
    }
}

/// Подсказка блокировки «Тест» в Совместимом режиме (ТЗ-62, T1.0-B2).
pub const BLOCKER_HINT_SHARED_MODE: &str = "Проверка недоступна в Shared";
/// Подсказка блокировки «Тест» без активного потока (ТЗ-65, T1.0-B2).
pub const BLOCKER_HINT_NO_STREAM: &str = "Запустите воспроизведение";
/// Подсказка блокировки «Тест» при неподтверждённых параметрах драйвера
/// (ОВ-14, T1.0-B2).
pub const BLOCKER_HINT_DRIVER_PARAMS_UNCONFIRMED: &str =
    "Мешает тесту: параметры драйвера не подтверждены — см. «Путь сигнала»";
/// Подсказка блокировки «Тест» на mute (T1.0-B2).
pub const BLOCKER_HINT_MUTED: &str = "Выключите mute";

/// Текст кнопки-бейджа «Тест»: пройден (ТЗ-61, T1.0-B2).
pub const TEST_BADGE_PASSED: &str = "Тест ✓";
/// Текст кнопки-бейджа «Тест»: провален (ТЗ-61, T1.0-B2).
pub const TEST_BADGE_FAILED: &str = "Тест ✕";
/// Текст кнопки-бейджа «Тест»: результат неизвестен (ТЗ-61, T1.0-B2).
pub const TEST_BADGE_UNKNOWN: &str = "Тест ?";

/// Текст кнопки-бейджа «Тест» во время прогона (ТЗ-61, T1.0-B2):
/// `elapsed_secs`/`max_secs` — прошедшее и максимальное время в секундах.
pub fn test_badge_running(elapsed_secs: u64, max_secs: u64) -> String {
    format!("Тест {elapsed_secs} / {max_secs} с")
}

/// Подсказка «Тест» доступен, но последний прогон не завершён (ТЗ-61, ТЗ-65,
/// ОВ-14, ОВ-20, T1.0-B2): `why` — причина незавершённости ([`incomplete_why_label`]).
pub fn test_hint_incomplete(why: IncompleteWhy) -> String {
    format_named("Последний тест не завершён: {why}", &[("why", incomplete_why_label(why))])
}

/// Подсказка «Тест» доступен и не завершён ничем (ТЗ-61, ТЗ-62, ТЗ-65,
/// ОВ-14, ОВ-20, T1.0-B2).
pub const TEST_HINT_ENABLED: &str = "Проверить bit-perfect тракт";

/// Пояснение строки диалога настроек режима, зафиксированной режимом:
/// «<значение> — задано режимом» (T1.0-B2; перенесено из
/// `src/app/mode_dialog.rs::build_row`, §6.27, ТЗ-95).
pub fn mode_dialog_fixed_by_mode(shown: &str) -> String {
    format_named("{shown} — задано режимом", &[("shown", shown)])
}

/// Пояснение/значение строки или пункта списка диалога настроек режима, пока
/// не реализованного (T1.0-B2; перенесено из `src/app/mode_dialog.rs`,
/// И-Т19, ТЗ-130, §6.27).
pub const MODE_DIALOG_NOT_IMPLEMENTED: &str = "пока не реализовано";

/// Пояснение переключателя «ЦАП понимает DoP» без выбранного устройства
/// режима (T1.0-B2; перенесено из `src/app/mode_dialog.rs::row_device_dop`,
/// ADR-25, ТЗ-145).
pub const MODE_DIALOG_DEVICE_NOT_SELECTED: &str = "устройство не выбрано";

/// Подписи пунктов списка «Фильтр SRC» в порядке отображения (T1.0-B2;
/// перенесено из `src/app/mode_dialog.rs::SRC_FILTER_OPTIONS`, ТЗ-94,
/// ТЗ-130, §6.27); два последних — синтетические `NotImplemented`-пункты
/// минимальной фазы, непредставимые в `SrcFilter` (И-Т19).
pub const SRC_FILTER_STEEP: &str = "Крутой";
pub const SRC_FILTER_SLOW: &str = "Медленный";
pub const SRC_FILTER_VERY_SLOW: &str = "Очень медленный";
pub const SRC_FILTER_STEEP_SHORT_LATENCY: &str = "Крутой, короткая задержка";
pub const SRC_FILTER_SLOW_SHORT_LATENCY: &str = "Медленный, короткая задержка";

/// Подписи пунктов списка «Буфер устройства» в порядке отображения
/// (T1.0-B2; перенесено из `src/app/mode_dialog.rs::DEVICE_BUFFER_OPTIONS`
/// / `device_buffer_label`, ТЗ-94, ТЗ-131, §6.27).
pub const DEVICE_BUFFER_20MS: &str = "20 мс";
pub const DEVICE_BUFFER_40MS: &str = "40 мс";
pub const DEVICE_BUFFER_100MS: &str = "100 мс";
pub const DEVICE_BUFFER_200MS: &str = "200 мс";
pub const DEVICE_BUFFER_400MS: &str = "400 мс";

/// Подписи пунктов списка «Фильтр DSD → PCM» в порядке отображения
/// (T1.0-B2; перенесено из `src/app/mode_dialog.rs::DSD_FILTER_OPTIONS`,
/// ТЗ-94, ТЗ-128, §6.27).
pub const DSD_FILTER_24K: &str = "24 кГц";
pub const DSD_FILTER_30K: &str = "30 кГц";
pub const DSD_FILTER_50K: &str = "50 кГц";

/// Подписи пунктов списка «Политика несовпадения частоты» в порядке
/// отображения (T1.0-B2; перенесено из
/// `src/app/mode_dialog.rs::RATE_FALLBACK_OPTIONS`, ТЗ-94, §6.27).
pub const RATE_FALLBACK_SAME_FAMILY: &str = "Та же семья частот";
pub const RATE_FALLBACK_NEAREST: &str = "Ближайшая";
pub const RATE_FALLBACK_NO_DOWNSAMPLE: &str = "Без понижения";

/// Подписи пунктов списка «DSD выше возможностей ЦАП» в порядке отображения
/// (T1.0-B2; перенесено из
/// `src/app/mode_dialog.rs::DSD_ABOVE_DAC_OPTIONS`, ТЗ-34, ТЗ-94, §6.27);
/// второй — синтетический `NotImplemented`-пункт, непредставимый в
/// `DsdAboveDac`.
pub const DSD_ABOVE_DAC_CONVERT_TO_PCM: &str = "Преобразовать в PCM";
pub const DSD_ABOVE_DAC_REMODULATION: &str = "Ремодуляция";

/// Ошибка валидации поля `freq_min` вкладки Visualization → Spectrum
/// (§9.3, T1.0-B2); `freq_min`/`freq_max`/`bands` — имена ключей настроек,
/// не переводятся (AGENTS.md).
pub const VIZ_ERROR_FREQ_MIN: &str = "freq_min: целое 1..22050 и строго меньше freq_max";
/// Ошибка валидации поля `freq_max` вкладки Visualization → Spectrum
/// (§9.3, T1.0-B2).
pub const VIZ_ERROR_FREQ_MAX: &str = "freq_max: целое 1..22050 и строго больше freq_min";
/// Ошибка валидации поля `bands` вкладки Visualization → Spectrum (§9.3,
/// T1.0-B2).
pub const VIZ_ERROR_BANDS: &str = "bands: целое 4..128";

/// Подпись источника обложки в списке «Приоритет источников обложки»
/// вкладки «Обложки» (T1.0-B7; перенесено из
/// `src/settings.rs::CoverSource::label()`, было на английском).
pub fn cover_source_label(source: CoverSource) -> &'static str {
    match source {
        CoverSource::Folder => "Из папки (cover.jpg, …)",
        CoverSource::Embedded => "Встроенная в файл (APIC)",
        CoverSource::Internet => "Из интернета (iTunes)",
    }
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
    fn format_kbps_is_not_declined() {
        assert_eq!(format_kbps(1411), "1411 кбит/с");
        assert_eq!(format_kbps(1), "1 кбит/с");
    }

    #[test]
    fn format_channels_declines_by_count() {
        assert_eq!(format_channels(1), "1 канал");
        assert_eq!(format_channels(2), "2 канала");
        assert_eq!(format_channels(5), "5 каналов");
    }

    #[test]
    fn format_track_count_declines_by_count() {
        assert_eq!(format_track_count(1), "1 трек");
        assert_eq!(format_track_count(3), "3 трека");
        assert_eq!(format_track_count(0), "0 треков");
    }

    #[test]
    fn status_audio_ready_substitutes_name() {
        assert_eq!(status_audio_ready("Motu M2 (USB Audio)"), "Аудио: Motu M2 (USB Audio) готово");
    }

    #[test]
    fn tray_tooltip_unavailable_substitutes_error() {
        assert_eq!(
            tray_tooltip_unavailable("device missing"),
            "Воспроизведение недоступно: device missing"
        );
    }

    #[test]
    fn status_adding_tracks_declines_items() {
        assert_eq!(status_adding_tracks(1), "Добавление треков\u{2026} 1 элемент");
        assert_eq!(status_adding_tracks(3), "Добавление треков\u{2026} 3 элемента");
        assert_eq!(status_adding_tracks(5), "Добавление треков\u{2026} 5 элементов");
    }

    #[test]
    fn status_scanning_declines_tracks() {
        assert_eq!(status_scanning(1), "Сканирование\u{2026} 1 трек");
        assert_eq!(status_scanning(5), "Сканирование\u{2026} 5 треков");
    }

    #[test]
    fn status_scan_finished_nothing_new_declines_total() {
        assert_eq!(
            status_scan_finished_nothing_new(0),
            "Сканирование завершено: новых нет (найдено 0 треков)"
        );
        assert_eq!(
            status_scan_finished_nothing_new(5),
            "Сканирование завершено: новых нет (найдено 5 треков)"
        );
    }

    #[test]
    fn nbp_reason_label_covers_all_variants() {
        let cases = [
            (NbpReason::TestFailed, "тест не пройден"),
            (NbpReason::DriverParamsUnconfirmed, "параметры драйвера не подтверждены"),
            (NbpReason::DsdToPcm, "DSD→PCM"),
            (NbpReason::Resample, "ресемплинг"),
            (NbpReason::Downmix, "downmix"),
            (NbpReason::Truncation, "усечение разрядности"),
            (NbpReason::FloatToInt, "квантование float-источника"),
            (NbpReason::Int32ToFloat, "int32 → float"),
            (NbpReason::Mute, "mute"),
            (NbpReason::Volume, "громкость"),
            (NbpReason::Dither, "дизеринг"),
        ];
        for (reason, label) in cases {
            assert_eq!(nbp_reason_label(reason), label, "reason={reason:?}");
        }
    }

    #[test]
    fn badge_not_bit_perfect_without_more() {
        assert_eq!(badge_not_bit_perfect(NbpReason::Volume, 0), "Не bit-perfect: громкость");
    }

    #[test]
    fn badge_not_bit_perfect_with_more_appends_plus_n() {
        assert_eq!(badge_not_bit_perfect(NbpReason::Resample, 1), "Не bit-perfect: ресемплинг +1");
    }

    #[test]
    fn incomplete_why_label_covers_all_variants() {
        let cases = [
            (IncompleteWhy::Underrun, "underrun"),
            (IncompleteWhy::Xrun, "xrun"),
            (IncompleteWhy::MirrorOverflow, "переполнение зеркала"),
            (IncompleteWhy::Seek, "перемотка"),
            (IncompleteWhy::TrackChanged, "смена трека"),
            (IncompleteWhy::ModeChanged, "смена режима"),
            (IncompleteWhy::DeviceChanged, "смена устройства"),
            (IncompleteWhy::LockOpened, "открыт замок"),
        ];
        for (why, label) in cases {
            assert_eq!(incomplete_why_label(why), label, "why={why:?}");
        }
    }

    #[test]
    fn test_blocker_hint_shared_mode_and_no_stream() {
        assert_eq!(test_blocker_hint(&TestBlocker::SharedMode), "Проверка недоступна в Shared");
        assert_eq!(test_blocker_hint(&TestBlocker::NoStream), "Запустите воспроизведение");
    }

    #[test]
    fn test_blocker_hint_lossy_names_reason_and_links_signal_path() {
        let hint = test_blocker_hint(&TestBlocker::Lossy(NbpReason::Volume));
        assert_eq!(hint, "Мешает тесту: громкость — см. «Путь сигнала»");
    }

    #[test]
    fn test_blocker_hint_driver_params_unconfirmed_and_muted() {
        assert_eq!(
            test_blocker_hint(&TestBlocker::DriverParamsUnconfirmed),
            "Мешает тесту: параметры драйвера не подтверждены — см. «Путь сигнала»"
        );
        assert_eq!(test_blocker_hint(&TestBlocker::Muted), "Выключите mute");
    }

    #[test]
    fn test_badge_running_formats_elapsed_over_max() {
        assert_eq!(test_badge_running(3, 10), "Тест 3 / 10 с");
    }

    #[test]
    fn test_hint_incomplete_names_reason() {
        assert_eq!(test_hint_incomplete(IncompleteWhy::Seek), "Последний тест не завершён: перемотка");
    }

    #[test]
    fn mode_dialog_fixed_by_mode_substitutes_shown_value() {
        assert_eq!(mode_dialog_fixed_by_mode("Exclusive"), "Exclusive — задано режимом");
    }

    #[test]
    fn cover_source_label_covers_all_variants() {
        let cases = [
            (CoverSource::Folder, "Из папки (cover.jpg, …)"),
            (CoverSource::Embedded, "Встроенная в файл (APIC)"),
            (CoverSource::Internet, "Из интернета (iTunes)"),
        ];
        for (source, label) in cases {
            assert_eq!(cover_source_label(source), label, "source={source:?}");
        }
    }
}
