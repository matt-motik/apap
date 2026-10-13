//! Тесты `wheel_fixtures_<замер>_<шаг>` (§7.2, §7.4, ТЗ-36, НФ-2, НФ-8):
//! каждая фикстура `tests/data/wheel/<замер>/<NN>_<шаг>.fixture` — запись
//! одного шага замера `wheel_test.md` — прогоняется через `FakeWheelInput` с
//! поставляемой таблицей `wheel_table.toml`, и число щелчков сравнивается с
//! `expected_steps` шапки.
//!
//! Воспроизведение строк — в порядке файла, как приложение по §6.11:
//! - `line` / `pixel` — событие winit в `push_winit`; если `slint_dy` непуст,
//!   сразу парный `scroll-event` в `slider_scroll` (щелчки учитываются);
//!   если пуст — указатель вне ползунка: `end_tick`, щелчков нет (В-2);
//! - `slint` — `scroll-event` без пары winit: `slider_scroll`;
//! - `tray` — `tray_scroll` (В-5); хост — по `platform` шапки.
//!
//! Щелчки группируются по свайпам: новый свайп начинается при паузе строго
//! больше 300 мс между соседними значимыми событиями (ползунок — строки с
//! непустым `slint_dy`, трей — строки `tray` с непустым `delta_y`). Одно
//! число в `expected_steps` — сумма щелчков за шаг, несколько — щелчки по
//! свайпам. Знак `+` — громче.
//!
//! Список тестов явный; `wheel_fixtures_list_matches_files` следит, чтобы
//! новая фикстура не осталась без теста. Новые замеры добавляют фикстуры и
//! строки таблицы, код модуля ввода не меняется (НФ-8).

use std::time::Duration;

use serde::Deserialize;

use super::{
    parse_wheel_table, Desktop, FakeWheelInput, Session, SlintScroll, TrayHost, TrayScroll,
    WheelDelta, WheelEvent, WheelInput, WheelPhase, WheelTable,
};
use crate::audio::clock::ClockInstant;

/// Каталог фикстур в репозитории (§7.4, ТЗ-49).
const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/wheel");

/// Граница свайпа при группировке ожиданий, мс (§7.4, `wheel_test.md` §3):
/// пауза строго больше — новый свайп.
const SWIPE_GAP_MS: f64 = 300.0;

/// Заголовок CSV-части фикстуры (§7.4).
const CSV_HEADER: &str = "t_ms,kind,phase,delta_x,delta_y,modifiers,slint_dy";

/// Файл фикстуры: раздел `[fixture]` (§7.4).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureFile {
    fixture: Header,
}

/// Шапка фикстуры (§7.4). `device`, `natural_scroll`, `direction`, `action`
/// и `origin` на прогон не влияют: класс устройства определяет нормализатор.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    platform: String,
    session: String,
    #[serde(rename = "device")]
    _device: String,
    source: String,
    #[serde(rename = "natural_scroll")]
    _natural_scroll: String,
    #[serde(rename = "direction")]
    _direction: String,
    #[serde(rename = "action")]
    _action: String,
    expected_steps: Vec<i32>,
    #[serde(rename = "origin")]
    _origin: String,
}

/// Источник событий шага (§7.4): определяет, какие строки значимы.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Slider,
    Tray,
}

/// Вид строки данных (§7.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Line,
    Pixel,
    Tray,
    Slint,
}

/// Строка данных фикстуры; числа — строками, как в файле.
struct Row<'a> {
    line_no: usize,
    t_ms: f64,
    kind: Kind,
    phase: &'a str,
    delta_x: &'a str,
    delta_y: &'a str,
    slint_dy: &'a str,
}

/// Поставляемая таблица данных (§6.11, НФ-8).
fn shipped_table() -> WheelTable {
    match parse_wheel_table(include_str!("wheel_table.toml")) {
        Ok(table) => table,
        Err(e) => panic!("wheel_table.toml не разобран: {e}"),
    }
}

