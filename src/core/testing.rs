//! Стенд С3 для тестов `AppCore` без Slint (§7.1, §7.2, ADR-19).
//!
//! `Harness` оборачивает `MemStore` и воссоздаёт порядок `main` для С3
//! (ADR-23 шаги 0–2, §8 С3): чтение и разбор обоих файлов (`persist::boot`),
//! затем копии `*.bad` до появления окна (`persist::write_bad_copies`), затем
//! `AppCore::new`. Писателя `apap-persist` и задержки по времени (С4) стенд
//! не моделирует — `flush` вызывается напрямую, синхронно.

use super::{AppCore, FlushOutcome};
use crate::persist::{self, ConfigFile, ConfigPaths, WorkFile};
use crate::platform::fs::{MemStore, OpCounts, ReadErrorClass};
use std::path::PathBuf;

/// Каталог настроек стенда — не каталог пользователя (ТЗ-49).
fn test_dir() -> PathBuf {
    PathBuf::from("/cfg")
}

pub(crate) struct Harness {
    fs: MemStore,
    paths: ConfigPaths,
}

impl Harness {
    /// Пустой каталог: оба файла отсутствуют, пока не вызван `put_*`.
    pub(crate) fn new() -> Harness {
        Harness { fs: MemStore::new(), paths: ConfigPaths::in_dir(test_dir()) }
    }

    /// Положить байты `settings.toml` «на диск» до запуска.
    pub(crate) fn put_settings(&self, bytes: &[u8]) {
        self.fs.put(&self.paths.settings, bytes);
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

    /// Запуск (ADR-23 шаги 0–2, §8 С3): чтение + разбор, копии `*.bad` до
    /// появления окна, затем `AppCore`. Ничего не возвращает про сами копии —
    /// их результат проверяется через `bad_copy_bytes`/`bad_copy_counts`.
    pub(crate) fn boot(&self) -> AppCore {
        let boot = persist::boot(&self.fs, &self.paths);
        let mut writer = self.fs.clone();
        let _ = persist::write_bad_copies(&boot, &mut writer, &self.paths);
        AppCore::new(boot)
    }

    /// `AppCore::flush` через стенд (временная синхронная запись С3).
    pub(crate) fn flush(&self, core: &mut AppCore) -> Vec<FlushOutcome> {
        let mut writer = self.fs.clone();
        core.flush(&mut writer, &self.paths)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::keys::{KeyPath, LoadNoteKind};
    use crate::persist::settings_file::Settings;
    use crate::persist::state_file::{Origin, SessionState, StateChange};

    /// Нечитаемый `settings.toml` (ОВС-6 в, ТЗ-7, §6.1): автозаписи
    /// запрещены на весь сеанс, `flush` не пишет файл.
    #[test]
    fn unreadable_settings_forbids_auto_write() {
        let h = Harness::new();
        h.put_state(b"");
        h.fail_read_settings(ReadErrorClass::NoAccess);
        let mut core = h.boot();
        assert!(core.settings_read_failed_message().is_some());
        assert_eq!(h.settings_counts().writes, 0);

        let outcomes = h.flush(&mut core);
        assert!(matches!(outcomes[0], FlushOutcome::Forbidden(ConfigFile::Settings)));
        assert_eq!(h.settings_counts().writes, 0);
    }

    /// То же для `state.toml` (ОВС-6 в, ТЗ-7, §6.1): запрет действует
    /// независимо на каждый файл.
    #[test]
    fn unreadable_state_forbids_auto_write() {
        let h = Harness::new();
        h.put_settings(b"");
        h.fail_read_state(ReadErrorClass::NoAccess);
        let mut core = h.boot();
        assert_eq!(h.state_counts().writes, 0);

        let outcomes = h.flush(&mut core);
        assert!(matches!(outcomes[1], FlushOutcome::Forbidden(ConfigFile::State)));
        assert_eq!(h.state_counts().writes, 0);
    }

    /// Неразбираемый `settings.toml` (ТЗ-6, ТЗ-21, И-Р12, И-Р18, §2.13):
    /// копия `*.bad` с исходными байтами пишется один раз до любой `flush`
    /// (до появления окна), значения в памяти — по умолчанию.
    #[test]
    fn unparsable_settings_writes_bad_copy_once() {
        let h = Harness::new();
        h.put_settings(b"[[\n");
        h.put_state(b"");
        let mut core = h.boot();

        assert_eq!(core.settings(), &Settings::default());
        assert_eq!(h.bad_copy_bytes(WorkFile::Settings).as_deref(), Some(&b"[[\n"[..]));
        assert_eq!(h.bad_copy_counts(WorkFile::Settings).writes, 1);

        h.flush(&mut core);
        // Повторная запись копии при flush не происходит (И-Р12: одна копия на файл).
        assert_eq!(h.bad_copy_counts(WorkFile::Settings).writes, 1);
    }

    /// То же для `state.toml` (ТЗ-6, ТЗ-21, И-Р12, И-Р18, §2.13).
    #[test]
    fn unparsable_state_writes_bad_copy_once() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"[[\n");
        let mut core = h.boot();

        assert_eq!(core.state(), &SessionState::default());
        assert_eq!(h.bad_copy_bytes(WorkFile::State).as_deref(), Some(&b"[[\n"[..]));
        assert_eq!(h.bad_copy_counts(WorkFile::State).writes, 1);

        h.flush(&mut core);
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

    /// Файл без изменений не перезаписывается (ТЗ-9, §6.1): эталон из
    /// прочитанных байт совпадает с сериализацией значений по умолчанию.
    #[test]
    fn unchanged_files_not_rewritten() {
        let h = Harness::new();
        let settings_bytes = crate::persist::settings_file::serialize_settings(&Settings::default())
            .expect("serialize settings");
        let state_bytes =
            crate::persist::state_file::serialize_state(&SessionState::default()).expect("serialize state");
        h.put_settings(&settings_bytes);
        h.put_state(&state_bytes);
        let mut core = h.boot();

        let outcomes = h.flush(&mut core);
        assert!(matches!(outcomes[0], FlushOutcome::Unchanged(ConfigFile::Settings)));
        assert!(matches!(outcomes[1], FlushOutcome::Unchanged(ConfigFile::State)));
        assert_eq!(h.settings_counts().writes, 0);
        assert_eq!(h.state_counts().writes, 0);
    }

    /// Изменение состояния доходит до диска за один `flush` (ADR-4, §6.1):
    /// сквозная проверка пути записи через стенд.
    #[test]
    fn changed_state_written_once() {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let mut core = h.boot();

        core.change_state(Origin::User, StateChange::Shuffle(true));
        let outcomes = h.flush(&mut core);
        assert!(matches!(outcomes[1], FlushOutcome::Written(ConfigFile::State)));
        assert_eq!(h.state_counts().writes, 1);
        assert!(core.state().shuffle());

        // Повторный flush без новых изменений не пишет снова.
        let outcomes = h.flush(&mut core);
        assert!(matches!(outcomes[1], FlushOutcome::Unchanged(ConfigFile::State)));
        assert_eq!(h.state_counts().writes, 1);
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
}
