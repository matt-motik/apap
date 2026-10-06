//! AppCore — логика приложения без Slint (ADR-19, §2.1).
//!
//! Действующие настройки и состояние, трек-лист, шлюз главного окна,
//! центр сообщений и прочая логика, не зависящая от UI-фреймворка,
//! живут здесь; `src/app/` — тонкая прослойка Slint над `AppCore`.

pub mod exit;
pub mod gate;
pub mod geometry;
pub mod messages;
#[cfg(test)]
pub(crate) mod testing;

use std::path::Path;
use std::sync::Arc;

use exit::{ExitCoordinator, ExitOutcome, ExitPhase, ExitReason, ExitReport, ReplyWaiter};
use geometry::GeometryTracker;
use messages::{Message, MessageButtons, MessageLevel};

use crate::audio::clock::{Clock, ClockInstant};
use crate::journal::{Journal, JournalRecord, WriteTarget};
use crate::persist::keys::{LoadNote, Parsed};
use crate::persist::settings_file::{serialize_settings, Settings};
use crate::persist::state_file::{serialize_state, Origin, SessionState, StateChange, WindowGeometry};
use crate::persist::tracker::{PersistTracker, ReplyEffect};
use crate::persist::writer::{WriterCmd, WriterHandle, WriterReply};
use crate::persist::{Boot, ConfigFile, ConfigPaths, ReferenceText, SerializeError, Snapshot, SnapshotId, WorkFile};
use crate::platform::fs::{FileWriter, ReadError, WriteError};

/// Эталон и запрет автозаписи одного файла (§2.2, §2.12, И-Р3, И-Р20).
/// Упрощённый аналог `FileTrack` (§2.7) без очереди «в пути» и дедлайнов —
/// они появляются вместе с писателем `apap-persist` (С4, §8).
#[derive(Default)]
struct FileState {
    reference: ReferenceText,
    auto_forbidden: bool,
}

/// Заметки разбора обоих файлов при запуске (ТЗ-5), собранные для будущих
/// записей журнала «заметки загрузки» / «неразбираемый файл» — вариантов
/// `JournalRecord` для них пока нет (добавление вне вайтлиста С3, см.
/// `persist::journal_records_for_boot`); этот тип — то, что потребуется шагу,
/// который их добавит.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct StartupNotes {
    pub settings: Vec<LoadNote>,
    pub state: Vec<LoadNote>,
}

/// Разбирает `Parsed<T>` по общим правилам (§2.3, §6.1) независимо от `T`:
/// значение — всегда из `Parsed`; эталон — только из `Parsed::Parsed`
/// (ОВ-2); запрет автозаписи и ошибка чтения — только из `Parsed::ReadFailed`
/// (И-Р20, ОВС-6 в).
fn split_parsed<T>(parsed: Parsed<T>) -> (T, FileState, Vec<LoadNote>, Option<ReadError>) {
    match parsed {
        Parsed::Absent { value } => (value, FileState::default(), Vec::new(), None),
        Parsed::Parsed { value, notes, reference } => {
            (value, FileState { reference, auto_forbidden: false }, notes, None)
        }
        Parsed::Unparsable { value, .. } => (value, FileState::default(), Vec::new(), None),
        Parsed::ReadFailed { value, err } => {
            (value, FileState { reference: ReferenceText::default(), auto_forbidden: true }, Vec::new(), Some(err))
        }
    }
}

/// Итог попытки синхронно записать один файл во `flush` (§2.6, §2.12,
/// И-Р3, И-Р20).
#[derive(Debug)]
pub enum FlushOutcome {
    /// Сериализованный текст не отличается от эталона — запись не нужна (И-Р3).
    Unchanged(ConfigFile),
    /// Автозапись файла запрещена до конца сеанса (И-Р20, ОВС-6 в).
    Forbidden(ConfigFile),
    /// Файл записан; эталон обновлён.
    Written(ConfigFile),
    /// Сериализация или запись завершились ошибкой (ТЗ-20).
    Failed { file: ConfigFile, err: WriteError },
}

