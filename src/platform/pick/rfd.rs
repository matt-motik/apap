//! `RfdPicker` — реализация `FilePicker` на `rfd::AsyncFileDialog` (ADR-10,
//! ТЗ-53, §2.8). Диалог строится синхронно в `pick()`: `set_parent` и
//! `set_directory` заимствуют свои аргументы только на время построения
//! builder'а, поэтому возвращаемый future не держит заимствований и остаётся
//! `'static`, как требует `FilePicker::pick`.

use super::{FilePicker, PickRequest, PickResult};
use std::future::Future;
use std::pin::Pin;

/// Расширения аудиофайлов для `AddFiles` (как в прежнем блокирующем
/// `rfd::FileDialog` из `app/mod.rs`, ТЗ-53).
const AUDIO_EXTENSIONS: &[&str] = &["flac", "mp3", "ogg", "wav", "aac", "m4a", "dsf", "aiff"];
/// Расширения плейлистов для `OpenPlaylist` (ТЗ-53).
const PLAYLIST_EXTENSIONS: &[&str] = &["m3u", "m3u8"];
/// Расширения темы для `ThemeFile` (ТЗ-53).
const THEME_EXTENSIONS: &[&str] = &["toml"];
/// Имя файла по умолчанию при сохранении плейлиста — расширение `.m3u8`
/// (ОВ-17, ТЗ-53).
const DEFAULT_PLAYLIST_FILE_NAME: &str = "playlist.m3u8";

/// Выбор файлов/каталогов через системные диалоги `rfd` (ADR-10, §2.8).
#[derive(Default)]
pub struct RfdPicker;

impl FilePicker for RfdPicker {
    fn pick(
        &self,
        req: PickRequest,
        parent: Option<&slint::Window>,
    ) -> Pin<Box<dyn Future<Output = PickResult>>> {
        // Владеющий handle окна строится здесь же: `set_parent` принимает
        // ссылку на него, а сам handle переживает построение диалога без
        // заимствования `parent` (ADR-10).
        let parent_handle = parent.map(|window| window.window_handle());

        match req {
            PickRequest::AddFiles { start } => {
                let mut dialog =
                    ::rfd::AsyncFileDialog::new().add_filter("Audio", AUDIO_EXTENSIONS);
                if let Some(start) = &start {
                    dialog = dialog.set_directory(start);
                }
                if let Some(handle) = &parent_handle {
                    dialog = dialog.set_parent(handle);
                }
                Box::pin(async move {
                    match dialog.pick_files().await {
                        Some(files) => {
                            PickResult::Paths(files.into_iter().map(|f| f.path().to_path_buf()).collect())
                        }
                        None => PickResult::Cancelled,
                    }
                })
            }
            PickRequest::AddFolder { start } => {
                let mut dialog = ::rfd::AsyncFileDialog::new();
                if let Some(start) = &start {
                    dialog = dialog.set_directory(start);
                }
                if let Some(handle) = &parent_handle {
                    dialog = dialog.set_parent(handle);
                }
                Box::pin(async move {
                    match dialog.pick_folder().await {
                        Some(folder) => PickResult::Paths(vec![folder.path().to_path_buf()]),
                        None => PickResult::Cancelled,
                    }
                })
            }
            PickRequest::OpenPlaylist { start } => {
                let mut dialog =
                    ::rfd::AsyncFileDialog::new().add_filter("Playlist", PLAYLIST_EXTENSIONS);
                if let Some(start) = &start {
                    dialog = dialog.set_directory(start);
                }
                if let Some(handle) = &parent_handle {
                    dialog = dialog.set_parent(handle);
                }
                Box::pin(async move {
                    match dialog.pick_file().await {
                        Some(file) => PickResult::Paths(vec![file.path().to_path_buf()]),
                        None => PickResult::Cancelled,
                    }
                })
            }
            PickRequest::SavePlaylist { start } => {
                // `.m3u8` — расширение по умолчанию (ОВ-17): оно первое в
                // списке фильтров и задаёт имя файла-подсказки.
                let mut dialog = ::rfd::AsyncFileDialog::new()
                    .add_filter("Playlist (M3U8)", &["m3u8"])
                    .add_filter("Playlist (M3U)", &["m3u"])
                    .set_file_name(DEFAULT_PLAYLIST_FILE_NAME);
                if let Some(start) = &start {
                    dialog = dialog.set_directory(start);
                }
                if let Some(handle) = &parent_handle {
                    dialog = dialog.set_parent(handle);
                }
                Box::pin(async move {
                    match dialog.save_file().await {
                        Some(file) => PickResult::Paths(vec![file.path().to_path_buf()]),
                        None => PickResult::Cancelled,
                    }
                })
            }
            PickRequest::ThemeFile { start } => {
                let mut dialog =
                    ::rfd::AsyncFileDialog::new().add_filter("Theme", THEME_EXTENSIONS);
                if let Some(start) = &start {
                    dialog = dialog.set_directory(start);
                }
                if let Some(handle) = &parent_handle {
                    dialog = dialog.set_parent(handle);
                }
                Box::pin(async move {
                    match dialog.pick_file().await {
                        Some(file) => PickResult::Paths(vec![file.path().to_path_buf()]),
                        None => PickResult::Cancelled,
                    }
                })
            }
        }
    }
}
