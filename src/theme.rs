//! Система тем на основе TOML-файлов (спека T1.0).
//!
//! Модуль не зависит от Slint: структура тем, дефолтные шаблоны, сканирование
//! каталога `themes/` и чистый парсер HEX-строк. Валидация HEX выполняется в
//! bin-крейте (`validate_colors`), здесь — только структурная через serde.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Базовая цветовая схема темы: определяет `FluentPalette.color_scheme` и
/// набор иконок верхней панели (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StandardPalette {
    Dark,
    Light,
}

/// Контракт из ровно 22 app-specific цветов (сверено с `ui/theme.slint` и
/// `apply_theme()`). Отсутствие любого поля — ошибка (достигается serde).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ColorsData {
    pub bg_window: String,
    pub bg_surface: String,
    pub bg_toolbar: String,
    pub bg_elevated: String,
    pub bg_overlay: String,
    pub border_subtle: String,
    pub border_default: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub text_tertiary: String,
    pub text_dim: String,
    pub text_on_accent: String,
    pub text_error: String,
    pub accent: String,
    pub accent_container: String,
    pub accent_on: String,
    pub surface_hover: String,
    pub surface_active: String,
    pub surface_selected: String,
    pub viz_1: String,
    pub viz_2: String,
    pub viz_3: String,
}

/// Полное описание темы из TOML-файла.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ThemeData {
    /// Отображается под ComboBox выбора темы.
    pub name: String,
    /// `None` → пустая строка в UI.
    pub description: Option<String>,
    pub standard_palette: StandardPalette,
    pub colors: ColorsData,
}

impl ThemeData {
    /// Структурная валидация через serde: наличие всех 22 полей `[colors]` и
    /// полей метаданных. HEX-строки на этом уровне не проверяются (§2.3).
    pub fn load_from_file(path: &Path) -> Result<Self, ThemeError> {
        let text = fs::read_to_string(path)?;
        let data: ThemeData = toml::from_str(&text)?;
        Ok(data)
    }
}

/// Одна найденная тема из `themes/*.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeEntry {
    /// Имя файла без расширения `.toml` (имя темы).
    pub file_stem: String,
    /// `ThemeData::name` (пустая строка, если файл невалиден).
    pub name: String,
    pub description: Option<String>,
    pub valid: bool,
}

/// Ошибки загрузки темы.
#[derive(Debug)]
pub enum ThemeError {
    Io(std::io::Error),
    Deserialize(toml::de::Error),
}

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThemeError::Io(e) => write!(f, "io error: {e}"),
            ThemeError::Deserialize(e) => write!(f, "toml error: {e}"),
        }
    }
}

impl std::error::Error for ThemeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ThemeError::Io(e) => Some(e),
            ThemeError::Deserialize(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for ThemeError {
    fn from(e: std::io::Error) -> Self {
        ThemeError::Io(e)
    }
}

impl From<toml::de::Error> for ThemeError {
    fn from(e: toml::de::Error) -> Self {
        ThemeError::Deserialize(e)
    }
}

/// Разбор HEX-строки `#RRGGBB` / `#AARRGGBB` → `(a, r, g, b)`.
///
/// Возвращает `None` на любой некорректный ввод (длина, не-hex символы,
/// не-ASCII). При 6-значном формате альфа = 255.
pub fn parse_hex(hex: &str) -> Option<(u8, u8, u8, u8)> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let bytes = hex.as_bytes();
    if !bytes.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let val = |i: usize| -> Option<u8> { u8::from_str_radix(&hex[i..i + 2], 16).ok() };
    if hex.len() == 8 {
        let (a, r, g, b) = (val(0)?, val(2)?, val(4)?, val(6)?);
        Some((a, r, g, b))
    } else {
        let (r, g, b) = (val(0)?, val(2)?, val(4)?);
        Some((255, r, g, b))
    }
}

/// Дефолтный шаблон тёмной темы (значения из текущей dark-ветки
/// `apply_theme()`).
pub const DEFAULT_DARK_TOML: &str = r##"name = "Dark"
description = "Системная тёмная тема"
standard_palette = "dark"

[colors]
bg_window = "#121018"
bg_surface = "#1a1720"
bg_toolbar = "#211e28"
bg_elevated = "#252230"
bg_overlay = "#00000088"
border_subtle = "#2d2a38"
border_default = "#3a3645"
text_primary = "#e6e1ec"
text_secondary = "#a9a3b8"
text_tertiary = "#7c7690"
text_dim = "#5c5670"
text_on_accent = "#ffffff"
text_error = "#f2b8b5"
accent = "#d0bcff"
accent_container = "#4f378b"
accent_on = "#eaddff"
surface_hover = "#322e3c"
surface_active = "#3a2f1f"
surface_selected = "#2d2a38"
viz_1 = "#d35400"
viz_2 = "#f1c40f"
viz_3 = "#e74c3c"
"##;

/// Дефолтный шаблон светлой темы (значения из текущей light-ветки
/// `apply_theme()`).
pub const DEFAULT_LIGHT_TOML: &str = r##"name = "Light"
description = "Системная светлая тема"
standard_palette = "light"

[colors]
bg_window = "#f8f5fa"
bg_surface = "#ffffff"
bg_toolbar = "#f3edf7"
bg_elevated = "#ffffff"
bg_overlay = "#00000044"
border_subtle = "#e4dde8"
border_default = "#cac4d0"
text_primary = "#1d1b20"
text_secondary = "#49454f"
text_tertiary = "#79747e"
text_dim = "#938f99"
text_on_accent = "#ffffff"
text_error = "#b3261e"
accent = "#6750a4"
accent_container = "#eaddff"
accent_on = "#21005d"
surface_hover = "#e8e0ec"
surface_active = "#d0c4db"
surface_selected = "#e4dde8"
viz_1 = "#b14a00"
viz_2 = "#c4a00a"
viz_3 = "#c0392b"
"##;