/// Записи журнала о неудачной записи рабочего файла (ТЗ-20) — для случаев
/// `FlushOutcome::Failed`, по образцу `persist::journal_records_for_bad_copies`.
pub fn journal_records_for_flush(outcomes: &[FlushOutcome]) -> Vec<JournalRecord> {
    outcomes
        .iter()
        .filter_map(|o| match o {
            FlushOutcome::Failed { file, err } => {
                Some(JournalRecord::WriteFailed { target: WriteTarget::Work(file.work()), err: err.clone() })
            }
            _ => None,
        })
        .collect()
}

/// Пишет один файл, если это разрешено и текст отличается от эталона;
/// обновляет эталон при успехе (И-Р3, И-Р20).
fn flush_file(
    file: ConfigFile,
    track: &mut FileState,
    bytes: Result<Arc<[u8]>, SerializeError>,
    writer: &mut dyn FileWriter,
    path: &Path,
) -> FlushOutcome {
    if track.auto_forbidden {
        return FlushOutcome::Forbidden(file);
    }
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(SerializeError(msg)) => return FlushOutcome::Failed { file, err: WriteError::serialize(&msg, path) },
    };
    if !track.reference.differs(&bytes) {
        return FlushOutcome::Unchanged(file);
    }
    match writer.write_atomic(path, &bytes) {
        Ok(()) => {
            track.reference = ReferenceText::of(bytes);
            FlushOutcome::Written(file)
        }
        Err(err) => FlushOutcome::Failed { file, err },
    }
}

/// Внешние зависимости нового API `AppCore` (ADR-19, §2.12): писатель
/// `apap-persist`, пути конфигурации, инжектируемые часы, ожидание ответа
/// на пути выхода и журнал.
///
/// ОТКЛОНЕНИЕ от §2.12: полный `AppDeps` спецификации содержит также
/// `Lifecycle`, движок (`EngineSink`) и загрузчик обложек — они появляются
/// на последующих этапах. Остановка движка на пути выхода здесь не
/// типизирована отдельным полем — `exit()` принимает её замыканием
/// `&mut dyn FnMut() -> bool`, чтобы не заводить трейт под ещё не
/// существующий тип движка.
pub struct AppDeps {
    pub writer: WriterHandle,
    pub paths: ConfigPaths,
    pub clock: Box<dyn Clock>,
    pub waiter: Box<dyn ReplyWaiter>,
    pub journal: Arc<dyn Journal>,
}

/// Ядро приложения без Slint (ADR-19, §2.12): владеет настройками и
/// состоянием сессии, их эталонными текстами и запретом автозаписи, а также
/// (при наличии `deps`) `PersistTracker`, координатором выхода и трекером
/// геометрии окна.
/// Полный состав `AppCore` по спецификации (`Playlist`, `UiGate`,
/// `MessageCenter`, `LoadState` и т. д.) появляется поэтапно; на этом шаге —
/// то, что нужно для чтения/изменения настроек и состояния, синхронной
/// записи (старый мост) и отложенной записи через писателя (новый API,
/// §6.4, §6.8, §6.10).
pub struct AppCore {
    settings: Settings,
    settings_file: FileState,
    /// Ошибка чтения `settings.toml`, если он не прочитан (ОВС-6 в, §2.13).
    /// `state.toml` не даёт сообщения — только запрет автозаписи (И-Р20).
    settings_read_failed: Option<ReadError>,
    state: SessionState,
    state_file: FileState,
    startup_notes: StartupNotes,
    /// `None` в режиме старого моста (`new(boot)`) — методы нового API
    /// в этом режиме не выполняют I/O (§2.12).
    deps: Option<AppDeps>,
    tracker: PersistTracker,
    exit: ExitCoordinator,
    geometry: GeometryTracker,
}

/// Общая часть `new`/`with_deps`: разбирает `boot` и строит `PersistTracker`
/// с тем же эталоном и запретом автозаписи, что уже применяет старый мост
/// (ТЗ-11, И-Р20, §2.7, §2.12).
#[allow(clippy::type_complexity)]
fn from_boot(boot: Boot) -> (Settings, FileState, Option<ReadError>, SessionState, FileState, StartupNotes, PersistTracker) {
    let (settings, settings_file, settings_notes, settings_read_failed) = split_parsed(boot.settings);
    let (state, state_file, state_notes, _) = split_parsed(boot.state);

    let mut tracker = PersistTracker::new(settings.save_interval, settings_file.reference.clone(), state_file.reference.clone());
    if settings_file.auto_forbidden {
        tracker.forbid_auto(ConfigFile::Settings);
    }
    if state_file.auto_forbidden {
        tracker.forbid_auto(ConfigFile::State);
    }

    let startup_notes = StartupNotes { settings: settings_notes, state: state_notes };
    (settings, settings_file, settings_read_failed, state, state_file, startup_notes, tracker)
}