fn parse_f64(text: &str, what: &str, path: &str, line_no: usize) -> f64 {
    match text.parse::<f64>() {
        Ok(v) if v.is_finite() => v,
        _ => panic!("{path}:{line_no}: {what} = \"{text}\" — не число"),
    }
}

fn parse_f32(text: &str, what: &str, path: &str, line_no: usize) -> f32 {
    match text.parse::<f32>() {
        Ok(v) if v.is_finite() => v,
        _ => panic!("{path}:{line_no}: {what} = \"{text}\" — не число"),
    }
}

/// Необязательное смещение по `x`: пустое поле — 0.
fn parse_x(text: &str, path: &str, line_no: usize) -> f64 {
    if text.is_empty() {
        0.0
    } else {
        parse_f64(text, "delta_x", path, line_no)
    }
}

fn parse_i32(text: &str, what: &str, path: &str, line_no: usize) -> i32 {
    match text.parse::<i32>() {
        Ok(v) => v,
        Err(_) => panic!("{path}:{line_no}: {what} = \"{text}\" — не целое"),
    }
}

/// Момент события из `t_ms` фикстуры.
fn instant(t_ms: f64, path: &str, line_no: usize) -> ClockInstant {
    match Duration::try_from_secs_f64(t_ms / 1000.0) {
        Ok(d) => ClockInstant::from_start(d),
        Err(e) => panic!("{path}:{line_no}: t_ms = {t_ms} — не время: {e}"),
    }
}

/// Делит текст фикстуры на TOML-шапку и строки CSV по строке `---` (§7.4).
/// Возвращает шапку и строки данных с номерами строк файла (с 1).
fn split_fixture<'a>(text: &'a str, path: &str) -> (String, Vec<(usize, &'a str)>) {
    let mut head = String::new();
    let mut lines = text.lines().enumerate();
    let mut found = false;
    for (_, line) in lines.by_ref() {
        if line == "---" {
            found = true;
            break;
        }
        head.push_str(line);
        head.push('\n');
    }
    assert!(found, "{path}: нет разделителя `---` между шапкой и CSV");
    match lines.next() {
        Some((_, header)) if header == CSV_HEADER => {}
        other => panic!("{path}: заголовок CSV {other:?}, ожидался \"{CSV_HEADER}\""),
    }
    let rows = lines
        .filter(|(_, line)| !line.is_empty())
        .map(|(i, line)| (i + 1, line))
        .collect();
    (head, rows)
}

/// Разбор строки данных CSV (§7.4): ровно семь полей, без кавычек.
fn parse_row<'a>(line: &'a str, line_no: usize, path: &str) -> Row<'a> {
    let fields: Vec<&str> = line.split(',').collect();
    let [t_ms, kind, phase, delta_x, delta_y, _modifiers, slint_dy] = fields.as_slice() else {
        panic!(
            "{path}:{line_no}: полей {}, ожидалось 7: \"{line}\"",
            fields.len()
        );
    };
    let kind = match *kind {
        "line" => Kind::Line,
        "pixel" => Kind::Pixel,
        "tray" => Kind::Tray,
        "slint" => Kind::Slint,
        other => panic!("{path}:{line_no}: неизвестный kind = \"{other}\""),
    };
    Row {
        line_no,
        t_ms: parse_f64(t_ms, "t_ms", path, line_no),
        kind,
        phase,
        delta_x,
        delta_y,
        slint_dy,
    }
}

fn phase(text: &str, path: &str, line_no: usize) -> WheelPhase {
    match text {
        "started" => WheelPhase::Started,
        "moved" => WheelPhase::Moved,
        "ended" => WheelPhase::Ended,
        "cancelled" => WheelPhase::Cancelled,
        other => panic!("{path}:{line_no}: неизвестная phase = \"{other}\""),
    }
}

