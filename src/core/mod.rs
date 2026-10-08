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

use std::sync::Arc;

use exit::{ExitCoordinator, ExitOutcome, ExitPhase, ExitReason, ExitReport, ReplyWaiter};
use geometry::GeometryTracker;
use messages::{Message, MessageButtons, MessageLevel};

use crate::audio::clock::{Clock, ClockInstant};
use crate::engine::messages::EngineCmd;
use crate::engine::run::EngineSender;
use crate::journal::{Journal, JournalRecord, WriteTarget};
use crate::persist::keys::{LoadNote, Parsed};
use crate::persist::settings_file::{serialize_settings, Settings};
use crate::persist::state_file::{
    self, serialize_state, Origin, SessionState, StateChange, WindowGeometry,
};
use crate::persist::tracker::{PersistTracker, ReplyEffect};
use crate::persist::writer::{WriterCmd, WriterHandle, WriterReply};
use crate::persist::{Boot, ConfigFile, ConfigPaths, ReferenceText, SerializeError, Snapshot, SnapshotId, WorkFile};
use crate::platform::fs::{ReadError, WriteError};
use crate::playlist::compare::CompareKeys;
use crate::playlist::model::{Playlist, PlaylistEffect, SortDir, SortKey, TrackId};
use crate::playlist::shuffle::ShuffleState;
use crate::playlist::Track;
use crate::settings::ColumnId;
use rand::rngs::StdRng;
use rand::SeedableRng;

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

/// МОСТ (удаляется на шаге 28): `playlist::model::SortKey` →
/// `persist::state_file::SortKey` для `StateChange::Sort` (§6.13, ТЗ-43).
/// Пока `SessionState` хранит ключ сортировки в старом представлении —
/// колонка и направление без типа `SortColumn`.
fn to_state_sort_key(key: Option<SortKey>) -> Option<state_file::SortKey> {
    key.map(|k| state_file::SortKey {
        column: k.column.column(),
        direction: match k.dir {
            SortDir::Asc => state_file::SortDirection::Asc,
            SortDir::Desc => state_file::SortDirection::Desc,
        },
    })
}

/// Команды UI → движок (§2.12, ADR-01): неблокирующая отправка,
/// без ответа и без доступа к `JoinHandle` движка.
pub trait EngineSink {
    fn send(&self, cmd: EngineCmd);
}

/// `EngineSink` поверх `EngineSender` (§2.12, ADR-01): результат отправки
/// игнорируется здесь — мёртвый движок доходит до UI отдельно, через
/// `EngineFault`/`EngineEvent`, а не через возврат `send()`.
impl EngineSink for EngineSender {
    fn send(&self, cmd: EngineCmd) {
        let _ = EngineSender::send(self, cmd);
    }
}

/// Пустая реализация `EngineSink` (ТЗ-88): используется `main`, когда поток
/// `apap-engine` не запустился — окно ошибки показывает `MusicApp`, а
/// `AppDeps.engine` всё равно должен быть каким-то значением.
pub struct NoEngine;

impl EngineSink for NoEngine {
    fn send(&self, _cmd: EngineCmd) {}
}

/// Внешние зависимости `AppCore` (ADR-19, §2.12): писатель
/// `apap-persist`, пути конфигурации, инжектируемые часы, ожидание ответа
/// на пути выхода, журнал и команды движку.
///
/// ОТКЛОНЕНИЕ от §2.12: полный `AppDeps` спецификации содержит также
/// `Lifecycle` и загрузчик обложек — они появляются на последующих этапах.
/// Остановка движка на пути выхода здесь всё ещё не идёт через `engine` —
/// `exit()` принимает её замыканием `&mut dyn FnMut() -> bool` (переводится
/// на `deps.engine` отдельным шагом 55).
pub struct AppDeps {
    pub writer: WriterHandle,
    pub paths: ConfigPaths,
    pub clock: Box<dyn Clock>,
    pub waiter: Box<dyn ReplyWaiter>,
    pub journal: Arc<dyn Journal>,
    /// Команды движку (01_audio_modes, §2.12, ADR-01).
    pub engine: Box<dyn EngineSink>,
}

