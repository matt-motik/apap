//! AppCore — логика приложения без Slint (ADR-19, §2.1).
//!
//! Действующие настройки и состояние, трек-лист, шлюз главного окна,
//! центр сообщений и прочая логика, не зависящая от UI-фреймворка,
//! живут здесь; `src/app/` — тонкая прослойка Slint над `AppCore`.

pub mod gate;
pub mod geometry;
pub mod messages;
#[cfg(test)]
pub(crate) mod testing;

use std::path::Path;
use std::sync::Arc;

use messages::{Message, MessageButtons, MessageLevel};

use crate::journal::{JournalRecord, WriteTarget};
use crate::persist::keys::{LoadNote, Parsed};
use crate::persist::settings_file::{serialize_settings, Settings};
use crate::persist::state_file::{serialize_state, Origin, SessionState, StateChange};
use crate::persist::{Boot, ConfigFile, ConfigPaths, ReferenceText, SerializeError};
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

/// Ядро приложения без Slint (ADR-19, §2.12): владеет настройками и
/// состоянием сессии, их эталонными текстами и запретом автозаписи.
/// Полный состав `AppCore` по спецификации (`AppDeps`, `PersistTracker`,
/// `Playlist`, `UiGate`, `MessageCenter`, `ExitCoordinator`, `GeometryTracker`,
/// `LoadState` и т. д.) появляется поэтапно; на С3 — только то, что нужно
/// для чтения, изменения и синхронной записи `settings.toml`/`state.toml`.
pub struct AppCore {
    settings: Settings,
    settings_file: FileState,
    /// Ошибка чтения `settings.toml`, если он не прочитан (ОВС-6 в, §2.13).
    /// `state.toml` не даёт сообщения — только запрет автозаписи (И-Р20).
    settings_read_failed: Option<ReadError>,
    state: SessionState,
    state_file: FileState,
    startup_notes: StartupNotes,
}

impl AppCore {
    /// Строит `AppCore` из результата `persist::boot` (ADR-23 шаг 2, §2.12).
    ///
    /// Отклонение от §2.12: спецификация задаёт `new(deps: AppDeps, boot: Boot)`;
    /// на С3 `AppDeps` и его трейты (`WriterHandle`, `Lifecycle`, `Clock`,
    /// `EngineSink`, ...) ещё не существуют, поэтому конструктор этого шага —
    /// `new(boot: Boot)` без `deps`.
    pub fn new(boot: Boot) -> AppCore {
        let (settings, settings_file, settings_notes, settings_read_failed) = split_parsed(boot.settings);
        let (state, state_file, state_notes, _) = split_parsed(boot.state);
        AppCore {
            settings,
            settings_file,
            settings_read_failed,
            state,
            state_file,
            startup_notes: StartupNotes { settings: settings_notes, state: state_notes },
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

    /// Единственный изменитель состояния сессии (И-Т7, ADR-22, §2.12).
    /// `origin` пока не используется: его потребитель — `PersistTracker`
    /// (С4, писатель); здесь он только закрывает форму вызова по И-Т7.
    pub fn change_state(&mut self, _origin: Origin, ch: StateChange) {
        self.state.apply(ch);
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
