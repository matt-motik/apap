//! Загрузка плейлиста: типы потока `apap-playlist` и чистые парсеры трёх
//! случаев ОВ-8 (ADR-5, ADR-16, §6.12, ТЗ-21).

use std::path::{Path, PathBuf};

use super::compare::CompareKeys;
use super::model::SortKey;
use super::{is_supported_audio, Track};
use crate::platform::fs::ReadError;

/// Поколение загрузки плейлиста (ADR-16).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct LoadGen(u64);

impl LoadGen {
    /// Следующее поколение; переполнение не паникует (ADR-16 — счётчик, не идентификатор).
    pub fn next(self) -> LoadGen {
        LoadGen(self.0.checked_add(1).unwrap_or(0))
    }
}

/// Источник загрузки: стартовый файл или файл, выбранный командой (§6.12).
#[derive(Clone, PartialEq, Debug)]
pub enum LoadSource {
    Startup(PathBuf),
    Command(PathBuf),
}

/// Задание потоку `apap-playlist` (§6.12).
#[derive(Clone, PartialEq, Debug)]
pub struct LoadJob {
    pub gen: LoadGen,
    pub source: LoadSource,
    /// Действующий ключ на момент запуска загрузки.
    pub sort: Option<SortKey>,
}

/// Результат загрузки, отправляемый потоком `apap-playlist` в UI (§6.12).
#[derive(Clone, Debug)]
pub enum LoadOutcome {
    Loaded {
        gen: LoadGen,
        rows: Vec<(Track, CompareKeys)>,
        visible: Vec<u32>,
    },
    /// ТЗ-21, случай 1.
    StartupAbsent { gen: LoadGen },
    /// ТЗ-21, случай 2: причина для журнала.
    StartupCorrupt { gen: LoadGen, reason: Box<str> },
    /// ТЗ-21, случай 3; для Command — ошибка чтения выбранного файла (ТЗ-13 б).
    ReadFailed { gen: LoadGen, err: ReadError },
}

impl LoadOutcome {
    pub fn gen(&self) -> LoadGen {
        match self {
            LoadOutcome::Loaded { gen, .. } => *gen,
            LoadOutcome::StartupAbsent { gen } => *gen,
            LoadOutcome::StartupCorrupt { gen, .. } => *gen,
            LoadOutcome::ReadFailed { gen, .. } => *gen,
        }
    }
}

/// Снимает один завершающий `\r` (допуск CRLF, не новый формат — ADR-5).
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// Разбор стартового `playlist.m3u` (ОВ-8, ADR-5, §6.12): ведущий UTF-8 BOM
/// игнорируется, разделитель строк — `\n`, завершающий `\r` отбрасывается.
/// Каждая строка должна быть пустой, комментарием (`#…`) или абсолютным
/// путём — иначе случай 2 (`Err` с текстом для журнала). Существование файла
/// по пути не проверяется. Непустой абсолютный путь неподдерживаемого
/// формата — не повреждение, просто пропускается; повторы отбрасываются,
/// остаётся первое вхождение.
pub fn parse_startup(bytes: &[u8]) -> Result<Vec<PathBuf>, Box<str>> {
    let text = std::str::from_utf8(bytes).map_err(|_| Box::from("невалидный UTF-8"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (idx, raw_line) in text.split('\n').enumerate() {
        let line = strip_cr(raw_line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = Path::new(line);
        if !path.is_absolute() {
            return Err(format!("строка {}: не абсолютный путь", idx + 1).into());
        }
        if is_supported_audio(path) && seen.insert(path.to_path_buf()) {
            out.push(path.to_path_buf());
        }
    }
    Ok(out)
}

/// Разбор файла, выбранного командой «Загрузить плейлист» (§6.12): ОВ-8 к
/// нему не применяется. `None` — невалидный UTF-8 (вызывающий отображает
/// `ReadFailed`, класс `Io`). BOM и `\r` допускаются как в `parse_startup`.
/// Строки, не являющиеся абсолютным путём к поддерживаемому файлу,
/// пропускаются молча; повторы отбрасываются, остаётся первое вхождение.
pub fn parse_command(bytes: &[u8]) -> Option<Vec<PathBuf>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        let line = strip_cr(raw_line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = Path::new(line);
        if path.is_absolute() && is_supported_audio(path) && seen.insert(path.to_path_buf()) {
            out.push(path.to_path_buf());
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn load_command_skips_unsupported_lines() {
        let a = abs("a.flac");
        let b = abs("b.flac");
        let text = format!(
            "# comment\n;comment\nrelative/path.flac\n{}\n{}\n{}\nnote.txt\n",
            a.display(),
            b.display(),
            a.display(),
        );
        let out = parse_command(text.as_bytes()).expect("valid utf8");
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn startup_parse_relative_path_is_corrupt() {
        let err = parse_startup(b"relative/path.flac\n").unwrap_err();
        assert!(err.contains("строка 1"), "unexpected reason: {err}");
    }

    #[test]
    fn startup_parse_semicolon_line_is_corrupt() {
        let err = parse_startup(b";comment\n").unwrap_err();
        assert!(err.contains("строка 1"), "unexpected reason: {err}");
    }

    #[test]
    fn startup_parse_invalid_utf8_is_corrupt() {
        let err = parse_startup(&[0xff, 0xfe, 0xfd]).unwrap_err();
        assert!(err.contains("UTF-8"), "unexpected reason: {err}");
    }

    #[test]
    fn startup_parse_nonexistent_absolute_paths_ok() {
        let p = abs("does-not-exist-startup.flac");
        let text = format!("{}\n", p.display());
        let out = parse_startup(text.as_bytes()).expect("ok");
        assert_eq!(out, vec![p]);
    }

    #[test]
    fn startup_parse_bom_and_crlf_tolerated() {
        let p = abs("bom-crlf.flac");
        let mut text = String::from("\u{feff}");
        text.push_str("# header\r\n");
        text.push_str(&p.display().to_string());
        text.push_str("\r\n");
        let out = parse_startup(text.as_bytes()).expect("ok");
        assert_eq!(out, vec![p]);
    }

    #[test]
    fn command_parse_invalid_utf8_is_none() {
        assert!(parse_command(&[0xff, 0xfe, 0xfd]).is_none());
    }
}