impl AppCore {
    /// Строит `AppCore` из результата `persist::boot` без зависимостей
    /// писателя (ADR-23 шаг 2, §2.12) — старый мост `src/app/`: `flush` пишет
    /// синхронно, методы нового API (`tick`, `exit`, ...) не выполняют I/O.
    pub fn new(boot: Boot) -> AppCore {
        let (settings, settings_file, settings_read_failed, state, state_file, startup_notes, tracker) = from_boot(boot);
        AppCore {
            settings,
            settings_file,
            settings_read_failed,
            state,
            state_file,
            startup_notes,
            deps: None,
            tracker,
            exit: ExitCoordinator::new(),
            geometry: GeometryTracker::new(),
        }
    }

    /// Строит `AppCore` с зависимостями нового API (ADR-19, §2.12).
    ///
    /// ОТКЛОНЕНИЕ от §2.12: там это тот же конструктор `new(deps, boot)`;
    /// здесь — отдельное имя, так как `new(boot)` уже занят старым мостом.
    pub fn with_deps(deps: AppDeps, boot: Boot) -> AppCore {
        let (settings, settings_file, settings_read_failed, state, state_file, startup_notes, tracker) = from_boot(boot);
        AppCore {
            settings,
            settings_file,
            settings_read_failed,
            state,
            state_file,
            startup_notes,
            deps: Some(deps),
            tracker,
            exit: ExitCoordinator::new(),
            geometry: GeometryTracker::new(),
        }
    }

