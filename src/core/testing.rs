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

use super::{AppCore, AppDeps, TickOutput};
use crate::audio::clock::{Clock, ClockInstant};
use crate::audio::testing::ManualClock;
use crate::core::exit::{ManualWaiter, ReplyWaiter};
use crate::journal::{Journal, JournalRecord, VecJournal};
use crate::persist::keys::Parsed;
use crate::persist::tracker::ReplyEffect;
use crate::persist::writer::spawn_writer;
use crate::persist::{self, BadCopyOutcome, ConfigFile, ConfigPaths, WorkFile};
use crate::platform::fs::{FileWriter, FsCall, MemStore, OpCounts, ReadErrorClass, WriteErrorClass, WriteStep};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Каталог настроек стенда — не каталог пользователя (ТЗ-49).
fn test_dir() -> PathBuf {
    PathBuf::from("/cfg")
}

pub(crate) struct Harness {
    fs: MemStore,
    paths: ConfigPaths,
    journal: Arc<VecJournal>,
    clock: ManualClock,
    playlist: RefCell<Arc<[u8]>>,
}

impl Harness {
    /// Пустой каталог: оба файла отсутствуют, пока не вызван `put_*`.
    pub(crate) fn new() -> Harness {
        Harness {
            fs: MemStore::new(),
            paths: ConfigPaths::in_dir(test_dir()),
            journal: Arc::new(VecJournal::default()),
            clock: ManualClock::new(),
            playlist: RefCell::new(Arc::from(&b""[..])),
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
        let deps = AppDeps { writer, paths: self.paths.clone(), clock, waiter, journal };
        AppCore::with_deps(deps, boot)
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

    /// Текущий снимок плейлиста для `tick`/`advance` (§7.1): источник байт
    /// настраивается тестом через `set_playlist`.
    fn playlist_snapshot(&self) -> Arc<[u8]> {
        self.playlist.borrow().clone()
    }

    /// Задать байты плейлиста, которые вернёт замыкание `tick` (§7.1).
    pub(crate) fn set_playlist(&self, bytes: &[u8]) {
        *self.playlist.borrow_mut() = Arc::from(bytes);
    }

    /// Продвигает инжектируемые часы на `d`, затем выполняет один тик
    /// `AppCore` (ADR-19, §6.4, §7.1): срабатывание дедлайна отложенной
    /// записи и разбор ответов писателя идут по симулированному времени.
    pub(crate) fn advance(&self, core: &mut AppCore, d: Duration) -> TickOutput {
        self.clock.advance(d);
        core.tick(None, &|| self.playlist_snapshot())
    }

    /// Тикает, пока у `PersistTracker` остаётся хоть один файл «в полёте»
    /// (§7.1, §6.10): тот же протокол опроса, что у `ManualWaiter` — ждёт
    /// ответа писателя `apap-persist`, не продвигая симулированное время.
    /// Срок — 2 с реального времени; превышение — ошибка теста (зависший
    /// писатель), а не штатный исход.
    pub(crate) fn settle(&self, core: &mut AppCore) {
        let start = Instant::now();
        loop {
            core.tick(None, &|| self.playlist_snapshot());
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
            let out = core.tick(None, &|| self.playlist_snapshot());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::visualizer::VisualizationMode;
    use crate::core::exit::{ExitOutcome, ExitPhase, ExitReason, TermSignal};
    use crate::core::messages::{MessageCenter, MessageLevel};
    use crate::journal::WriteTarget;
    use crate::platform::fs::FsOp;
    use crate::platform::lifecycle::PlatformCaps;
    use crate::platform::notify::{FakeNotifier, Notifier};
    use crate::persist::keys::{KeyPath, LoadNoteKind};
    use crate::persist::settings_file::{SaveInterval, Settings, ThemeName};
    use crate::persist::state_file::{Origin, PhysPos, PhysSize, SessionState, SizeUnits, StateChange, WindowGeometry};
    use crate::settings::{RepeatMode, ResamplerAlgorithm};

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

        let outcome = core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b""[..]));
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

        let outcome = core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b""[..]));
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
    /// (`muted`), недопустимое значение (`volume`), неизвестный ключ (`bogus`).
    #[test]
    fn parse_by_keys_three_notes() {
        let h = Harness::new();
        h.put_state(
            b"volume = 200\nrepeat = \"off\"\nshuffle = false\nbogus = 1\n\n\
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
    /// `set_playlist` задаёт байты, которые вернёт замыкание `tick`; после
    /// успешной записи флаг «грязного» плейлиста снят.
    #[test]
    fn playlist_dirty_written_by_timer() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        h.set_playlist(b"track1.flac\ntrack2.flac\n");
        core.playlist_changed();
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
                let volume = u8::try_from((block * 6 + u32::from(i)) % 256).expect("fits u8");
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
        core.tick(Some(actual), &|| Arc::from(&b""[..]));

        h.advance(&mut core, Duration::from_secs(600));
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::State), 0);

