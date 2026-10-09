//! Загрузка плейлиста: типы потока `apap-playlist` и чистые парсеры трёх
//! случаев ОВ-8 (ADR-5, ADR-16, §6.12, ТЗ-21).

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use super::compare::{compare_keys, CompareKeys};
use super::model::SortKey;
use super::{is_supported_audio, track_for_path, Track};
use crate::platform::fs::{FileReader, ReadError, ReadErrorClass};

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

/// Выполняет задание `job` потока `apap-playlist`: читает файл по
/// источнику, разбирает его (`parse_startup`/`parse_command`),
/// последовательно опрашивает теги каждого пути через `probe` и строит
/// видимый порядок по `job.sort` (ADR-16, §6.12, ТЗ-21, ТЗ-47).
pub fn run_load(job: &LoadJob, reader: &dyn FileReader, probe: &dyn Fn(&Path) -> Track) -> LoadOutcome {
    let gen = job.gen;
    let path: &Path = match &job.source {
        LoadSource::Startup(p) | LoadSource::Command(p) => p,
    };

    let bytes = match reader.read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return match (&job.source, err.class) {
                (LoadSource::Startup(_), ReadErrorClass::NotFound) => LoadOutcome::StartupAbsent { gen },
                _ => LoadOutcome::ReadFailed { gen, err },
            };
        }
    };

    let paths = match &job.source {
        LoadSource::Startup(_) => match parse_startup(&bytes) {
            Ok(paths) => paths,
            Err(reason) => return LoadOutcome::StartupCorrupt { gen, reason },
        },
        LoadSource::Command(_) => match parse_command(&bytes) {
            Some(paths) => paths,
            // Невалидный UTF-8 в выбранном файле — не случай ОВ-8, просто
            // ошибка чтения (§6.12, ТЗ-13 б).
            None => {
                let err = ReadError {
                    class: ReadErrorClass::Io,
                    os_code: None,
                    os_text: "невалидный UTF-8".into(),
                    path: path.to_path_buf(),
                };
                return LoadOutcome::ReadFailed { gen, err };
            }
        },
    };

    let rows: Vec<(Track, CompareKeys)> = paths
        .iter()
        .map(|p| {
            let track = probe(p);
            let keys = CompareKeys::from_track(&track);
            (track, keys)
        })
        .collect();

    // Видимый порядок строится тем же сравнением, что и пересортировка
    // плейлиста (`compare_keys`, §2.7); сортировка индексов стабильна, так
    // что совпадающие ключи сохраняют исходный порядок.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    if let Some(key) = job.sort {
        order.sort_by(|&a, &b| compare_keys(&rows[a].1, &rows[b].1, key));
    }
    let visible: Vec<u32> = order.into_iter().filter_map(|i| u32::try_from(i).ok()).collect();

    LoadOutcome::Loaded { gen, rows, visible }
}

/// Запускает загрузку плейлиста в потоке `apap-playlist`; результат
/// приходит в `done` (ADR-16, §6.12).
pub trait PlaylistLoader {
    /// Запускает загрузку; результат — в done.
    fn start(&self, job: LoadJob, reader: Arc<dyn FileReader>, done: Sender<LoadOutcome>);
}

/// Загрузка в отдельном потоке `apap-playlist` (ADR-16, §6.12).
pub struct ThreadLoader;

impl PlaylistLoader for ThreadLoader {
    fn start(&self, job: LoadJob, reader: Arc<dyn FileReader>, done: Sender<LoadOutcome>) {
        let gen = job.gen;
        let path = match &job.source {
            LoadSource::Startup(p) | LoadSource::Command(p) => p.clone(),
        };
        let done_ok = done.clone();
        let spawned = thread::Builder::new().name("apap-playlist".into()).spawn(move || {
            let outcome = run_load(&job, reader.as_ref(), &track_for_path);
            // Получатель мог быть отброшен (приложение завершается) — не паникуем.
            let _ = done_ok.send(outcome);
        });
        if spawned.is_err() {
            // Поток не создан: без ответа `UiGate` держал бы загрузку вечно
            // (ТЗ-48) — отдаём ReadFailed класса Io вместо тишины.
            let err = ReadError {
                class: ReadErrorClass::Io,
                os_code: None,
                os_text: "не удалось создать поток apap-playlist".into(),
                path,
            };
            let _ = done.send(LoadOutcome::ReadFailed { gen, err });
        }
    }
}

/// Ожидающее завершения задание и отправитель его результата.
type PendingJob = (LoadJob, Sender<LoadOutcome>);

/// Подмена `PlaylistLoader` для тестов (ADR-16 «подмена»): загрузка не
/// начинается, пока тест не вызовет [`ManualLoader::finish`]; чтобы
/// смоделировать «зависшую» загрузку (ТЗ-17), просто не вызывать `finish`
/// для этого задания.
#[derive(Clone, Default)]
pub struct ManualLoader {
    jobs: Arc<Mutex<Vec<PendingJob>>>,
}

impl PlaylistLoader for ManualLoader {
    fn start(&self, job: LoadJob, _reader: Arc<dyn FileReader>, done: Sender<LoadOutcome>) {
        self.lock().push((job, done));
    }
}

impl ManualLoader {
    /// Ожидающие завершения задания (клон, очередь не меняется).
    pub fn jobs(&self) -> Vec<LoadJob> {
        self.lock().iter().map(|(job, _)| job.clone()).collect()
    }