    /// Текущее время по инжектируемым часам (ADR-20); без `deps` (мост) —
    /// начало отсчёта `ClockInstant::START`.
    fn now(&self) -> ClockInstant {
        match &self.deps {
            Some(deps) => deps.clock.now(),
            None => ClockInstant::START,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Заменяет настройки целиком — действие «Сохранить» диалога настроек
    /// (ТЗ-28, §2.12). Запрет автозаписи (И-Р20) на `set_settings` не влияет:
    /// «Сохранить» пишет как обычно (ОВС-6 в) — различение «автоматической»
    /// и «ручной» записи приходит вместе с писателем `apap-persist` (С4).
    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = settings;
    }

    /// Единственный изменитель состояния сессии (И-Т7, ADR-22, §2.12). При
    /// наличии `deps` взводит дедлайн отложенной записи `state.toml` для
    /// `Origin::User` — `PersistTracker::on_state_changed` сам не взводит
    /// его для `Origin::Program` (ТЗ-11).
    pub fn change_state(&mut self, origin: Origin, ch: StateChange) {
        self.state.apply(ch);
        if self.deps.is_some() {
            let now = self.now();
            self.tracker.on_state_changed(origin, now);
        }
    }

    /// Плейлист изменён пользователем — взводит дедлайн отложенной записи
    /// `playlist.m3u` (ТЗ-12, §2.7, §6.4). Без `deps` — без эффекта.
    pub fn playlist_changed(&mut self) {
        if self.deps.is_some() {
            let now = self.now();
            self.tracker.on_playlist_changed(now);
        }
    }

    /// Запрещает запись плейлиста до конца сеанса (случай 3 ОВ-8, §2.7).
    pub fn forbid_playlist(&mut self) {
        self.tracker.forbid_playlist();
    }

    /// Программная установка геометрии окна (восстановление при старте или
    /// повторное применение после `show()`) — запоминается как эхо, не
    /// взводит дедлайн записи (ADR-22, §6.17).
    pub fn program_set_geometry(&mut self, g: WindowGeometry) {
        self.geometry.program_set(g);
    }

    /// Окно показано: следующее показание геометрии на тике — ожидаемое
    /// эхо повторного применения после `show()` (ОВС-5 а, ТЗ-11, §6.17).
    pub fn window_shown(&mut self) {
        self.geometry.window_shown();
    }

    /// Синхронно пишет `settings.toml`/`state.toml`, если текст отличается
    /// от эталона (И-Р3) и автозапись файла не запрещена (И-Р20). Временная
    /// реализация до писателя `apap-persist`: пишет файл целиком в прежние
    /// моменты (С4, §8 С3).
    pub fn flush(&mut self, writer: &mut dyn FileWriter, paths: &ConfigPaths) -> Vec<FlushOutcome> {
        let settings_bytes = serialize_settings(&self.settings);
        let settings_outcome =
            flush_file(ConfigFile::Settings, &mut self.settings_file, settings_bytes, writer, &paths.settings);
        let state_bytes = serialize_state(&self.state);
        let state_outcome = flush_file(ConfigFile::State, &mut self.state_file, state_bytes, writer, &paths.state);
        vec![settings_outcome, state_outcome]
    }

    /// Отправляет снимок писателю `apap-persist`: заводит `SnapshotId`,
    /// отмечает его в `PersistTracker` как «в пути» и шлёт `WriterCmd::Write`
    /// (§6.5). Без `deps` — только заводит `SnapshotId`, без I/O.
    fn send_snapshot(&mut self, file: WorkFile, bytes: Arc<[u8]>, playlist_seq: Option<u64>) -> SnapshotId {
        let id = self.tracker.next_id();
        let snap = Snapshot { id, file, bytes };
        self.tracker.sent(&snap, playlist_seq);
        if let Some(deps) = &self.deps {
            deps.writer.send(WriterCmd::Write(snap));
        }
        id
    }

    /// Заметки разбора обоих файлов при запуске — данные для будущих записей
    /// журнала «заметки загрузки» / «неразбираемый файл» (см. `StartupNotes`).
    pub fn startup_notes(&self) -> &StartupNotes {
        &self.startup_notes
    }

    /// Окно Error при непрочитанном `settings.toml` (ОВС-6 в, §2.13); `None`,
    /// если файл прочитан или отсутствует. `state.toml` такого окна не даёт.
    pub fn settings_read_failed_message(&self) -> Option<Message> {
        let err = self.settings_read_failed.as_ref()?;
        Some(Message {
            level: MessageLevel::Error,
            title: "Файл настроек не прочитан".into(),
            body: format!(
                "Файл настроек не удалось прочитать ({}). Используются значения по умолчанию. \
                 Автоматическое сохранение отключено до перезапуска. Кнопка «Сохранить» в окне \
                 настроек заменит файл.",
                err.os_text
            )
            .into(),
            buttons: MessageButtons::Ok,
        })
    }

    /// Постоянный текст рядом с «Сохранить» в диалоге настроек, пока
    /// действует запрет автозаписи `settings.toml` (ОВС-6 в, §2.13).
    pub fn settings_save_notice(&self) -> Option<&'static str> {
        self.settings_file
            .auto_forbidden
            .then_some("Файл настроек не прочитан — «Сохранить» заменит его значениями из этого окна")
    }

    /// Один тик цикла приложения (ADR-3, ADR-22, §6.4): разбирает ответы
    /// писателя, проверяет срок отложенной записи и показание геометрии
    /// окна. `geometry` — текущее показание, если платформа его отдаёт;
    /// `playlist_bytes` сериализует плейлист лениво — только когда он
    /// действительно нужен к отправке. Без `deps` (мост) — пустой результат.
    pub fn tick(&mut self, geometry: Option<WindowGeometry>, playlist_bytes: &dyn Fn() -> Arc<[u8]>) -> TickOutput {
        let Some(deps) = &self.deps else {
            return TickOutput { effects: Vec::new(), other: Vec::new() };
        };

        let mut effects = Vec::new();
        let mut other = Vec::new();
        while let Some(reply) = deps.writer.try_recv() {
            if let WriterReply::Failed { file, err, .. } = &reply {
                deps.journal.record(JournalRecord::WriteFailed { target: WriteTarget::Work(*file), err: err.clone() });
            }
            match self.tracker.on_reply(&reply) {
                ReplyEffect::None => other.push(reply),
                effect => effects.push(effect),
            }
        }

        let now = self.now();
        let due = self.tracker.poll_deadline(now);

        if due.state && self.tracker.auto_allowed(ConfigFile::State) && !self.tracker.timer_stopped(WorkFile::State) {
            match serialize_state(&self.state) {
                Ok(bytes) => {
                    if self.tracker.toml_needs_write(ConfigFile::State, &bytes) {
                        self.send_snapshot(WorkFile::State, bytes, None);
                    }
                }
                Err(SerializeError(msg)) => {
                    if let Some(deps) = &self.deps {
                        let err = WriteError::serialize(&msg, &deps.paths.state);
                        deps.journal.record(JournalRecord::WriteFailed { target: WriteTarget::Work(WorkFile::State), err });
                    }
                }
            }
        }

        if due.playlist && self.tracker.playlist_writable(true) {
            let bytes = playlist_bytes();
            self.send_snapshot(WorkFile::Playlist, bytes, None);
        }

        if let Some(g) = geometry {
            if let Some((g, origin)) = self.geometry.observe(g) {
                self.change_state(origin, StateChange::Window(g));
            }
        }

        TickOutput { effects, other }
    }

