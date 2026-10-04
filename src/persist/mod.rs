//! Хранение настроек, состояния и плейлиста (`docs/02_settings_persistence_v1.0/`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod keys;
pub mod settings_file;

/// Рабочий файл единственного писателя (ТЗ-3, §2.2). Порядок вариантов =
/// порядок записи на пути выхода (ТЗ-14).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum WorkFile {
    Playlist,
    State,
    Settings,
}

impl WorkFile {
    /// Имя файла в каталоге настроек.
    pub const fn file_name(self) -> &'static str {
        match self {
            WorkFile::Playlist => "playlist.m3u",
            WorkFile::State => "state.toml",
            WorkFile::Settings => "settings.toml",
        }
    }
}

/// Файл, разбираемый по ключам (ТЗ-5, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigFile {
    Settings,
    State,
}

impl ConfigFile {
    /// Тот же файл как рабочий файл писателя.
    pub const fn work(self) -> WorkFile {
        match self {
            ConfigFile::Settings => WorkFile::Settings,
            ConfigFile::State => WorkFile::State,
        }
    }
}

/// Пути файлов приложения (§2.2, ADR-19). Строится только в `main` из
/// `settings::config_dir()`; остальной код получает пути отсюда и сам каталог
/// пользователя не ищет, поэтому тесты не трогают его файлы (ТЗ-49).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    /// Каталог настроек.
    pub dir: PathBuf,
    /// `settings.toml`.
    pub settings: PathBuf,
    /// `state.toml`.
    pub state: PathBuf,
    /// `playlist.m3u`.
    pub playlist: PathBuf,
    /// Журнал `apap.log` (ОВС-4 б); ротация — `apap.1.log`, `apap.2.log` рядом.
    pub journal: PathBuf,
}

impl ConfigPaths {
    /// Пути всех файлов внутри каталога `dir`.
    pub fn in_dir(dir: PathBuf) -> ConfigPaths {
        ConfigPaths {
            settings: dir.join(WorkFile::Settings.file_name()),
            state: dir.join(WorkFile::State.file_name()),
            playlist: dir.join(WorkFile::Playlist.file_name()),
            journal: dir.join("apap.log"),
            dir,
        }
    }

    /// Путь рабочего файла.
    pub fn work(&self, f: WorkFile) -> &Path {
        match f {
            WorkFile::Playlist => &self.playlist,
            WorkFile::State => &self.state,
            WorkFile::Settings => &self.settings,
        }
    }

    /// «<имя>.bad» рядом с файлом: одна копия на файл (ТЗ-6, ТЗ-21, НФ-7).
    pub fn bad_copy(&self, f: WorkFile) -> PathBuf {
        self.dir.join(format!("{}.bad", f.file_name()))
    }
}

/// Эталонный текст файла (ОВ-2): текст последней успешной записи, до неё —
/// прочитанный при запуске. `None` — файла не было или он неразбираемый (§2.2).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ReferenceText(Option<Arc<[u8]>>);

impl ReferenceText {
    /// Эталон из прочитанных байт файла.
    pub fn of(bytes: Arc<[u8]>) -> ReferenceText {
        ReferenceText(Some(bytes))
    }

    /// `true`, если `text` отличается от эталона; пустой эталон отличается от любого текста.
    pub fn differs(&self, text: &[u8]) -> bool {
        match &self.0 {
            Some(bytes) => bytes.as_ref() != text,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_paths_in_dir_places_all_files_in_dir() {
        let dir = PathBuf::from("/cfg/music_player");
        let p = ConfigPaths::in_dir(dir.clone());
        assert_eq!(p.dir, dir);
        assert_eq!(p.settings, dir.join("settings.toml"));
        assert_eq!(p.state, dir.join("state.toml"));
        assert_eq!(p.playlist, dir.join("playlist.m3u"));
        assert_eq!(p.journal, dir.join("apap.log"));
    }

    #[test]
    fn config_paths_work_and_bad_copy() {
        let dir = PathBuf::from("/cfg/music_player");
        let p = ConfigPaths::in_dir(dir.clone());
        assert_eq!(p.work(WorkFile::Settings), p.settings.as_path());
        assert_eq!(p.work(WorkFile::State), p.state.as_path());
        assert_eq!(p.work(WorkFile::Playlist), p.playlist.as_path());
        assert_eq!(p.bad_copy(WorkFile::Settings), dir.join("settings.toml.bad"));
        assert_eq!(p.bad_copy(ConfigFile::State.work()), dir.join("state.toml.bad"));
        assert_eq!(p.bad_copy(WorkFile::Playlist), dir.join("playlist.m3u.bad"));
    }
}