    /// Завершает ожидающее задание с `gen == outcome.gen()`: убирает его из
    /// очереди и отправляет `outcome` его отправителю. `false` — такого
    /// задания нет.
    pub fn finish(&self, outcome: LoadOutcome) -> bool {
        let mut jobs = self.lock();
        let Some(i) = jobs.iter().position(|(job, _)| job.gen == outcome.gen()) else {
            return false;
        };
        let (_, done) = jobs.remove(i);
        let _ = done.send(outcome);
        true
    }

    fn lock(&self) -> MutexGuard<'_, Vec<PendingJob>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::MemStore;
    use super::super::model::{SortColumn, SortDir};

    fn job(gen: LoadGen, source: LoadSource, sort: Option<SortKey>) -> LoadJob {
        LoadJob { gen, source, sort }
    }

    fn test_probe(path: &Path) -> Track {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Track { path: path.to_path_buf(), title: stem, ..Track::default() }
    }

    #[test]
    fn run_load_startup_missing_is_absent() {
        let reader = MemStore::new();
        let path = abs("missing-playlist.m3u");
        let j = job(LoadGen::default(), LoadSource::Startup(path), None);
        let out = run_load(&j, &reader, &test_probe);
        assert!(matches!(out, LoadOutcome::StartupAbsent { gen } if gen == j.gen));
    }

    #[test]
    fn run_load_startup_read_error_is_read_failed() {
        let reader = MemStore::new();
        let path = abs("noaccess-playlist.m3u");
        reader.fail_read(&path, ReadErrorClass::NoAccess);
        let j = job(LoadGen::default(), LoadSource::Startup(path.clone()), None);
        let out = run_load(&j, &reader, &test_probe);
        match out {
            LoadOutcome::ReadFailed { gen, err } => {
                assert_eq!(gen, j.gen);
                assert_eq!(err.class, ReadErrorClass::NoAccess);
                assert_eq!(err.path, path);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn run_load_startup_corrupt() {
        let reader = MemStore::new();
        let path = abs("corrupt-playlist.m3u");
        reader.put(&path, b"relative/path.flac\n");
        let j = job(LoadGen::default(), LoadSource::Startup(path), None);
        let out = run_load(&j, &reader, &test_probe);
        match out {
            LoadOutcome::StartupCorrupt { gen, reason } => {
                assert_eq!(gen, j.gen);
                assert!(reason.contains("строка 1"), "unexpected reason: {reason}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn run_load_startup_nonexistent_paths_loaded() {
        let reader = MemStore::new();
        let playlist_path = abs("startup-playlist.m3u");
        let a = abs("does-not-exist-a.flac");
        let b = abs("does-not-exist-b.flac");
        let text = format!("{}\n{}\n", a.display(), b.display());
        reader.put(&playlist_path, text.as_bytes());
        let j = job(LoadGen::default(), LoadSource::Startup(playlist_path), None);
        let out = run_load(&j, &reader, &test_probe);
        match out {
            LoadOutcome::Loaded { gen, rows, visible } => {
                assert_eq!(gen, j.gen);
                let paths: Vec<PathBuf> = rows.iter().map(|(t, _)| t.path.clone()).collect();
                assert_eq!(paths, vec![a, b]);
                assert_eq!(visible, vec![0, 1]);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn run_load_sorts_by_job_key() {
        let reader = MemStore::new();
        let playlist_path = abs("sort-playlist.m3u");
        let zzz = abs("zzz.flac");
        let aaa = abs("aaa.flac");
        let text = format!("{}\n{}\n", zzz.display(), aaa.display());
        reader.put(&playlist_path, text.as_bytes());

        let key = SortKey { column: SortColumn::Title, dir: SortDir::Asc };
        let j = job(LoadGen::default(), LoadSource::Startup(playlist_path.clone()), Some(key));
        let out = run_load(&j, &reader, &test_probe);
        match out {
            LoadOutcome::Loaded { rows, visible, .. } => {
                assert_eq!(visible, vec![1, 0]);
                assert_eq!(rows[1].0.path, aaa);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }

        let j_unsorted = job(j.gen, LoadSource::Startup(playlist_path), None);
        let out = run_load(&j_unsorted, &reader, &test_probe);
        match out {
            LoadOutcome::Loaded { visible, .. } => assert_eq!(visible, vec![0, 1]),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn run_load_command_invalid_utf8_is_read_failed() {
        let reader = MemStore::new();
        let path = abs("invalid-utf8.m3u");
        reader.put(&path, &[0xff, 0xfe, 0xfd]);
        let j = job(LoadGen::default(), LoadSource::Command(path.clone()), None);
        let out = run_load(&j, &reader, &test_probe);
        match out {
            LoadOutcome::ReadFailed { gen, err } => {
                assert_eq!(gen, j.gen);
                assert_eq!(err.class, ReadErrorClass::Io);
                assert_eq!(err.path, path);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn manual_loader_finish_delivers_only_matching_gen() {
        let loader = ManualLoader::default();
        let gen0 = LoadGen::default();
        let gen1 = gen0.next();
        let (tx0, rx0) = std::sync::mpsc::channel();
        let (tx1, rx1) = std::sync::mpsc::channel();
        let reader: Arc<dyn FileReader> = Arc::new(MemStore::new());
        loader.start(job(gen0, LoadSource::Startup(abs("a.m3u")), None), reader.clone(), tx0);
        loader.start(job(gen1, LoadSource::Startup(abs("b.m3u")), None), reader, tx1);
        assert_eq!(loader.jobs().len(), 2);

        assert!(loader.finish(LoadOutcome::StartupAbsent { gen: gen1 }));
        assert!(rx1.try_recv().is_ok());
        assert!(rx0.try_recv().is_err());
        let remaining = loader.jobs();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].gen, gen0);

        assert!(!loader.finish(LoadOutcome::StartupAbsent { gen: gen1 }));
    }

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
