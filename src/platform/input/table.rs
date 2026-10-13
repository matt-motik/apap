//! Таблица данных модуля ввода колёсика (ADR-8, ТЗ-36, НФ-8, §2.8, §6.11):
//! типы строк, разбор `wheel_table.toml` и поиск правила. Значения — данные
//! таблицы, а не константы кода: новые замеры меняют файл, а не модуль (НФ-8).

use serde::Deserialize;

use super::{Desktop, DeviceClass, Session, TrayHost};

/// Таблица данных (ТЗ-36, НФ-8, §2.8): `src/platform/input/wheel_table.toml`,
/// включается `include_str!`.
#[derive(Clone, PartialEq, Debug)]
pub struct WheelTable {
    pub slider: Vec<SliderRow>,
    pub tray: Vec<TrayRow>,
    /// Правило по умолчанию для класса «тачпад» (ОВС-2 а).
    pub default_touchpad: TouchpadParams,
    /// Окно «целое после дробного = тачпад» на Windows, мс (§6.11).
    pub integer_after_fraction_ms: u32,
}

/// Строка ползунка: «платформа × сеанс × класс устройства» (§2.8, ADR-8).
#[derive(Clone, PartialEq, Debug)]
pub struct SliderRow {
    pub desktop: Desktop,
    /// `None` — любой сеанс.
    pub session: Option<Session>,
    pub class: DeviceClass,
    pub rule: SliderRule,
    pub outcome: Outcome,
}

/// Правило нормализации ползунка (§2.8, §6.11).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SliderRule {
    /// k делений в событии = k щелчков, без ограничения скорости (В-3).
    Division,
    Touchpad(TouchpadParams),
}

/// Накопитель тачпада (В-4; ОВС-2 а; §2.8).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TouchpadParams {
    /// Порог шага в строках для `Lines` (0,5 строки).
    pub threshold_lines: f32,
    /// Порог шага в логических px для `Px` (30 px).
    pub threshold_px: f64,
    /// K: не больше шагов на свайп.
    pub max_steps: u8,
    /// Граница свайпа: пауза больше этого, мс; после неё остаток сбрасывается.
    pub swipe_gap_ms: u32,
}

/// Строка трея: «хост» (§2.8, В-5).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TrayRow {
    pub host: TrayHost,
    /// +1 / −1: знак «вверх = громче» (В-5).
    pub sign: i8,
    /// Единица деления; 0 — по модулю первого события (неизвестный хост, В-5).
    pub division: u32,
    pub outcome: Outcome,
}

/// Исход для протокола приёмки (ТЗ-36).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Experiment,
    PlatformDocs,
    DefaultRule,
    NoSource,
}

/// Ошибка разбора таблицы (§2.8): текст для журнала `WheelTable` (§6.11).
#[derive(Clone, PartialEq, Debug)]
pub struct TableError(pub Box<str>);

impl std::fmt::Display for TableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TableError {}

/// Строка трея неизвестного хоста: знак как у KDE, деление — модуль первого
/// события (В-5, §6.11).
const UNKNOWN_TRAY_ROW: TrayRow =
    TrayRow { host: TrayHost::Unknown, sign: 1, division: 0, outcome: Outcome::DefaultRule };

impl WheelTable {
    /// Встроенное правило по умолчанию тех же значений, что в
    /// `wheel_table.toml` (§6.11, ОВС-2 а): используется при ошибке разбора
    /// таблицы в рантайме. Строк ползунка нет — любое обращение даёт правило
    /// по умолчанию для класса.
    pub fn builtin() -> WheelTable {
        WheelTable {
            slider: Vec::new(),
            tray: vec![UNKNOWN_TRAY_ROW],
            default_touchpad: TouchpadParams {
                threshold_lines: 0.5,
                threshold_px: 30.0,
                max_steps: 5,
                swipe_gap_ms: 300,
            },
            integer_after_fraction_ms: 200,
        }
    }