    /// «Сохранить» в диалоге настроек (ТЗ-28, §6.4): пишет `settings.toml`
    /// немедленно, независимо от запрета автозаписи и эталона; меняет срок
    /// отложенной записи, если интервал изменился. Без `deps` — только
    /// заменяет настройки в памяти (мост).
    pub fn save_settings_now(&mut self, settings: Settings) -> Option<ReplyEffect> {
        let old_interval = self.settings.save_interval;
        self.settings = settings;
        self.deps.as_ref()?;

        if self.settings.save_interval != old_interval {
            let now = self.now();
            self.tracker.set_interval(self.settings.save_interval, now);
        }

        let bytes = match serialize_settings(&self.settings) {
            Ok(bytes) => bytes,
            Err(SerializeError(msg)) => {
                let Some(deps) = &self.deps else { return None };
                let err = WriteError::serialize(&msg, &deps.paths.settings);
                deps.journal.record(JournalRecord::WriteFailed { target: WriteTarget::Work(WorkFile::Settings), err: err.clone() });
                return Some(ReplyEffect::Failed(WorkFile::Settings, err.class));
            }
        };
        if !self.tracker.toml_needs_write(ConfigFile::Settings, &bytes) {
            return None;
        }
        self.send_snapshot(WorkFile::Settings, bytes, None);
        None
    }

    /// «Повторить» (ТЗ-20, §6.8): немедленная отправка снимков указанных
    /// файлов независимо от дедлайна и эталона; плейлист — если он не под
    /// постоянным запретом (случай 3 ОВ-8). Без `deps` — без эффекта.
    pub fn retry(&mut self, files: &[WorkFile], playlist_bytes: &dyn Fn() -> Arc<[u8]>) -> Vec<ReplyEffect> {
        let mut effects = Vec::new();
        if self.deps.is_none() {
            return effects;
        }

        for &file in files {
            match file {
                WorkFile::Settings => match serialize_settings(&self.settings) {
                    Ok(bytes) => {
                        self.send_snapshot(WorkFile::Settings, bytes, None);
                    }
                    Err(SerializeError(msg)) => {
                        if let Some(deps) = &self.deps {
                            let err = WriteError::serialize(&msg, &deps.paths.settings);
                            deps.journal.record(JournalRecord::WriteFailed {
                                target: WriteTarget::Work(WorkFile::Settings),
                                err: err.clone(),
                            });
                            effects.push(ReplyEffect::Failed(WorkFile::Settings, err.class));
                        }
                    }
                },
                WorkFile::State => match serialize_state(&self.state) {
                    Ok(bytes) => {
                        self.send_snapshot(WorkFile::State, bytes, None);
                    }
                    Err(SerializeError(msg)) => {
                        if let Some(deps) = &self.deps {
                            let err = WriteError::serialize(&msg, &deps.paths.state);
                            deps.journal.record(JournalRecord::WriteFailed {
                                target: WriteTarget::Work(WorkFile::State),
                                err: err.clone(),
                            });
                            effects.push(ReplyEffect::Failed(WorkFile::State, err.class));
                        }
                    }
                },
                WorkFile::Playlist => {
                    if self.tracker.playlist_writable(false) {
                        let bytes = playlist_bytes();
                        self.send_snapshot(WorkFile::Playlist, bytes, None);
                    }
                }
            }
        }
        effects
    }