/// Создаёт каталог `themes/` и файлы `dark.toml`/`light.toml`, если их нет.
/// Существующие файлы никогда не перезаписываются (§1, §3).
pub fn create_default_themes(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    for (name, content) in [
        ("dark.toml", DEFAULT_DARK_TOML),
        ("light.toml", DEFAULT_LIGHT_TOML),
    ] {
        let path = dir.join(name);
        if !path.exists() {
            fs::write(&path, content)?;
        }
    }
    Ok(())
}

/// Сканирует только `*.toml` верхнего уровня каталога `themes/`.
/// Вложенные папки игнорируются; сортировка лексикографическая,
/// case-insensitive (§3, §8.1).
pub fn scan_themes_dir(dir: &Path) -> Vec<ThemeEntry> {
    let mut files: Vec<(String, PathBuf)> = match fs::read_dir(dir) {
        Ok(read) => read
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter(|e| e.path().extension().map(|x| x == "toml").unwrap_or(false))
            .map(|e| {
                let path = e.path();
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (stem, path)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort_by_key(|(stem, _)| stem.to_lowercase());
    files
        .into_iter()
        .map(|(stem, path)| entry_for(&stem, &path))
        .collect()
}

fn entry_for(stem: &str, path: &Path) -> ThemeEntry {
    match ThemeData::load_from_file(path) {
        Ok(data) => ThemeEntry {
            file_stem: stem.to_owned(),
            name: data.name,
            description: data.description,
            valid: true,
        },
        Err(_) => ThemeEntry {
            file_stem: stem.to_owned(),
            name: String::new(),
            description: None,
            valid: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mp_theme_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_load_default_dark() {
        let dir = temp_dir("load_dark");
        create_default_themes(&dir).unwrap();
        let data = ThemeData::load_from_file(&dir.join("dark.toml")).unwrap();
        assert_eq!(data.name, "Dark");
        assert_eq!(data.standard_palette, StandardPalette::Dark);
        assert_eq!(data.colors.accent, "#d0bcff");
        assert_eq!(data.description.as_deref(), Some("Системная тёмная тема"));
    }

    #[test]
    fn test_load_default_light() {
        let dir = temp_dir("load_light");
        create_default_themes(&dir).unwrap();
        let data = ThemeData::load_from_file(&dir.join("light.toml")).unwrap();
        assert_eq!(data.name, "Light");
        assert_eq!(data.standard_palette, StandardPalette::Light);
        assert_eq!(data.colors.text_primary, "#1d1b20");
    }

    #[test]
    fn test_missing_color_field() {
        let dir = temp_dir("missing_field");
        fs::create_dir_all(&dir).unwrap();
        let broken = DEFAULT_LIGHT_TOML.replace("viz_3 = \"#c0392b\"\n", "");
        fs::write(dir.join("broken.toml"), broken).unwrap();
        assert!(ThemeData::load_from_file(&dir.join("broken.toml")).is_err());
    }

    #[test]
    fn test_scan_themes_dir_ignores_subdirs() {
        let dir = temp_dir("scan");
        create_default_themes(&dir).unwrap();
        fs::write(dir.join("custom.toml"), DEFAULT_LIGHT_TOML).unwrap();
        fs::write(dir.join("readme.txt"), "not a theme").unwrap();
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested").join("hidden.toml"), DEFAULT_DARK_TOML).unwrap();

        let entries = scan_themes_dir(&dir);
        let stems: Vec<&str> = entries.iter().map(|e| e.file_stem.as_str()).collect();
        assert_eq!(stems, vec!["custom", "dark", "light"]);
        assert!(entries.iter().all(|e| e.valid));
    }

    #[test]
    fn test_create_default_themes_no_overwrite() {
        let dir = temp_dir("no_overwrite");
        fs::create_dir_all(&dir).unwrap();
        let custom = r##"name = "MyDark"
standard_palette = "dark"
[colors]
bg_window = "#112233"
"##;
        // intentional: не хватает полей — проверяем только что файл не тронут.
        fs::write(dir.join("dark.toml"), custom).unwrap();
        create_default_themes(&dir).unwrap();
        create_default_themes(&dir).unwrap();
        assert_eq!(fs::read_to_string(dir.join("dark.toml")).unwrap(), custom);
        assert!(dir.join("light.toml").exists());
        assert!(ThemeData::load_from_file(&dir.join("light.toml")).is_ok());
    }

    #[test]
    fn test_parse_hex_valid() {
        assert_eq!(parse_hex("#121018"), Some((255, 0x12, 0x10, 0x18)));
        assert_eq!(parse_hex("112233"), Some((255, 0x11, 0x22, 0x33)));
        assert_eq!(parse_hex("#00000088"), Some((0x00, 0x00, 0x00, 0x88)));
        assert_eq!(parse_hex("aaff0000"), Some((0xaa, 0xff, 0x00, 0x00)));
        assert_eq!(parse_hex("#FFFFFF"), Some((255, 0xff, 0xff, 0xff)));
    }

    #[test]
    fn test_parse_hex_invalid() {
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#"), None);
        assert_eq!(parse_hex("#12345"), None);
        assert_eq!(parse_hex("#1234567"), None);
        assert_eq!(parse_hex("#gggggg"), None);
        assert_eq!(parse_hex("#12345g"), None);
        assert_eq!(parse_hex("ФфФФФФ"), None);
    }
}