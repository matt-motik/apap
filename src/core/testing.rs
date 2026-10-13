//! Стенд С3/С4 для тестов `AppCore` без Slint (§7.1, §7.2, ADR-19).
//!
//! `Harness` оборачивает `MemStore` и воссоздаёт порядок `main` для С3
//! (ADR-23 шаги 0–2, §8 С3): чтение и разбор обоих файлов (`persist::boot`),
//! затем копии `*.bad` до появления окна — на стенде пишутся прямо в
//! `MemStore` (детерминированно, без ожидания ответа писателя по часам).
//! Дальше стенд собирает `AppDeps` (ADR-19, §2.12): писатель `apap-persist`
//! через `spawn_writer` на той же `MemStore`, инжектируемые `ManualClock` и
//! `ManualWaiter` (ожидание ответа без сна по симулированному времени,
//! §6.10) и общий `Arc<VecJournal>` — и строит `AppCore::with_deps`.
//!
//! `advance`/`settle` поверх `tick` продвигают `ManualClock` и дожидаются
//! ответа писателя (§7.1).

use super::messages::Message;
use super::{AppCore, AppDeps, EngineSink, TickOutput};
use crate::audio::clock::{Clock, ClockInstant};
use crate::audio::testing::ManualClock;
use crate::core::exit::{ManualWaiter, ReplyWaiter};
use crate::core::io::{CacheKind, CacheSizes, IoDone, IoJob, IoWorker};
use crate::engine::messages::EngineCmd;
use crate::journal::{Journal, JournalRecord, VecJournal};
use crate::persist::keys::Parsed;
use crate::persist::settings_file::ThemeName;
use crate::persist::tracker::ReplyEffect;
use crate::persist::writer::spawn_writer;
use crate::persist::{self, BadCopyOutcome, ConfigFile, ConfigPaths, WorkFile};
use crate::platform::fs::{FileReader, FileWriter, FsCall, MemStore, OpCounts, ReadErrorClass, WriteErrorClass, WriteStep};
use crate::playlist::load::{run_load, ManualLoader, PlaylistLoader};
use crate::playlist::Track;
use crate::theme::{ThemeData, ThemeEntry};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Фиктивный `EngineSink` стенда (§2.12, ADR-01): команды не уходят
/// никакому движку, только копятся в общем журнале для проверки в тестах.
/// `EngineCmd` не `Clone` (несёт `Arc<Path>`/`rtrb::Producer`) — записывается
/// текст `Debug`, этого достаточно, чтобы различать команды в assert'ах.
struct RecordingEngine {
    log: Rc<RefCell<Vec<String>>>,
}

impl EngineSink for RecordingEngine {
    fn send(&self, cmd: EngineCmd) {
        self.log.borrow_mut().push(format!("{cmd:?}"));
    }
}

/// Каталог настроек стенда — не каталог пользователя (ТЗ-49).
fn test_dir() -> PathBuf {
    PathBuf::from("/cfg")
}

pub(crate) struct Harness {
    fs: MemStore,
    paths: ConfigPaths,
    journal: Arc<VecJournal>,
    clock: ManualClock,
    engine_log: Rc<RefCell<Vec<String>>>,
    /// Загрузчик плейлиста стенда (ADR-16, §6.12): задания копятся здесь,
    /// тесты разрешают их вызовом `loader.finish(...)`.
    pub(crate) loader: ManualLoader,
    /// Фиктивный поток `apap-io` стенда (§7.1, ADR-20, §2.12): клон уходит
    /// в `AppDeps.io` при каждом `boot`, состояние общее — тесты опрашивают
    /// журнал заданий и задают ответы через это поле.
    pub(crate) io: FakeIo,
}

impl Harness {
    /// Пустой каталог: оба файла отсутствуют, пока не вызван `put_*`.
    pub(crate) fn new() -> Harness {
        Harness {
            fs: MemStore::new(),
            paths: ConfigPaths::in_dir(test_dir()),
            journal: Arc::new(VecJournal::default()),
            clock: ManualClock::new(),
            engine_log: Rc::new(RefCell::new(Vec::new())),
            loader: ManualLoader::default(),
            io: FakeIo::new(),
        }
    }

    /// Положить байты `settings.toml` «на диск» до запуска.
    pub(crate) fn put_settings(&self, bytes: &[u8]) {
        self.fs.put(&self.paths.settings, bytes);
    }

    /// Положить «на диск» `settings.toml`, уже равный сериализации настроек
    /// по умолчанию — без расхождения с эталоном `PersistTracker` сразу
    /// после `boot`. Нужен тестам пути выхода (ТЗ-32, §6.10), которым
    /// `settings.toml` не интересен: иначе `exit` каждый раз отправляет и
    /// его снимок — расхождение с «пустым» `put_settings(b"")` против
    /// реальной сериализации значений по умолчанию.
    pub(crate) fn put_default_settings(&self) {
        let bytes = crate::persist::settings_file::serialize_settings(&crate::persist::settings_file::Settings::default())
            .expect("сериализация настроек по умолчанию");
        self.put_settings(&bytes);
    }

    /// Положить байты `state.toml` «на диск» до запуска.
    pub(crate) fn put_state(&self, bytes: &[u8]) {
        self.fs.put(&self.paths.state, bytes);
    }

    /// Внедрить ошибку чтения `settings.toml` (ОВС-6 в).
    pub(crate) fn fail_read_settings(&self, class: ReadErrorClass) {
        self.fs.fail_read(&self.paths.settings, class);
    }

    /// Внедрить ошибку чтения `state.toml` (ОВС-6 в).
    pub(crate) fn fail_read_state(&self, class: ReadErrorClass) {
        self.fs.fail_read(&self.paths.state, class);
    }

    /// Внедрить ошибку записи копии `<file>.bad` (ТЗ-7).
    pub(crate) fn fail_write_bad_copy(&self, file: WorkFile, step: WriteStep, class: WriteErrorClass) {
        self.fs.fail_write(&self.paths.bad_copy(file), step, class, 1);
    }

    /// Положить байты `playlist.m3u` «на диск» до запуска (ТЗ-21, §6.12).
    pub(crate) fn put_playlist(&self, bytes: &[u8]) {
        self.fs.put(self.paths.work(WorkFile::Playlist), bytes);
    }

    /// Внедрить ошибку чтения рабочего файла `file` (ТЗ-21, ОВС-6 в).
    pub(crate) fn fail_read_work(&self, file: WorkFile, class: ReadErrorClass) {
        self.fs.fail_read(self.paths.work(file), class);
    }

    /// Разрешает текущее задание загрузки (стартовой или по команде,
    /// ТЗ-13 б) реальным `run_load` (ADR-16, §6.12, ТЗ-21, ТЗ-47) поверх
    /// `MemStore` стенда — пробник отдаёт трек с заголовком по имени файла,
    /// этого достаточно для путей и сортировки по заголовку. `false` —
    /// задания нет (загрузка не запущена или уже разрешена).
    pub(crate) fn finish_pending_load(&self) -> bool {
        let Some(job) = self.loader.jobs().into_iter().next() else { return false };
        fn probe(path: &Path) -> Track {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            Track { path: path.to_path_buf(), title: stem, ..Track::default() }
        }
        let outcome = run_load(&job, &self.fs, &probe);
        self.loader.finish(outcome)
    }

    /// Алиас `finish_pending_load` для существующих тестов стартовой
    /// загрузки (ADR-16, §6.12, ТЗ-21, ТЗ-47).
    pub(crate) fn finish_startup_load(&self) -> bool {
        self.finish_pending_load()
    }

    /// Положить байты файла, выбранного командой «Загрузить плейлист»,
    /// «на диск» по произвольному пути (ТЗ-13 б, ADR-16, §6.12) — такой
    /// путь лежит вне рабочих файлов сессии (`ConfigPaths`).
    pub(crate) fn put_command_playlist(&self, path: &Path, bytes: &[u8]) {
        self.fs.put(path, bytes);
    }

    /// Внедрить ошибку чтения произвольного пути (ТЗ-13 б, ОВС-6 в) — для
    /// файлов команды «Загрузить плейлист», лежащих вне `ConfigPaths`.
    pub(crate) fn fail_read_path(&self, path: &Path, class: ReadErrorClass) {
        self.fs.fail_read(path, class);
    }

    /// Внедрить ошибку записи произвольного пути (ТЗ-20, ADR-17, §6.18) —
    /// для файла экспорта «Сохранить плейлист», лежащего вне `ConfigPaths`.
    pub(crate) fn fail_write_path(&self, path: &Path, step: WriteStep, class: WriteErrorClass, times: u32) {
        self.fs.fail_write(path, step, class, times);
    }