    /// Путь выхода (ADR-7, ТЗ-14, ТЗ-17, ТЗ-32, НФ-9, §6.10): останавливает
    /// движок, отправляет снимки изменившихся файлов в порядке `Playlist`,
    /// `State`, `Settings`, ждёт итоговые снимки (новые и уже бывшие в полёте)
    /// не дольше `EXIT_BUDGET` от запроса и журналирует итог
    /// (`JournalRecord::ExitSummary`). Повторный запрос — `Ignored` (И-Т8).
    ///
    /// ОТКЛОНЕНИЯ от §2.12/§6.10 (мост до С5/С6): остановка движка —
    /// замыкание `release_engine` (вызывается один раз до записи, его итог —
    /// `engine_ack`), а не `EngineSink`; текст плейлиста — замыкание
    /// `playlist_bytes` (модель `Playlist` переходит в `AppCore` на С6);
    /// черновик диалога и сообщения закрывает `MusicApp` до вызова.
    pub fn exit(
        &mut self,
        reason: ExitReason,
        release_engine: &mut dyn FnMut() -> bool,
        playlist_bytes: &dyn Fn() -> Arc<[u8]>,
    ) -> ExitOutcome {
        let now = self.now();
        let Some(until) = self.exit.begin(reason, now) else {
            return ExitOutcome::Ignored;
        };
        if self.deps.is_none() {
            self.exit.finish();
            return ExitOutcome::Completed;
        }

        let mut report = ExitReport::new(reason);
        report.engine_ack = release_engine();

        // Итоговый снимок каждого файла, ответ на который ждём.
        let mut waiting: Vec<(WorkFile, SnapshotId)> = Vec::new();

        // Плейлист: флаг и не запрещён (ТЗ-17); `playlist_writable` не
        // учитывается — одна попытка независимо от прежних неудач (ТЗ-20).
        if !self.tracker.playlist_forbidden() && self.tracker.playlist_dirty() {
            let id = self.send_snapshot(WorkFile::Playlist, playlist_bytes(), None);
            waiting.push((WorkFile::Playlist, id));
        } else {
            self.classify_unsent(WorkFile::Playlist, self.tracker.playlist_forbidden(), &mut waiting, &mut report);
        }

        for (cfg, work) in [(ConfigFile::State, WorkFile::State), (ConfigFile::Settings, WorkFile::Settings)] {
            let allowed = self.tracker.auto_allowed(cfg);
            let bytes = match work {
                WorkFile::Settings => serialize_settings(&self.settings),
                _ => serialize_state(&self.state),
            };
            match bytes {
                Ok(bytes) if allowed && self.tracker.toml_needs_write(cfg, &bytes) => {
                    let id = self.send_snapshot(work, bytes, None);
                    waiting.push((work, id));
                }
                Ok(_) => self.classify_unsent(work, !allowed, &mut waiting, &mut report),
                Err(SerializeError(msg)) => {
                    let Some(deps) = &self.deps else { break };
                    let err = WriteError::serialize(&msg, deps.paths.work(work));
                    deps.journal.record(JournalRecord::WriteFailed { target: WriteTarget::Work(work), err: err.clone() });
                    report.failed.push((work, err));
                }
            }
        }

        while !waiting.is_empty() {
            let Some(deps) = &self.deps else { break };
            if deps.clock.now() >= until {
                break;
            }
            let Some(reply) = deps.waiter.wait(deps.writer.replies(), until, deps.clock.as_ref()) else {
                break;
            };
            // Любой ответ учитывается трекером: эталон, флаг, «в полёте» (§6.5).
            self.tracker.on_reply(&reply);
            match reply {
                WriterReply::Written { file, id } => {
                    if let Some(pos) = waiting.iter().position(|w| *w == (file, id)) {
                        waiting.remove(pos);
                        report.written.push(file);
                    }
                }
                WriterReply::Failed { file, id, err } => {
                    deps.journal.record(JournalRecord::WriteFailed { target: WriteTarget::Work(file), err: err.clone() });
                    if let Some(pos) = waiting.iter().position(|w| *w == (file, id)) {
                        waiting.remove(pos);
                        report.failed.push((file, err));
                    }
                }
                _ => {}
            }
        }
        report.timed_out.extend(waiting.iter().map(|(f, _)| *f));

        if let Some(deps) = &self.deps {
            deps.journal.record(JournalRecord::ExitSummary(report));
            let now = deps.clock.now();
            if now < until {
                deps.journal.flush(until.saturating_since(now));
            }
        }
        self.exit.finish();
        ExitOutcome::Completed
    }

