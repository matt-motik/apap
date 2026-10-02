//! Хранение настроек, состояния и плейлиста (`docs/02_settings_persistence_v1.0/`).

use std::path::PathBuf;

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
            settings: dir.join("settings.toml"),
            state: dir.join("state.toml"),
            playlist: dir.join("playlist.m3u"),
            journal: dir.join("apap.log"),
            dir,
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
}