/// Ядро приложения без Slint (ADR-19, §2.12): владеет настройками и
/// состоянием сессии, их эталонными текстами и запретом автозаписи,
/// моделью плейлиста, проходом Shuffle, `PersistTracker`, координатором
/// выхода и трекером геометрии окна. Полный состав `AppCore` по
/// спецификации (`UiGate`, `MessageCenter`, `LoadState` и т. д.) появляется
/// поэтапно; на этом шаге — то, что нужно для чтения/изменения настроек,
/// состояния, плейлиста и прохода Shuffle, и отложенной записи через
/// писателя `apap-persist` (§3.4, §3.5, §4.2, §6.4, §6.8, §6.10, §6.13).
pub struct AppCore {
    settings: Settings,
    settings_file: FileState,
    /// Ошибка чтения `settings.toml`, если он не прочитан (ОВС-6 в, §2.13).
    /// `state.toml` не даёт сообщения — только запрет автозаписи (И-Р20).
    settings_read_failed: Option<ReadError>,
    state: SessionState,
    /// Модель плейлиста — состав, исходный и видимый порядок (§4.2, §6.13,
    /// ТЗ-12, ТЗ-43).
    playlist: Playlist,
    /// Проход Shuffle по видимому порядку плейлиста (§3.4, §3.5, §6.13,
    /// ТЗ-45, ТЗ-46).
    shuffle: ShuffleState,
    /// Генератор для перемешивания прохода Shuffle (§3.4, ТЗ-45).
    rng: StdRng,
    startup_notes: StartupNotes,
    deps: Option<AppDeps>,
    tracker: PersistTracker,
    exit: ExitCoordinator,
    geometry: GeometryTracker,
}

/// Общая часть загрузки `with_deps`: разбирает `boot` и строит
/// `PersistTracker` с эталоном и запретом автозаписи по разбору файлов
/// (ТЗ-11, И-Р20, §2.7, §2.12).
#[allow(clippy::type_complexity)]
fn from_boot(boot: Boot) -> (Settings, FileState, Option<ReadError>, SessionState, StartupNotes, PersistTracker) {
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
    (settings, settings_file, settings_read_failed, state, startup_notes, tracker)
}

impl AppCore {
    /// Строит `AppCore` с зависимостями писателя `apap-persist` (ADR-19,
    /// §2.12).
    ///
    /// ОТКЛОНЕНИЕ от §2.12: там это конструктор `new(deps, boot)`; здесь —
    /// отдельное имя `with_deps`, чтобы не путать с обычным `new()`.
    pub fn with_deps(deps: AppDeps, boot: Boot) -> AppCore {
        let (settings, settings_file, settings_read_failed, state, startup_notes, tracker) = from_boot(boot);
        AppCore {
            settings,
            settings_file,
            settings_read_failed,
            state,
            playlist: Playlist::new(),
            shuffle: ShuffleState::default(),
            rng: StdRng::from_entropy(),
            startup_notes,
            deps: Some(deps),
            tracker,
            exit: ExitCoordinator::new(),
            geometry: GeometryTracker::new(),
        }
    }