    /// Файл без нового снимка на пути выхода (§6.10): если в полёте есть
    /// снимок — его текст и есть итог, ждём ответ; иначе файл запрещён
    /// (`forbidden`) или не изменился (`unchanged`).
    fn classify_unsent(
        &self,
        file: WorkFile,
        forbidden: bool,
        waiting: &mut Vec<(WorkFile, SnapshotId)>,
        report: &mut ExitReport,
    ) {
        if let Some(id) = self.tracker.last_in_flight(file) {
            waiting.push((file, id));
        } else if forbidden {
            report.forbidden.push(file);
        } else {
            report.unchanged.push(file);
        }
    }

    /// Доступ к `PersistTracker` для менеджеров плейлиста/UI (§2.12).
    pub fn tracker(&self) -> &PersistTracker {
        &self.tracker
    }

    /// Текущая фаза пути выхода (§6.10).
    pub fn exit_phase(&self) -> ExitPhase {
        self.exit.phase()
    }
}

/// Результат одного тика (§6.4): эффекты ответов писателя, уже обработанные
/// `PersistTracker` (`Succeeded`/`Failed`), и прочие ответы
/// (`Superseded`/`Exported`/`BadCopySaved`/...) — их разбирают другие
/// менеджеры на последующих этапах.
pub struct TickOutput {
    pub effects: Vec<ReplyEffect>,
    pub other: Vec<WriterReply>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::{self, ConfigPaths};
    use crate::platform::fs::{MemStore, ReadErrorClass};
    use std::path::PathBuf;

    fn paths() -> ConfigPaths {
        ConfigPaths::in_dir(PathBuf::from("/cfg"))
    }

    fn boot_with_defaults(fs: &MemStore, p: &ConfigPaths) -> Boot {
        let settings_bytes = serialize_settings(&Settings::default()).expect("serialize settings");
        let state_bytes = serialize_state(&SessionState::default()).expect("serialize state");
        fs.put(&p.settings, &settings_bytes);
        fs.put(&p.state, &state_bytes);
        persist::boot(fs, p)
    }

    #[test]
    fn flush_skips_unchanged_files() {
        let fs = MemStore::new();
        let p = paths();
        let boot = boot_with_defaults(&fs, &p);
        let mut core = AppCore::new(boot);
        let mut writer = fs.clone();

        let outcomes = core.flush(&mut writer, &p);

        assert!(outcomes.iter().all(|o| matches!(o, FlushOutcome::Unchanged(_))));
        assert_eq!(fs.counts(&p.settings).writes, 0);
        assert_eq!(fs.counts(&p.state).writes, 0);
    }

    #[test]
    fn flush_writes_state_after_change_state() {
        let fs = MemStore::new();
        let p = paths();
        let boot = boot_with_defaults(&fs, &p);
        let mut core = AppCore::new(boot);
        core.change_state(Origin::User, StateChange::Volume(42));
        let mut writer = fs.clone();

        let outcomes = core.flush(&mut writer, &p);

        assert!(matches!(outcomes[0], FlushOutcome::Unchanged(ConfigFile::Settings)));
        assert!(matches!(outcomes[1], FlushOutcome::Written(ConfigFile::State)));
        assert_eq!(fs.counts(&p.settings).writes, 0);
        assert_eq!(fs.counts(&p.state).writes, 1);
    }

    #[test]
    fn flush_skips_forbidden_settings_after_read_failure() {
        let fs = MemStore::new();
        let p = paths();
        fs.fail_read(&p.settings, ReadErrorClass::NoAccess);
        let boot = persist::boot(&fs, &p);
        assert!(matches!(boot.settings, Parsed::ReadFailed { .. }));
        let mut core = AppCore::new(boot);
        assert!(core.settings_read_failed_message().is_some());
        assert_eq!(core.settings_save_notice(), Some("Файл настроек не прочитан — «Сохранить» заменит его значениями из этого окна"));
        let mut writer = fs.clone();

        let outcomes = core.flush(&mut writer, &p);

        assert!(matches!(outcomes[0], FlushOutcome::Forbidden(ConfigFile::Settings)));
        assert_eq!(fs.counts(&p.settings).writes, 0);
    }
}