    /// Правило ползунка (§6.11): точная строка «платформа × сеанс × класс»,
    /// затем строка «любой сеанс», иначе правило по умолчанию для класса.
    pub fn slider_rule(
        &self,
        desktop: Desktop,
        session: Option<Session>,
        class: DeviceClass,
    ) -> SliderRule {
        let find = |wanted: Option<Session>| {
            self.slider
                .iter()
                .find(|row| row.desktop == desktop && row.class == class && row.session == wanted)
        };
        match find(session).or_else(|| find(None)) {
            Some(row) => row.rule,
            None => default_rule(class, self.default_touchpad),
        }
    }

    /// Строка трея для хоста (§6.11, В-5); нет строки — правило неизвестного
    /// хоста: знак `+`, деление по модулю первого события.
    pub fn tray_row(&self, host: TrayHost) -> TrayRow {
        match self.tray.iter().find(|row| row.host == host) {
            Some(row) => *row,
            None => UNKNOWN_TRAY_ROW,
        }
    }
}

/// Правило по умолчанию для класса (ADR-8, ОВС-2 а): колесо — 1 деление =
/// 1 щелчок, тачпад — накопитель с параметрами `default_touchpad`.
fn default_rule(class: DeviceClass, default_touchpad: TouchpadParams) -> SliderRule {
    match class {
        DeviceClass::Wheel => SliderRule::Division,
        DeviceClass::Touchpad => SliderRule::Touchpad(default_touchpad),
    }
}

/// Сырой вид файла `wheel_table.toml` (§6.11). Неизвестные ключи — ошибка.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    integer_after_fraction_ms: u32,
    default_touchpad: RawDefaultTouchpad,
    #[serde(default)]
    slider: Vec<RawSliderRow>,
    #[serde(default)]
    tray: Vec<RawTrayRow>,
}

/// Сырой раздел `[default_touchpad]`: все четыре параметра обязательны.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefaultTouchpad {
    threshold_lines: f32,
    threshold_px: f64,
    max_steps: u8,
    swipe_gap_ms: u32,
}

/// Сырая строка `[[slider]]`: параметры тачпада необязательны.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSliderRow {
    desktop: String,
    session: Option<String>,
    class: String,
    rule: String,
    outcome: String,
    threshold_lines: Option<f32>,
    threshold_px: Option<f64>,
    max_steps: Option<u8>,
    swipe_gap_ms: Option<u32>,
}

/// Сырая строка `[[tray]]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrayRow {
    host: String,
    sign: i64,
    division: u32,
    outcome: String,
}

/// Разбор таблицы данных (§2.8, §6.11, НФ-8). Ошибка — неизвестный ключ или
/// значение, `sign` не из {−1, +1}, `max_steps == 0`, порог ≤ 0 или не
/// конечный. Паники нет: в рантайме при ошибке берётся `WheelTable::builtin`.
pub fn parse_wheel_table(text: &str) -> Result<WheelTable, TableError> {
    let raw: RawTable = toml::from_str(text).map_err(|e| err(format!("wheel_table.toml: {e}")))?;

    let d = raw.default_touchpad;
    let default_touchpad = check_touchpad(
        TouchpadParams {
            threshold_lines: d.threshold_lines,
            threshold_px: d.threshold_px,
            max_steps: d.max_steps,
            swipe_gap_ms: d.swipe_gap_ms,
        },
        "default_touchpad",
    )?;

    let mut slider = Vec::with_capacity(raw.slider.len());
    for (index, row) in raw.slider.iter().enumerate() {
        slider.push(slider_row(row, default_touchpad, &format!("slider[{index}]"))?);
    }

    let mut tray = Vec::with_capacity(raw.tray.len());
    for (index, row) in raw.tray.iter().enumerate() {
        tray.push(tray_row(row, &format!("tray[{index}]"))?);
    }

    Ok(WheelTable {
        slider,
        tray,
        default_touchpad,
        integer_after_fraction_ms: raw.integer_after_fraction_ms,
    })
}

fn err(message: String) -> TableError {
    TableError(message.into_boxed_str())
}

fn unknown(place: &str, key: &str, value: &str) -> TableError {
    err(format!("{place}: неизвестное значение {key} = \"{value}\""))
}