/// Подмена модуля ввода для платформы шапки (§7.1, §6.11 «Хост трея»).
fn input_for(header: &Header, path: &str) -> FakeWheelInput {
    let desktop = match header.platform.as_str() {
        "kde" => Desktop::Kde,
        "gnome" => Desktop::Gnome,
        "windows" => Desktop::Windows,
        "macos" => Desktop::Macos,
        other => panic!("{path}: неизвестная platform = \"{other}\""),
    };
    let session = match header.session.as_str() {
        "wayland" => Session::Wayland,
        "x11" => Session::X11,
        "win32" => Session::Win32,
        "appkit" => Session::AppKit,
        other => panic!("{path}: неизвестный session = \"{other}\""),
    };
    let mut input = FakeWheelInput::new(shipped_table(), desktop, Some(session));
    match desktop {
        Desktop::Kde => input.set_tray_host(TrayHost::KdePlasma),
        Desktop::Gnome => input.set_tray_host(TrayHost::GnomeAppIndicator),
        Desktop::OtherLinux | Desktop::Windows | Desktop::Macos => {}
    }
    input
}

/// Воспроизводит одну строку (§6.11). `Some(щелчки)` — событие значимо для
/// источника шага; `None` — до громкости не доходит.
fn replay_row(
    input: &mut FakeWheelInput,
    row: &Row<'_>,
    source: Source,
    path: &str,
) -> Option<i32> {
    let n = row.line_no;
    let t = instant(row.t_ms, path, n);
    match row.kind {
        Kind::Line | Kind::Pixel => {
            let delta = if row.kind == Kind::Line {
                WheelDelta::Lines {
                    x: if row.delta_x.is_empty() {
                        0.0
                    } else {
                        parse_f32(row.delta_x, "delta_x", path, n)
                    },
                    y: parse_f32(row.delta_y, "delta_y", path, n),
                }
            } else {
                WheelDelta::Px {
                    x: parse_x(row.delta_x, path, n),
                    y: parse_f64(row.delta_y, "delta_y", path, n),
                }
            };
            input.push_winit(WheelEvent {
                t,
                delta,
                phase: phase(row.phase, path, n),
                momentum: None,
            });
            if row.slint_dy.is_empty() {
                // Указатель вне ползунка: `scroll-event` в этом тике нет (В-2).
                input.end_tick();
                return None;
            }
            let dy = parse_f32(row.slint_dy, "slint_dy", path, n);
            let clicks = input.slider_scroll(SlintScroll { t, dx: 0.0, dy }).0;
            input.end_tick();
            (source == Source::Slider).then_some(clicks)
        }
        Kind::Slint => {
            let dy = parse_f32(row.slint_dy, "slint_dy", path, n);
            let clicks = input.slider_scroll(SlintScroll { t, dx: 0.0, dy }).0;
            input.end_tick();
            (source == Source::Slider).then_some(clicks)
        }
        Kind::Tray => {
            // Vertical: delta ksni в delta_y; Horizontal: в delta_x (§7.4).
            let (text, vertical) = if row.delta_y.is_empty() {
                (row.delta_x, false)
            } else {
                (row.delta_y, true)
            };
            let delta = parse_i32(text, "delta", path, n);
            let clicks = input.tray_scroll(TrayScroll { t, delta, vertical }).0;
            (source == Source::Tray && vertical).then_some(clicks)
        }
    }
}