    /// Тикает, пока `tick` не вернёт хотя бы одно сообщение (ADR-13,
    /// §6.12): ответы карантина (`Quarantined`/`QuarantineFailed`)
    /// перехватываются внутри `tick` до `tracker.on_reply` и не
    /// регистрируются как «в полёте» — `settle`/`settle_ticks` их не
    /// видят. Срок — 2 с реального времени, как у `settle`.
    pub(crate) fn settle_messages(&self, core: &mut AppCore) -> Vec<Message> {
        let start = Instant::now();
        loop {
            let out = core.tick(None);
            if !out.messages.is_empty() {
                return out.messages;
            }
            if let Some(wake) = self.fs.next_wake() {
                self.clock.advance(wake.saturating_since(self.clock.now()));
            }
            assert!(start.elapsed() < Duration::from_secs(2), "settle_messages: сообщение не пришло за 2 с реального времени");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Тикает, пока байты по произвольному пути `path` не появятся в
    /// `MemStore` (ADR-17, §6.18, ТЗ-13 а): экспорт «Сохранить плейлист» не
    /// трогает `PersistTracker`, поэтому `settle` здесь неприменим — опрашивает
    /// хранилище напрямую, как `settle_messages` опрашивает сообщения. Срок —
    /// 2 с реального времени, как у `settle`.
    pub(crate) fn settle_path(&self, core: &mut AppCore, path: &Path) -> Vec<u8> {
        let start = Instant::now();
        loop {
            core.tick(None);
            if let Some(bytes) = self.fs.get(path) {
                return bytes;
            }
            if let Some(wake) = self.fs.next_wake() {
                self.clock.advance(wake.saturating_since(self.clock.now()));
            }
            assert!(start.elapsed() < Duration::from_secs(2), "settle_path: байты не появились за 2 с реального времени");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Запуск (ADR-23 шаги 0–2, §8 С3): чтение + разбор, копии `*.bad` до
    /// появления окна (пишутся прямо в `MemStore` стенда — детерминированный
    /// аналог `WriterCmd::BadCopy` из `bad_copies_via_writer`, без ожидания
    /// по реальным часам), затем `AppCore` с зависимостями нового API
    /// (ADR-19, §2.12) — писатель `apap-persist` на той же `MemStore`,
    /// инжектируемые `ManualClock`/`ManualWaiter` и общий журнал. Записи
    /// журнала для обоих шагов (ТЗ-5, ТЗ-6, ТЗ-7, §6.1) накапливаются в
    /// `journal()`; результат самих копий также проверяется через
    /// `bad_copy_bytes`/`bad_copy_counts`.
    pub(crate) fn boot(&self) -> AppCore {
        let boot = persist::boot(&self.fs, &self.paths);
        for rec in persist::journal_records_for_boot(&boot) {
            self.journal.record(rec);
        }
        let outcomes = self.write_bad_copies(&boot);
        for rec in persist::journal_records_for_bad_copies(&boot, &outcomes) {
            self.journal.record(rec);
        }

        let writer = spawn_writer(Box::new(self.fs.clone()), self.paths.clone());
        let clock: Box<dyn Clock> = Box::new(self.clock.clone());
        let waiter: Box<dyn ReplyWaiter> = Box::new(ManualWaiter::new(self.clock.clone(), self.fs.clone()));
        let journal: Arc<dyn Journal> = Arc::clone(&self.journal) as Arc<dyn Journal>;
        let engine: Box<dyn EngineSink> = Box::new(RecordingEngine { log: Rc::clone(&self.engine_log) });
        let loader: Box<dyn PlaylistLoader> = Box::new(self.loader.clone());
        let reader: Arc<dyn FileReader> = Arc::new(self.fs.clone());
        let io: Box<dyn IoWorker> = Box::new(self.io.clone());
        let deps = AppDeps { writer, paths: self.paths.clone(), clock, waiter, journal, engine, loader, reader, io };
        AppCore::with_deps(deps, boot)
    }

    /// Команды, отправленные движку через `AppDeps.engine` за всё время
    /// стенда (§2.12, ADR-01), как текст `Debug` (`EngineCmd` не `Clone`).
    pub(crate) fn engine_cmds(&self) -> Vec<String> {
        self.engine_log.borrow().clone()
    }

    /// Копии `*.bad` неразбираемых файлов (И-Р12, И-Р18) — прямая запись в
    /// `MemStore` стенда вместо `WriterCmd::BadCopy` писателю: тот же итог,
    /// без недетерминированного ожидания ответа по реальным часам.
    fn write_bad_copies(&self, boot: &persist::Boot) -> Vec<BadCopyOutcome> {
        let mut writer = self.fs.clone();
        let mut out = Vec::new();
        if let Parsed::Unparsable { original, .. } = &boot.settings {
            out.push(self.write_bad_copy(&mut writer, ConfigFile::Settings, original));
        }
        if let Parsed::Unparsable { original, .. } = &boot.state {
            out.push(self.write_bad_copy(&mut writer, ConfigFile::State, original));
        }
        out
    }

    fn write_bad_copy(&self, writer: &mut dyn FileWriter, file: ConfigFile, original: &[u8]) -> BadCopyOutcome {
        let path = self.paths.bad_copy(file.work());
        let result = writer.write_atomic(&path, original).map(|()| path);
        BadCopyOutcome { file, result }
    }

    /// Записи журнала, накопленные за `boot` (ТЗ-5, ТЗ-6, ТЗ-7, §6.1).
    pub(crate) fn journal(&self) -> Vec<JournalRecord> {
        self.journal.records()
    }

    pub(crate) fn bad_copy_bytes(&self, file: WorkFile) -> Option<Vec<u8>> {
        self.fs.get(&self.paths.bad_copy(file))
    }

    pub(crate) fn settings_counts(&self) -> OpCounts {
        self.fs.counts(&self.paths.settings)
    }

    pub(crate) fn state_counts(&self) -> OpCounts {
        self.fs.counts(&self.paths.state)
    }

    pub(crate) fn bad_copy_counts(&self, file: WorkFile) -> OpCounts {
        self.fs.counts(&self.paths.bad_copy(file))
    }

    /// Продвигает инжектируемые часы на `d`, затем выполняет один тик
    /// `AppCore` (ADR-19, §6.4, §7.1): срабатывание дедлайна отложенной
    /// записи и разбор ответов писателя идут по симулированному времени.
    /// Байты плейлиста (§4.2, ТЗ-12) тик берёт сам, лениво — из реальной
    /// модели плейлиста `AppCore`.
    pub(crate) fn advance(&self, core: &mut AppCore, d: Duration) -> TickOutput {
        self.clock.advance(d);
        core.tick(None)
    }

    /// Тикает, пока у `PersistTracker` остаётся хоть один файл «в полёте»
    /// (§7.1, §6.10): тот же протокол опроса, что у `ManualWaiter` — ждёт
    /// ответа писателя `apap-persist`, не продвигая симулированное время.
    /// Срок — 2 с реального времени; превышение — ошибка теста (зависший
    /// писатель), а не штатный исход.
    pub(crate) fn settle(&self, core: &mut AppCore) {
        let start = Instant::now();
        loop {
            core.tick(None);
            let pending = [WorkFile::Playlist, WorkFile::State, WorkFile::Settings]
                .into_iter()
                .any(|f| core.tracker().last_in_flight(f).is_some());
            if !pending {
                return;
            }
            // Если писатель ждёт искусственную задержку (`delay_write_work`),
            // симулированные часы сами не идут — подвинуть их к ближайшему
            // пробуждению, как это делает `ManualWaiter` на выходе (§6.10).
            if let Some(wake) = self.fs.next_wake() {
                self.clock.advance(wake.saturating_since(self.clock.now()));
            }
            assert!(start.elapsed() < Duration::from_secs(2), "settle: писатель не ответил за 2 с реального времени");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Как `settle`, но возвращает эффекты `tick` каждого отдельного тика
    /// (ТЗ-20, §6.8): нужно тестам окна ошибки записи, которым важно, какие
    /// `ReplyEffect` пришли В ОДНОМ тике (слияние нескольких неудач писателя
    /// в одно окно), а не только их общее число.
    pub(crate) fn settle_ticks(&self, core: &mut AppCore) -> Vec<Vec<ReplyEffect>> {
        let start = Instant::now();
        let mut ticks = Vec::new();
        loop {
            let out = core.tick(None);
            ticks.push(out.effects);
            let pending = [WorkFile::Playlist, WorkFile::State, WorkFile::Settings]
                .into_iter()
                .any(|f| core.tracker().last_in_flight(f).is_some());
            if !pending {
                return ticks;
            }
            if let Some(wake) = self.fs.next_wake() {
                self.clock.advance(wake.saturating_since(self.clock.now()));
            }
            assert!(start.elapsed() < Duration::from_secs(2), "settle_ticks: писатель не ответил за 2 с реального времени");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Журнал вызовов модуля ФС с именами потоков (И-Р11, ТЗ-22, §7.1).
    pub(crate) fn fs_calls(&self) -> Vec<FsCall> {
        self.fs.calls()
    }

    /// Число успешных записей рабочего файла `file` (§7.1) — по счётчикам
    /// `MemStore`, независимо от журнала.
    pub(crate) fn writes(&self, file: WorkFile) -> usize {
        let counts = self.fs.counts(self.paths.work(file));
        usize::try_from(counts.writes).expect("счётчик записей укладывается в usize")
    }

    /// Внедрить ошибку записи рабочего файла `file` (ТЗ-20, §6.8).
    pub(crate) fn fail_write_work(&self, file: WorkFile, step: WriteStep, class: WriteErrorClass, times: u32) {
        self.fs.fail_write(self.paths.work(file), step, class, times);
    }

    /// Задержать следующую запись рабочего файла `file` на `d` симулированного
    /// времени от текущего момента (§7.1 `playlist_flag_kept_if_changed_during_write`).
    pub(crate) fn delay_write_work(&self, file: WorkFile, d: Duration) {
        let until = self.clock.now().saturating_add(d);
        self.fs.delay_write(self.paths.work(file), until, self.clock.clone());
    }

    /// Содержимое рабочего файла «на диске» — то, что переживёт крах
    /// процесса без пути выхода (§7.1 `crash_keeps_last_written_version`).
    pub(crate) fn disk_bytes(&self, file: WorkFile) -> Option<Vec<u8>> {
        self.fs.get(self.paths.work(file))
    }

    /// Текущий момент инжектируемых часов стенда (ТЗ-32, §6.10
    /// `exit_budget_five_seconds`): нужен, чтобы измерить длительность
    /// синхронного `exit()` по симулированному времени.
    pub(crate) fn now(&self) -> ClockInstant {
        self.clock.now()
    }
}

/// Ответ `FakeIo` на `ReadTheme` для одного имени темы (§7.1, ADR-20).
type ThemeAnswer = Result<Arc<ThemeData>, Box<str>>;

/// Общее мутируемое состояние `FakeIo` за `Rc<RefCell<..>>` (§7.1, ADR-20):
/// отвечает на задания синхронно внутри `submit`, чтобы не заводить
/// реальный поток `apap-io` в тестах `AppCore`.
#[derive(Default)]
struct FakeIoState {
    sizes: CacheSizes,
    themes: Vec<ThemeEntry>,
    theme_data: Vec<(ThemeName, ThemeAnswer)>,
    queue: VecDeque<IoDone>,
    jobs: Vec<IoJob>,
}

/// Фиктивный `IoWorker` потока `apap-io` для тестов `AppCore` без реального
/// потока (§7.1, ADR-20): `submit` вычисляет ответ немедленно и кладёт его в
/// очередь, `try_recv` отдаёт её по порядку — если `hold` не взведён.
/// `hold` симулирует медленный поток: ответы готовы (посчитаны `submit`), но
/// не выдаются, пока тест не снимет задержку. Клоны делят одно состояние
/// через `Rc`, как `FakePicker` делит журнал запросов — нужно, чтобы тест
/// мог опрашивать `FakeIo` после того, как она ушла в `Box<dyn IoWorker>`.
#[derive(Clone)]
pub(crate) struct FakeIo {
    state: Rc<RefCell<FakeIoState>>,
    hold: Rc<Cell<bool>>,
}

impl FakeIo {
    pub(crate) fn new() -> FakeIo {
        FakeIo { state: Rc::new(RefCell::new(FakeIoState::default())), hold: Rc::new(Cell::new(false)) }
    }

    /// Задать размер дисковых кэшей, возвращаемый `CacheSizes`/`Cleared`
    /// (ТЗ-22, ТЗ-34).
    pub(crate) fn set_sizes(&self, sizes: CacheSizes) {
        self.state.borrow_mut().sizes = sizes;
    }

    /// Задать список тем, возвращаемый `ListThemes` (ТЗ-22).
    pub(crate) fn set_themes(&self, themes: Vec<ThemeEntry>) {
        self.state.borrow_mut().themes = themes;
    }

    /// Задать ответ `ReadTheme` для имени `name`; без вызова — `try_recv`
    /// вернёт `Err("not found")` (ТЗ-22, ТЗ-27, ТЗ-29).
    pub(crate) fn set_theme(&self, name: ThemeName, data: ThemeAnswer) {
        self.state.borrow_mut().theme_data.push((name, data));
    }

    /// Задержать выдачу уже посчитанных ответов (`try_recv`), не трогая
    /// `submit` — симуляция медленного потока `apap-io` (§7.1).
    pub(crate) fn set_hold(&self, hold: bool) {
        self.hold.set(hold);
    }

    /// Журнал заданий в порядке `submit` (§7.1).
    pub(crate) fn jobs(&self) -> Vec<IoJob> {
        self.state.borrow().jobs.clone()
    }

    fn count(&self, matches: impl Fn(&IoJob) -> bool) -> usize {
        self.state.borrow().jobs.iter().filter(|j| matches(j)).count()
    }

    pub(crate) fn count_cache_sizes(&self) -> usize {
        self.count(|j| matches!(j, IoJob::CacheSizes { .. }))
    }

    pub(crate) fn count_clear(&self) -> usize {
        self.count(|j| matches!(j, IoJob::ClearCache { .. }))
    }

    pub(crate) fn count_read_theme(&self) -> usize {
        self.count(|j| matches!(j, IoJob::ReadTheme { .. }))
    }

    pub(crate) fn count_list_themes(&self) -> usize {
        self.count(|j| matches!(j, IoJob::ListThemes { .. }))
    }
}

impl IoWorker for FakeIo {
    fn submit(&self, job: IoJob) {
        let mut state = self.state.borrow_mut();
        state.jobs.push(job.clone());
        let done = match job {
            IoJob::ReadTheme { gen, name } => {
                let result = state
                    .theme_data
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_else(|| Err("not found".into()));
                IoDone::Theme { gen, result }
            }
            IoJob::ListThemes { gen } => IoDone::Themes { gen, entries: state.themes.clone() },
            IoJob::CacheSizes { gen } => IoDone::CacheSizes { gen, sizes: state.sizes },
            IoJob::ClearCache { gen, which } => {
                match which {
                    CacheKind::Visualization => state.sizes.viz_disk = 0,
                    CacheKind::Covers => state.sizes.covers = 0,
                    CacheKind::All => {
                        state.sizes.viz_disk = 0;
                        state.sizes.covers = 0;
                    }
                }
                IoDone::Cleared { gen, sizes: state.sizes }
            }
        };
        state.queue.push_back(done);
    }

    fn try_recv(&self) -> Option<IoDone> {
        if self.hold.get() {
            return None;
        }
        self.state.borrow_mut().queue.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::visualizer::VisualizationMode;
    use crate::core::dialog::{ApplyStep, ThemeLoad};
    use crate::core::exit::{ExitOutcome, ExitPhase, ExitReason, TermSignal};
    use crate::core::SaveButton;
    use crate::core::messages::{MessageCenter, MessageLevel};
    use crate::core::gate::{BlockReason, MainCmd, UiGate};
    use crate::core::load::{LoadApplied, LoadState};
    use crate::journal::{HostSource, WriteTarget};
    use crate::platform::fs::FsOp;
    use crate::platform::input::{
        Desktop, FakeWheelInput, Outcome, Session, TrayHost, TrayRow, TrayScroll, WheelTable,
    };
    use crate::platform::lifecycle::{ExitEntry, FakeLifecycle, Lifecycle, PlatformCaps, TrayEvent};
    use crate::platform::notify::{FakeNotifier, Notification, Notifier};
    use crate::persist::keys::{KeyPath, LoadNoteKind};
    use crate::persist::settings_file::{SaveInterval, Settings, ThemeName};
    use crate::persist::state_file::{
        Origin, PhysPos, PhysSize, SessionState, SizeUnits, StateChange,
        WindowGeometry,
    };
    use crate::platform::pick::fake::FakePicker;
    use crate::platform::pick::{FilePicker, PickRequest};
    use crate::playlist::compare::{compare_keys, CompareKeys};
    use crate::playlist::model::SortKey;
    use crate::playlist::Track;
    use crate::audio::backend::SharedDeviceId;
    use crate::core::volume::{Clicks, VolumeBlock, VolumeCmd, VolumeOutcome, VolumeSource};
    use crate::settings::playback::{DeviceBuffer, ModeKind, PlaybackState, SharedDeviceChoice, SrcFilter, Volume};
    use crate::settings::{ColumnId, RepeatMode};
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::task::{Context, Poll, Waker};

    /// Нечитаемый `settings.toml` (ОВС-6 в, ТЗ-7, ТЗ-14, ТЗ-32, §6.10):
    /// автозаписи запрещены на весь сеанс — `exit` не отправляет снимок
    /// настроек, файл остаётся в `report.forbidden`.
    #[test]
    fn unreadable_settings_forbids_exit_write() {
        let h = Harness::new();
        h.put_state(b"");
        h.fail_read_settings(ReadErrorClass::NoAccess);
        let mut core = h.boot();
        assert!(core.settings_read_failed_message().is_some());
        assert_eq!(h.settings_counts().writes, 0);

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.settings_counts().writes, 0);

        let report = h
            .journal()
            .into_iter()
            .find_map(|r| match r {
                JournalRecord::ExitSummary(report) => Some(report),
                _ => None,
            })
            .expect("ExitSummary journaled");
        assert!(report.forbidden.contains(&WorkFile::Settings));
    }

    /// Неразбираемый `settings.toml` (ТЗ-6, ТЗ-21, И-Р12, И-Р18, §2.13):
    /// копия `*.bad` с исходными байтами пишется один раз при запуске (до
    /// появления окна), значения в памяти — по умолчанию.
    #[test]
    fn unparsable_settings_writes_bad_copy_once() {
        let h = Harness::new();
        h.put_settings(b"[[\n");
        h.put_state(b"");
        let core = h.boot();

        assert_eq!(core.settings(), &Settings::default());
        assert_eq!(h.bad_copy_bytes(WorkFile::Settings).as_deref(), Some(&b"[[\n"[..]));
        assert_eq!(h.bad_copy_counts(WorkFile::Settings).writes, 1);
    }

    /// То же для `state.toml` (ТЗ-6, ТЗ-21, И-Р12, И-Р18, §2.13).
    #[test]
    fn unparsable_state_writes_bad_copy_once() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"[[\n");
        let core = h.boot();

        assert_eq!(core.state(), &SessionState::default());
        assert_eq!(h.bad_copy_bytes(WorkFile::State).as_deref(), Some(&b"[[\n"[..]));
        assert_eq!(h.bad_copy_counts(WorkFile::State).writes, 1);
    }

    /// `AppDeps.engine` подключён (§2.12, ADR-01), но ничего в `AppCore` пока
    /// не отправляет команды движку — это остаётся на последующие шаги
    /// (например, перевод `exit()` на `deps.engine`, шаг 55).
    #[test]
    fn boot_sends_no_engine_commands() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let _core = h.boot();

        assert_eq!(h.engine_cmds(), Vec::<String>::new());
    }

    /// Запуск — чистая функция без записи (ТЗ-4, §8 С3: «запуск ничего не
    /// пишет»): ни рабочие файлы, ни копии `*.bad` не пишутся при валидных
    /// файлах на диске.
    #[test]
    fn startup_writes_nothing() {
        let h = Harness::new();
        h.put_settings(b"theme = \"dark\"\n");
        h.put_state(b"");
        let _core = h.boot();

        assert_eq!(h.settings_counts().writes, 0);
        assert_eq!(h.state_counts().writes, 0);
        assert_eq!(h.bad_copy_counts(WorkFile::Settings).writes, 0);
        assert_eq!(h.bad_copy_counts(WorkFile::State).writes, 0);
    }

    /// Файл без изменений не перезаписывается на выходе (ТЗ-9, ТЗ-14, §6.10):
    /// эталон из прочитанных байт совпадает с сериализацией значений по
    /// умолчанию, плейлист не отправляется — все три файла попадают в
    /// `report.unchanged`.
    #[test]
    fn exit_skips_unchanged_files() {
        let h = Harness::new();
        let settings_bytes = crate::persist::settings_file::serialize_settings(&Settings::default())
            .expect("serialize settings");
        let state_bytes =
            crate::persist::state_file::serialize_state(&SessionState::default()).expect("serialize state");
        h.put_settings(&settings_bytes);
        h.put_state(&state_bytes);
        let mut core = h.boot();

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.settings_counts().writes, 0);
        assert_eq!(h.state_counts().writes, 0);

        let report = h
            .journal()
            .into_iter()
            .find_map(|r| match r {
                JournalRecord::ExitSummary(report) => Some(report),
                _ => None,
            })
            .expect("ExitSummary journaled");
        let mut unchanged = report.unchanged.clone();
        unchanged.sort_by_key(|f| format!("{f:?}"));
        let mut expected = vec![WorkFile::Playlist, WorkFile::State, WorkFile::Settings];
        expected.sort_by_key(|f| format!("{f:?}"));
        assert_eq!(unchanged, expected);
    }

    /// Заметки разбора по ключам (ТЗ-5, ТЗ-8, ТЗ-9, §6.2) доходят до
    /// `AppCore::startup_notes`: отсутствующий ключ — `Missing`, неверное
    /// значение — `Invalid`, незнакомый лист — `Unknown`.
    #[test]
    fn settings_key_notes_reach_startup_notes() {
        let h = Harness::new();
        h.put_settings(b"save_interval = 45\nbogus_leaf = 1\n");
        h.put_state(b"");
        let core = h.boot();

        let notes = &core.startup_notes().settings;
        let theme_note = notes.iter().find(|n| n.key == KeyPath::new("theme")).expect("theme note present");
        assert!(matches!(theme_note.kind, LoadNoteKind::Missing));

        let interval_note =
            notes.iter().find(|n| n.key == KeyPath::new("save_interval")).expect("save_interval note present");
        assert!(matches!(interval_note.kind, LoadNoteKind::Invalid { .. }));

        let unknown_note =
            notes.iter().find(|n| n.key == KeyPath::new("bogus_leaf")).expect("bogus_leaf note present");
        assert!(matches!(unknown_note.kind, LoadNoteKind::Unknown));
    }

    /// Три заметки одного файла — одна запись `LoadNotes` (ТЗ-5, §6.1,
    /// §7.2 `parse_by_keys_three_notes`): отсутствующий обязательный ключ
    /// (`playback.compatible.muted`), недопустимое значение
    /// (`playback.compatible.volume`), неизвестный ключ (`bogus`).
    #[test]
    fn parse_by_keys_three_notes() {
        let h = Harness::new();
        h.put_state(
            b"repeat = \"off\"\nshuffle = false\nbogus = 1\n\n\
              [playback]\nactive = \"compatible\"\n\n\
              [playback.compatible]\nvolume = 200\n\n\
              [playback.optimal]\nvolume = 50\nmuted = false\n\n\
              [playback.strict]\nmuted = false\n\n\
              [visualization]\nmode = \"off\"\n\n\
              [window]\nmaximized = false\nfullscreen = false\nunits = \"physical\"\n",
        );
        let _core = h.boot();

        let records = h.journal();
        let load_notes: Vec<&JournalRecord> =
            records.iter().filter(|r| matches!(r, JournalRecord::LoadNotes { file: ConfigFile::State, .. })).collect();
        assert_eq!(load_notes.len(), 1);
        match load_notes[0] {
            JournalRecord::LoadNotes { notes, .. } => assert_eq!(notes.len(), 3),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Неразбираемый `settings.toml` с успешной копией `*.bad` (ТЗ-6, ТЗ-7,
    /// §6.1): одна запись `Unparsable` с путём копии.
    #[test]
    fn unparsable_settings_journal_has_copy_path() {
        let h = Harness::new();
        h.put_settings(b"[[\n");
        let _core = h.boot();

        let records = h.journal();
        assert_eq!(records.len(), 1);
        match &records[0] {
            JournalRecord::Unparsable { file: ConfigFile::Settings, error, copy } => {
                assert!(!error.is_empty());
                assert_eq!(copy.as_deref().ok(), Some(h.paths.bad_copy(WorkFile::Settings).as_path()));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Неразбираемый `state.toml` без окна: только запись журнала (ТЗ-7,
    /// §6.1, §7.2 `unparsable_state_journal_only`).
    #[test]
    fn unparsable_state_journal_only() {
        let h = Harness::new();
        h.put_state(b"[[\n");
        let _core = h.boot();

        let records = h.journal();
        assert_eq!(records.len(), 1);
        assert!(matches!(&records[0], JournalRecord::Unparsable { file: ConfigFile::State, .. }));
    }

    /// Копия `*.bad` не записалась: `Unparsable.copy` несёт `WriteError`
    /// (ТЗ-7, §6.1, §7.2 `unparsable_settings_copy_failed_warning_text`).
    #[test]
    fn unparsable_settings_copy_failed_is_journaled() {
        let h = Harness::new();
        h.put_settings(b"[[\n");
        h.fail_write_bad_copy(WorkFile::Settings, WriteStep::WriteData, WriteErrorClass::NoSpace);
        let _core = h.boot();

        let records = h.journal();
        assert_eq!(records.len(), 1);
        match &records[0] {
            JournalRecord::Unparsable { file: ConfigFile::Settings, copy, .. } => {
                assert!(copy.is_err());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Тик без изменений ничего не пишет (ТЗ-4, §6.4, §7.1): дедлайн не
    /// взведён — продвижение часов на любой срок не вызывает запись.
    /// Асинхронный аналог `startup_writes_nothing` через `advance`/`settle`.
    #[test]
    fn tick_without_change_writes_nothing() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval * 2);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(h.writes(WorkFile::Settings), 0);
    }

    /// Дедлайн срабатывает, но итоговый текст совпал с эталоном — запись
    /// не отправляется (И-Р3, ТЗ-9, §6.4, §7.1
    /// `change_and_revert_writes_nothing`): асинхронный аналог
    /// `unchanged_files_not_rewritten` через изменение и обратное изменение.
    #[test]
    fn change_and_revert_writes_nothing() {
        let h = Harness::new();
        let state_bytes =
            crate::persist::state_file::serialize_state(&SessionState::default()).expect("serialize state");
        h.put_settings(b"");
        h.put_state(&state_bytes);
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Volume(42));
        core.change_state(Origin::User, StateChange::Volume(100));

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(core.state(), &SessionState::default());
    }

    /// Изменение состояния доходит до писателя за один тик по дедлайну
    /// (ADR-4, §6.4, §7.1): асинхронный аналог `changed_state_written_once`
    /// через `advance`/`settle`; повторный тик без новых изменений не
    /// пишет снова.
    #[test]
    fn tick_writes_changed_state_once() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Shuffle(true));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::State), 1);
        assert!(core.state().shuffle());

        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1);
    }

    /// Нечитаемый `state.toml` запрещает отложенную автозапись на весь
    /// сеанс (ОВС-6 в, ТЗ-7, §6.4, §7.1): асинхронный аналог
    /// `unreadable_state_forbids_auto_write` — дедлайн срабатывает, но
    /// `auto_allowed` не пропускает отправку снимка.
    #[test]
    fn unreadable_state_forbids_tick_write() {
        let h = Harness::new();
        h.put_settings(b"");
        h.fail_read_state(ReadErrorClass::NoAccess);
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Shuffle(true));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::State), 0);
    }

    /// Изменение плейлиста доходит до писателя по тому же дедлайну, что и
    /// состояние (ТЗ-12, §2.7, §6.4, §7.1 `playlist_dirty_written_by_timer`):
    /// добавление треков в модель плейлиста взводит флаг «грязного» через
    /// `playlist_add`; после успешной записи флаг снят.
    #[test]
    fn playlist_dirty_written_by_timer() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.playlist_add(vec![track_path("track1.flac"), track_path("track2.flac")]);
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::Playlist), 1);
        assert!(!core.tracker().playlist_dirty());
    }