    /// Текущее время по инжектируемым часам (ADR-20); без `deps` — начало
    /// отсчёта `ClockInstant::START`.
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
    /// его для `Origin::Program` (ТЗ-11). Включение Shuffle (переход
    /// выключено -> включено) начинает новый проход `ShuffleState` (§3.4).
    pub fn change_state(&mut self, origin: Origin, ch: StateChange) {
        let starts_shuffle = matches!(ch, StateChange::Shuffle(true)) && !self.state.shuffle();
        self.state.apply(ch);
        if starts_shuffle {
            self.shuffle = ShuffleState::new_pass(self.playlist.visible(), &mut self.rng);
        }
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

    /// Доступ к модели плейлиста (§4.2, §6.13).
    pub fn playlist(&self) -> &Playlist {
        &self.playlist
    }

    /// Добавление треков в плейлист (ТЗ-12, ТЗ-41, ТЗ-45, §6.13).
    pub fn playlist_add(&mut self, tracks: Vec<(Track, CompareKeys)>) -> Vec<TrackId> {
        let (ids, effect) = self.playlist.add(tracks);
        self.apply_playlist_effect(effect);
        ids
    }

    /// Удаление строк из плейлиста (ТЗ-12, §6.13).
    pub fn playlist_remove(&mut self, ids: &[TrackId]) {
        let effect = self.playlist.remove(ids);
        self.apply_playlist_effect(effect);
    }

    /// Очистка плейлиста (ТЗ-12, §6.13).
    pub fn playlist_clear(&mut self) {
        let effect = self.playlist.clear();
        self.apply_playlist_effect(effect);
    }

    /// Щелчок по заголовку колонки `c` (ТЗ-42, ТЗ-43, §3.3, §6.13): `hidden` —
    /// колонка не входит в видимые колонки настроек (ТЗ-31).
    pub fn playlist_header_click(&mut self, c: ColumnId) {
        let hidden = !self.settings.columns.visible_columns().contains(&c);
        let effect = self.playlist.header_click(c, hidden);
        self.apply_playlist_effect(effect);
    }

    /// Ручное изменение порядка плейлиста (ТЗ-44, §6.13).
    pub fn playlist_reorder(&mut self, moved: &[TrackId], before: Option<TrackId>) {
        let effect = self.playlist.reorder(moved, before);
        self.apply_playlist_effect(effect);
    }

    /// Обновление тегов строки плейлиста — без эффекта (§3.1, ТЗ-43).
    pub fn playlist_update_tags(&mut self, id: TrackId, track: Track, keys: CompareKeys) {
        self.playlist.update_tags(id, track, keys);
    }

    /// Замена плейлиста целиком по окончании загрузки (ADR-16): без флага
    /// «изменён» и без изменения состояния. При включённом Shuffle —
    /// новый проход по загруженному видимому порядку (§3.5 п. 2).
    pub fn playlist_replace(&mut self, rows: Vec<(Track, CompareKeys)>, visible: Vec<u32>, sort: Option<SortKey>) {
        self.playlist.replace(rows, visible, sort);
        if self.state.shuffle() {
            self.shuffle = ShuffleState::new_pass(self.playlist.visible(), &mut self.rng);
        }
    }

    /// Доступ к проходу Shuffle (§3.4, §6.13).
    pub fn shuffle_state(&self) -> &ShuffleState {
        &self.shuffle
    }

    /// Первый несыгранный трек прохода Shuffle — «Далее» (§3.4, ТЗ-46,
    /// Т-ТЗ-46). Несыгранных нет: при Repeat All начинается новый проход и
    /// возвращается его первый трек, иначе — `None` (ТЗ-45).
    pub fn shuffle_first(&mut self) -> Option<TrackId> {
        let repeat = self.state.repeat();
        self.shuffle.first(self.playlist.visible(), repeat, &mut self.rng)
    }

    /// Начато воспроизведение `id` в проходе Shuffle (§3.4, ТЗ-45).
    pub fn shuffle_started(&mut self, id: TrackId) {
        self.shuffle.started(id);
    }

    /// Последний сыгранный трек прохода Shuffle — «Назад» (§3.4, ТЗ-46).
    pub fn shuffle_previous(&self) -> Option<TrackId> {
        self.shuffle.previous()
    }

    /// «Назад» в проходе Shuffle (§3.4, ТЗ-45): переводит последний трек
    /// `history` в текущий, прежний текущий возвращается в начало
    /// несыгранных. `None`, если `history` пуста.
    pub fn shuffle_back(&mut self) -> Option<TrackId> {
        self.shuffle.back()
    }

    /// Байты `playlist.m3u` в исходном порядке — снимок для писателя
    /// `apap-persist` (ТЗ-12, §6.5, §6.13).
    pub fn playlist_m3u(&self) -> Arc<[u8]> {
        let mut out = String::new();
        for t in self.playlist.source_order() {
            out.push_str(&t.path.to_string_lossy());
            out.push('\n');
        }
        Arc::from(out.into_bytes())
    }

    /// Применяет эффект операции плейлиста (§6.13): `dirty` взводит дедлайн
    /// отложенной записи `playlist.m3u`, `sort_changed` — переносит новый
    /// ключ сортировки в `SessionState` через мост `to_state_sort_key`.
    /// `order_changed` при включённом Shuffle перестраивает `ShuffleState`
    /// (§3.4, ТЗ-45); передача перестановки `Sequencer` и замыкание серии
    /// пропусков — на этапе 01_audio_modes С8.
    fn apply_playlist_effect(&mut self, e: PlaylistEffect) {
        if e.dirty {
            self.playlist_changed();
        }
        if e.sort_changed {
            let bridge = to_state_sort_key(self.playlist.sort_key());
            self.change_state(Origin::User, StateChange::Sort(bridge));
        }
        if e.order_changed && self.state.shuffle() {
            self.shuffle.rebuild(self.playlist.visible(), &mut self.rng);
        }
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
    /// Движок останавливается командой `EngineCmd::Shutdown` через
    /// `EngineSink` до записи; запись от его ответа не зависит (ОВС-16,
    /// ТЗ-136, §6.28 п. 2а).
    ///
    /// ОТКЛОНЕНИЯ от §2.12/§6.10 (мост до С5/С6): `ShutdownComplete`
    /// принимается не в ожидании писателя, а замыканием `await_engine`
    /// (вызывается один раз, после отправки снимков — писатель уже пишет;
    /// своё ожидание до 2 с от запроса, §6.28 п. 3; итог — `engine_ack`):
    /// события движка читает `MusicApp`; текст плейлиста — замыкание
    /// `playlist_bytes` (модель `Playlist` переходит в `AppCore` на С6);
    /// черновик диалога и сообщения закрывает `MusicApp` до вызова.
    pub fn exit(
        &mut self,
        reason: ExitReason,
        await_engine: &mut dyn FnMut() -> bool,
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
        if let Some(deps) = &self.deps {
            deps.engine.send(EngineCmd::Shutdown);
        }

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

        // Снимки уже у писателя: ожидание движка запись не задерживает (ТЗ-136).
        report.engine_ack = await_engine();

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