/// Прогон одной фикстуры (§7.2 `wheel_fixtures_<замер>_<шаг>`, ТЗ-36).
fn run_fixture(dir: &str, file: &str) {
    let path = format!("{FIXTURE_ROOT}/{dir}/{file}");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => panic!("{path}: не прочитан: {e}"),
    };
    let (head, rows) = split_fixture(&text, &path);
    let header = match toml::from_str::<FixtureFile>(&head) {
        Ok(parsed) => parsed.fixture,
        Err(e) => panic!("{path}: шапка не разобрана: {e}"),
    };
    let source = match header.source.as_str() {
        "slider" => Source::Slider,
        "tray" => Source::Tray,
        other => panic!("{path}: неизвестный source = \"{other}\""),
    };
    assert!(
        !header.expected_steps.is_empty(),
        "{path}: expected_steps пуст — ожидание не задано"
    );
    assert!(!rows.is_empty(), "{path}: нет строк данных");

    let mut input = input_for(&header, &path);
    // Щелчки по свайпам: пауза строго больше 300 мс между значимыми событиями.
    let mut swipes: Vec<i32> = Vec::new();
    let mut last_t: Option<f64> = None;
    let mut significant = 0usize;
    for (line_no, line) in &rows {
        let row = parse_row(line, *line_no, &path);
        let Some(clicks) = replay_row(&mut input, &row, source, &path) else {
            continue;
        };
        significant += 1;
        let new_swipe = match last_t {
            Some(prev) => row.t_ms - prev > SWIPE_GAP_MS,
            None => true,
        };
        last_t = Some(row.t_ms);
        match swipes.last_mut() {
            Some(sum) if !new_swipe => *sum += clicks,
            _ => swipes.push(clicks),
        }
    }
    assert!(
        significant > 0,
        "{path}: нет значимых событий для source = {source:?}"
    );

    let total: i32 = swipes.iter().sum();
    if let [expected] = header.expected_steps.as_slice() {
        assert_eq!(
            total, *expected,
            "{path}: сумма щелчков за шаг {total}, ожидалось {expected}; по свайпам: {swipes:?}"
        );
    } else {
        assert_eq!(
            swipes, header.expected_steps,
            "{path}: щелчки по свайпам (сумма {total}) не равны expected_steps"
        );
    }
}

/// Порождает по одному `#[test]` на фикстуру и общий список `FIXTURES`
/// (каталог замера → файлы) для проверки полноты.
macro_rules! wheel_fixture_tests {
    ($( $dir:literal => [ $( $name:ident: $file:literal ),* $(,)? ] ),* $(,)?) => {
        /// Явный список фикстур с тестами: каталог замера и его файлы (§7.4).
        const FIXTURES: &[(&str, &[&str])] = &[ $( ($dir, &[ $( $file ),* ]) ),* ];

        $( $(
            #[test]
            fn $name() {
                run_fixture($dir, $file);
            }
        )* )*
    };
}