    /// N=30; изменения громкости в t=0, 10, 20 с — одна запись в t=30 с со
    /// значением из t=20 с (ТЗ-11, §6.4, §7.2 `state_written_after_n_seconds_once`).
    #[test]
    fn state_written_after_n_seconds_once() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Volume(10)); // t=0, срок = 30с
        h.advance(&mut core, Duration::from_secs(10)); // t=10
        core.change_state(Origin::User, StateChange::Volume(20));
        h.advance(&mut core, Duration::from_secs(10)); // t=20
        core.change_state(Origin::User, StateChange::Volume(30));
        h.advance(&mut core, Duration::from_secs(10)); // t=30 — дедлайн срабатывает
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::State), 1);
        assert_eq!(core.state().playback().volume, 30);
    }

    /// Изменения каждые 5 с в течение 10 мин — запись каждые N=30 с, ни
    /// одна пара записей не расходится дальше N (ТЗ-11, §6.4,
    /// §7.2 `continuous_series_written_every_n`).
    #[test]
    fn continuous_series_written_every_n() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        // 20 блоков по 30 с (интервал N по умолчанию); внутри каждого блока —
        // изменения каждые 5 с, т.е. несколько «перезаписей» значения до
        // срабатывания дедлайна. `settle` в конце блока дожидается ответа
        // писателя перед тем, как следующий блок мог бы взвести новый срок,
        // иначе гонка между симулированными часами и реальным потоком
        // писателя делает счётчик записей недетерминированным.
        for block in 0..20u32 {
            for i in 0..6u16 {
                // `% 100 + 1` держит значение в допустимом диапазоне громкости
                // 0..=100 (И-Т15) и меняется на каждом вызове (соседние члены
                // последовательности никогда не совпадают), так что каждый
                // `change_state` — настоящее изменение, взводящее дедлайн.
                let n = block * 6 + u32::from(i);
                let volume = u8::try_from(n % 100 + 1).expect("fits u8");
                core.change_state(Origin::User, StateChange::Volume(volume));
                h.advance(&mut core, Duration::from_secs(5));
            }
            h.settle(&mut core);
        }

        // 600 с опроса / 30 с интервал = 20 записей по дедлайну.
        assert_eq!(h.writes(WorkFile::State), 20);
    }

    /// N=120; изменение в t=0; «Сохранить» с N=10 в t=100 с сжимает срок до
    /// t=110 с (ТЗ-11, ТЗ-33, У-1, §6.4, §6.6, §7.2 `interval_change_shortens_deadline`).
    #[test]
    fn interval_change_shortens_deadline() {
        let h = Harness::new();
        h.put_settings(b"save_interval = 120\n");
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Volume(42)); // t=0, срок = 120с
        h.advance(&mut core, Duration::from_secs(100)); // t=100с

        let mut settings = core.settings().clone();
        settings.save_interval = SaveInterval::S10;
        core.save_settings_now(settings); // «Сохранить» — срок сжимается до t=110с
        h.settle(&mut core);

        h.advance(&mut core, Duration::from_secs(9)); // t=109с — ещё не время
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);

        h.advance(&mut core, Duration::from_secs(1)); // t=110с
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1);
    }

    /// Полный `state.toml`; восстановление геометрии при старте (эхо
    /// Program, ОВС-5 а); 10 минут простоя — 0 запусков отсчёта; при выходе
    /// одна запись, т.к. первое показание после показа отличается от
    /// запрошенного (ТЗ-11, ОВ-2, ОВ-3, ОВС-5 а, §6.17, §7.2
    /// `idle_ten_minutes_writes_nothing`).
    #[test]
    fn idle_ten_minutes_writes_nothing() {
        let h = Harness::new();
        let restored = WindowGeometry {
            position: Some(PhysPos { x: 10, y: 20 }),
            size: Some(PhysSize { width: 800, height: 600 }),
            size_units: SizeUnits::Physical,
            maximized: false,
            fullscreen: false,
        };
        let mut state = SessionState::default();
        state.apply(StateChange::Window(restored));
        let state_bytes = crate::persist::state_file::serialize_state(&state).expect("serialize state");
        h.put_settings(b"");
        h.put_state(&state_bytes);
        let mut core = h.boot();

        core.program_set_geometry(restored);
        core.window_shown();

        let actual = WindowGeometry {
            position: Some(PhysPos { x: 30, y: 40 }),
            size: Some(PhysSize { width: 800, height: 600 }),
            size_units: SizeUnits::Physical,
            maximized: false,
            fullscreen: false,
        };
        // Первое показание после показа окна — эхо Program, не пользователь.
        core.tick(Some(actual));

        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);

        core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(h.writes(WorkFile::State), 1);
    }

    /// `change_state(Program, …)` не взводит срок; при выходе — одна запись
    /// (ТЗ-11, ОВ-3, §6.4, §7.2 `program_change_does_not_start_timer`).
    #[test]
    fn program_change_does_not_start_timer() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::Program, StateChange::Volume(55));

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);

        core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(h.writes(WorkFile::State), 1);
    }

    /// Ошибка записи `playlist.m3u` останавливает дальнейшие попытки по
    /// отсчёту: флаг «грязного» плейлиста остаётся взведённым, а новое
    /// изменение плейлиста за 10 мин не приводит ни к одной записи
    /// (ТЗ-12, ТЗ-20, §6.8, §7.2 `playlist_write_error_stops_timer_writes`).
    #[test]
    fn playlist_write_error_stops_timer_writes() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        h.fail_write_work(WorkFile::Playlist, WriteStep::WriteData, WriteErrorClass::NoSpace, 100);
        core.playlist_add(vec![track_path("track1.flac")]);

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
        assert!(core.tracker().playlist_dirty());

        // Новое изменение взводит срок заново, но попытки по таймеру
        // всё равно блокированы (`stopped`), пока нет явного `retry`.
        core.playlist_add(vec![track_path("track2.flac")]);
        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
    }

    /// Задержка записи на 2 с; во время неё плейлист снова меняется — после
    /// ответа на первый снимок флаг остаётся взведённым, а следующая запись
    /// по новому сроку содержит оба трека (ТЗ-12, §6.4, §7.2
    /// `playlist_flag_kept_if_changed_during_write`).
    #[test]
    fn playlist_flag_kept_if_changed_during_write() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let interval = core.settings().save_interval.duration();

        h.delay_write_work(WorkFile::Playlist, interval + Duration::from_secs(2));
        core.playlist_add(vec![track_path("track1.flac")]);
        h.advance(&mut core, interval); // срок истёк — первый снимок отправлен, но задержан

        // во время задержанной записи плейлист снова меняется
        core.playlist_add(vec![track_path("track2.flac")]);

        h.settle(&mut core); // ждёт ответ на первый снимок, продвигая часы до конца задержки
        assert_eq!(h.writes(WorkFile::Playlist), 1);
        assert!(core.tracker().playlist_dirty());

        h.advance(&mut core, interval); // новый срок — пишет актуальный плейлист с обоими треками
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 2);
        assert_eq!(h.disk_bytes(WorkFile::Playlist).as_deref(), Some(&b"track1.flac\ntrack2.flac\n"[..]));
    }

    /// Изменить громкость; дождаться записи; изменить ещё раз; уничтожить
    /// `AppCore` без пути выхода — на диске остаётся значение первого
    /// изменения (ТЗ-16, §6.4, §7.2 `crash_keeps_last_written_version`).
    #[test]
    fn crash_keeps_last_written_version() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Volume(11));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1);

        core.change_state(Origin::User, StateChange::Volume(22));
        drop(core); // уничтожение без пути выхода — второе изменение никуда не уходит

        let mut expected = SessionState::default();
        expected.apply(StateChange::Volume(11));
        let expected_bytes =
            crate::persist::state_file::serialize_state(&expected).expect("serialize state");
        assert_eq!(h.disk_bytes(WorkFile::State), Some(expected_bytes.to_vec()));
        assert_eq!(h.writes(WorkFile::State), 1);
    }

    /// Переключение mute путём пользователя (`AppCore::toggle_mute`) взводит
    /// срок N; через N с — одна запись. Повторное переключение снимает mute и
    /// снова взводит срок N — вторая запись (ТЗ-38, ADR-14, §6.4, §7.2
    /// `mute_starts_timer`).
    #[test]
    fn mute_starts_timer() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        assert!(core.toggle_mute(), "первое переключение включает mute");
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::State), 1);
        assert!(core.state().playback().muted);

        assert!(!core.toggle_mute(), "повторное переключение снимает mute");
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::State), 2);
        assert!(!core.state().playback().muted);
    }

    /// Mute хранится по режимам и переживает перезапуск: mute, включённый в
    /// Строгом режиме, не затрагивает Совместимый и Оптимальный и после
    /// выхода и нового запуска на тех же файлах остаётся за Строгим (ТЗ-38,
    /// ADR-14, §7.2 `mute_per_mode_survives_restart`).
    #[test]
    fn mute_per_mode_survives_restart() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        assert_eq!(core.state().active_mode(), ModeKind::Compatible);
        assert!(!core.state().playback().muted);

        core.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Strict));
        assert!(core.toggle_mute(), "mute Строгого режима включён");
        assert!(core.state().playback().muted);

        // Возврат в Совместимый: его mute не тронут, mute Строгого сохранён.
        core.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Compatible));
        assert!(!core.state().playback().muted);
        assert!(core.state().playback_state().strict_muted);

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        // Перезапуск: новый `AppCore` из тех же "файлов на диске".
        let mut core2 = h.boot();
        assert_eq!(core2.state().active_mode(), ModeKind::Compatible);
        assert!(!core2.state().playback().muted);
        assert!(core2.state().playback_state().strict_muted);
        assert!(!core2.state().playback_state().compatible.muted);
        assert!(!core2.state().playback_state().optimal.muted);

        core2.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Strict));
        assert!(core2.state().playback().muted);
    }

    /// Все источники единой команды громкости (ADR-14, ТЗ-37, §2.11).
    const ALL_VOLUME_SOURCES: [VolumeSource; 4] = [
        VolumeSource::Slider,
        VolumeSource::SliderWheel,
        VolumeSource::TrayWheel,
        VolumeSource::Hotkey,
    ];

    /// Громкость из процентов; значение теста обязано лежать в 0..=100.
    fn vol(pct: u8) -> Volume {
        Volume::new(pct).expect("громкость теста в диапазоне 0..=100")
    }

    /// Общий сценарий недоступной громкости (ТЗ-37, ADR-14, И-Р13, §7.2):
    /// в режиме `mode` с фиксацией `lock` каждая команда каждого источника
    /// отклоняется с причиной `expected`, не меняет состояние и не взводит
    /// срок N; mute при этом переключается и срок N запускает (ТЗ-38).
    /// Возвращает стенд и ядро для дополнительных проверок теста.
    fn assert_volume_blocked(mode: ModeKind, lock: bool, expected: VolumeBlock) -> (Harness, AppCore) {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let interval = core.settings().save_interval.duration();

        let mut s = core.settings().clone();
        s.playback.optimal.volume_lock = lock;
        core.set_settings(s);

        // Смена режима сама взводит срок N — дожидаемся его записи, чтобы
        // он не мешал проверке заблокированных команд.
        core.change_state(Origin::User, StateChange::ActiveMode(mode));
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(core.volume_block(), Some(expected));
        assert!(core.tracker().deadline().is_none(), "срок N от смены режима уже истёк");
        let writes_before = h.writes(WorkFile::State);
        let snapshot = core.state().clone();

        let cmds = [
            VolumeCmd::Set(vol(40)),
            VolumeCmd::Step(Clicks(3)),
            VolumeCmd::Step(Clicks(-3)),
        ];
        for src in ALL_VOLUME_SOURCES {
            for cmd in cmds {
                assert_eq!(
                    core.volume(src, cmd),
                    VolumeOutcome::Blocked(expected),
                    "источник {src:?}, команда {cmd:?}"
                );
                assert!(
                    core.tracker().deadline().is_none(),
                    "срок N взведён заблокированной командой: {src:?}, {cmd:?}"
                );
            }
        }
        assert_eq!(core.state(), &snapshot, "заблокированные команды изменили состояние");

        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), writes_before, "срок N не должен был запускаться");

        // Mute доступен и при недоступной громкости; он взводит срок N.
        assert!(core.toggle_mute());
        assert!(core.state().playback().muted);
        assert!(core.tracker().deadline().is_some(), "mute взводит срок N");
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), writes_before + 1);

        (h, core)
    }

    /// Ползунок показывает громкость активного режима: команды любого
    /// источника меняют её, смена режима подставляет громкость нового режима,
    /// а громкость прежнего сохраняется (ТЗ-35, ADR-14, §7.2
    /// `slider_shows_active_mode_volume`).
    #[test]
    fn slider_shows_active_mode_volume() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        assert_eq!(core.state().active_mode(), ModeKind::Compatible);

        assert_eq!(
            core.volume(VolumeSource::Slider, VolumeCmd::Set(vol(40))),
            VolumeOutcome::Changed(vol(40))
        );
        assert_eq!(core.state().playback().volume, 40);
        assert!((core.state().playback().gain() - 0.40).abs() < 1e-6);

        assert_eq!(
            core.volume(VolumeSource::TrayWheel, VolumeCmd::Step(Clicks(3))),
            VolumeOutcome::Changed(vol(46))
        );
        assert_eq!(core.state().playback().volume, 46);

        let optimal_default = PlaybackState::default().optimal.volume.get();
        core.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Optimal));
        assert_eq!(core.state().playback().volume, optimal_default);
        assert_eq!(core.state().playback_state().compatible.volume.get(), 46);

        core.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Compatible));
        assert_eq!(core.state().playback().volume, 46);
    }

    /// В Строгом режиме ползунок, колёсико над ним, колёсико трея и горячие
    /// клавиши не меняют громкость, значение ползунка и не запускают срок N;
    /// mute переключается (ТЗ-37, ТЗ-38, ADR-14, И-Р13, §7.2
    /// `strict_mode_wheel_and_hotkeys_do_not_change_volume`).
    #[test]
    fn strict_mode_wheel_and_hotkeys_do_not_change_volume() {
        let (_h, core) = assert_volume_blocked(ModeKind::Strict, false, VolumeBlock::StrictMode);
        assert!(core.state().playback_state().strict_muted);
    }

    /// В Оптимальном режиме с «Фиксировать громкость на 100 %» команды всех
    /// источников отклоняются без изменения состояния и срока N; mute
    /// переключается; после снятия фиксации громкость снова меняется (ТЗ-37,
    /// ТЗ-38, ADR-14, И-Р13, §7.2 `optimal_volume_lock_ignores_all_sources`).
    #[test]
    fn optimal_volume_lock_ignores_all_sources() {
        let (_h, mut core) = assert_volume_blocked(ModeKind::Optimal, true, VolumeBlock::Locked);
        assert!(core.state().playback_state().optimal.muted);

        let mut s = core.settings().clone();
        s.playback.optimal.volume_lock = false;
        core.set_settings(s);
        assert_eq!(core.volume_block(), None);
        assert_eq!(
            core.volume(VolumeSource::Slider, VolumeCmd::Set(vol(40))),
            VolumeOutcome::Changed(vol(40))
        );
        assert_eq!(core.state().playback().volume, 40);
    }

    /// Общий шаг «событие → `advance(10 мин)` → 0 записей `settings.toml`»
    /// для строк §5.1 матрицы с «—» в столбце `settings.toml` (ТЗ-10, §7.2:
    /// семейство `event_*_does_not_write_settings`).
    fn assert_event_does_not_write_settings(apply: impl FnOnce(&mut AppCore)) {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        apply(&mut core);

        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::Settings), 0);
    }

    /// Громкость не пишет `settings.toml` (ТЗ-10, §5.1, §7.2).
    #[test]
    fn volume_change_does_not_write_settings() {
        assert_event_does_not_write_settings(|core| {
            core.change_state(Origin::User, StateChange::Volume(42));
        });
    }

    /// Mute не пишет `settings.toml` (ТЗ-10, §5.1, §7.2, §7.3 строка «Mute»).
    #[test]
    fn event_mute_does_not_write_settings() {
        assert_event_does_not_write_settings(|core| {
            core.change_state(Origin::User, StateChange::Muted(true));
        });
    }

    /// Геометрия окна пользователем не пишет `settings.toml` (ТЗ-10, ADR-22,
    /// §5.1, §7.2).
    #[test]
    fn event_geometry_does_not_write_settings() {
        assert_event_does_not_write_settings(|core| {
            let g = WindowGeometry {
                position: Some(PhysPos { x: 1, y: 2 }),
                size: Some(PhysSize { width: 640, height: 480 }),
                size_units: SizeUnits::Physical,
                maximized: false,
                fullscreen: false,
            };
            core.change_state(Origin::User, StateChange::Window(g));
        });
    }

    /// Тип визуализации не пишет `settings.toml` (ТЗ-10, §5.1, §7.2, §7.3
    /// строка «Тип визуализации»).
    #[test]
    fn event_viz_mode_does_not_write_settings() {
        assert_event_does_not_write_settings(|core| {
            core.change_state(Origin::User, StateChange::VizMode(VisualizationMode::Spectrum));
        });
    }

    /// Repeat/Shuffle не пишут `settings.toml` (ТЗ-10, §5.1, §7.2, §7.3
    /// строка «Repeat/Shuffle»).
    #[test]
    fn event_repeat_shuffle_does_not_write_settings() {
        assert_event_does_not_write_settings(|core| {
            core.change_state(Origin::User, StateChange::Repeat(RepeatMode::All));
            core.change_state(Origin::User, StateChange::Shuffle(true));
        });
    }

    /// Последний каталог не пишет `settings.toml` (ТЗ-10, §5.1, §7.2, §7.3
    /// строка «Последний каталог»).
    #[test]
    fn event_last_dir_does_not_write_settings() {
        assert_event_does_not_write_settings(|core| {
            core.change_state(Origin::User, StateChange::LastDir(PathBuf::from("/music")));
        });
    }

    /// «Сохранить» в диалоге настроек с изменением темы, устройства вывода
    /// и алгоритма ресемплинга (SRC-фильтра) — один немедленный `flush` с
    /// байтами, содержащими все три новых значения (ТЗ-28, §6.9). Путь —
    /// через диалог: `dialog_open` → `dialog_edit` → `dialog_save`.
    #[test]
    fn dialog_save_single_write_all_fields() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.dialog_open().expect("диалог открыт");
        let (theme, _) = pick_other_theme(&mut core);
        core.dialog_edit(|d| {
            d.settings.playback.compatible.device = SharedDeviceChoice::Named(SharedDeviceId::new("hw:1,0"));
            d.settings.playback.compatible.src_filter = SrcFilter::Slow;
        });
        let saved = core.dialog_save().expect("отличия есть — сохранено");
        assert!(saved.write.is_none(), "снимок ушёл писателю без ошибки сериализации");
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::Settings), 1);
        let bytes = h.disk_bytes(WorkFile::Settings).expect("settings written");
        let text = String::from_utf8(bytes).expect("settings.toml is valid utf-8");
        assert!(text.contains(&format!("theme = \"{}\"", theme.as_str())), "тема в файле: {text}");
        assert!(text.contains("device = \"hw:1,0\""));
        assert!(text.contains("src_filter = \"slow\""));
    }

    /// «Сохранить» без изменений относительно текущего состояния недоступно:
    /// `dialog_save` возвращает `None`, запись не отправляется, даже когда
    /// дедлайн затем срабатывает (ТЗ-27, ТЗ-28, §6.9).
    #[test]
    fn dialog_save_without_changes_does_not_write() {
        let h = Harness::new();
        let settings_bytes = crate::persist::settings_file::serialize_settings(&Settings::default())
            .expect("serialize settings");
        h.put_settings(&settings_bytes);
        h.put_state(b"");
        let mut core = h.boot();

        core.dialog_open().expect("диалог открыт");
        core.dialog_edit(|_| {});
        assert!(!core.save_button().enabled, "без отличий «Сохранить» недоступно");
        let result = core.dialog_save();
        assert!(result.is_none());
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 0);

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval * 2);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 0);
    }

    /// Нет места на диске при записи `state.toml` по отсчёту: файл на диске
    /// не меняется, в журнале — одна запись с классом/текстом ОС/путём,
    /// эффект тика — ровно один `Failed` (одно окно Error); следующие 10 мин
    /// простоя/тиков — 0 новых попыток записи, несмотря на новое изменение
    /// (ТЗ-20, §6.8).
    #[test]
    fn no_space_state_one_window_no_timer_retries() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.change_state(Origin::User, StateChange::Volume(42));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        let effects: Vec<ReplyEffect> = h.settle_ticks(&mut core).into_iter().flatten().collect();

        assert_eq!(effects, vec![ReplyEffect::Failed(WorkFile::State, WriteErrorClass::NoSpace)]);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(h.disk_bytes(WorkFile::State), Some(b"".to_vec()));

        let records = h.journal();
        let fails: Vec<&JournalRecord> = records
            .iter()
            .filter(|r| matches!(r, JournalRecord::WriteFailed { target: WriteTarget::Work(WorkFile::State), .. }))
            .collect();
        assert_eq!(fails.len(), 1);
        match fails[0] {
            JournalRecord::WriteFailed { err, .. } => {
                assert_eq!(err.class, WriteErrorClass::NoSpace);
                assert!(!err.os_text.is_empty());
                assert_eq!(err.path, h.paths.state);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Новое изменение взводит срок заново, но попытки по таймеру
        // всё равно блокированы (`stopped`), пока нет явного `retry`.
        core.change_state(Origin::User, StateChange::Volume(99));
        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);
    }

    /// После «освобождения места» (ошибка снята — однократный `fail_write`
    /// уже отработал) «Повторить» немедленно отправляет снимок: запись
    /// доходит до диска, эффект тика — один `Succeeded` (окно закрывается);
    /// следующее изменение снова пишется по отсчёту через N (ТЗ-20, §6.8).
    #[test]
    fn retry_after_space_freed() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.change_state(Origin::User, StateChange::Volume(42));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert!(core.tracker().timer_stopped(WorkFile::State));

        let retry_effects = core.retry(&[WorkFile::State]);
        assert!(retry_effects.is_empty());
        let reply_effects: Vec<ReplyEffect> = h.settle_ticks(&mut core).into_iter().flatten().collect();
        assert_eq!(reply_effects, vec![ReplyEffect::Succeeded(WorkFile::State)]);
        assert_eq!(h.writes(WorkFile::State), 1);
        assert!(!core.tracker().timer_stopped(WorkFile::State));

        core.change_state(Origin::User, StateChange::Volume(77));
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 2);
    }

    /// Пользователь нажал «ОК» без «Повторить»: запись по отсчёту остаётся
    /// остановленной на весь сеанс, несмотря на дальнейшие изменения —
    /// 10 минут простоя/тиков дают 0 попыток записи (ТЗ-20, §6.8).
    #[test]
    fn ok_keeps_timer_writes_stopped() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.change_state(Origin::User, StateChange::Volume(42));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);

        for i in 0..10u32 {
            core.change_state(Origin::User, StateChange::Volume(u8::try_from(i).expect("fits u8")));
            h.advance(&mut core, Duration::from_secs(60));
            h.settle(&mut core);
        }

        assert_eq!(h.writes(WorkFile::State), 0);
    }

    /// Файловая система только для чтения на обоих рабочих файлах: изменение
    /// громкости и плейлиста по одному отсчёту приводят к двум неудачным
    /// записям — по одной на файл; UI сводит их в одно окно Error (ТЗ-20, §6.8).
    #[test]
    fn readonly_media_one_window_two_files() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::ReadOnlyFs, 1);
        h.fail_write_work(WorkFile::Playlist, WriteStep::WriteData, WriteErrorClass::ReadOnlyFs, 1);

        core.change_state(Origin::User, StateChange::Volume(42));
        core.playlist_add(vec![track_path("track1.flac")]);

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        let ticks = h.settle_ticks(&mut core);

        // Ответы писателя могут прийти в разные тики (опрос раз в 5 мс), поэтому
        // проверяется весь отсчёт: ровно одна ошибка на файл. Сведение в одно
        // окно — забота MessageCenter (§6.15).
        let mut failed: Vec<WorkFile> = ticks
            .iter()
            .flatten()
            .filter_map(|e| match e {
                ReplyEffect::Failed(f, WriteErrorClass::ReadOnlyFs) => Some(*f),
                _ => None,
            })
            .collect();
        failed.sort();
        assert_eq!(failed, vec![WorkFile::Playlist, WorkFile::State], "по одной ошибке на state.toml и playlist.m3u");

        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
    }

    /// Все три файла нуждаются в записи, но каждый задержан на 10 с —
    /// дольше бюджета выхода: `exit` возвращает `Completed` ровно через 5 с
    /// симулированного времени (сам бюджет), ни один ответ писателя не
    /// успевает прийти, все три файла — в `timed_out`, на диске остаются
    /// прежние версии (ТЗ-14, ТЗ-32, НФ-9, §6.10 `exit_budget_five_seconds`).
    #[test]
    fn exit_budget_five_seconds() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        h.delay_write_work(WorkFile::Settings, Duration::from_secs(10));
        let mut settings = core.settings().clone();
        settings.playback.compatible.device = SharedDeviceChoice::Named(SharedDeviceId::new("hw:1,0"));
        core.save_settings_now(settings);

        h.delay_write_work(WorkFile::State, Duration::from_secs(10));
        core.change_state(Origin::User, StateChange::Volume(42));

        h.delay_write_work(WorkFile::Playlist, Duration::from_secs(10));
        core.playlist_add(vec![track_path("track1.flac")]);

        let before_settings = h.disk_bytes(WorkFile::Settings);
        let before_state = h.disk_bytes(WorkFile::State);
        let before_playlist = h.disk_bytes(WorkFile::Playlist);

        let start = h.now();
        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        let elapsed = h.now().saturating_since(start);

        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(elapsed, Duration::from_secs(5), "бюджет выхода — ровно EXIT_BUDGET");

        let report = h
            .journal()
            .into_iter()
            .find_map(|r| match r {
                JournalRecord::ExitSummary(report) => Some(report),
                _ => None,
            })
            .expect("ExitSummary journaled");
        let mut timed_out = report.timed_out;
        timed_out.sort();
        assert_eq!(timed_out, vec![WorkFile::Playlist, WorkFile::State, WorkFile::Settings]);
        assert!(report.written.is_empty());
        assert!(report.failed.is_empty());

        assert_eq!(h.disk_bytes(WorkFile::Settings), before_settings);
        assert_eq!(h.disk_bytes(WorkFile::State), before_state);
        assert_eq!(h.disk_bytes(WorkFile::Playlist), before_playlist);

        // Писатель всё ещё ждёт задержки (t0+10с) по всем трём файлам —
        // дать ему ответить, прежде чем стенд уничтожится, иначе поток
        // `apap-persist` будет бесконечно опрашивать часы, которые больше
        // никто не двигает.
        h.settle(&mut core);
    }

    /// Плейлист отвечает через 4 с, `state.toml` — только спустя 6 с
    /// (абсолютные моменты `delay_write_work`, а не последовательные
    /// длительности: писатель `apap-persist` — один поток FIFO, запись
    /// state не начинается раньше ответа на плейлист, поэтому разница 4с/6с
    /// достаточна, чтобы смоделировать «плейлист успел, state не успел» при
    /// бюджете 5 с). `exit` укладывает плейлист в бюджет, а `state.toml`
    /// уходит в `timed_out` (ТЗ-14, ТЗ-32, НФ-9, §6.10
    /// `exit_partial_within_budget`).
    #[test]
    fn exit_partial_within_budget() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        core.playlist_add(vec![track_path("track1.flac")]);
        core.change_state(Origin::User, StateChange::Volume(42));

        h.delay_write_work(WorkFile::Playlist, Duration::from_secs(4));
        h.delay_write_work(WorkFile::State, Duration::from_secs(6));

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        assert_eq!(h.disk_bytes(WorkFile::Playlist).as_deref(), Some(&b"track1.flac\n"[..]));
        assert_eq!(h.disk_bytes(WorkFile::State), Some(b"".to_vec()));

        let report = h
            .journal()
            .into_iter()
            .find_map(|r| match r {
                JournalRecord::ExitSummary(report) => Some(report),
                _ => None,
            })
            .expect("ExitSummary journaled");
        assert_eq!(report.written, vec![WorkFile::Playlist]);
        assert_eq!(report.timed_out, vec![WorkFile::State]);

        // `state.toml` всё ещё ждёт свою задержку (t0+6с) — дать писателю
        // ответить, прежде чем стенд уничтожится (см. комментарий выше).
        h.settle(&mut core);
    }

    /// Запись `state.toml` по отсчёту уже провалилась («нет места») и
    /// таймер остановлен (ТЗ-20, §6.8) — путь выхода не смотрит на
    /// `timer_stopped`, поэтому при выходе отправляется ровно одна новая
    /// попытка, и она проходит (внедрённая неудача была одноразовой)
    /// (ТЗ-14, ТЗ-20, ТЗ-32, НФ-9, §6.10
    /// `exit_retries_previously_failed_file_once`).
    #[test]
    fn exit_retries_previously_failed_file_once() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.change_state(Origin::User, StateChange::Volume(42));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert!(core.tracker().timer_stopped(WorkFile::State));

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.writes(WorkFile::State), 1);

        let report = h
            .journal()
            .into_iter()
            .find_map(|r| match r {
                JournalRecord::ExitSummary(report) => Some(report),
                _ => None,
            })
            .expect("ExitSummary journaled");
        assert_eq!(report.written, vec![WorkFile::State]);
    }

    /// Запись `state.toml` и `settings.toml` путём выхода — ровно по одному
    /// снимку на изменённый файл для КАЖДОЙ причины выхода, включая все три
    /// сигнала завершения процесса и обработчики ОС (ТЗ-14, ТЗ-15, §7.2
    /// `exit_writes_settings_once`): без изменений оба файла остаются в
    /// `report.unchanged` без единой записи, изменение обоих — по одной
    /// записи на файл.
    #[test]
    fn exit_writes_settings_once() {
        let reasons = [
            ExitReason::WindowClose,
            ExitReason::TrayQuit,
            ExitReason::Signal(TermSignal::Term),
            ExitReason::Signal(TermSignal::Int),
            ExitReason::Signal(TermSignal::Hup),
            ExitReason::WindowsSessionEnd,
            ExitReason::MacosTerminate,
        ];
        let settings_bytes =
            crate::persist::settings_file::serialize_settings(&Settings::default()).expect("serialize settings");
        let state_bytes =
            crate::persist::state_file::serialize_state(&SessionState::default()).expect("serialize state");

        for reason in reasons {
            let h = Harness::new();
            h.put_settings(&settings_bytes);
            h.put_state(&state_bytes);
            let mut core = h.boot();

            let outcome = core.exit(reason, &mut || true);
            assert_eq!(outcome, ExitOutcome::Completed, "{reason:?}: без изменений");
            assert_eq!(h.writes(WorkFile::State), 0, "{reason:?}: state.toml не пишется без изменений");
            assert_eq!(h.writes(WorkFile::Settings), 0, "{reason:?}: settings.toml не пишется без изменений");

            let h = Harness::new();
            h.put_settings(&settings_bytes);
            h.put_state(&state_bytes);
            let mut core = h.boot();

            core.change_state(Origin::User, StateChange::Volume(42));
            let mut settings = core.settings().clone();
            settings.playback.compatible.device = SharedDeviceChoice::Named(SharedDeviceId::new("hw:1,0"));
            core.set_settings(settings);

            let outcome = core.exit(reason, &mut || true);
            assert_eq!(outcome, ExitOutcome::Completed, "{reason:?}: с изменениями");
            assert_eq!(h.writes(WorkFile::State), 1, "{reason:?}: state.toml записан ровно раз");
            assert_eq!(h.writes(WorkFile::Settings), 1, "{reason:?}: settings.toml записан ровно раз");
        }
    }

    /// Выход шлёт движку ровно одну `EngineCmd::Shutdown` до ожидания его
    /// ответа (`await_engine` видит команду уже отправленной), ответ даёт
    /// `engine_ack` (§6.10, §6.28 п. 1 (01_audio_modes)).
    #[test]
    fn exit_sends_shutdown_to_engine() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        let shutdown = format!("{:?}", crate::engine::messages::EngineCmd::Shutdown);
        let mut seen_at_await = Vec::new();
        let outcome = core.exit(ExitReason::WindowClose, &mut || {
            seen_at_await = h.engine_cmds();
            true
        });
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(seen_at_await, vec![shutdown.clone()], "Shutdown отправлен до ожидания движка");
        assert_eq!(h.engine_cmds(), vec![shutdown], "ровно одна команда за выход");

        let report = h
            .journal()
            .into_iter()
            .find_map(|r| match r {
                JournalRecord::ExitSummary(report) => Some(report),
                _ => None,
            })
            .expect("ExitSummary journaled");
        assert!(report.engine_ack);
    }

    /// Движок не ответил `ShutdownComplete` в свой срок — `settings.toml` и
    /// `state.toml` всё равно записаны, `engine_ack = false` (ОВС-16,
    /// ТЗ-136, §6.28 п. 2а (01_audio_modes); `exit_does_not_wait_engine`).
    #[test]
    fn shutdown_writes_settings_even_on_timeout() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Volume(42));
        let mut settings = core.settings().clone();
        settings.playback.compatible.device = SharedDeviceChoice::Named(SharedDeviceId::new("hw:1,0"));
        core.set_settings(settings);

        let outcome = core.exit(ExitReason::WindowClose, &mut || false);
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.writes(WorkFile::Settings), 1, "settings.toml записан без ответа движка");
        assert_eq!(h.writes(WorkFile::State), 1, "state.toml записан без ответа движка");

        let report = h
            .journal()
            .into_iter()
            .find_map(|r| match r {
                JournalRecord::ExitSummary(report) => Some(report),
                _ => None,
            })
            .expect("ExitSummary journaled");
        assert!(!report.engine_ack);
        assert!(report.written.contains(&WorkFile::Settings));
        assert!(report.written.contains(&WorkFile::State));
        assert!(report.timed_out.is_empty());
    }

    /// Завершение сеанса Windows выполняет путь выхода синхронно: к моменту
    /// возврата `exit()` запись уже на диске, без дополнительного
    /// тика/`settle` (ADR-7, ТЗ-15, §7.2
    /// `windows_session_end_runs_exit_synchronously`). Цикл событий решение
    /// не завершает (`quits_event_loop() == false`) — это остаётся
    /// обработчику ОС.
    #[test]
    fn windows_session_end_runs_exit_synchronously() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Volume(42));

        let outcome = core.exit(ExitReason::WindowsSessionEnd, &mut || true);

        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.writes(WorkFile::State), 1);
        assert_eq!(core.exit_phase(), ExitPhase::Done);
        assert!(!ExitReason::WindowsSessionEnd.quits_event_loop());
    }

    /// Повторный запрос на выход во время уже идущего (синхронного) пути
    /// игнорируется (ТЗ-14, И-Т8): второй вызов `exit` сразу после первого
    /// (`TrayQuit` после `WindowClose`) возвращает `Ignored`, не меняет
    /// счётчики записи и не добавляет новую запись `ExitSummary` в журнал
    /// (ТЗ-14, ТЗ-32, НФ-9, §6.10 `repeated_tray_quit_ignored`).
    #[test]
    fn repeated_tray_quit_ignored() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        h.delay_write_work(WorkFile::State, Duration::from_secs(10));
        core.change_state(Origin::User, StateChange::Volume(42));

        let first = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(first, ExitOutcome::Completed);
        let writes_after_first = h.writes(WorkFile::State);

        let second = core.exit(ExitReason::TrayQuit, &mut || true);
        assert_eq!(second, ExitOutcome::Ignored);
        assert_eq!(h.writes(WorkFile::State), writes_after_first);

        let summaries =
            h.journal().iter().filter(|r| matches!(r, JournalRecord::ExitSummary(_))).count();
        assert_eq!(summaries, 1);

        // `state.toml` всё ещё ждёт свою задержку (t0+10с) — дать писателю
        // ответить, прежде чем стенд уничтожится (см. комментарий выше).
        h.settle(&mut core);
    }

    /// Нет места на диске при записи по отсчёту; пользователь не нажал
    /// «Повторить» — запись остаётся остановленной (ТЗ-20, §6.8). Путь
    /// выхода всё равно отправляет файл ещё раз без оглядки на
    /// `timer_stopped`, и он успешно уходит на диск (ТЗ-14, ТЗ-20, ТЗ-32,
    /// НФ-9, §6.10 `exit_after_space_freed_without_retry`).
    #[test]
    fn exit_after_space_freed_without_retry() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.change_state(Origin::User, StateChange::Volume(42));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert!(core.tracker().timer_stopped(WorkFile::State));

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        let expected = crate::persist::state_file::serialize_state(core.state()).expect("serialize state");
        assert_eq!(h.disk_bytes(WorkFile::State).as_deref(), Some(&expected[..]));
    }

    /// Запись рабочих файлов идёт только в потоке писателя `apap-persist`
    /// (ТЗ-22, НФ-5, И-Р11, §6.6): ни отложенная запись по сроку, ни
    /// «Сохранить», ни путь выхода не пишут из вызывающего (UI) потока.
    /// Чтение при старте (`boot`, до окна, ADR-23 шаг 2) в проверку не входит.
    #[test]
    fn no_file_io_on_ui_thread() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();
        let boot_calls = h.fs_calls().len();

        core.change_state(Origin::User, StateChange::Shuffle(true));
        core.playlist_add(vec![track_path("/music/a.flac")]);
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        let mut settings = core.settings().clone();
        settings.save_interval = SaveInterval::S10;
        core.save_settings_now(settings);
        h.settle(&mut core);

        core.change_state(Origin::User, StateChange::Volume(17));
        let outcome = core.exit(ExitReason::TrayQuit, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        let after_boot = &h.fs_calls()[boot_calls..];
        assert!(after_boot.iter().any(|c| c.op != FsOp::Read), "ожидались записи после старта");
        for call in after_boot {
            assert_eq!(&*call.thread, "apap-persist", "ввод-вывод вне писателя: {call:?}");
        }
        assert!(h.writes(WorkFile::Settings) >= 1);
        assert!(h.writes(WorkFile::State) >= 1);
        assert!(h.writes(WorkFile::Playlist) >= 1);
    }

    /// Проводит эффекты `ReplyEffect::Failed` тика через `MessageCenter` →
    /// `Notifier`, как это делает `apply_reply_effects`/`apply_msg_effect`
    /// в `app/mod.rs` (ADR-9, §7.2): здесь — минимальный повтор того же
    /// провода для тестов стенда `AppCore` без Slint.
    fn apply_write_failures(
        effects: Vec<ReplyEffect>,
        messages: &mut MessageCenter,
        notifier: &FakeNotifier,
        caps: PlatformCaps,
    ) {
        for effect in effects {
            if let ReplyEffect::Failed(file, class) = effect {
                let msg_effect = messages.write_failed(file, class, caps);
                if let Some(n) = msg_effect.notify {
                    notifier.notify(n);
                }
            }
        }
    }

    /// Окно скрыто в трей — ошибка записи `state.toml` уведомляет через
    /// `Notifier` ровно один раз, даже если следом падает ещё один файл
    /// (`settings.toml`); при открытии окна (`set_in_tray(false)`)
    /// показывается сводное окно Error, упоминающее `state.toml` (ADR-9,
    /// §7.2, ТЗ-52 `tray_hidden_error_notifies_once`).
    #[test]
    fn tray_hidden_error_notifies_once() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        let caps = PlatformCaps { tray: true, notifications: true };
        let mut messages = MessageCenter::default();
        let notifier = FakeNotifier::new();
        messages.window_shown();
        messages.set_in_tray(true);

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.change_state(Origin::User, StateChange::Volume(42));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        let effects: Vec<ReplyEffect> = h.settle_ticks(&mut core).into_iter().flatten().collect();
        apply_write_failures(effects, &mut messages, &notifier, caps);

        h.fail_write_work(WorkFile::Settings, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        let mut settings = core.settings().clone();
        settings.playback.compatible.device = SharedDeviceChoice::Named(SharedDeviceId::new("hw:1,0"));
        core.save_settings_now(settings);
        let effects: Vec<ReplyEffect> = h.settle_ticks(&mut core).into_iter().flatten().collect();
        apply_write_failures(effects, &mut messages, &notifier, caps);

        assert_eq!(notifier.count(), 1, "уведомление — одно на появление сводной записи");

        let effect = messages.set_in_tray(false);
        let shown = effect.show.expect("окно ошибок записи показано при открытии");
        assert_eq!(shown.level, MessageLevel::Error);
        assert!(shown.body.contains("state.toml"));
    }

    /// Без трея (`PlatformCaps::default()`) ошибка записи `state.toml` сразу
    /// открывает окно Error, без единого уведомления через `Notifier`
    /// (ADR-9, §7.2, ТЗ-52 `no_tray_error_shows_window`).
    #[test]
    fn no_tray_error_shows_window() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        let caps = PlatformCaps::default();
        let mut messages = MessageCenter::default();
        let notifier = FakeNotifier::new();
        messages.window_shown();

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.change_state(Origin::User, StateChange::Volume(42));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        let effects: Vec<ReplyEffect> = h.settle_ticks(&mut core).into_iter().flatten().collect();
        apply_write_failures(effects, &mut messages, &notifier, caps);

        let shown = messages.shown().expect("окно ошибки записи показано без трея");
        assert_eq!(shown.level, MessageLevel::Error);
        assert_eq!(notifier.count(), 0);
    }

    /// `TrayQuit` при открытом окне Error закрывает его без вопроса —
    /// `messages.dismiss_for_exit()` вызывается ДО `core.exit()`, как в
    /// `app::exit` (ADR-7, §6.10); путь выхода делает ровно одну новую
    /// попытку записи `state.toml`, которая проходит (ADR-9, §7.2, ТЗ-14,
    /// ТЗ-52 `tray_quit_closes_error_window_one_attempt`).
    #[test]
    fn tray_quit_closes_error_window_one_attempt() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        let caps = PlatformCaps { tray: true, notifications: true };
        let mut messages = MessageCenter::default();
        let notifier = FakeNotifier::new();
        messages.window_shown();

        h.fail_write_work(WorkFile::State, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.change_state(Origin::User, StateChange::Volume(42));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        let effects: Vec<ReplyEffect> = h.settle_ticks(&mut core).into_iter().flatten().collect();
        apply_write_failures(effects, &mut messages, &notifier, caps);
        assert!(messages.is_shown(), "окно Error открыто после неудачной записи");

        let before = h.state_counts();

        messages.dismiss_for_exit();
        assert!(!messages.is_shown(), "окно закрыто без вопроса");

        let outcome = core.exit(ExitReason::TrayQuit, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        let after = h.state_counts();
        let attempts_before = before.writes + before.failures;
        let attempts_after = after.writes + after.failures;
        assert_eq!(attempts_after - attempts_before, 1, "путь выхода делает ровно одну новую попытку");
        assert_eq!(after.writes, before.writes + 1, "вторая попытка успешна — файл записан");
        assert!(!messages.is_shown(), "после выхода ничего не показано заново");
    }

    /// Подмены платформы покрывают выход, ФС и уведомления уже на этом
    /// этапе (ТЗ-54): поведение работает целиком на подменах, на любой ОС,
    /// без `cfg(target_os)`. Подмены ввода и выбора файла (ТЗ-54 п.2–3)
    /// появятся на следующих этапах — здесь не проверяются. `FakeLifecycle`
    /// принимает `ExitEntry`, которая приводит в действие `AppCore::exit`
    /// того же стенда: запись уходит в `MemStore`, как и в прямых вызовах
    /// `core.exit` в тестах выше.
    #[test]
    fn platform_fakes_cover_exit_fs_input_notify_pick() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let core = Rc::new(RefCell::new(h.boot()));
        core.borrow_mut().change_state(Origin::User, StateChange::Volume(42));

        let mut lifecycle = FakeLifecycle::with_tray();
        let (tray_tx, _tray_rx) = mpsc::channel::<TrayEvent>();
        let core_for_exit = Rc::clone(&core);
        let entry: ExitEntry = Rc::new(move |reason| {
            core_for_exit.borrow_mut().exit(reason, &mut || true)
        });
        lifecycle.install(None, entry, tray_tx).expect("install succeeds on the fake");

        let outcome = lifecycle.fire(ExitReason::WindowClose).expect("ExitEntry installed");
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.writes(WorkFile::State), 1, "exit пишет через ту же ФС-подмену");

        let notifier = FakeNotifier::new();
        notifier.notify(Notification { level: MessageLevel::Error, title: "title".into(), body: "body".into() });
        assert_eq!(notifier.count(), 1);
        assert_eq!(&*notifier.calls()[0].body, "body");
    }

    /// Деление колёсика над значком трея в тестах: неизвестный хост
    /// встроенной таблицы берёт единицей модуль первого события (§6.11
    /// «Трей», В-5), поэтому `+TRAY_NOTCH` — один щелчок «громче».
    const TRAY_NOTCH: i32 = 120;

    /// Прокрутка над значком трея тем же путём, что `TrayEvent::Scroll` в
    /// `poll_tray` (`app/mod.rs`; ADR-8, §6.11 «Трей», В-5, ТЗ-24, ТЗ-36):
    /// событие нормализует модуль ввода ядра (`input_mut().tray_scroll`),
    /// ненулевые щелчки уходят в единую команду громкости `AppCore::volume`
    /// с источником `TrayWheel` — её же вызывает `apply_volume_cmd`. Шлюз
    /// главного окна на этом пути не спрашивается. `None` — щелчков нет,
    /// команда не подана.
    fn tray_wheel(h: &Harness, core: &mut AppCore, delta: i32) -> Option<VolumeOutcome> {
        let ev = TrayScroll { t: h.clock.now(), delta, vertical: true };
        let clicks = core.input_mut().tray_scroll(ev);
        (clicks.0 != 0).then(|| core.volume(VolumeSource::TrayWheel, VolumeCmd::Step(clicks)))
    }

    /// Трей работает во время диалога настроек (ТЗ-24, §5.1: строка «с
    /// треем; диалог»; ADR-8, §6.11 «Трей», В-5): `UiGate` блокирует команды
    /// главного окна, но трей не ходит через `UiGate` (`poll_tray` в
    /// `app/mod.rs` подаёт `TrayScroll` в модуль ввода ядра и щелчки — в
    /// команду громкости) — пять щелчков колёсика над значком (+10 %) и
    /// «Сохранить» с одним изменённым параметром (SRC-фильтр ресемплера)
    /// проходят несмотря на блокировку: одна запись `settings.toml`, затем
    /// одна запись `state.toml` по отсчёту с новой громкостью.
    #[test]
    fn tray_works_during_dialog() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let mut gate = UiGate::default();
        gate.block(BlockReason::Dialog);
        assert!(!gate.allows(MainCmd::PlayPause), "главное окно блокировано диалогом настроек");

        // Исходный уровень ниже максимума, чтобы +10 % были видны.
        core.change_state(Origin::User, StateChange::Volume(32));

        // Пять событий по одному делению: `TrayEvent::Scroll` → модуль ввода
        // → `VolumeCmd::Step`, мимо `UiGate`. Каждый щелчок — +2 %.
        for step in 1..=5u8 {
            assert_eq!(
                tray_wheel(&h, &mut core, TRAY_NOTCH),
                Some(VolumeOutcome::Changed(vol(32 + 2 * step))),
                "щелчок {step}"
            );
        }
        assert_eq!(core.state().playback().volume, 42, "колёсико трея: +5 щелчков = +10 %");

        let mut settings = core.settings().clone();
        settings.playback.compatible.src_filter = SrcFilter::VerySlow;
        core.save_settings_now(settings);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 1, "«Сохранить» — одна запись settings.toml");

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1, "громкость уходит в state.toml по отсчёту");
        assert_eq!(core.state().playback().volume, 42);
    }

    /// Трей работает во время открытого окна сообщения (ТЗ-24, ТЗ-52):
    /// `UiGate` блокирован причиной `Message` (как при открытом окне
    /// ошибки записи), но трей шлёт изменение громкости тем же путём, что
    /// и `TrayEvent::Scroll` в `poll_tray`: событие — в модуль ввода ядра,
    /// щелчки — в команду громкости (ADR-8, §6.11 «Трей», В-5; один щелчок
    /// = 2 %); изменение доходит до `state.toml` по отсчёту, несмотря на
    /// блокировку.
    #[test]
    fn tray_works_during_message() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let mut gate = UiGate::default();
        gate.block(BlockReason::Message);
        assert!(!gate.allows(MainCmd::Volume), "главное окно блокировано сообщением");

        // Исходный уровень ниже максимума, чтобы щелчок колёсика вверх
        // (+2 %) был виден.
        core.change_state(Origin::User, StateChange::Volume(50));
        let after = core.state().playback().volume + 2;
        // Одно деление над значком трея: модуль ввода → `VolumeCmd::Step`.
        assert_eq!(tray_wheel(&h, &mut core, TRAY_NOTCH), Some(VolumeOutcome::Changed(vol(52))));
        assert_eq!(after, 52);
        // Событие без щелчков (`delta == 0`) команду громкости не подаёт —
        // как в `poll_tray`.
        assert_eq!(tray_wheel(&h, &mut core, 0), None);

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1);
        assert_eq!(core.state().playback().volume, after);
    }

    /// Таблица модуля ввода с двумя строками трея (§2.8, §6.11 «Трей», В-5):
    /// неизвестный хост — «вверх = громче», деление `TRAY_NOTCH`; GNOME
    /// AppIndicator — обратный знак, то же деление. Строк ползунка нет.
    fn tray_table() -> WheelTable {
        let row = |host, sign| TrayRow { host, sign, division: TRAY_NOTCH.unsigned_abs(), outcome: Outcome::Experiment };
        WheelTable {
            tray: vec![row(TrayHost::Unknown, 1), row(TrayHost::GnomeAppIndicator, -1)],
            ..WheelTable::builtin()
        }
    }

    /// `AppCore::set_tray_host` фиксирует хост трея и способ его определения
    /// в журнале (ADR-8, §2.9, §6.11 «Хост трея», ТЗ-36): одна запись
    /// `JournalRecord::TrayHost` на вызов, с переданными значениями и в
    /// порядке вызовов.
    #[test]
    fn set_tray_host_journals_host_and_source() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let tray_hosts = |h: &Harness| -> Vec<(TrayHost, HostSource)> {
            h.journal()
                .into_iter()
                .filter_map(|r| match r {
                    JournalRecord::TrayHost { host, via } => Some((host, via)),
                    _ => None,
                })
                .collect()
        };
        assert!(tray_hosts(&h).is_empty(), "до события `TrayEvent::Host` записи нет");

        core.set_tray_host(TrayHost::KdePlasma, HostSource::DbusOwner);
        assert_eq!(tray_hosts(&h), [(TrayHost::KdePlasma, HostSource::DbusOwner)]);

        core.set_tray_host(TrayHost::Unknown, HostSource::None);
        assert_eq!(
            tray_hosts(&h),
            [(TrayHost::KdePlasma, HostSource::DbusOwner), (TrayHost::Unknown, HostSource::None)]
        );
    }

    /// `AppCore::set_input` заменяет модуль ввода (ADR-8, §2.12, §6.11,
    /// ТЗ-36): прежний отбрасывается вместе с состоянием, `input_mut` отдаёт
    /// результат новой подмены. Модуль по умолчанию (неизвестный хост,
    /// единица — модуль первого события) после события в 40 считает 20
    /// половиной щелчка; свежая подмена с той же таблицей берёт 20 новой
    /// единицей, а подмена с таблицей `tray_table` делит на `TRAY_NOTCH`.
    #[test]
    fn set_input_replaces_wheel_input() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let scroll = |core: &mut AppCore, delta| {
            core.input_mut().tray_scroll(TrayScroll { t: h.clock.now(), delta, vertical: true })
        };

        assert_eq!(scroll(&mut core, 40), Clicks(1), "единица модуля по умолчанию — 40");
        assert_eq!(scroll(&mut core, 20), Clicks(0), "20 — половина единицы прежнего модуля");

        // Та же встроенная таблица, новое состояние: накопитель и единица
        // прежнего модуля отброшены.
        core.set_input(Box::new(FakeWheelInput::builtin(Desktop::OtherLinux, None)));
        assert_eq!(scroll(&mut core, 20), Clicks(1), "единица новой подмены — 20");

        // Другая таблица: деление `TRAY_NOTCH`, а не модуль первого события.
        core.set_input(Box::new(FakeWheelInput::new(tray_table(), Desktop::Gnome, Some(Session::Wayland))));
        assert_eq!(scroll(&mut core, 20), Clicks(0));
        assert_eq!(scroll(&mut core, 2 * TRAY_NOTCH), Clicks(2));
    }

    /// `AppCore::set_tray_host` доходит до модуля ввода (ADR-8, §6.11 «Хост
    /// трея», В-5, ТЗ-36): хост выбирает строку таблицы для `tray_scroll`.
    /// До вызова действует строка неизвестного хоста («вверх = громче»),
    /// после `GnomeAppIndicator` — строка с обратным знаком, и та же
    /// прокрутка убавляет громкость командой `TrayWheel`.
    #[test]
    fn set_tray_host_selects_tray_row() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        core.set_input(Box::new(FakeWheelInput::new(tray_table(), Desktop::Gnome, Some(Session::Wayland))));
        core.change_state(Origin::User, StateChange::Volume(50));

        assert_eq!(tray_wheel(&h, &mut core, TRAY_NOTCH), Some(VolumeOutcome::Changed(vol(52))));

        core.set_tray_host(TrayHost::GnomeAppIndicator, HostSource::XdgCurrentDesktop);
        assert_eq!(tray_wheel(&h, &mut core, TRAY_NOTCH), Some(VolumeOutcome::Changed(vol(50))));
        assert_eq!(tray_wheel(&h, &mut core, -2 * TRAY_NOTCH), Some(VolumeOutcome::Changed(vol(54))));
        assert_eq!(core.state().playback().volume, 54);
    }

    /// Опрашивает future один раз без реального исполнителя — как
    /// `poll_once` в тестах `FakePicker` (`platform/pick/fake.rs`).
    fn poll_once<F: Future + ?Sized>(fut: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        fut.as_mut().poll(&mut cx)
    }

    /// Меню неактивно, пока открыто окно сообщения (ТЗ-23, ТЗ-52 п. 1,
    /// ADR-12, И-Р8, §7.2): слой UI взводит причину `BlockReason::Message` на
    /// шлюзе ядра (`show_message` в `app/mod.rs`), и шлюз `AppCore` отклоняет
    /// каждую `MainCmd`. Открытие диалога настроек — тоже команда главного
    /// окна: `dialog_open` возвращает `None` — черновик не создан, заданий
    /// `apap-io` нет, настройки, состояние, плейлист и команды движку
    /// прежние, записей нет. Закрытие окна сообщения (`unblock`) возвращает
    /// меню в прежнее состояние.
    #[test]
    fn menu_inactive_while_message_open() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        for cmd in all_main_cmds() {
            assert!(core.gate().allows(cmd), "{cmd:?} разрешена до открытия окна сообщения");
        }

        core.gate_mut().block(BlockReason::Message);
        for cmd in all_main_cmds() {
            assert!(!core.gate().allows(cmd), "{cmd:?} must be denied while message window is open");
        }
        assert!(core.gate().window_blocked());

        let settings = core.settings().clone();
        let state = core.state().clone();
        let engine = h.engine_cmds();
        let playlist_len = core.playlist().len();

        assert_eq!(core.dialog_open(), None, "открытие диалога отклонено шлюзом");

        assert!(core.draft().is_none(), "черновик не создан");
        assert!(h.io.jobs().is_empty(), "задания apap-io не отправлены");
        assert_eq!(h.io.count_cache_sizes(), 0);
        assert_eq!(core.settings(), &settings);
        assert_eq!(core.state(), &state);
        assert_eq!(h.engine_cmds(), engine);
        assert_eq!(core.playlist().len(), playlist_len);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 0);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(h.writes(WorkFile::Playlist), 0);

        core.gate_mut().unblock(BlockReason::Message);
        assert!(!core.gate().window_blocked());
        for cmd in all_main_cmds() {
            assert!(core.gate().allows(cmd), "{cmd:?} снова разрешена после закрытия окна сообщения");
        }
    }

    /// Меню неактивно, пока открыт выбор файла (§4.4 ТЗ, §7.2, ТЗ-23,
    /// ТЗ-53, ADR-10, ADR-12): модель потока `pick_async` из `app/mod.rs` на
    /// шлюзе ядра — разрешённая команда взводит на шлюзе `AppCore` причину
    /// `FilePicker` и запускает `FakePicker::never()` (future никогда не
    /// завершается, диалог «висит открытым»). Пока он не отвечен, каждая
    /// `MainCmd` отклонена шлюзом, включая «Добавить файлы / папку»,
    /// «Загрузить / Сохранить плейлист»; повторная попытка выбора, как и в
    /// обработчике `app/mod.rs`, сначала проверяет `gate.allows` и до
    /// `picker.pick` не доходит — `FakePicker::calls()` остаётся равным 1.
    /// `dialog_open` возвращает `None`: черновик не создан, заданий `apap-io`
    /// нет. Снятие блокировки возвращает меню в прежнее состояние.
    #[test]
    fn menu_inactive_while_picker_open() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let picker = FakePicker::never();

        assert!(core.gate().allows(MainCmd::AddFiles), "выбор файла разрешён до открытия диалога");
        core.gate_mut().block(BlockReason::FilePicker);
        let mut fut = picker.pick(PickRequest::AddFiles { start: None }, None);
        assert!(matches!(poll_once(&mut fut), Poll::Pending), "диалог «висит открытым»");
        assert_eq!(picker.calls(), 1);

        for cmd in all_main_cmds() {
            assert!(!core.gate().allows(cmd), "{cmd:?} must be denied while file picker is open");
        }
        assert!(core.gate().window_blocked());

        // Второй выбор, как в обработчике `app/mod.rs`: сначала проверяется
        // `gate.allows`, и только при `true` вызывается `picker.pick`.
        for cmd in [MainCmd::AddFiles, MainCmd::AddFolder, MainCmd::LoadPlaylist, MainCmd::SavePlaylist] {
            if core.gate().allows(cmd) {
                drop(picker.pick(PickRequest::AddFiles { start: None }, None));
            }
        }
        assert_eq!(picker.calls(), 1, "второй выбор не открыт — picker.pick не вызван");

        let settings = core.settings().clone();
        let state = core.state().clone();
        assert_eq!(core.dialog_open(), None, "открытие диалога отклонено шлюзом");
        assert!(core.draft().is_none(), "черновик не создан");
        assert!(h.io.jobs().is_empty(), "задания apap-io не отправлены");
        assert_eq!(core.settings(), &settings);
        assert_eq!(core.state(), &state);
        assert_eq!(core.playlist().len(), 0);

        drop(fut);
        core.gate_mut().unblock(BlockReason::FilePicker);
        assert!(!core.gate().window_blocked());
        for cmd in all_main_cmds() {
            assert!(core.gate().allows(cmd), "{cmd:?} снова разрешена после закрытия диалога выбора");
        }
    }

    /// Открытый выбор файла не останавливает цикл событий (ТЗ-53, ADR-10,
    /// ADR-12, §6.4, §7.2): `FakePicker::never()` «никогда не отвечает»,
    /// причина `FilePicker` взведена на шлюзе ядра и не снимается. За 30 с
    /// посекундных тиков ядро не блокируется, и срок N, взведённый изменением
    /// громкости до открытия выбора, срабатывает — `state.toml` записан
    /// ровно один раз. Колёсико трея при открытом выборе меняет громкость:
    /// команды трея идут мимо шлюза главного окна (ADR-12, ТЗ-24) — тот же
    /// путь, что `TrayEvent::Scroll` в `poll_tray`: событие нормализует
    /// модуль ввода ядра, щелчки идут в команду громкости с источником
    /// `TrayWheel` (ADR-8, §6.11 «Трей», В-5) и меняют действующее состояние,
    /// хотя `MainCmd::Volume` главного окна шлюзом отклонена; новое значение
    /// доходит до `state.toml` следующим сроком N.
    #[test]
    fn picker_does_not_block_loop() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let picker = FakePicker::never();

        // Изменение состояния до открытия выбора взводит срок N (ТЗ-11).
        core.change_state(Origin::User, StateChange::Volume(40));
        let interval = core.settings().save_interval.duration();
        assert_eq!(interval, Duration::from_secs(30), "N по умолчанию — 30 с");

        core.gate_mut().block(BlockReason::FilePicker);
        let mut fut = picker.pick(PickRequest::AddFiles { start: None }, None);
        assert!(matches!(poll_once(&mut fut), Poll::Pending), "выбор файла «висит открытым»");

        // 29 тиков по 1 с: цикл идёт, срок N ещё не наступил.
        for _ in 0..29 {
            h.advance(&mut core, Duration::from_secs(1));
            assert!(matches!(poll_once(&mut fut), Poll::Pending));
        }
        assert_eq!(h.writes(WorkFile::State), 0, "до срока N записи нет");

        // 30-й тик: срок N срабатывает при открытом выборе файла.
        h.advance(&mut core, Duration::from_secs(1));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1, "срок N сработал при открытом выборе файла");
        assert!(core.gate().window_blocked(), "выбор файла всё ещё открыт");
        assert!(matches!(poll_once(&mut fut), Poll::Pending));

        // Колёсико трея: мимо шлюза главного окна (ADR-12, ТЗ-24).
        assert!(!core.gate().allows(MainCmd::Volume), "громкость из главного окна шлюзом отклонена");
        let after = core.state().playback().volume + 2;
        assert_eq!(tray_wheel(&h, &mut core, TRAY_NOTCH), Some(VolumeOutcome::Changed(vol(after))));
        assert_eq!(core.state().playback().volume, after, "колёсико трея меняет громкость");

        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 2, "громкость из трея записана следующим сроком N");
        assert_eq!(picker.calls(), 1);
        assert!(core.gate().window_blocked());
    }

    /// «Очистить плейлист» из меню взводит флаг изменений плейлиста (ТЗ-34,
    /// ТЗ-12, §6.9, §6.13, §6.4, §7.2): очистка плейлиста — команда главного
    /// окна, а не действие диалога настроек; шлюз её пропускает
    /// (`MainCmd::ClearPlaylist`), `playlist_clear` опустошает плейлист и
    /// взводит флаг; через N с — ровно одна запись `playlist.m3u` (пустого),
    /// флаг снят, дальнейших записей нет.
    #[test]
    fn clear_playlist_menu_marks_dirty() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let interval = core.settings().save_interval.duration();

        // Непустой плейлист, уже записанный на диск: флаг снят.
        core.playlist_add(vec![track_path("track1.flac"), track_path("track2.flac")]);
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(core.playlist().len(), 2);
        assert_eq!(h.writes(WorkFile::Playlist), 1);
        assert!(!core.tracker().playlist_dirty());
        let filled = h.disk_bytes(WorkFile::Playlist).expect("playlist.m3u на диске");

        assert!(core.gate().allows(MainCmd::ClearPlaylist), "«Очистить плейлист» разрешена шлюзом");
        core.playlist_clear();
        assert_eq!(core.playlist().len(), 0, "плейлист пуст");
        assert!(core.tracker().playlist_dirty(), "флаг изменений плейлиста взведён");
        assert_eq!(h.writes(WorkFile::Playlist), 1, "до срока N записи нет");

        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 2, "через N с — ровно одна запись плейлиста");
        assert!(!core.tracker().playlist_dirty());
        let empty = h.disk_bytes(WorkFile::Playlist).expect("playlist.m3u на диске");
        assert_eq!(empty, core.playlist_m3u().to_vec(), "записан пустой плейлист");
        assert_ne!(empty, filled);
        let text = String::from_utf8(empty).expect("playlist.m3u is valid utf-8");
        assert!(!text.contains("track1.flac") && !text.contains("track2.flac"), "путей треков нет: {text}");

        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 2, "повторной записи нет");
    }

    /// Трек с единственным заданным полем — путём (§4.2, ТЗ-12): тестам
    /// записи `playlist.m3u`, которым нужен только исходный порядок путей.
    fn track_path(path: &str) -> (Track, CompareKeys) {
        let t = Track { path: PathBuf::from(path), ..Track::default() };
        let keys = CompareKeys::from_track(&t);
        (t, keys)
    }

    /// Трек для тестов сортировки (§7, ТЗ-12, ТЗ-40, ТЗ-43): путь и
    /// исполнитель — единственные поля, которые задействует сравнение по
    /// колонке «Исполнитель» (`CompareKeys::from_track`).
    fn track_with_artist(path: &str, artist: &str) -> (Track, CompareKeys) {
        let t = Track { path: PathBuf::from(path), artist: Some(artist.into()), ..Track::default() };
        let keys = CompareKeys::from_track(&t);
        (t, keys)
    }

    /// Трек для теста экспорта (ADR-17, §6.18, ТЗ-13 а): путь и название —
    /// единственные поля, которые задействуют сравнение по колонке
    /// «Название» и подпись `#EXTINF` (`serialize_extm3u`).
    fn track_with_title(path: &str, title: &str) -> (Track, CompareKeys) {
        let t = Track { path: PathBuf::from(path), title: title.into(), ..Track::default() };
        let keys = CompareKeys::from_track(&t);
        (t, keys)
    }

    /// Видимый порядок, который построил бы загрузчик (ОТКЛОНЕНИЕ §7,
    /// ТЗ-40, ТЗ-43): индексы `rows` в исходном порядке, затем стабильная
    /// сортировка по `key` — та же логика, что `Playlist::resort` (§3.2,
    /// §6.13, И-Р14), но без сборки `Playlist`.
    fn loader_visible_order(rows: &[(Track, CompareKeys)], key: Option<SortKey>) -> Vec<u32> {
        let mut visible: Vec<u32> = (0..rows.len() as u32).collect();
        if let Some(key) = key {
            visible.sort_by(|&a, &b| compare_keys(&rows[a as usize].1, &rows[b as usize].1, key));
        }
        visible
    }

    /// Клик по заголовку сортировки взводит срок записи `state.toml`
    /// (`StateChange::Sort`, ТЗ-40), но не трогает флаг «грязного» плейлиста
    /// (ТЗ-12, §7): по отсчёту пишется только `state.toml`.
    #[test]
    fn sort_change_does_not_write_playlist() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let rows = vec![track_with_artist("/music/b.flac", "Bob"), track_with_artist("/music/a.flac", "Alice")];
        core.playlist_replace(rows, vec![0, 1], None);
        assert!(!core.tracker().playlist_dirty());

        core.playlist_header_click(ColumnId::Artist);
        assert!(!core.tracker().playlist_dirty());
        let expected_state = crate::persist::state_file::serialize_state(core.state()).expect("serialize state");

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::Playlist), 0);
        assert_eq!(h.writes(WorkFile::State), 1);
        assert_eq!(h.disk_bytes(WorkFile::State).as_deref(), Some(expected_state.as_ref()));
    }

    /// Ключ сортировки переживает перезапуск (ТЗ-40, ТЗ-43, §7).
    ///
    /// Тест проверяет только модель: эмулирует загрузчика вручную — переносит ключ сортировки,
    /// прочитанный из `state.toml` после перезапуска, напрямую в модель
    /// плейлиста и пересчитывает видимый порядок так же, как это сделал
    /// бы загрузчик (`loader_visible_order`). Путь через настоящую стартовую
    /// загрузку (`start_startup_load`, ADR-16) — `startup_load_shows_full_sorted_list`.
    #[test]
    fn sort_key_restored_after_restart() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let rows = vec![
            track_with_artist("/music/b.flac", "Bob"),
            track_with_artist("/music/a.flac", "Alice"),
            track_with_artist("/music/c.flac", "Carol"),
        ];
        core.playlist_replace(rows.clone(), vec![0, 1, 2], None);

        core.playlist_header_click(ColumnId::Artist); // Asc
        core.playlist_header_click(ColumnId::Artist); // Desc — нетривиальный ключ
        let sort_before = core.playlist().sort_key().expect("ключ сортировки взведён");
        let order_before: Vec<PathBuf> = {
            let pl = core.playlist();
            pl.visible().iter().filter_map(|&id| pl.get(id)).map(|t| t.path.clone()).collect()
        };

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);
        let state_bytes = h.disk_bytes(WorkFile::State).expect("state.toml записан на выходе");

        let h2 = Harness::new();
        h2.put_state(&state_bytes);
        let mut core2 = h2.boot();

        let sort_after = core2.state().sort();
        assert_eq!(sort_after, Some(sort_before));
        let visible2 = loader_visible_order(&rows, sort_after);
        core2.playlist_replace(rows, visible2, sort_after);

        let order_after: Vec<PathBuf> = {
            let pl = core2.playlist();
            pl.visible().iter().filter_map(|&id| pl.get(id)).map(|t| t.path.clone()).collect()
        };
        assert_eq!(order_after, order_before);
        assert_eq!(core2.playlist().sort_key(), Some(sort_before));
    }

    /// Случай 1 (ТЗ-21, ADR-16, §6.12): `playlist.m3u` отсутствует на
    /// диске — стартовая загрузка отдаёт пустой плейлист без сообщения, и
    /// запись `playlist.m3u` не происходит до первого изменения.
    #[test]
    fn startup_playlist_missing() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.start_startup_load();
        assert!(h.finish_startup_load());
        let report = core.poll_load().expect("итог стартовой загрузки получен");
        assert!(report.applied.is_none());
        assert!(report.message.is_none());
        assert!(core.playlist().is_empty());
        assert_eq!(h.writes(WorkFile::Playlist), 0);

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
    }

    /// Случай 2 (ТЗ-21, НФ-7, ADR-16, §6.12): `playlist.m3u` повреждён —
    /// три варианта повреждения (невалидный UTF-8; относительный путь;
    /// строка `;...`, которую `parse_startup` не считает комментарием, в
    /// отличие от `#...`, см. `startup_parse_semicolon_line_is_corrupt` в
    /// `playlist::load`) уходят в карантин через писателя: исходный файл
    /// заменяется байт-в-байт равной копией `playlist.m3u.bad`,
    /// `playlist.m3u` отсутствует до первой новой записи, в журнал попадает
    /// ровно одна запись `PlaylistCorrupt`, и пользователю показывается
    /// ровно одно предупреждение (сообщение карантина приходит позже,
    /// ответом писателя внутри `tick`, а не синхронно из `poll_load`).
    #[test]
    fn startup_playlist_corrupt_quarantined() {
        let cases: [&[u8]; 3] = [&[0xff, 0xfe, 0xfd], b"relative/path.flac\n", b";comment\n"];
        for original in cases {
            let h = Harness::new();
            h.put_settings(b"");
            h.put_state(b"");
            h.put_playlist(original);
            let mut core = h.boot();

            core.start_startup_load();
            assert!(h.finish_startup_load());
            let report = core.poll_load().expect("итог стартовой загрузки получен");
            assert!(report.applied.is_none());
            assert!(report.message.is_none(), "сообщение карантина приходит позже, через tick");

            let messages = h.settle_messages(&mut core);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].level, MessageLevel::Warning);

            assert_eq!(h.disk_bytes(WorkFile::Playlist), None);
            assert_eq!(h.bad_copy_bytes(WorkFile::Playlist).as_deref(), Some(original));
            assert_eq!(h.writes(WorkFile::Playlist), 0);

            let corrupt_records = h
                .journal()
                .into_iter()
                .filter(|r| matches!(r, JournalRecord::PlaylistCorrupt { .. }))
                .count();
            assert_eq!(corrupt_records, 1);
        }
    }

    /// Случай 2 (ТЗ-21, ADR-16, §6.12), ветка неудачи переноса: `playlist.m3u`
    /// повреждён, но сам перенос в карантин не удался — `rename_replace`
    /// проверяет внедрённую ошибку шага `Replace` ДО удаления исходного
    /// файла (§6.6), поэтому исходный файл остаётся нетронутым; запись
    /// `playlist.m3u` запрещается до перезапуска, и пользователю
    /// показывается ровно одна ошибка.
    #[test]
    fn startup_playlist_quarantine_failed_is_case_3() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let original: &[u8] = &[0xff, 0xfe, 0xfd];
        h.put_playlist(original);
        h.fail_write_bad_copy(WorkFile::Playlist, WriteStep::Replace, WriteErrorClass::Io);
        let mut core = h.boot();

        core.start_startup_load();
        assert!(h.finish_startup_load());
        let report = core.poll_load().expect("итог стартовой загрузки получен");
        assert!(report.applied.is_none());
        assert!(report.message.is_none());

        let messages = h.settle_messages(&mut core);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].level, MessageLevel::Error);

        assert_eq!(h.disk_bytes(WorkFile::Playlist).as_deref(), Some(original));
        assert!(core.tracker().playlist_forbidden());

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
    }

    /// Случай 3 (ТЗ-21, ADR-16, §6.12): `playlist.m3u` не прочитан (ошибка
    /// чтения, не связанная с содержимым) — запись `playlist.m3u`
    /// запрещается до перезапуска, в журнал попадает `ReadFailed`, и
    /// пользователю сразу (синхронно, в самом отчёте `poll_load`)
    /// показывается ровно одна ошибка; исходный файл не меняется.
    #[test]
    fn startup_playlist_unreadable() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let original = b"/music/a.flac\n";
        h.put_playlist(original);
        h.fail_read_work(WorkFile::Playlist, ReadErrorClass::Io);
        let mut core = h.boot();

        core.start_startup_load();
        assert!(h.finish_startup_load());
        let report = core.poll_load().expect("итог стартовой загрузки получен");
        assert!(report.applied.is_none());
        let message = report.message.expect("ошибка чтения приходит сразу");
        assert_eq!(message.level, MessageLevel::Error);
        assert!(core.tracker().playlist_forbidden());

        let read_failed = h
            .journal()
            .into_iter()
            .filter(|r| matches!(r, JournalRecord::ReadFailed { file: WorkFile::Playlist, .. }))
            .count();
        assert_eq!(read_failed, 1);

        core.playlist_add(vec![track_path("/music/b.flac")]);
        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
        assert_eq!(h.disk_bytes(WorkFile::Playlist).as_deref(), Some(original.as_slice()));
    }

    /// Абсолютные пути к несуществующим на диске файлам — не повреждение
    /// (ТЗ-21, ADR-16, §6.12): `parse_startup` не проверяет существование
    /// файла, поэтому такие строки дают обычный случай `Loaded` со
    /// строками плейлиста, без копии `.bad`.
    #[test]
    fn startup_playlist_nonexistent_paths_not_corrupt() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        h.put_playlist(b"/music/missing-a.flac\n/music/missing-b.flac\n");
        let mut core = h.boot();

        core.start_startup_load();
        assert!(h.finish_startup_load());
        let report = core.poll_load().expect("итог стартовой загрузки получен");
        assert_eq!(report.applied, Some(LoadApplied::Replaced));
        assert!(report.message.is_none());

        assert_eq!(core.playlist().len(), 2);
        assert_eq!(h.bad_copy_bytes(WorkFile::Playlist), None);
    }

    /// Пока стартовая загрузка не завершилась, плейлист пуст, а состояние
    /// загрузки — `Startup` (ТЗ-47, ADR-16, §6.12); после завершения —
    /// полный список в порядке, заданном ключом сортировки из `state.toml`.
    #[test]
    fn startup_load_shows_full_sorted_list() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"sort.column = \"title\"\nsort.direction = \"asc\"\n");
        h.put_playlist(b"/music/b.flac\n/music/a.flac\n/music/c.flac\n");
        let mut core = h.boot();

        core.start_startup_load();
        assert!(matches!(core.load_state(), LoadState::Startup { .. }));
        assert_eq!(core.playlist().len(), 0);

        assert!(h.finish_startup_load());
        let report = core.poll_load().expect("итог стартовой загрузки получен");
        assert_eq!(report.applied, Some(LoadApplied::Replaced));
        assert!(core.load_state().is_idle());

        let titles: Vec<String> = core
            .playlist()
            .visible()
            .iter()
            .filter_map(|&id| core.playlist().get(id))
            .map(|t| t.title.clone())
            .collect();
        assert_eq!(titles, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    /// Зависшая загрузка (ТЗ-17, ADR-16, §6.12): задание ушло в
    /// `ManualLoader`, но `finish` не вызывается — выход не ждёт
    /// загрузчик и не трогает существующий `playlist.m3u`.
    #[test]
    fn startup_load_hang_exit_keeps_playlist() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let original = b"/music/a.flac\n";
        h.put_playlist(original);
        let mut core = h.boot();

        core.start_startup_load();
        assert_eq!(h.loader.jobs().len(), 1, "задание ушло в загрузчик");

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        assert_eq!(h.writes(WorkFile::Playlist), 0);
        assert_eq!(h.disk_bytes(WorkFile::Playlist).as_deref(), Some(original.as_slice()));
    }

    /// Запрет записи `playlist.m3u` после ошибки чтения переживает попытки
    /// автосохранения и выход (ТЗ-12, ТЗ-21, ADR-16, §6.12): последующее
    /// изменение плейлиста не снимает запрет `forbid_playlist`.
    #[test]
    fn unreadable_playlist_never_written() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        h.fail_read_work(WorkFile::Playlist, ReadErrorClass::NoAccess);
        let mut core = h.boot();

        core.start_startup_load();
        assert!(h.finish_startup_load());
        core.poll_load();
        assert!(core.tracker().playlist_forbidden());

        core.playlist_add(vec![track_path("/music/a.flac")]);
        assert!(core.tracker().playlist_dirty());

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 0);

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
    }

    /// Ответы `FakeIo` эхо-возвращают `gen` запроса — сопоставление
    /// заданий/ответов не теряется (§7.1, ADR-20, ТЗ-22).
    #[test]
    fn platform_fakes_io_echoes_gen() {
        let io = FakeIo::new();
        io.submit(IoJob::CacheSizes { gen: 7 });
        io.submit(IoJob::ListThemes { gen: 9 });

        let first = io.try_recv().expect("первый ответ готов");
        let second = io.try_recv().expect("второй ответ готов");
        assert_eq!(first.gen(), 7);
        assert_eq!(second.gen(), 9);
        assert!(io.try_recv().is_none());
    }

    /// `ClearCache` обнуляет только размер своего вида кэша и сохраняет
    /// результат для следующего `CacheSizes` (ТЗ-22, ТЗ-34).
    #[test]
    fn platform_fakes_io_clear_cache_zeroes_right_kind() {
        let io = FakeIo::new();
        io.set_sizes(CacheSizes { viz_disk: 100, covers: 200 });

        io.submit(IoJob::ClearCache { gen: 1, which: CacheKind::Visualization });
        match io.try_recv().expect("ответ Cleared готов") {
            IoDone::Cleared { sizes, .. } => {
                assert_eq!(sizes, CacheSizes { viz_disk: 0, covers: 200 });
            }
            other => panic!("expected Cleared, got {other:?}"),
        }

        io.submit(IoJob::CacheSizes { gen: 2 });
        match io.try_recv().expect("ответ CacheSizes готов") {
            IoDone::CacheSizes { sizes, .. } => {
                assert_eq!(sizes, CacheSizes { viz_disk: 0, covers: 200 });
            }
            other => panic!("expected CacheSizes, got {other:?}"),
        }
    }

    /// Запрошенная, но не заданная тема возвращает `Err("not found")`
    /// (ТЗ-27, ТЗ-29).
    #[test]
    fn platform_fakes_io_read_theme_not_found() {
        let io = FakeIo::new();
        io.submit(IoJob::ReadTheme { gen: 1, name: ThemeName::default() });
        match io.try_recv().expect("ответ Theme готов") {
            IoDone::Theme { result, .. } => {
                assert_eq!(result.unwrap_err().as_ref(), "not found");
            }
            other => panic!("expected Theme, got {other:?}"),
        }
    }

    /// Заданная через `set_theme` тема возвращается `Ok`; `set_themes`
    /// задаёт список для `ListThemes` (ТЗ-22, ТЗ-27, ТЗ-29).
    #[test]
    fn platform_fakes_io_read_theme_and_list_themes_configured() {
        let io = FakeIo::new();
        let name = ThemeName::new("dark").expect("валидное имя темы");
        let data = Arc::new(ThemeData {
            name: "dark".to_string(),
            description: None,
            standard_palette: crate::theme::StandardPalette::Dark,
            colors: crate::theme::ColorsData {
                bg_window: "#000000".to_string(),
                bg_surface: "#000000".to_string(),
                bg_toolbar: "#000000".to_string(),
                bg_elevated: "#000000".to_string(),
                bg_overlay: "#000000".to_string(),
                border_subtle: "#000000".to_string(),
                border_default: "#000000".to_string(),
                text_primary: "#000000".to_string(),
                text_secondary: "#000000".to_string(),
                text_tertiary: "#000000".to_string(),
                text_dim: "#000000".to_string(),
                text_on_accent: "#000000".to_string(),
                text_error: "#000000".to_string(),
                accent: "#000000".to_string(),
                accent_container: "#000000".to_string(),
                accent_on: "#000000".to_string(),
                surface_hover: "#000000".to_string(),
                surface_active: "#000000".to_string(),
                surface_selected: "#000000".to_string(),
                viz_1: "#000000".to_string(),
                viz_2: "#000000".to_string(),
                viz_3: "#000000".to_string(),
            },
        });
        io.set_theme(name.clone(), Ok(data.clone()));
        io.set_themes(vec![ThemeEntry {
            file_stem: "dark".to_string(),
            name: "dark".to_string(),
            description: None,
            valid: true,
        }]);

        io.submit(IoJob::ReadTheme { gen: 1, name: name.clone() });
        match io.try_recv().expect("ответ Theme готов") {
            IoDone::Theme { result, .. } => assert_eq!(result.expect("тема найдена"), data),
            other => panic!("expected Theme, got {other:?}"),
        }

        io.submit(IoJob::ListThemes { gen: 2 });
        match io.try_recv().expect("ответ Themes готов") {
            IoDone::Themes { entries, .. } => assert_eq!(entries.len(), 1),
            other => panic!("expected Themes, got {other:?}"),
        }
    }

    /// `set_hold(true)` придерживает уже посчитанные ответы — `try_recv`
    /// отдаёт их только после снятия задержки (§7.1, ADR-20).
    #[test]
    fn platform_fakes_io_hold_delays_delivery() {
        let io = FakeIo::new();
        io.set_hold(true);
        io.submit(IoJob::CacheSizes { gen: 1 });
        assert!(io.try_recv().is_none());

        io.set_hold(false);
        let done = io.try_recv().expect("ответ выдан после снятия hold");
        assert_eq!(done.gen(), 1);
    }

    /// Журнал заданий считает их по виду независимо от порядка (§7.1).
    #[test]
    fn platform_fakes_io_journal_counts_by_kind() {
        let io = FakeIo::new();
        io.submit(IoJob::CacheSizes { gen: 1 });
        io.submit(IoJob::ClearCache { gen: 2, which: CacheKind::All });
        io.submit(IoJob::ReadTheme { gen: 3, name: ThemeName::default() });
        io.submit(IoJob::ListThemes { gen: 4 });
        io.submit(IoJob::CacheSizes { gen: 5 });

        assert_eq!(io.count_cache_sizes(), 2);
        assert_eq!(io.count_clear(), 1);
        assert_eq!(io.count_read_theme(), 1);
        assert_eq!(io.count_list_themes(), 1);
        assert_eq!(io.jobs().len(), 5);
    }

    /// Клон `FakeIo` делит состояние с оригиналом — как `Box<dyn IoWorker>`
    /// внутри `AppCore` остаётся наблюдаемым через сохранённый хендл
    /// (§7.1, ADR-20).
    #[test]
    fn platform_fakes_io_clone_shares_state() {
        let io = FakeIo::new();
        let boxed: Box<dyn IoWorker> = Box::new(io.clone());
        boxed.submit(IoJob::ListThemes { gen: 1 });

        assert_eq!(io.count_list_themes(), 1);
        assert!(boxed.try_recv().is_some());
    }

    /// Пути текущего видимого порядка плейлиста (помогает собрать ожидание
    /// без допущений о внутреннем хранении, см. `sort_key_restored_after_restart`).
    fn playlist_paths(core: &AppCore) -> Vec<PathBuf> {
        core.playlist().visible().iter().filter_map(|&id| core.playlist().get(id)).map(|t| t.path.clone()).collect()
    }

    /// Командная загрузка заменяет плейлист и сразу играет первый трек по
    /// ключу сортировки, взведённому в `state.toml` (ТЗ-13 б, ADR-16,
    /// §6.12): пока загрузка не завершилась, прежний плейлист цел.
    ///
    /// ОТКЛОНЕНИЕ: Open/стоп движка — у MusicApp через фасад до С5; здесь
    /// проверяется эффект LoadApplied::PlayNow.
    #[test]
    fn load_playlist_plays_first_by_key() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"sort.column = \"title\"\nsort.direction = \"asc\"\n");
        let command_path = PathBuf::from("/import/mix.m3u");
        h.put_command_playlist(&command_path, b"/music/zebra.flac\n/music/apple.flac\n/music/mango.flac\n");
        let mut core = h.boot();
        core.playlist_add(vec![track_path("/music/x.flac")]);
        assert_eq!(playlist_paths(&core), vec![PathBuf::from("/music/x.flac")]);

        core.start_command_load(command_path.clone());
        assert!(matches!(core.load_state(), LoadState::Command { .. }));
        assert_eq!(playlist_paths(&core), vec![PathBuf::from("/music/x.flac")], "плейлист цел до завершения загрузки");

        assert!(h.finish_pending_load());
        let report = core.poll_load().expect("итог командной загрузки получен");
        let first = match report.applied {
            Some(LoadApplied::PlayNow { first }) => first,
            other => panic!("expected PlayNow, got {other:?}"),
        };
        let first_id = first.expect("первый трек выбран");
        let first_title = core.playlist().get(first_id).expect("трек найден").title.clone();
        assert_eq!(first_title, "apple", "первый по ключу Title ↑ среди загруженных");

        let paths = playlist_paths(&core);
        assert_eq!(paths.len(), 3);
        assert!(!paths.contains(&PathBuf::from("/music/x.flac")), "прежний трек не пережил замену");
        assert!(core.tracker().playlist_dirty());

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 1);
        let bytes = h.disk_bytes(WorkFile::Playlist).expect("playlist записан");
        let text = String::from_utf8_lossy(&bytes);
        for p in ["/music/zebra.flac", "/music/apple.flac", "/music/mango.flac"] {
            assert!(text.contains(p), "{text} должен содержать {p}");
        }
        assert!(!text.contains("/music/x.flac"));
    }

    /// Командная загрузка запускает воспроизведение независимо от того,
    /// что было «до» неё — ядро не хранит паузу/стоп отдельно: при Shuffle
    /// первый трек прохода берётся из только что загруженного плейлиста
    /// (ТЗ-13 б, §3.4).
    ///
    /// ОТКЛОНЕНИЕ: Open/стоп движка — у MusicApp через фасад до С5; здесь
    /// проверяется эффект LoadApplied::PlayNow.
    #[test]
    fn load_playlist_from_pause_and_stop_starts_playback() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        h.put_playlist(b"/music/x.flac\n");
        let command_path = PathBuf::from("/import/mix.m3u");
        h.put_command_playlist(&command_path, b"/music/zebra.flac\n/music/apple.flac\n/music/mango.flac\n");
        let mut core = h.boot();
        core.change_state(Origin::Program, StateChange::Shuffle(true));

        core.start_command_load(command_path);
        assert!(h.finish_pending_load());
        let report = core.poll_load().expect("итог командной загрузки получен");
        let first = match report.applied {
            Some(LoadApplied::PlayNow { first }) => first,
            other => panic!("expected PlayNow, got {other:?}"),
        };
        let first_id = first.expect("первый трек выбран даже без состояния воспроизведения «до»");
        let first_path = core.playlist().get(first_id).expect("трек найден").path.clone();
        assert!(playlist_paths(&core).contains(&first_path), "первый трек — член только что загруженного плейлиста");
    }

    /// Ошибка чтения файла командной загрузки не трогает прежний плейлист
    /// и не запрещает его запись — в отличие от стартовой загрузки,
    /// `on_command_outcome` не вызывает `forbid_playlist` (ТЗ-13 б, §6.12).
    #[test]
    fn load_playlist_unreadable_keeps_everything() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let original = b"/music/x.flac\n";
        h.put_playlist(original);
        let command_path = PathBuf::from("/import/mix.m3u");
        h.fail_read_path(&command_path, ReadErrorClass::NoAccess);
        let mut core = h.boot();
        core.start_startup_load();
        assert!(h.finish_startup_load());
        core.poll_load().expect("итог стартовой загрузки получен");

        core.start_command_load(command_path);
        assert!(h.finish_pending_load());
        let report = core.poll_load().expect("итог командной загрузки получен");
        assert!(report.applied.is_none());
        let message = report.message.expect("ошибка чтения приходит сразу, в самом отчёте");
        assert_eq!(message.level, MessageLevel::Error);

        assert_eq!(playlist_paths(&core), vec![PathBuf::from("/music/x.flac")]);
        assert!(!core.tracker().playlist_dirty());
        assert!(!core.tracker().playlist_forbidden(), "командная ReadFailed не запрещает запись плейлиста");

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
        assert_eq!(h.disk_bytes(WorkFile::Playlist).as_deref(), Some(original.as_slice()));
    }

    /// Пустой файл командной загрузки (нет валидных строк) заменяет
    /// плейлист пустым и останавливает воспроизведение — `PlayNow { first:
    /// None }` — и всё равно взводит флаг «грязного» плейлиста (ТЗ-13 б,
    /// §6.12).
    ///
    /// ОТКЛОНЕНИЕ: Open/стоп движка — у MusicApp через фасад до С5; здесь
    /// проверяется эффект LoadApplied::PlayNow.
    #[test]
    fn load_playlist_empty_stops_and_marks_dirty() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        h.put_playlist(b"/music/x.flac\n");
        let command_path = PathBuf::from("/import/mix.m3u");
        h.put_command_playlist(&command_path, b"");
        let mut core = h.boot();

        core.start_command_load(command_path);
        assert!(h.finish_pending_load());
        let report = core.poll_load().expect("итог командной загрузки получен");
        assert_eq!(report.applied, Some(LoadApplied::PlayNow { first: None }));
        assert!(report.message.is_none());
        assert!(core.playlist().is_empty());
        assert!(core.tracker().playlist_dirty());

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 1);
        assert_eq!(h.disk_bytes(WorkFile::Playlist).as_deref(), Some(b"".as_slice()));
    }

    /// Выход во время зависшей командной загрузки с «грязным» плейлистом
    /// (ТЗ-17, ADR-16, §6.12): задание ушло в `ManualLoader`, но `finish` не
    /// вызывается — выход не ждёт загрузчик и пишет прежний плейлист ровно
    /// один раз, потому что флаг был взведён до начала загрузки.
    #[test]
    fn load_command_exit_writes_old_playlist_if_dirty() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        h.put_playlist(b"/music/x.flac\n");
        let mut core = h.boot();

        core.playlist_add(vec![track_path("/music/y.flac")]);
        assert!(core.tracker().playlist_dirty());
        let expected = playlist_paths(&core);

        core.start_command_load(PathBuf::from("/import/mix.m3u"));
        assert_eq!(h.loader.jobs().len(), 1, "задание ушло в загрузчик и не разрешено");

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        assert_eq!(h.writes(WorkFile::Playlist), 1);
        let bytes = h.disk_bytes(WorkFile::Playlist).expect("playlist записан на выходе");
        let text = String::from_utf8_lossy(&bytes);
        for p in &expected {
            assert!(text.contains(p.to_str().expect("путь")), "{text} должен содержать {p:?}");
        }
    }

    /// Выход во время зависшей командной загрузки без «грязного» плейлиста
    /// (ТЗ-17, ТЗ-48, ADR-16, §6.12): ничего не менялось — выход не пишет
    /// ни одного файла, независимо от висящего задания загрузки.
    #[test]
    fn load_command_exit_clean_writes_nothing() {
        let h = Harness::new();
        let settings_bytes =
            crate::persist::settings_file::serialize_settings(&Settings::default()).expect("serialize settings");
        let state_bytes = crate::persist::state_file::serialize_state(&SessionState::default()).expect("serialize state");
        h.put_settings(&settings_bytes);
        h.put_state(&state_bytes);
        h.put_playlist(b"/music/x.flac\n");
        let mut core = h.boot();
        assert!(!core.tracker().playlist_dirty());

        core.start_command_load(PathBuf::from("/import/mix.m3u"));
        assert_eq!(h.loader.jobs().len(), 1, "задание ушло в загрузчик и не разрешено");

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        assert_eq!(h.writes(WorkFile::Playlist), 0);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(h.disk_bytes(WorkFile::Playlist).as_deref(), Some(b"/music/x.flac\n".as_slice()));
    }

    /// Дедлайн отложенной записи истекает, пока командная загрузка всё ещё
    /// висит в `ManualLoader` (ТЗ-17, ADR-16, §6.12): пишется прежний
    /// плейлист, загружаемый (но не разрешённый) список в файл не попадает.
    #[test]
    fn load_command_timer_writes_old_playlist() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        h.put_playlist(b"/music/x.flac\n");
        let mut core = h.boot();

        core.playlist_add(vec![track_path("/music/y.flac")]);
        assert!(core.tracker().playlist_dirty());
        let expected = playlist_paths(&core);

        core.start_command_load(PathBuf::from("/import/mix.m3u"));
        assert_eq!(h.loader.jobs().len(), 1, "задание ушло в загрузчик и не разрешено");

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::Playlist), 1);
        let bytes = h.disk_bytes(WorkFile::Playlist).expect("playlist записан по таймеру");
        let text = String::from_utf8_lossy(&bytes);
        for p in &expected {
            assert!(text.contains(p.to_str().expect("путь")), "{text} должен содержать {p:?}");
        }
        assert!(!text.contains("import"), "загружаемый (но не разрешённый) список не попадает в запись");
    }

    /// Экспорт «Сохранить плейлист» пишет видимый порядок таблицы, а не
    /// исходный порядок и не перестановку Shuffle (ADR-17, §6.18, ТЗ-13 а):
    /// сортировка по «Название ↑» переставляет строки, Shuffle включён и
    /// заведомо не совпадает с видимым порядком — экспорт игнорирует обе
    /// перестановки навигации и берёт `Playlist::visible()`. Экспорт не
    /// трогает `playlist.m3u` и флаг «плейлист изменён».
    #[test]
    fn export_writes_visible_order_with_extinf() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let rows = vec![track_with_title("/music/b.flac", "Beta"), track_with_title("/music/a.flac", "Alpha")];
        core.playlist_replace(rows, vec![0, 1], None);
        core.playlist_header_click(ColumnId::Title); // Asc по названию: Alpha, Beta
        core.change_state(Origin::User, StateChange::Shuffle(true));
        let dirty_before = core.tracker().playlist_dirty();

        let path = PathBuf::from("/export/out.m3u");
        core.export(path.clone());
        let bytes = h.settle_path(&mut core, &path);

        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(text, "#EXTM3U\n#EXTINF:-1,Alpha\n/music/a.flac\n#EXTINF:-1,Beta\n/music/b.flac\n");
        assert_eq!(h.writes(WorkFile::Playlist), 0);
        assert_eq!(core.tracker().playlist_dirty(), dirty_before);
    }

    /// Успешный экспорт не показывает сообщений (ADR-17, §6.18, ТЗ-13 а,
    /// ТЗ-52).
    #[test]
    fn export_success_no_window() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        core.playlist_add(vec![track_path("/music/x.flac")]);

        let path = PathBuf::from("/export/out.m3u");
        core.export(path.clone());
        let bytes = h.settle_path(&mut core, &path);
        assert!(!bytes.is_empty());

        // Дать тику разобрать ответ `Exported`, который пришёл вместе с
        // записанными байтами или сразу за ними, и убедиться, что он не
        // превращается в сообщение.
        let out = core.tick(None);
        assert!(out.messages.is_empty());
    }

    /// Каждая неудачная попытка экспорта — отдельное окно Error без
    /// дедупликации (ADR-17, §6.18, ТЗ-13 а, ТЗ-20): два провала подряд дают
    /// два сообщения и две записи в журнале.
    #[test]
    fn export_error_window_per_attempt() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        core.playlist_add(vec![track_path("/music/x.flac")]);

        let path = PathBuf::from("/export/out.m3u");
        h.fail_write_path(&path, WriteStep::WriteData, WriteErrorClass::NoSpace, 2);

        core.export(path.clone());
        let first = h.settle_messages(&mut core);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].level, MessageLevel::Error);
        assert_eq!(&*first[0].title, "Не удалось сохранить плейлист");

        core.export(path.clone());
        let second = h.settle_messages(&mut core);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].level, MessageLevel::Error);

        let failures: Vec<_> = h
            .journal()
            .into_iter()
            .filter(|r| matches!(r, JournalRecord::WriteFailed { target: WriteTarget::Export(p), .. } if *p == path))
            .collect();
        assert_eq!(failures.len(), 2, "два провала — две отдельные записи журнала, без дедупликации");
    }

    /// ТЗ-93, §7 `mode_settings_preserved_across_switch_and_restart`:
    /// Оптимальный режим «Медленный»/50 %, Совместимый «Очень медленный»/
    /// 70 % — переключение активного режима туда и обратно и перезапуск
    /// (повторная загрузка `AppCore` из тех же файлов) сохраняют оба набора
    /// значений: настройки фильтра в `ModeSettings` и громкость каждого
    /// режима в `PlaybackState`.
    #[test]
    fn mode_settings_preserved_across_switch_and_restart() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();

        let mut settings = core.settings().clone();
        settings.playback.compatible.src_filter = SrcFilter::VerySlow;
        settings.playback.optimal.src_filter = SrcFilter::Slow;
        core.set_settings(settings);

        core.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Compatible));
        core.change_state(Origin::User, StateChange::Volume(70));
        core.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Optimal));
        core.change_state(Origin::User, StateChange::Volume(50));

        // Переключение туда и обратно перед перезапуском не теряет ни одно значение.
        core.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Compatible));
        assert_eq!(core.state().playback().volume, 70);
        core.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Optimal));
        assert_eq!(core.state().playback().volume, 50);

        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        // Перезапуск: новый `AppCore` из тех же "файлов на диске".
        let mut core2 = h.boot();
        assert_eq!(core2.settings().playback.compatible.src_filter, SrcFilter::VerySlow);
        assert_eq!(core2.settings().playback.optimal.src_filter, SrcFilter::Slow);

        assert_eq!(core2.state().active_mode(), ModeKind::Optimal);
        assert_eq!(core2.state().playback().volume, 50);
        core2.change_state(Origin::User, StateChange::ActiveMode(ModeKind::Compatible));
        assert_eq!(core2.state().playback().volume, 70);
    }

    /// ТЗ-135, §7 `load_correction_not_written_until_save`: файл с
    /// `[playback.optimal] buffer = 500, device_buffer_ms = 400` нарушает
    /// И-Р22 (нужно `buffer ≥ 2 × device_buffer`) — после загрузки в памяти
    /// исправленное значение `DeviceBuffer::Ms40` и заметка `Adjusted`, но
    /// исправление не пишется на диск само: запись происходит только по
    /// явному «Сохранить».
    #[test]
    fn load_correction_not_written_until_save() {
        let h = Harness::new();
        h.put_settings(b"[playback]\nschema = 2\n\n[playback.optimal]\nbuffer = 500\ndevice_buffer_ms = 400\n");
        h.put_state(b"");
        let mut core = h.boot();

        assert_eq!(core.settings().playback.optimal.device_buffer, DeviceBuffer::Ms40);

        let notes = &core.startup_notes().settings;
        let note = notes
            .iter()
            .find(|n| n.key == KeyPath::new("playback.optimal.device_buffer_ms"))
            .expect("заметка о согласовании device_buffer присутствует");
        match &note.kind {
            LoadNoteKind::Adjusted { reason } => {
                assert_eq!(
                    reason.as_ref(),
                    "буфер устройства 400 мс больше половины буфера плеера 500 мс — установлено 40 мс"
                );
            }
            other => panic!("ожидалась заметка Adjusted, получено {other:?}"),
        }

        assert_eq!(h.writes(WorkFile::Settings), 0, "исправление не пишется само по себе");

        core.save_settings_now(core.settings().clone());
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 1, "явное «Сохранить» пишет исправленное значение");
    }

    /// Исчерпывающий перечень `MainCmd` (§2.11, ADR-12) с одним вариантом
    /// `SortBy`. `match` без `_`-ветки: новый вариант `MainCmd` ломает
    /// компиляцию и напоминает дополнить список.
    fn all_main_cmds() -> Vec<MainCmd> {
        let all = vec![
            MainCmd::AddFiles,
            MainCmd::AddFolder,
            MainCmd::LoadPlaylist,
            MainCmd::SavePlaylist,
            MainCmd::RemoveCurrent,
            MainCmd::ClearPlaylist,
            MainCmd::SortBy(ColumnId::Title),
            MainCmd::DropFiles,
            MainCmd::Next,
            MainCmd::Prev,
            MainCmd::PlayPause,
            MainCmd::Stop,
            MainCmd::Seek,
            MainCmd::PlayRow,
            MainCmd::Volume,
            MainCmd::ToggleMute,
            MainCmd::SwitchMode,
            MainCmd::Repeat,
            MainCmd::Shuffle,
            MainCmd::VizCycle,
            MainCmd::ColumnResize,
            MainCmd::OpenSettings,
        ];
        for cmd in &all {
            match cmd {
                MainCmd::AddFiles
                | MainCmd::AddFolder
                | MainCmd::LoadPlaylist
                | MainCmd::SavePlaylist
                | MainCmd::RemoveCurrent
                | MainCmd::ClearPlaylist
                | MainCmd::SortBy(_)
                | MainCmd::DropFiles
                | MainCmd::Next
                | MainCmd::Prev
                | MainCmd::PlayPause
                | MainCmd::Stop
                | MainCmd::Seek
                | MainCmd::PlayRow
                | MainCmd::Volume
                | MainCmd::ToggleMute
                | MainCmd::SwitchMode
                | MainCmd::Repeat
                | MainCmd::Shuffle
                | MainCmd::VizCycle
                | MainCmd::ColumnResize
                | MainCmd::OpenSettings => {}
            }
        }
        all
    }

    /// Имя темы, заведомо отличное от темы действующих настроек — для правки
    /// темы в черновике (§6.9, ТЗ-25).
    fn other_theme(core: &AppCore) -> ThemeName {
        let light = ThemeName::new("light").expect("valid theme name");
        if core.settings().theme == light {
            ThemeName::new("dark").expect("valid theme name")
        } else {
            light
        }
    }

    /// Набор правок черновика для тестов диалога (§6.9, ТЗ-25): тема, два
    /// флага настроек, тип визуализации и «Сбросить ширины».
    fn edit_draft(core: &mut AppCore) {
        let theme = other_theme(core);
        core.dialog_edit(|d| d.settings.theme = theme);
        core.dialog_edit(|d| {
            d.settings.minimize_to_tray = !d.settings.minimize_to_tray;
            d.settings.scroll_to_playing = !d.settings.scroll_to_playing;
        });
        core.dialog_edit(|d| {
            d.viz_mode = if d.viz_mode == VisualizationMode::Spectrum {
                VisualizationMode::Oscilloscope
            } else {
                VisualizationMode::Spectrum
            };
        });
        core.dialog_edit(|d| d.reset_widths = true);
    }

    /// Меню неактивно, пока открыт диалог настроек (ТЗ-23, ADR-12, И-Р8,
    /// §6.9, §7.2): после `dialog_open` шлюз отклоняет каждую `MainCmd`,
    /// включая «Удалить текущий трек», «Очистить плейлист», V и повторное
    /// открытие диалога. Повторный `dialog_open` возвращает `None` — 0
    /// изменений настроек, состояния, черновика, заданий `apap-io`, записей
    /// и команд движку. «Отмена» возвращает меню в прежнее состояние.
    #[test]
    fn menu_inactive_while_dialog_open() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        for cmd in all_main_cmds() {
            assert!(core.gate().allows(cmd), "{cmd:?} разрешена до открытия диалога");
        }

        assert!(core.dialog_open().is_some(), "первое открытие создаёт черновик");
        for cmd in all_main_cmds() {
            assert!(!core.gate().allows(cmd), "{cmd:?} must be denied while dialog is open");
        }
        assert!(core.gate().window_blocked());

        // Черновик с правкой: повторное открытие не должно заменить его свежим.
        core.dialog_edit(|d| d.settings.minimize_to_tray = !d.settings.minimize_to_tray);

        let settings = core.settings().clone();
        let state = core.state().clone();
        let draft = core.draft().cloned();
        let jobs = h.io.jobs();
        let engine = h.engine_cmds();
        let playlist_len = core.playlist().len();

        assert_eq!(core.dialog_open(), None, "повторное открытие отклонено шлюзом");

        assert_eq!(core.settings(), &settings);
        assert_eq!(core.state(), &state);
        assert_eq!(core.draft().cloned(), draft, "черновик прежний, второй не создан");
        assert_eq!(h.io.jobs(), jobs, "второе задание apap-io не отправлено");
        assert_eq!(h.io.count_cache_sizes(), 1);
        assert_eq!(h.engine_cmds(), engine);
        assert_eq!(core.playlist().len(), playlist_len);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 0);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(h.writes(WorkFile::Playlist), 0);

        core.dialog_cancel();
        assert!(!core.gate().window_blocked());
        for cmd in all_main_cmds() {
            assert!(core.gate().allows(cmd), "{cmd:?} снова разрешена после «Отмены»");
        }
    }

    /// Правка черновика диалога ничего не пишет (ТЗ-25, §6.9, §7.3 строка
    /// «Открыть диалог; изменить в копии»): после нескольких `dialog_edit`
    /// (тема, флаги настроек, тип визуализации, «Сбросить ширины») и 10 мин
    /// простоя — 0 записей `settings.toml` и `state.toml`, действующие
    /// настройки и состояние прежние, 0 команд движку; изменился только
    /// черновик.
    #[test]
    fn event_dialog_edit_writes_nothing() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let settings = core.settings().clone();
        let state = core.state().clone();
        let engine = h.engine_cmds();

        core.dialog_open().expect("диалог открыт");
        let fresh = core.draft().cloned().expect("черновик создан");
        assert_eq!(fresh.settings, settings, "черновик — копия действующих настроек");
        assert_eq!(fresh.viz_mode, state.viz_mode());

        edit_draft(&mut core);

        let draft = core.draft().expect("черновик на месте");
        assert_ne!(draft, &fresh, "черновик изменился");
        assert_ne!(draft.settings.theme, settings.theme);
        assert_ne!(draft.settings.minimize_to_tray, settings.minimize_to_tray);
        assert_ne!(draft.settings.scroll_to_playing, settings.scroll_to_playing);
        assert_ne!(draft.viz_mode, state.viz_mode());
        assert!(draft.reset_widths);

        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::Settings), 0);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(core.settings(), &settings, "действующие настройки не тронуты");
        assert_eq!(core.state(), &state, "состояние сессии не тронуто");
        assert_eq!(h.engine_cmds(), engine, "0 команд движку");
    }

    /// Размер дисковых кэшей считается один раз за открытие диалога (ТЗ-22,
    /// §6.9, ADR-20): `dialog_open` отправляет ровно одно задание
    /// `CacheSizes` и возвращает его поколение; правки черновика, тики и
    /// повторные `dialog_open` новых не добавляют. Новое открытие после
    /// «Отмены» — второе задание с другим поколением.
    #[test]
    fn cache_size_counted_once_per_dialog() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        assert_eq!(h.io.count_cache_sizes(), 0, "до открытия диалога подсчёта нет");

        let first = core.dialog_open().expect("диалог открыт");
        assert_eq!(h.io.jobs(), vec![IoJob::CacheSizes { gen: first }]);

        edit_draft(&mut core);
        assert_eq!(core.dialog_open(), None);
        assert_eq!(core.dialog_open(), None);
        // Диалог открыт 10 с (§7.2): тики подсчёт не повторяют.
        for _ in 0..100 {
            h.advance(&mut core, Duration::from_millis(100));
        }
        assert_eq!(h.io.count_cache_sizes(), 1, "один подсчёт за открытие");

        core.dialog_cancel();
        assert_eq!(h.io.count_cache_sizes(), 1, "«Отмена» подсчёт не запускает");

        let second = core.dialog_open().expect("диалог открыт снова");
        assert_ne!(second, first, "у нового задания другое поколение");
        let sizes: Vec<IoJob> =
            h.io.jobs().into_iter().filter(|j| matches!(j, IoJob::CacheSizes { .. })).collect();
        assert_eq!(sizes, vec![IoJob::CacheSizes { gen: first }, IoJob::CacheSizes { gen: second }]);
    }

    /// «Отмена» диалога настроек (ТЗ-26, §6.9, ADR-11): тема, флаги и тип
    /// визуализации изменены в черновике, затем `dialog_cancel` — черновик
    /// уничтожен, действующие настройки и состояние прежние, 0 записей
    /// `settings.toml` (и `state.toml`), 0 команд движку, шлюз разблокирован.
    #[test]
    fn dialog_cancel_leaves_settings_and_file_unchanged() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let settings = core.settings().clone();
        let state = core.state().clone();
        let engine = h.engine_cmds();

        core.dialog_open().expect("диалог открыт");
        edit_draft(&mut core);
        assert_ne!(core.draft().expect("черновик на месте").settings, settings);

        core.dialog_cancel();

        assert!(core.draft().is_none(), "черновик уничтожен");
        assert!(!core.gate().window_blocked());
        assert!(core.gate().allows(MainCmd::OpenSettings), "шлюз разблокирован");
        assert_eq!(core.settings(), &settings);
        assert_eq!(core.state(), &state);

        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 0);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(h.disk_bytes(WorkFile::Settings), Some(Vec::new()), "файл настроек на диске прежний");
        assert_eq!(h.engine_cmds(), engine, "0 команд движку");

        // Повторная «Отмена» без черновика безопасна.
        core.dialog_cancel();
        assert!(core.draft().is_none());
        assert!(core.gate().allows(MainCmd::OpenSettings));
    }

    /// Очистка кэша переживает «Отмену» (ТЗ-26, ТЗ-34, §6.9): `clear_cache`
    /// отправляет ровно одно задание `ClearCache` с возвращённым поколением
    /// и не трогает черновик; `dialog_cancel` задание не отзывает, настройки
    /// прежние, 0 записей. Очистка работает и без открытого диалога.
    #[test]
    fn cache_clear_survives_cancel() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        h.io.set_sizes(CacheSizes { viz_disk: 100, covers: 200 });
        let mut core = h.boot();
        let settings = core.settings().clone();
        let state = core.state().clone();

        let opened = core.dialog_open().expect("диалог открыт");
        edit_draft(&mut core);
        let draft = core.draft().cloned();

        let gen = core.clear_cache(CacheKind::Covers);

        assert_ne!(gen, opened, "у очистки своё поколение");
        assert_eq!(core.draft().cloned(), draft, "очистка — вне черновика");
        let job = IoJob::ClearCache { gen, which: CacheKind::Covers };
        assert_eq!(h.io.count_clear(), 1);
        assert!(h.io.jobs().contains(&job));

        core.dialog_cancel();

        assert!(core.draft().is_none());
        assert_eq!(h.io.count_clear(), 1, "«Отмена» очистку не повторяет");
        assert!(h.io.jobs().contains(&job), "«Отмена» задание не отзывает");
        // Очистка действительно выполнена: следующий подсчёт видит пустой кэш.
        h.io.submit(IoJob::CacheSizes { gen: u64::MAX });
        let mut last = None;
        while let Some(done) = h.io.try_recv() {
            last = Some(done);
        }
        assert_eq!(
            last,
            Some(IoDone::CacheSizes { gen: u64::MAX, sizes: CacheSizes { viz_disk: 100, covers: 0 } })
        );
        assert_eq!(core.settings(), &settings);
        assert_eq!(core.state(), &state);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 0);
        assert_eq!(h.writes(WorkFile::State), 0);

        // Без открытого диалога очистка тоже уходит в `apap-io`.
        let again = core.clear_cache(CacheKind::All);
        assert_ne!(again, gen);
        assert!(core.draft().is_none(), "очистка диалог не открывает");
        assert_eq!(h.io.count_clear(), 2);
        assert_eq!(h.io.jobs().last(), Some(&IoJob::ClearCache { gen: again, which: CacheKind::All }));
    }

    /// Выбирает в черновике другую тему и доводит её чтение до `Ready`
    /// (§6.9, ADR-20, ТЗ-22, ТЗ-29): поколение ответа берётся из
    /// `ThemeLoad::Loading` черновика. Возвращает имя темы и её данные.
    fn pick_other_theme(core: &mut AppCore) -> (ThemeName, Arc<ThemeData>) {
        let name = other_theme(core);
        let picked = name.clone();
        core.dialog_edit(|d| d.settings.theme = picked);
        let gen = match core.draft().map(|d| &d.theme) {
            Some(ThemeLoad::Loading { gen }) => *gen,
            other => panic!("ожидалось чтение темы (Loading), получено {other:?}"),
        };
        let parsed: ThemeData = toml::from_str(crate::theme::DEFAULT_DARK_TOML).expect("built-in theme parses");
        let data = Arc::new(parsed);
        assert!(core.dialog_theme_loaded(gen, Ok(Arc::clone(&data))), "ответ ожидаемого поколения принят");
        (name, data)
    }

    /// Ненулевой набор ширин колонок для состояния сессии (Т-2, §2.6).
    fn some_widths() -> std::collections::BTreeMap<ColumnId, crate::persist::state_file::WidthPct> {
        use crate::persist::state_file::WidthPct;
        std::collections::BTreeMap::from([
            (ColumnId::Title, WidthPct::new(40.0).expect("valid width")),
            (ColumnId::Artist, WidthPct::new(25.0).expect("valid width")),
        ])
    }

    /// Число команд движку с данным именем варианта `EngineCmd` (§2.12):
    /// журнал стенда хранит текст `Debug`, имя варианта — его начало.
    fn engine_cmd_count(h: &Harness, variant: &str) -> usize {
        h.engine_cmds().iter().filter(|c| c.starts_with(variant)).count()
    }

    /// Кнопка «Сохранить» активна только при отличиях (ТЗ-27, §6.9, §7.2):
    /// без диалога и сразу после открытия — неактивна; выбрана другая тема —
    /// пока она читается (`Loading`), кнопка неактивна с подсказкой о теме;
    /// тема прочитана — активна; возврат прежней темы — снова неактивна, без
    /// подсказки («изменить и вернуть» отличия не даёт).
    #[test]
    fn save_button_disabled_without_changes() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let original = core.settings().theme.clone();

        assert_eq!(core.save_button(), SaveButton { enabled: false, tooltip: None }, "без диалога");

        core.dialog_open().expect("диалог открыт");
        assert_eq!(core.save_button(), SaveButton { enabled: false, tooltip: None }, "отличий нет");

        // Тема выбрана, но ещё читается: применять нечего (ТЗ-29).
        let name = other_theme(&core);
        core.dialog_edit(|d| d.settings.theme = name);
        let gen = match core.draft().map(|d| &d.theme) {
            Some(ThemeLoad::Loading { gen }) => *gen,
            other => panic!("ожидалось чтение темы (Loading), получено {other:?}"),
        };
        let loading = core.save_button();
        assert!(!loading.enabled, "пока тема читается, кнопка неактивна");
        assert!(loading.tooltip.is_some_and(|t| t.contains("Тема")), "подсказка называет параметр");

        let parsed: ThemeData = toml::from_str(crate::theme::DEFAULT_DARK_TOML).expect("built-in theme parses");
        assert!(core.dialog_theme_loaded(gen, Ok(Arc::new(parsed))));
        assert_eq!(core.save_button(), SaveButton { enabled: true, tooltip: None }, "тема изменена");

        core.dialog_edit(|d| d.settings.theme = original);
        assert_eq!(core.draft().map(|d| &d.theme), Some(&ThemeLoad::Unchanged));
        assert_eq!(core.save_button(), SaveButton { enabled: false, tooltip: None }, "тема возвращена");
        assert!(core.dialog_save().is_none(), "неактивная кнопка ничего не сохраняет");
        assert!(core.draft().is_some());
    }

    /// Недопустимое сочетание буферов (ТЗ-27, И-Р22, §6.9, §7.2): буфер плеера
    /// 100 мс меньше двух буферов устройства 400 мс в Строгом режиме — кнопка
    /// неактивна, подсказка «<param>: <reason>» содержит имя параметра из
    /// `validate`; `dialog_save` возвращает `None`: черновик цел, диалог
    /// открыт, 0 записей, 0 команд движку, действующие настройки прежние.
    #[test]
    fn save_button_invalid_value_tooltip() {
        use crate::settings::playback::BufferMs;

        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let settings = core.settings().clone();
        let state = core.state().clone();
        let engine = h.engine_cmds();

        core.dialog_open().expect("диалог открыт");
        core.dialog_edit(|d| {
            d.settings.playback.strict.buffer = BufferMs::new(100).expect("valid buffer");
            d.settings.playback.strict.device_buffer = DeviceBuffer::Ms400;
        });

        let draft = core.draft().cloned().expect("черновик на месте");
        let invalid = crate::core::dialog::validate(&draft);
        let first = invalid.first().expect("сочетание буферов недопустимо");
        assert_eq!(first.param, "Буфер устройства (Строгий режим)");

        let button = core.save_button();
        assert!(!button.enabled, "при недопустимом значении кнопка неактивна");
        let tooltip = button.tooltip.expect("подсказка есть");
        assert!(tooltip.contains(first.param), "подсказка содержит имя параметра: {tooltip}");
        assert_eq!(tooltip, format!("{}: {}", first.param, first.reason));

        assert!(core.dialog_save().is_none(), "недопустимое значение не сохраняется");
        assert_eq!(core.draft(), Some(&draft), "черновик цел");
        assert!(core.gate().window_blocked(), "диалог открыт");
        assert_eq!(core.settings(), &settings);
        assert_eq!(core.state(), &state);

        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 0);
        assert_eq!(h.writes(WorkFile::State), 0);
        assert_eq!(h.engine_cmds(), engine, "0 команд движку");
    }

    /// Выбор режима в диалоге не переключает активный режим (ТЗ-30, §6.9,
    /// §7.2): при активном Совместимом показан Строгий — отличий нет, кнопка
    /// неактивна, `dialog_save` возвращает `None`; после «Отмены» и нового
    /// открытия Строгий показан вместе с правкой его параметра — «Сохранить»
    /// пишет настройки один раз и шлёт ровно один `SetModeSettings`, 0
    /// `SetActiveMode`; активный режим остаётся Совместимым.
    #[test]
    fn dialog_mode_selector_does_not_switch_mode() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        assert_eq!(core.state().active_mode(), ModeKind::Compatible);
        let settings = core.settings().clone();
        let state = core.state().clone();

        core.dialog_open().expect("диалог открыт");
        assert_eq!(core.draft().map(|d| d.shown_mode), Some(ModeKind::Compatible));
        core.dialog_edit(|d| d.shown_mode = ModeKind::Strict);
        assert_eq!(
            core.save_button(),
            SaveButton { enabled: false, tooltip: None },
            "выбор режима без других изменений — не отличие"
        );
        assert!(core.dialog_save().is_none());
        assert!(core.draft().is_some(), "диалог остался открыт");
        core.dialog_cancel();
        assert_eq!(core.settings(), &settings);
        assert_eq!(core.state(), &state);
        assert!(h.engine_cmds().is_empty(), "0 команд движку после «Отмены»");

        core.dialog_open().expect("диалог открыт снова");
        assert_eq!(core.draft().map(|d| d.shown_mode), Some(ModeKind::Compatible), "выбор режима не запомнен");
        core.dialog_edit(|d| {
            d.shown_mode = ModeKind::Strict;
            d.settings.playback.strict.device_buffer = DeviceBuffer::Ms100;
        });
        assert_eq!(core.save_button(), SaveButton { enabled: true, tooltip: None });

        let saved = core.dialog_save().expect("отличие есть — сохранено");
        assert!(matches!(&saved.plan[..], [ApplyStep::Engine(_)]), "план: {:?}", saved.plan);
        assert!(saved.write.is_none());
        assert!(core.draft().is_none());
        assert!(!core.gate().window_blocked());

        assert_eq!(core.state().active_mode(), ModeKind::Compatible, "активный режим не переключён");
        assert_eq!(core.state(), &state, "состояние сессии не тронуто");
        assert_eq!(core.settings().playback.strict.device_buffer, DeviceBuffer::Ms100);
        assert_eq!(engine_cmd_count(&h, "SetActiveMode"), 0);
        assert_eq!(engine_cmd_count(&h, "SetModeSettings"), 1);
        assert_eq!(h.engine_cmds().len(), 1, "других команд движку нет");

        h.settle(&mut core);
        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 1);
        assert_eq!(h.writes(WorkFile::State), 0, "состояние сессии не менялось");
    }

    /// «Сохранить» не откатывает состояние сессии к моменту открытия диалога
    /// (ТЗ-25, ADR-11, §6.9, §7.2): пока диалог открыт, изменены размер окна
    /// и развёрнутость, ширины колонок и Repeat; в диалоге выбрана другая
    /// тема. После «Сохранить» состояние — изменённое во время диалога, тема
    /// новая, в плане шаг `Theme` с теми же данными, что прочитаны при выборе.
    #[test]
    fn save_keeps_state_changed_during_dialog() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();
        let opened_state = core.state().clone();

        core.dialog_open().expect("диалог открыт");

        let geometry = WindowGeometry {
            position: Some(PhysPos { x: 30, y: 40 }),
            size: Some(PhysSize { width: 1024, height: 700 }),
            size_units: SizeUnits::Physical,
            maximized: true,
            fullscreen: false,
        };
        core.change_state(Origin::User, StateChange::Window(geometry));
        core.change_state(Origin::User, StateChange::ColumnWidths(some_widths()));
        core.change_state(Origin::User, StateChange::Repeat(RepeatMode::All));
        let changed_state = core.state().clone();
        assert_ne!(changed_state, opened_state);

        let (theme, data) = pick_other_theme(&mut core);
        assert!(core.save_button().enabled);

        let saved = core.dialog_save().expect("тема изменена — сохранено");

        assert_eq!(core.state(), &changed_state, "состояние не откатилось к снимку на момент открытия");
        assert_eq!(core.state().window(), geometry);
        assert_eq!(core.state().column_widths(), &some_widths());
        assert_eq!(core.state().repeat(), RepeatMode::All);
        assert_eq!(core.settings().theme, theme, "тема новая");
        assert!(
            saved.plan.iter().any(|s| matches!(s, ApplyStep::Theme(applied) if Arc::ptr_eq(applied, &data))),
            "план: {:?}",
            saved.plan
        );
        assert!(!saved.plan.iter().any(|s| matches!(s, ApplyStep::ResetWidths | ApplyStep::VizMode(_))));
        assert!(core.draft().is_none());

        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Settings), 1);
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1);
        let expected = crate::persist::state_file::serialize_state(&changed_state).expect("serialize state");
        assert_eq!(h.disk_bytes(WorkFile::State), Some(expected.to_vec()), "на диск ушло изменённое состояние");
    }

    /// «Сбросить ширины» уходит в состояние сессии (ТЗ-25, Т-2, §6.9, §7.2):
    /// при заданных ширинах «Сохранить» с одним лишь сбросом даёт план из
    /// `ResetWidths`, ширины в состоянии пусты; `settings.toml` этим
    /// сохранением не пишется и остаётся прежним, а `state.toml` пишется по
    /// отсчёту — уже без ширин.
    #[test]
    fn reset_widths_goes_to_state() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();
        let settings = core.settings().clone();
        let settings_bytes = h.disk_bytes(WorkFile::Settings);
        let interval = settings.save_interval.duration();

        core.change_state(Origin::User, StateChange::ColumnWidths(some_widths()));
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1);
        let with_widths = h.disk_bytes(WorkFile::State).expect("state written");

        core.dialog_open().expect("диалог открыт");
        assert!(!core.save_button().enabled);
        core.dialog_edit(|d| d.reset_widths = true);
        assert_eq!(core.save_button(), SaveButton { enabled: true, tooltip: None });

        let saved = core.dialog_save().expect("сброс заданных ширин — отличие");
        assert_eq!(saved.plan, vec![ApplyStep::ResetWidths]);
        assert!(saved.write.is_none());
        assert!(core.state().column_widths().is_empty(), "ширины по умолчанию");
        assert_eq!(core.settings(), &settings, "настройки не менялись");

        // Запись состояния — не сразу, а по отсчёту (Т-2).
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 1, "до срока state.toml не пишется");
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 2);
        let without_widths = h.disk_bytes(WorkFile::State).expect("state written");
        assert_ne!(without_widths, with_widths);
        let expected = crate::persist::state_file::serialize_state(core.state()).expect("serialize state");
        assert_eq!(without_widths, expected.to_vec(), "в записанном состоянии ширин нет");

        assert_eq!(h.writes(WorkFile::Settings), 0, "settings.toml этим сохранением не пишется");
        assert_eq!(h.disk_bytes(WorkFile::Settings), settings_bytes, "settings.toml прежний, без ширин");
    }

    /// Стенд с настройками по умолчанию на диске и открытым диалогом — для
    /// сценариев «открыть → одна правка → Сохранить» (§6.9, ТЗ-29).
    fn boot_with_dialog() -> (Harness, AppCore) {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();
        core.dialog_open().expect("диалог открыт");
        (h, core)
    }

    /// «Сохранить» применяет только изменённое (ТЗ-29, §6.9, §7.2). Каждый
    /// сценарий — свой стенд: открыть → одна правка → сохранить.
    ///
    /// «Перестроение модели плейлиста» и «перечисление устройств» — действия
    /// слоя UI, ядро их не выполняет; в ядре их отсутствие выражено
    /// отсутствием соответствующих шагов в плане применения
    /// (`ApplyStep::Columns`, `ApplyStep::Engine`) — это и проверяется.
    /// «0 переоткрытий» устройства — 0 команд движку.
    #[test]
    fn dialog_save_applies_only_edited_fields() {
        // Только тема: одно чтение темы, в плане один шаг `Theme`.
        {
            let (h, mut core) = boot_with_dialog();
            let (theme, data) = pick_other_theme(&mut core);
            let saved = core.dialog_save().expect("тема изменена — сохранено");
            assert!(
                matches!(&saved.plan[..], [ApplyStep::Theme(applied)] if Arc::ptr_eq(applied, &data)),
                "план: {:?}",
                saved.plan
            );
            assert_eq!(core.settings().theme, theme);
            assert_eq!(h.io.count_read_theme(), 1, "ровно одно чтение темы");
            assert_eq!(engine_cmd_count(&h, "SetModeSettings"), 0);
            assert!(h.engine_cmds().is_empty(), "команд движку нет: {:?}", h.engine_cmds());
        }

        // Только видимость колонки: тема не читается, в плане один шаг `Columns`.
        {
            let (h, mut core) = boot_with_dialog();
            core.dialog_edit(|d| {
                let col = d.settings.columns.defs.values_mut().next().expect("есть хотя бы одна колонка");
                col.visible = !col.visible;
            });
            let saved = core.dialog_save().expect("видимость колонки изменена — сохранено");
            assert!(
                matches!(&saved.plan[..], [ApplyStep::Columns { visibility: true, order: false, defs: false }]),
                "план: {:?}",
                saved.plan
            );
            assert_eq!(h.io.count_read_theme(), 0, "тема не читается");
            assert_eq!(engine_cmd_count(&h, "SetModeSettings"), 0);
            assert!(h.engine_cmds().is_empty(), "команд движку нет: {:?}", h.engine_cmds());
        }

        // Только размер обложки: один шаг `TopPanel`, движок не тронут
        // (0 переоткрытий устройства), тема не читается.
        {
            let (h, mut core) = boot_with_dialog();
            core.dialog_edit(|d| d.settings.top_panel.cover_size += 1.0);
            let saved = core.dialog_save().expect("размер обложки изменён — сохранено");
            assert_eq!(saved.plan, vec![ApplyStep::TopPanel]);
            assert_eq!(h.io.count_read_theme(), 0, "тема не читается");
            assert!(h.engine_cmds().is_empty(), "команд движку нет: {:?}", h.engine_cmds());
        }

        // Параметр неактивного режима: один `SetModeSettings` с одним
        // изменённым параметром, активный режим прежний.
        {
            let (h, mut core) = boot_with_dialog();
            assert_eq!(core.state().active_mode(), ModeKind::Compatible);
            assert_ne!(core.settings().playback.strict.device_buffer, DeviceBuffer::Ms100);
            core.dialog_edit(|d| d.settings.playback.strict.device_buffer = DeviceBuffer::Ms100);
            let saved = core.dialog_save().expect("параметр режима изменён — сохранено");
            match &saved.plan[..] {
                [ApplyStep::Engine(update)] => {
                    let changed = update.diff.as_ref().map(|m| m.changed.len());
                    assert_eq!(changed, Some(1), "ровно один изменённый параметр: {update:?}");
                }
                other => panic!("ожидался один шаг Engine, план: {other:?}"),
            }
            assert_eq!(engine_cmd_count(&h, "SetModeSettings"), 1);
            assert_eq!(h.engine_cmds().len(), 1, "других команд движку нет: {:?}", h.engine_cmds());
            assert_eq!(core.state().active_mode(), ModeKind::Compatible, "активный режим не переключён");
            assert_eq!(h.io.count_read_theme(), 0, "тема не читается");
        }
    }

    /// Ошибка записи `settings.toml` при «Сохранить» не мешает закрытию
    /// диалога и применению (ТЗ-20, ТЗ-28, §6.9, §7.2): диалог закрыт, тема
    /// действует и стоит в плане; ответ писателя даёт одно окно Error,
    /// называющее `settings.toml`; путь выхода делает ровно одну новую
    /// попытку записи настроек.
    #[test]
    fn save_write_error_closes_dialog_and_applies() {
        let h = Harness::new();
        h.put_default_settings();
        h.put_state(b"");
        let mut core = h.boot();
        let disk_before = h.disk_bytes(WorkFile::Settings);

        let caps = PlatformCaps::default();
        let mut messages = MessageCenter::default();
        let notifier = FakeNotifier::new();
        messages.window_shown();

        h.fail_write_work(WorkFile::Settings, WriteStep::WriteData, WriteErrorClass::NoSpace, 1);
        core.dialog_open().expect("диалог открыт");
        let (theme, data) = pick_other_theme(&mut core);

        let saved = core.dialog_save().expect("ошибка записи сохранению не мешает");
        assert!(saved.write.is_none(), "ошибка писателя приходит позже, ответом в tick");
        assert!(core.draft().is_none(), "диалог закрыт");
        assert!(!core.gate().window_blocked(), "окно разблокировано");
        assert_eq!(core.settings().theme, theme, "тема действует");
        assert!(
            saved.plan.iter().any(|s| matches!(s, ApplyStep::Theme(applied) if Arc::ptr_eq(applied, &data))),
            "план: {:?}",
            saved.plan
        );

        let effects: Vec<ReplyEffect> = h.settle_ticks(&mut core).into_iter().flatten().collect();
        assert_eq!(effects, vec![ReplyEffect::Failed(WorkFile::Settings, WriteErrorClass::NoSpace)]);
        apply_write_failures(effects, &mut messages, &notifier, caps);
        let shown = messages.shown().expect("окно ошибки записи показано");
        assert_eq!(shown.level, MessageLevel::Error);
        assert!(shown.body.contains("settings.toml"), "текст окна: {}", shown.body);
        assert_eq!(notifier.count(), 0, "без трея уведомлений нет");
        assert_eq!(h.disk_bytes(WorkFile::Settings), disk_before, "файл на диске прежний");
        assert_eq!(core.settings().theme, theme, "тема осталась применённой после ошибки");

        let before = h.settings_counts();
        assert_eq!(before.writes, 0);
        assert_eq!(before.failures, 1, "одна неудачная попытка при «Сохранить»");

        messages.dismiss_for_exit();
        let outcome = core.exit(ExitReason::WindowClose, &mut || true);
        assert_eq!(outcome, ExitOutcome::Completed);

        let after = h.settings_counts();
        assert_eq!(
            (after.writes + after.failures) - (before.writes + before.failures),
            1,
            "путь выхода делает ровно одну попытку записи настроек"
        );
        assert_eq!(after.writes, 1, "попытка при выходе успешна");
        let text = String::from_utf8(h.disk_bytes(WorkFile::Settings).expect("settings written"))
            .expect("settings.toml is valid utf-8");
        assert!(text.contains(&format!("theme = \"{}\"", theme.as_str())), "тема в файле: {text}");
    }

    /// Все причины выхода (ТЗ-14). `match` без ветки `_`: новый вариант
    /// `ExitReason`/`TermSignal` ломает сборку, пока не добавлен в список.
    fn all_exit_reasons() -> Vec<ExitReason> {
        let all = vec![
            ExitReason::WindowClose,
            ExitReason::TrayQuit,
            ExitReason::Signal(TermSignal::Term),
            ExitReason::Signal(TermSignal::Int),
            ExitReason::Signal(TermSignal::Hup),
            ExitReason::WindowsSessionEnd,
            ExitReason::MacosTerminate,
        ];
        for reason in &all {
            match reason {
                ExitReason::WindowClose
                | ExitReason::TrayQuit
                | ExitReason::Signal(TermSignal::Term | TermSignal::Int | TermSignal::Hup)
                | ExitReason::WindowsSessionEnd
                | ExitReason::MacosTerminate => {}
            }
        }
        all
    }

    /// Выход при открытом диалоге отбрасывает черновик (ТЗ-32, §6.10, §7.2) —
    /// по каждой причине выхода: тема, выбранная в черновике, не попадает ни
    /// в действующие настройки, ни в `settings.toml`; `state.toml` записан с
    /// громкостью, изменённой до открытия диалога.
    #[test]
    fn exit_with_open_dialog_discards_draft() {
        for reason in all_exit_reasons() {
            let h = Harness::new();
            h.put_default_settings();
            h.put_state(b"");
            let mut core = h.boot();
            let settings = core.settings().clone();
            let settings_bytes = h.disk_bytes(WorkFile::Settings).expect("settings.toml на диске");
            let old_state = core.state().clone();

            core.change_state(Origin::User, StateChange::Volume(42));
            assert_ne!(core.state(), &old_state, "{reason:?}: громкость изменена");
            let new_state = core.state().clone();

            core.dialog_open().expect("диалог открыт");
            let (draft_theme, _) = pick_other_theme(&mut core);
            assert_ne!(draft_theme, settings.theme);
            assert!(core.save_button().enabled, "{reason:?}: в черновике есть отличие");

            let outcome = core.exit(reason, &mut || true);
            assert_eq!(outcome, ExitOutcome::Completed, "{reason:?}");

            assert!(core.draft().is_none(), "{reason:?}: черновик отброшен");
            assert_eq!(core.settings(), &settings, "{reason:?}: настройки прежние");
            assert_eq!(core.state(), &new_state, "{reason:?}: состояние сессии не откатилось");

            let on_disk = h.disk_bytes(WorkFile::Settings).expect("settings.toml на диске");
            assert_eq!(on_disk, settings_bytes, "{reason:?}: settings.toml прежний");
            let text = String::from_utf8(on_disk).expect("settings.toml is valid utf-8");
            assert!(
                text.contains(&format!("theme = \"{}\"", settings.theme.as_str())),
                "{reason:?}: в settings.toml прежняя тема: {text}"
            );
            assert!(
                !text.contains(&format!("theme = \"{}\"", draft_theme.as_str())),
                "{reason:?}: темы черновика в settings.toml нет"
            );

            assert_eq!(h.writes(WorkFile::State), 1, "{reason:?}: state.toml записан");
            let expected = crate::persist::state_file::serialize_state(&new_state).expect("serialize state");
            assert_eq!(
                h.disk_bytes(WorkFile::State),
                Some(expected.to_vec()),
                "{reason:?}: в state.toml новая громкость"
            );
            let stale = crate::persist::state_file::serialize_state(&old_state).expect("serialize state");
            assert_ne!(expected.to_vec(), stale.to_vec(), "{reason:?}: записанное отличается от исходного");
        }
    }
}