/// Преобразование строки `[[slider]]` (§6.11).
fn slider_row(
    raw: &RawSliderRow,
    default_touchpad: TouchpadParams,
    place: &str,
) -> Result<SliderRow, TableError> {
    let desktop = match raw.desktop.as_str() {
        "kde" => Desktop::Kde,
        "gnome" => Desktop::Gnome,
        "other_linux" => Desktop::OtherLinux,
        "windows" => Desktop::Windows,
        "macos" => Desktop::Macos,
        other => return Err(unknown(place, "desktop", other)),
    };
    let session = match raw.session.as_deref() {
        None => None,
        Some("wayland") => Some(Session::Wayland),
        Some("x11") => Some(Session::X11),
        Some("win32") => Some(Session::Win32),
        Some("appkit") => Some(Session::AppKit),
        Some(other) => return Err(unknown(place, "session", other)),
    };
    let class = match raw.class.as_str() {
        "wheel" => DeviceClass::Wheel,
        "touchpad" => DeviceClass::Touchpad,
        other => return Err(unknown(place, "class", other)),
    };
    let has_params = raw.threshold_lines.is_some()
        || raw.threshold_px.is_some()
        || raw.max_steps.is_some()
        || raw.swipe_gap_ms.is_some();
    let rule = match raw.rule.as_str() {
        "touchpad" => SliderRule::Touchpad(check_touchpad(
            TouchpadParams {
                threshold_lines: raw.threshold_lines.unwrap_or(default_touchpad.threshold_lines),
                threshold_px: raw.threshold_px.unwrap_or(default_touchpad.threshold_px),
                max_steps: raw.max_steps.unwrap_or(default_touchpad.max_steps),
                swipe_gap_ms: raw.swipe_gap_ms.unwrap_or(default_touchpad.swipe_gap_ms),
            },
            place,
        )?),
        "division" | "default" if has_params => {
            return Err(err(format!(
                "{place}: параметры тачпада допустимы только при rule = \"touchpad\""
            )));
        }
        "division" => SliderRule::Division,
        "default" => default_rule(class, default_touchpad),
        other => return Err(unknown(place, "rule", other)),
    };
    let outcome = outcome(&raw.outcome, place)?;
    Ok(SliderRow { desktop, session, class, rule, outcome })
}

/// Преобразование строки `[[tray]]` (§6.11, В-5).
fn tray_row(raw: &RawTrayRow, place: &str) -> Result<TrayRow, TableError> {
    let host = match raw.host.as_str() {
        "kde_plasma" => TrayHost::KdePlasma,
        "gnome_appindicator" => TrayHost::GnomeAppIndicator,
        "unknown" => TrayHost::Unknown,
        other => return Err(unknown(place, "host", other)),
    };
    let sign: i8 = match raw.sign {
        1 => 1,
        -1 => -1,
        other => return Err(err(format!("{place}: sign = {other}, допустимо только 1 или -1"))),
    };
    let outcome = outcome(&raw.outcome, place)?;
    Ok(TrayRow { host, sign, division: raw.division, outcome })
}

fn outcome(value: &str, place: &str) -> Result<Outcome, TableError> {
    match value {
        "experiment" => Ok(Outcome::Experiment),
        "platform_docs" => Ok(Outcome::PlatformDocs),
        "default_rule" => Ok(Outcome::DefaultRule),
        "no_source" => Ok(Outcome::NoSource),
        other => Err(unknown(place, "outcome", other)),
    }
}