wheel_fixture_tests! {
    "kde-1" => [
        wheel_fixtures_kde_1_01: "01_mouse_slider_single_up.fixture",
        wheel_fixtures_kde_1_02: "02_mouse_slider_single_down.fixture",
        wheel_fixtures_kde_1_03: "03_mouse_slider_fast_up.fixture",
        wheel_fixtures_kde_1_04: "04_mouse_slider_shift_single_up.fixture",
        wheel_fixtures_kde_1_05: "05_mouse_tray_single_up.fixture",
        wheel_fixtures_kde_1_06: "06_mouse_tray_single_down.fixture",
        wheel_fixtures_kde_1_07: "07_mouse_tray_fast_up.fixture",
        wheel_fixtures_kde_1_08: "08_mouse_tray_shift_single_up.fixture",
    ],
    "kde-2" => [
        wheel_fixtures_kde_2_01: "01_mouse_slider_single_up.fixture",
        wheel_fixtures_kde_2_02: "02_mouse_slider_single_down.fixture",
        wheel_fixtures_kde_2_03: "03_mouse_slider_fast_up.fixture",
        wheel_fixtures_kde_2_04: "04_mouse_slider_shift_single_up.fixture",
        wheel_fixtures_kde_2_05: "05_mouse_tray_single_up.fixture",
        wheel_fixtures_kde_2_06: "06_mouse_tray_single_down.fixture",
        wheel_fixtures_kde_2_07: "07_mouse_tray_fast_up.fixture",
        wheel_fixtures_kde_2_08: "08_mouse_tray_shift_single_up.fixture",
    ],
    "kde-3" => [
        wheel_fixtures_kde_3_01: "01_mouse_slider_single_up.fixture",
        wheel_fixtures_kde_3_02: "02_mouse_slider_single_down.fixture",
        wheel_fixtures_kde_3_03: "03_mouse_slider_fast_up.fixture",
        wheel_fixtures_kde_3_04: "04_mouse_slider_shift_single_up.fixture",
        wheel_fixtures_kde_3_05: "05_mouse_tray_single_up.fixture",
        wheel_fixtures_kde_3_06: "06_mouse_tray_single_down.fixture",
        wheel_fixtures_kde_3_07: "07_mouse_tray_fast_up.fixture",
        wheel_fixtures_kde_3_08: "08_mouse_tray_shift_single_up.fixture",
        wheel_fixtures_kde_3_09: "09_touchpad_slider_short_up-natural_on.fixture",
        wheel_fixtures_kde_3_10: "10_touchpad_slider_long_slow_up-natural_on.fixture",
        wheel_fixtures_kde_3_11: "11_touchpad_slider_fast_up-natural_on.fixture",
        wheel_fixtures_kde_3_12: "12_touchpad_slider_short_down-natural_on.fixture",
        wheel_fixtures_kde_3_13: "13_touchpad_slider_long_slow_down-natural_on.fixture",
        wheel_fixtures_kde_3_14: "14_touchpad_slider_fast_down-natural_on.fixture",
        wheel_fixtures_kde_3_15: "15_touchpad_tray_short_up-natural_on.fixture",
        wheel_fixtures_kde_3_16: "16_touchpad_tray_long_slow_up-natural_on.fixture",
        wheel_fixtures_kde_3_17: "17_touchpad_tray_fast_up-natural_on.fixture",
        wheel_fixtures_kde_3_18: "18_touchpad_tray_short_down-natural_on.fixture",
        wheel_fixtures_kde_3_19: "19_touchpad_tray_long_slow_down-natural_on.fixture",
        wheel_fixtures_kde_3_20: "20_touchpad_tray_fast_down-natural_on.fixture",
        wheel_fixtures_kde_3_21: "21_touchpad_slider_short_up-natural_off.fixture",
        wheel_fixtures_kde_3_22: "22_touchpad_slider_long_slow_up-natural_off.fixture",
        wheel_fixtures_kde_3_23: "23_touchpad_slider_fast_up-natural_off.fixture",
        wheel_fixtures_kde_3_24: "24_touchpad_slider_short_down-natural_off.fixture",
        wheel_fixtures_kde_3_25: "25_touchpad_slider_long_slow_down-natural_off.fixture",
        wheel_fixtures_kde_3_26: "26_touchpad_slider_fast_down-natural_off.fixture",
        wheel_fixtures_kde_3_27: "27_touchpad_tray_short_up-natural_off.fixture",
        wheel_fixtures_kde_3_28: "28_touchpad_tray_long_slow_up-natural_off.fixture",
        wheel_fixtures_kde_3_29: "29_touchpad_tray_fast_up-natural_off.fixture",
        wheel_fixtures_kde_3_30: "30_touchpad_tray_short_down-natural_off.fixture",
        wheel_fixtures_kde_3_31: "31_touchpad_tray_long_slow_down-natural_off.fixture",
        wheel_fixtures_kde_3_32: "32_touchpad_tray_fast_down-natural_off.fixture",
    ],
    "gnome-1" => [
        wheel_fixtures_gnome_1_01: "01_mouse_slider_single_up.fixture",
        wheel_fixtures_gnome_1_02: "02_mouse_slider_single_down.fixture",
        wheel_fixtures_gnome_1_03: "03_mouse_slider_fast_up.fixture",
        wheel_fixtures_gnome_1_04: "04_mouse_slider_shift_single_up.fixture",
        wheel_fixtures_gnome_1_05: "05_mouse_tray_single_up.fixture",
        wheel_fixtures_gnome_1_06: "06_mouse_tray_single_down.fixture",
        wheel_fixtures_gnome_1_07: "07_mouse_tray_fast_up.fixture",
        wheel_fixtures_gnome_1_08: "08_mouse_tray_shift_single_up.fixture",
    ],
    "gnome-2" => [
        wheel_fixtures_gnome_2_01: "01_mouse_slider_single_up.fixture",
        wheel_fixtures_gnome_2_02: "02_mouse_slider_single_down.fixture",
        wheel_fixtures_gnome_2_03: "03_mouse_slider_fast_up.fixture",
        wheel_fixtures_gnome_2_04: "04_mouse_slider_shift_single_up.fixture",
        wheel_fixtures_gnome_2_05: "05_mouse_tray_single_up.fixture",
        wheel_fixtures_gnome_2_06: "06_mouse_tray_single_down.fixture",
        wheel_fixtures_gnome_2_07: "07_mouse_tray_fast_up.fixture",
        wheel_fixtures_gnome_2_08: "08_mouse_tray_shift_single_up.fixture",
        wheel_fixtures_gnome_2_09: "09_touchpad_slider_short_up-natural_on.fixture",
        wheel_fixtures_gnome_2_10: "10_touchpad_slider_long_slow_up-natural_on.fixture",
        wheel_fixtures_gnome_2_11: "11_touchpad_slider_fast_up-natural_on.fixture",
        wheel_fixtures_gnome_2_12: "12_touchpad_slider_short_down-natural_on.fixture",
        wheel_fixtures_gnome_2_13: "13_touchpad_slider_long_slow_down-natural_on.fixture",
        wheel_fixtures_gnome_2_14: "14_touchpad_slider_fast_down-natural_on.fixture",
        wheel_fixtures_gnome_2_15: "15_touchpad_tray_short_up-natural_on.fixture",
        wheel_fixtures_gnome_2_16: "16_touchpad_tray_long_slow_up-natural_on.fixture",
        wheel_fixtures_gnome_2_17: "17_touchpad_tray_fast_up-natural_on.fixture",
        wheel_fixtures_gnome_2_18: "18_touchpad_tray_short_down-natural_on.fixture",
        wheel_fixtures_gnome_2_19: "19_touchpad_tray_long_slow_down-natural_on.fixture",
        wheel_fixtures_gnome_2_20: "20_touchpad_tray_fast_down-natural_on.fixture",
        wheel_fixtures_gnome_2_21: "21_touchpad_slider_short_up-natural_off.fixture",
        wheel_fixtures_gnome_2_22: "22_touchpad_slider_long_slow_up-natural_off.fixture",
        wheel_fixtures_gnome_2_23: "23_touchpad_slider_fast_up-natural_off.fixture",
        wheel_fixtures_gnome_2_24: "24_touchpad_slider_short_down-natural_off.fixture",
        wheel_fixtures_gnome_2_25: "25_touchpad_slider_long_slow_down-natural_off.fixture",
        wheel_fixtures_gnome_2_26: "26_touchpad_slider_fast_down-natural_off.fixture",
        wheel_fixtures_gnome_2_27: "27_touchpad_tray_short_up-natural_off.fixture",
        wheel_fixtures_gnome_2_28: "28_touchpad_tray_long_slow_up-natural_off.fixture",
        wheel_fixtures_gnome_2_29: "29_touchpad_tray_fast_up-natural_off.fixture",
        wheel_fixtures_gnome_2_30: "30_touchpad_tray_short_down-natural_off.fixture",
        wheel_fixtures_gnome_2_31: "31_touchpad_tray_long_slow_down-natural_off.fixture",
        wheel_fixtures_gnome_2_32: "32_touchpad_tray_fast_down-natural_off.fixture",
    ],
    "windows" => [
        wheel_fixtures_windows_01: "01_mouse_slider_single_up.fixture",
        wheel_fixtures_windows_02: "02_mouse_slider_single_down.fixture",
        wheel_fixtures_windows_03: "03_mouse_slider_fast_up.fixture",
        wheel_fixtures_windows_04: "04_mouse_slider_shift_single_up.fixture",
        wheel_fixtures_windows_05: "05_touchpad_slider_short_up-natural_on.fixture",
        wheel_fixtures_windows_06: "06_touchpad_slider_long_slow_up-natural_on.fixture",
        wheel_fixtures_windows_07: "07_touchpad_slider_fast_up-natural_on.fixture",
        wheel_fixtures_windows_08: "08_touchpad_slider_short_down-natural_on.fixture",
        wheel_fixtures_windows_09: "09_touchpad_slider_long_slow_down-natural_on.fixture",
        wheel_fixtures_windows_10: "10_touchpad_slider_fast_down-natural_on.fixture",
        wheel_fixtures_windows_11: "11_touchpad_slider_short_up-natural_off.fixture",
        wheel_fixtures_windows_12: "12_touchpad_slider_long_slow_up-natural_off.fixture",
        wheel_fixtures_windows_13: "13_touchpad_slider_fast_up-natural_off.fixture",
        wheel_fixtures_windows_14: "14_touchpad_slider_short_down-natural_off.fixture",
        wheel_fixtures_windows_15: "15_touchpad_slider_long_slow_down-natural_off.fixture",
        wheel_fixtures_windows_16: "16_touchpad_slider_fast_down-natural_off.fixture",
    ],
}