        core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b""[..]));
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

        core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b""[..]));
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
        h.set_playlist(b"track1.flac\n");
        core.playlist_changed();

        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);
        assert_eq!(h.writes(WorkFile::Playlist), 0);
        assert!(core.tracker().playlist_dirty());

        // Новое изменение взводит срок заново, но попытки по таймеру
        // всё равно блокированы (`stopped`), пока нет явного `retry`.
        h.set_playlist(b"track1.flac\ntrack2.flac\n");
        core.playlist_changed();
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
        h.set_playlist(b"track1.flac\n");
        core.playlist_changed();
        h.advance(&mut core, interval); // срок истёк — первый снимок отправлен, но задержан

        // во время задержанной записи плейлист снова меняется
        h.set_playlist(b"track1.flac\ntrack2.flac\n");
        core.playlist_changed();

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

    /// Переключение mute взводит срок N; через N с — одна запись (ТЗ-38,
    /// §6.4, §7.2 `mute_starts_timer`).
    #[test]
    fn mute_starts_timer() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Muted(true));
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::State), 1);
        assert!(core.state().playback().muted);
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
    /// байтами, содержащими все три новых значения (ТЗ-28, §6.9).
    #[test]
    fn dialog_save_single_write_all_fields() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        let mut settings = core.settings().clone();
        settings.theme = ThemeName::new("dark").expect("valid theme name");
        settings.playback.audio_device = "hw:1,0".to_string();
        settings.playback.audio.resampler.algorithm = ResamplerAlgorithm::SincFast;
        core.save_settings_now(settings);
        h.settle(&mut core);

        assert_eq!(h.writes(WorkFile::Settings), 1);
        let bytes = h.disk_bytes(WorkFile::Settings).expect("settings written");
        let text = String::from_utf8(bytes).expect("settings.toml is valid utf-8");
        assert!(text.contains("theme = \"dark\""));
        assert!(text.contains("audio_device = \"hw:1,0\""));
        assert!(text.contains("[audio.resampler]\nalgorithm = \"sinc_fast\""));
    }

    /// «Сохранить» без изменений относительно текущего состояния — запись
    /// не отправляется, даже когда дедлайн затем срабатывает (ТЗ-28, §6.9).
    #[test]
    fn dialog_save_without_changes_does_not_write() {
        let h = Harness::new();
        let settings_bytes = crate::persist::settings_file::serialize_settings(&Settings::default())
            .expect("serialize settings");
        h.put_settings(&settings_bytes);
        h.put_state(b"");
        let mut core = h.boot();

        let result = core.save_settings_now(core.settings().clone());
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

        let retry_effects = core.retry(&[WorkFile::State], &|| Arc::from(&b""[..]));
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

        h.set_playlist(b"track1.flac\n");
        core.change_state(Origin::User, StateChange::Volume(42));
        core.playlist_changed();

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
        settings.playback.audio_device = "hw:1,0".into();
        core.save_settings_now(settings);

        h.delay_write_work(WorkFile::State, Duration::from_secs(10));
        core.change_state(Origin::User, StateChange::Volume(42));

        h.delay_write_work(WorkFile::Playlist, Duration::from_secs(10));
        h.set_playlist(b"track1.flac\n");
        core.playlist_changed();

        let before_settings = h.disk_bytes(WorkFile::Settings);
        let before_state = h.disk_bytes(WorkFile::State);
        let before_playlist = h.disk_bytes(WorkFile::Playlist);

        let start = h.now();
        let outcome = core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b"track1.flac\n"[..]));
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

        h.set_playlist(b"track1.flac\n");
        core.playlist_changed();
        core.change_state(Origin::User, StateChange::Volume(42));

        h.delay_write_work(WorkFile::Playlist, Duration::from_secs(4));
        h.delay_write_work(WorkFile::State, Duration::from_secs(6));

        let outcome = core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b"track1.flac\n"[..]));
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

        let outcome = core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b""[..]));
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

            let outcome = core.exit(reason, &mut || true, &|| Arc::from(&b""[..]));
            assert_eq!(outcome, ExitOutcome::Completed, "{reason:?}: без изменений");
            assert_eq!(h.writes(WorkFile::State), 0, "{reason:?}: state.toml не пишется без изменений");
            assert_eq!(h.writes(WorkFile::Settings), 0, "{reason:?}: settings.toml не пишется без изменений");

            let h = Harness::new();
            h.put_settings(&settings_bytes);
            h.put_state(&state_bytes);
            let mut core = h.boot();

            core.change_state(Origin::User, StateChange::Volume(42));
            let mut settings = core.settings().clone();
            settings.playback.audio_device = "hw:1,0".into();
            core.set_settings(settings);

            let outcome = core.exit(reason, &mut || true, &|| Arc::from(&b""[..]));
            assert_eq!(outcome, ExitOutcome::Completed, "{reason:?}: с изменениями");
            assert_eq!(h.writes(WorkFile::State), 1, "{reason:?}: state.toml записан ровно раз");
            assert_eq!(h.writes(WorkFile::Settings), 1, "{reason:?}: settings.toml записан ровно раз");
        }
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

        let outcome = core.exit(ExitReason::WindowsSessionEnd, &mut || true, &|| Arc::from(&b""[..]));

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

        let first = core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b""[..]));
        assert_eq!(first, ExitOutcome::Completed);
        let writes_after_first = h.writes(WorkFile::State);

        let second = core.exit(ExitReason::TrayQuit, &mut || true, &|| Arc::from(&b""[..]));
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

        let outcome = core.exit(ExitReason::WindowClose, &mut || true, &|| Arc::from(&b""[..]));
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
        h.set_playlist(b"/music/a.flac\n");
        core.playlist_changed();
        let interval = core.settings().save_interval.duration();
        h.advance(&mut core, interval);
        h.settle(&mut core);

        let mut settings = core.settings().clone();
        settings.save_interval = SaveInterval::S10;
        core.save_settings_now(settings);
        h.settle(&mut core);

        core.change_state(Origin::User, StateChange::Volume(17));
        let outcome = core.exit(ExitReason::TrayQuit, &mut || true, &|| Arc::from(&b"/music/b.flac\n"[..]));
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
        settings.playback.audio_device = "hw:1,0".into();
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

        let outcome = core.exit(ExitReason::TrayQuit, &mut || true, &|| Arc::from(&b""[..]));
        assert_eq!(outcome, ExitOutcome::Completed);

        let after = h.state_counts();
        let attempts_before = before.writes + before.failures;
        let attempts_after = after.writes + after.failures;
        assert_eq!(attempts_after - attempts_before, 1, "путь выхода делает ровно одну новую попытку");
        assert_eq!(after.writes, before.writes + 1, "вторая попытка успешна — файл записан");
        assert!(!messages.is_shown(), "после выхода ничего не показано заново");
    }
}