/// Проверка параметров накопителя (§6.11): пороги — делители в `touch_step`,
/// поэтому конечные и больше нуля; `max_steps` ≥ 1.
fn check_touchpad(params: TouchpadParams, place: &str) -> Result<TouchpadParams, TableError> {
    if !(params.threshold_lines.is_finite() && params.threshold_lines > 0.0) {
        return Err(err(format!("{place}: threshold_lines должен быть конечным и больше 0")));
    }
    if !(params.threshold_px.is_finite() && params.threshold_px > 0.0) {
        return Err(err(format!("{place}: threshold_px должен быть конечным и больше 0")));
    }
    if params.max_steps == 0 {
        return Err(err(format!("{place}: max_steps должен быть не меньше 1")));
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "integer_after_fraction_ms = 200\n\
        [default_touchpad]\n\
        threshold_lines = 0.5\n\
        threshold_px = 30.0\n\
        max_steps = 5\n\
        swipe_gap_ms = 300\n";

    fn shipped() -> WheelTable {
        match parse_wheel_table(include_str!("wheel_table.toml")) {
            Ok(table) => table,
            Err(e) => panic!("wheel_table.toml не разобран: {e}"),
        }
    }

    /// §7: `wheel_table_parses` (ТЗ-36, НФ-8).
    #[test]
    fn wheel_table_parses() {
        let table = shipped();
        for desktop in
            [Desktop::Kde, Desktop::Gnome, Desktop::OtherLinux, Desktop::Windows, Desktop::Macos]
        {
            assert!(table.slider.iter().any(|r| r.desktop == desktop), "нет строки {desktop:?}");
        }
        for host in [TrayHost::KdePlasma, TrayHost::GnomeAppIndicator, TrayHost::Unknown] {
            assert!(table.tray.iter().any(|r| r.host == host), "нет строки {host:?}");
        }
        let builtin = WheelTable::builtin();
        assert_eq!(table.default_touchpad, builtin.default_touchpad);
        assert_eq!(table.integer_after_fraction_ms, builtin.integer_after_fraction_ms);
    }

    #[test]
    fn wheel_table_rejects_bad_values() {
        let bad = [
            "[[slider]]\ndesktop = \"beos\"\nclass = \"wheel\"\nrule = \"division\"\noutcome = \"experiment\"\n",
            "[[slider]]\ndesktop = \"kde\"\nclass = \"wheel\"\nrule = \"division\"\noutcome = \"experiment\"\nextra = 1\n",
            "[[slider]]\ndesktop = \"kde\"\nclass = \"touchpad\"\nrule = \"touchpad\"\nmax_steps = 0\noutcome = \"experiment\"\n",
            "[[slider]]\ndesktop = \"kde\"\nclass = \"touchpad\"\nrule = \"touchpad\"\nthreshold_px = -1.0\noutcome = \"experiment\"\n",
            "[[slider]]\ndesktop = \"kde\"\nclass = \"touchpad\"\nrule = \"touchpad\"\nthreshold_lines = inf\noutcome = \"experiment\"\n",
            "[[tray]]\nhost = \"kde_plasma\"\nsign = 2\ndivision = 120\noutcome = \"experiment\"\n",
        ];
        for tail in bad {
            let text = format!("{HEAD}{tail}");
            assert!(parse_wheel_table(&text).is_err(), "принято: {tail}");
        }
        assert!(parse_wheel_table(HEAD).is_ok());
    }

    #[test]
    fn wheel_table_slider_rule_fallback() {
        let table = shipped();
        let touchpad = SliderRule::Touchpad(table.default_touchpad);
        // Точная строка.
        assert_eq!(
            table.slider_rule(Desktop::Kde, Some(Session::Wayland), DeviceClass::Wheel),
            SliderRule::Division
        );
        // Строка «любой сеанс».
        assert_eq!(
            table.slider_rule(Desktop::Kde, Some(Session::X11), DeviceClass::Touchpad),
            touchpad
        );
        // Строки нет вовсе — правило по умолчанию для класса.
        let builtin = WheelTable::builtin();
        assert_eq!(
            builtin.slider_rule(Desktop::Macos, None, DeviceClass::Wheel),
            SliderRule::Division
        );
        assert_eq!(
            builtin.slider_rule(Desktop::Windows, Some(Session::Win32), DeviceClass::Touchpad),
            SliderRule::Touchpad(builtin.default_touchpad)
        );
    }

    #[test]
    fn wheel_table_tray_rows() {
        let table = shipped();
        let kde = table.tray_row(TrayHost::KdePlasma);
        assert_eq!((kde.sign, kde.division, kde.outcome), (1, 120, Outcome::Experiment));
        let gnome = table.tray_row(TrayHost::GnomeAppIndicator);
        assert_eq!((gnome.sign, gnome.division, gnome.outcome), (-1, 1, Outcome::Experiment));
        // Строки хоста нет — правило неизвестного хоста.
        assert_eq!(WheelTable::builtin().tray_row(TrayHost::KdePlasma), UNKNOWN_TRAY_ROW);
    }
}