/// Файлы `.fixture` в `tests/data/wheel/*/` и список тестов совпадают: новая
/// фикстура не остаётся без теста, а тест — без файла (§7.4, НФ-8).
#[test]
fn wheel_fixtures_list_matches_files() {
    let mut on_disk: Vec<String> = Vec::new();
    let dirs = match std::fs::read_dir(FIXTURE_ROOT) {
        Ok(dirs) => dirs,
        Err(e) => panic!("{FIXTURE_ROOT}: каталог не прочитан: {e}"),
    };
    for dir in dirs {
        let dir = match dir {
            Ok(dir) => dir.path(),
            Err(e) => panic!("{FIXTURE_ROOT}: запись каталога не прочитана: {e}"),
        };
        if !dir.is_dir() {
            continue;
        }
        let files = match std::fs::read_dir(&dir) {
            Ok(files) => files,
            Err(e) => panic!("{}: каталог не прочитан: {e}", dir.display()),
        };
        for file in files {
            let file = match file {
                Ok(file) => file.path(),
                Err(e) => panic!("{}: запись каталога не прочитана: {e}", dir.display()),
            };
            if file.extension().and_then(|ext| ext.to_str()) != Some("fixture") {
                continue;
            }
            let names = (
                dir.file_name().and_then(|name| name.to_str()),
                file.file_name().and_then(|name| name.to_str()),
            );
            match names {
                (Some(dir_name), Some(file_name)) => {
                    on_disk.push(format!("{dir_name}/{file_name}"))
                }
                _ => panic!("{}: имя не в UTF-8", file.display()),
            }
        }
    }
    on_disk.sort();

    let mut listed: Vec<String> = FIXTURES
        .iter()
        .flat_map(|(dir, files)| files.iter().map(move |file| format!("{dir}/{file}")))
        .collect();
    listed.sort();

    assert_eq!(
        on_disk.len(),
        listed.len(),
        "файлов .fixture в {FIXTURE_ROOT}: {}, тестов в списке: {}",
        on_disk.len(),
        listed.len()
    );
    assert_eq!(
        on_disk, listed,
        "список тестов не совпадает с файлами {FIXTURE_ROOT}"
    );
}
