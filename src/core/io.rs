//! Поток `apap-io` (ADR-20): чтение тем, подсчёт и очистка дисковых кэшей
//! вне UI-потока; поколение на вид задания, ответы разбираются на тике
//! 100 мс (§2.12, ТЗ-22, ТЗ-34).

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::persist::settings_file::ThemeName;
use crate::theme::{ThemeData, ThemeEntry};

/// Задание потоку `apap-io` (§2.12, ADR-20, ТЗ-22, ТЗ-34).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IoJob {
    /// Чтение файла темы при открытии диалога и при выборе темы (ТЗ-22).
    ReadTheme { gen: u64, name: ThemeName },
    /// ОТКЛОНЕНИЕ от §2.12: список тем из `themes/` нужен диалогу настроек
    /// без обхода каталога в UI-потоке (ТЗ-22); в спецификации задания нет.
    ListThemes { gen: u64 },
    /// Подсчёт размера дисковых кэшей при открытии диалога и после очистки
    /// (ТЗ-22).
    CacheSizes { gen: u64 },
    /// Очистка дисковых кэшей вне UI-потока (ТЗ-34).
    ClearCache { gen: u64, which: CacheKind },
}

impl IoJob {
    /// Поколение задания — для сопоставления с ответом на тике (ADR-20).
    pub fn gen(&self) -> u64 {
        match self {
            IoJob::ReadTheme { gen, .. }
            | IoJob::ListThemes { gen }
            | IoJob::CacheSizes { gen }
            | IoJob::ClearCache { gen, .. } => *gen,
        }
    }
}

/// Ответ потока `apap-io`, разбираемый на тике 100 мс (§2.12, ADR-20).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IoDone {
    /// Результат `IoJob::ReadTheme` (ТЗ-22, ТЗ-27, ТЗ-29).
    Theme { gen: u64, result: Result<Arc<ThemeData>, Box<str>> },
    /// Результат `IoJob::ListThemes` — ОТКЛОНЕНИЕ от §2.12, см. `IoJob`.
    Themes { gen: u64, entries: Vec<ThemeEntry> },
    /// Результат `IoJob::CacheSizes` (ТЗ-22).
    CacheSizes { gen: u64, sizes: CacheSizes },
    /// Результат `IoJob::ClearCache` — новый размер после очистки (ТЗ-34).
    Cleared { gen: u64, sizes: CacheSizes },
}

impl IoDone {
    /// Поколение задания, которому отвечает этот результат (ADR-20).
    pub fn gen(&self) -> u64 {
        match self {
            IoDone::Theme { gen, .. }
            | IoDone::Themes { gen, .. }
            | IoDone::CacheSizes { gen, .. }
            | IoDone::Cleared { gen, .. } => *gen,
        }
    }
}

/// Какие дисковые кэши очищать (`IoJob::ClearCache`, ТЗ-34).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheKind {
    Visualization,
    Covers,
    All,
}

/// Размер дисковых кэшей в байтах (ТЗ-22). RAM-кэши считаются в UI-потоке,
/// сюда не входят.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheSizes {
    pub viz_disk: u64,
    pub covers: u64,
}

/// Канал заданий/ответов потока `apap-io` (§2.12, ADR-20).
pub trait IoWorker {
    /// Поставить задание в очередь потока `apap-io`.
    fn submit(&self, job: IoJob);
    /// Забрать готовый ответ без блокировки; `None` — ответа пока нет.
    fn try_recv(&self) -> Option<IoDone>;
}

/// Инжектированные каталоги потока `apap-io` (ADR-20, §2.12): каталог тем и
/// дисковые кэши обложек/визуализации. `main` передаёт реальные XDG-пути,
/// тесты — временные каталоги.
#[derive(Clone, Debug)]
pub struct IoPaths {
    pub themes: PathBuf,
    pub covers: PathBuf,
    pub viz: PathBuf,
}

/// Запускает именованный поток `apap-io` (ADR-20, ТЗ-22, ТЗ-34): читает
/// задания из канала `IoJob`, отвечает через канал `IoDone`. Поток
/// завершается, когда отправитель заданий (поле `tx` у `IoThread`) роняется.
pub fn spawn_io(paths: IoPaths) -> std::io::Result<IoThread> {
    let (job_tx, job_rx) = mpsc::channel::<IoJob>();
    let (done_tx, done_rx) = mpsc::channel::<IoDone>();
    let join = std::thread::Builder::new()
        .name("apap-io".into())
        .spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let done = handle(&paths, job);
                if done_tx.send(done).is_err() {
                    // UI-поток ушёл — приёмник ответов уже сброшен.
                    break;
                }
            }
        })?;
    Ok(IoThread { tx: Some(job_tx), rx: done_rx, join: Some(join) })
}

/// Хендл потока `apap-io` (ADR-20, §2.12): владеет каналами заданий/ответов
/// и `JoinHandle`. `tx` — `Option`, чтобы `Drop` мог явно его сбросить и
/// разбудить `recv()` в потоке перед `join()`.
pub struct IoThread {
    tx: Option<Sender<IoJob>>,
    rx: Receiver<IoDone>,
    join: Option<JoinHandle<()>>,
}

impl IoWorker for IoThread {
    fn submit(&self, job: IoJob) {
        // Поток мог уже завершиться (паника/Drop) — не паникуем на отправке.
        if let Some(tx) = &self.tx {
            let _ = tx.send(job);
        }
    }

    fn try_recv(&self) -> Option<IoDone> {
        self.rx.try_recv().ok()
    }
}

impl Drop for IoThread {
    fn drop(&mut self) {
        // Роняем отправителя заданий — `job_rx.recv()` в потоке вернёт `Err`,
        // поток выйдет из цикла и завершится. Работа потока — короткие
        // файловые операции, поэтому `join()` здесь ограничен по времени.
        self.tx = None;
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Выполняет одно задание потока `apap-io` над инжектированными путями
/// (ADR-20, §2.12). Чистая функция — тестируется без реального потока.
fn handle(paths: &IoPaths, job: IoJob) -> IoDone {
    match job {
        IoJob::ReadTheme { gen, name } => {
            let path = paths.themes.join(format!("{}.toml", name.as_str()));
            let result = match ThemeData::load_from_file(&path) {
                Ok(data) => Ok(Arc::new(data)),
                Err(e) => Err(e.to_string().into_boxed_str()),
            };
            IoDone::Theme { gen, result }
        }
        IoJob::ListThemes { gen } => {
            let entries = crate::theme::scan_themes_dir(&paths.themes);
            IoDone::Themes { gen, entries }
        }
        IoJob::CacheSizes { gen } => IoDone::CacheSizes { gen, sizes: cache_sizes(paths) },
        IoJob::ClearCache { gen, which } => {
            match which {
                CacheKind::Visualization => {
                    crate::audio::fulltrack::clear_disk_cache_in(&paths.viz);
                }
                CacheKind::Covers => {
                    crate::cover::clear_cover_cache_in(&paths.covers);
                }
                CacheKind::All => {
                    crate::audio::fulltrack::clear_disk_cache_in(&paths.viz);
                    crate::cover::clear_cover_cache_in(&paths.covers);
                }
            }
            IoDone::Cleared { gen, sizes: cache_sizes(paths) }
        }
    }
}

/// Текущий размер дисковых кэшей по инжектированным путям (ТЗ-22).
fn cache_sizes(paths: &IoPaths) -> CacheSizes {
    CacheSizes {
        viz_disk: crate::audio::fulltrack::disk_cache_size_in(&paths.viz),
        covers: crate::cover::cover_cache_size_in(&paths.covers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Временные каталоги тем/обложек/визуализации для одного теста;
    /// уникальный тег + pid процесса — изоляция между тестами и прогонами
    /// (приём из `src/theme.rs`/`src/cover.rs`).
    fn temp_paths(tag: &str) -> IoPaths {
        let base = std::env::temp_dir().join(format!("mp_core_io_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        IoPaths { themes: base.join("themes"), covers: base.join("covers"), viz: base.join("viz") }
    }

    #[test]
    fn core_io_reads_theme() {
        let paths = temp_paths("read_theme");
        fs::create_dir_all(&paths.themes).expect("create themes dir");
        fs::write(paths.themes.join("dark.toml"), crate::theme::DEFAULT_DARK_TOML)
            .expect("write theme file");
        let name = ThemeName::new("dark").expect("valid theme name");

        let done = handle(&paths, IoJob::ReadTheme { gen: 1, name });

        let IoDone::Theme { gen, result } = done else { panic!("expected Theme") };
        assert_eq!(gen, 1);
        let data = result.expect("theme should load");
        assert_eq!(data.name, "Dark");
    }

    #[test]
    fn core_io_read_theme_error_for_missing() {
        let paths = temp_paths("read_theme_missing");
        fs::create_dir_all(&paths.themes).expect("create themes dir");
        let name = ThemeName::new("ghost").expect("valid theme name");

        let done = handle(&paths, IoJob::ReadTheme { gen: 2, name });

        let IoDone::Theme { gen, result } = done else { panic!("expected Theme") };
        assert_eq!(gen, 2);
        assert!(result.is_err());
    }

    #[test]
    fn core_io_lists_themes() {
        let paths = temp_paths("list_themes");
        fs::create_dir_all(&paths.themes).expect("create themes dir");
        fs::write(paths.themes.join("dark.toml"), crate::theme::DEFAULT_DARK_TOML)
            .expect("write dark theme");
        fs::write(paths.themes.join("light.toml"), crate::theme::DEFAULT_LIGHT_TOML)
            .expect("write light theme");

        let done = handle(&paths, IoJob::ListThemes { gen: 3 });

        let IoDone::Themes { gen, entries } = done else { panic!("expected Themes") };
        assert_eq!(gen, 3);
        let stems: Vec<&str> = entries.iter().map(|e| e.file_stem.as_str()).collect();
        assert_eq!(stems, vec!["dark", "light"]);
    }

    #[test]
    fn core_io_counts_cache_sizes() {
        let paths = temp_paths("cache_sizes");
        fs::create_dir_all(&paths.covers).expect("create covers dir");
        fs::create_dir_all(&paths.viz).expect("create viz dir");
        fs::write(paths.covers.join("a.jpg"), vec![0u8; 100]).expect("write cover file");
        fs::write(paths.viz.join("b.png"), vec![0u8; 50]).expect("write viz file");

        let done = handle(&paths, IoJob::CacheSizes { gen: 4 });

        let IoDone::CacheSizes { gen, sizes } = done else { panic!("expected CacheSizes") };
        assert_eq!(gen, 4);
        assert_eq!(sizes.covers, 100);
        assert_eq!(sizes.viz_disk, 50);
    }

    #[test]
    fn core_io_clear_cache_kinds() {
        let viz_paths = temp_paths("clear_viz");
        fs::create_dir_all(&viz_paths.covers).expect("create covers dir");
        fs::create_dir_all(&viz_paths.viz).expect("create viz dir");
        fs::write(viz_paths.covers.join("a.jpg"), vec![0u8; 10]).expect("write cover file");
        fs::write(viz_paths.viz.join("b.png"), vec![0u8; 10]).expect("write viz file");
        let done =
            handle(&viz_paths, IoJob::ClearCache { gen: 5, which: CacheKind::Visualization });
        let IoDone::Cleared { gen, sizes } = done else { panic!("expected Cleared") };
        assert_eq!(gen, 5);
        assert_eq!(sizes.viz_disk, 0);
        assert_eq!(sizes.covers, 10);

        let covers_paths = temp_paths("clear_covers");
        fs::create_dir_all(&covers_paths.covers).expect("create covers dir");
        fs::create_dir_all(&covers_paths.viz).expect("create viz dir");
        fs::write(covers_paths.covers.join("a.jpg"), vec![0u8; 10]).expect("write cover file");
        fs::write(covers_paths.viz.join("b.png"), vec![0u8; 10]).expect("write viz file");
        let done = handle(&covers_paths, IoJob::ClearCache { gen: 6, which: CacheKind::Covers });
        let IoDone::Cleared { gen, sizes } = done else { panic!("expected Cleared") };
        assert_eq!(gen, 6);
        assert_eq!(sizes.covers, 0);
        assert_eq!(sizes.viz_disk, 10);

        let all_paths = temp_paths("clear_all");
        fs::create_dir_all(&all_paths.covers).expect("create covers dir");
        fs::create_dir_all(&all_paths.viz).expect("create viz dir");
        fs::write(all_paths.covers.join("a.jpg"), vec![0u8; 10]).expect("write cover file");
        fs::write(all_paths.viz.join("b.png"), vec![0u8; 10]).expect("write viz file");
        let done = handle(&all_paths, IoJob::ClearCache { gen: 7, which: CacheKind::All });
        let IoDone::Cleared { gen, sizes } = done else { panic!("expected Cleared") };
        assert_eq!(gen, 7);
        assert_eq!(sizes.covers, 0);
        assert_eq!(sizes.viz_disk, 0);
    }

    #[test]
    fn core_io_thread_roundtrip() {
        let paths = temp_paths("thread_roundtrip");
        fs::create_dir_all(&paths.themes).expect("create themes dir");
        fs::create_dir_all(&paths.covers).expect("create covers dir");
        fs::create_dir_all(&paths.viz).expect("create viz dir");

        let thread = spawn_io(paths).expect("spawn_io should succeed");
        thread.submit(IoJob::CacheSizes { gen: 7 });

        let mut done = None;
        for _ in 0..400 {
            if let Some(d) = thread.try_recv() {
                done = Some(d);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let done = done.expect("io thread should respond within 2s");
        assert_eq!(done.gen(), 7);

        drop(thread);
    }
}
