//! Текст окон сообщений (T1.0-B7, ТЗ-46, §6.8): подписи кнопок
//! [`MessageAction`] и заголовок сводного окна ошибок записи — перенесено
//! из `src/core/messages.rs`, у которого не было доступа к `i18n` (текст уже
//! был на русском, перенос без изменения содержания).
//!
//! Сообщения о загрузке/экспорте плейлиста (ADR-16, ADR-17, §6.12, §6.18,
//! ОВ-8, ТЗ-13 а/б, ТЗ-20, ТЗ-21, ТЗ-48, T1.0-B8) — перенесены из
//! `src/core/load.rs` байт-в-байт.

use std::path::Path;

use crate::core::messages::MessageAction;

/// Подпись кнопки окна сообщения (ТЗ-46).
pub fn message_action_label(action: MessageAction) -> &'static str {
    match action {
        MessageAction::RetryCapture => "Повторить захват",
        MessageAction::OpenAudioSettings => "Настройки аудио",
        MessageAction::SwitchToCompatible => "В Совместимый",
        MessageAction::Cancel => "Отмена",
        MessageAction::Ok => "OK",
        MessageAction::Retry => "Повторить",
        MessageAction::CaptureAndPlay => "Захватить и играть",
    }
}

/// Заголовок сводного окна ошибок записи рабочих файлов (§6.8).
pub const WRITE_ERRORS_TITLE: &str = "Не удалось сохранить данные";

/// Заголовок сообщения — случай 2 ОВ-8/ТЗ-21: повреждённый `playlist.m3u`
/// успешно перемещён в карантин писателем.
pub const PLAYLIST_QUARANTINED_TITLE: &str = "Плейлист повреждён";

/// Тело сообщения — случай 2 ОВ-8/ТЗ-21: `path` — путь к сохранённой копии
/// повреждённого файла.
pub fn playlist_quarantined_body(path: &Path) -> String {
    format!(
        "Файл плейлиста повреждён и не может быть прочитан. Копия повреждённого \
         файла сохранена: {}. Плейлист пуст.",
        path.display()
    )
}

/// Заголовок сообщения — случай 3 ОВ-8/ТЗ-21: плейлист недоступен в этом
/// сеансе (файл не прочитан или копия в карантин не записалась).
pub const PLAYLIST_UNREADABLE_TITLE: &str = "Плейлист не прочитан";

/// Тело сообщения — случай 3 ОВ-8/ТЗ-21: `path` — путь к файлу плейлиста,
/// `os_text` — системный текст ошибки чтения.
pub fn playlist_unreadable_body(path: &Path, os_text: &str) -> String {
    format!(
        "Файл плейлиста {} не удалось прочитать ({}). Плейлист пуст. Файл \
         playlist.m3u не будет записан в этом сеансе — следующий запуск повторит \
         попытку.",
        path.display(),
        os_text
    )
}

/// Заголовок сообщения командной загрузки (ТЗ-13 б, ТЗ-48, §6.12): файл,
/// выбранный в диалоге «Загрузить плейлист», не удалось прочитать.
pub const COMMAND_LOAD_UNREADABLE_TITLE: &str = "Не удалось загрузить плейлист";

/// Тело сообщения командной загрузки (ТЗ-13 б, ТЗ-48): `path` — выбранный
/// файл, `os_text` — системный текст ошибки чтения.
pub fn command_load_unreadable_body(path: &Path, os_text: &str) -> String {
    format!("Файл {} не удалось прочитать ({}). Текущий плейлист не изменён.", path.display(), os_text)
}

/// Заголовок сообщения экспорта «Сохранить плейлист» (ADR-17, §6.18,
/// ТЗ-13 а, ТЗ-20): ошибка записи файла экспорта.
pub const PLAYLIST_EXPORT_FAILED_TITLE: &str = "Не удалось сохранить плейлист";

/// Тело сообщения экспорта «Сохранить плейлист» (ADR-17, §6.18, ТЗ-13 а,
/// ТЗ-20): `path` — файл экспорта, `os_text` — системный текст ошибки записи.
pub fn playlist_export_failed_body(path: &Path, os_text: &str) -> String {
    format!("Файл {} не удалось записать ({}).", path.display(), os_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_action_label_covers_all_variants() {
        let cases = [
            (MessageAction::RetryCapture, "Повторить захват"),
            (MessageAction::OpenAudioSettings, "Настройки аудио"),
            (MessageAction::SwitchToCompatible, "В Совместимый"),
            (MessageAction::Cancel, "Отмена"),
            (MessageAction::Ok, "OK"),
            (MessageAction::Retry, "Повторить"),
            (MessageAction::CaptureAndPlay, "Захватить и играть"),
        ];
        for (action, label) in cases {
            assert_eq!(message_action_label(action), label, "action={action:?}");
        }
    }

    #[test]
    fn playlist_quarantined_body_includes_path() {
        let path = Path::new("/home/user/.config/music_player/playlist.m3u.bad");
        assert_eq!(
            playlist_quarantined_body(path),
            "Файл плейлиста повреждён и не может быть прочитан. Копия повреждённого \
             файла сохранена: /home/user/.config/music_player/playlist.m3u.bad. Плейлист пуст."
        );
    }

    #[test]
    fn playlist_unreadable_body_includes_path_and_os_text() {
        let path = Path::new("/home/user/.config/music_player/playlist.m3u");
        assert_eq!(
            playlist_unreadable_body(path, "нет доступа"),
            "Файл плейлиста /home/user/.config/music_player/playlist.m3u не удалось \
             прочитать (нет доступа). Плейлист пуст. Файл playlist.m3u не будет записан \
             в этом сеансе — следующий запуск повторит попытку."
        );
    }

    #[test]
    fn command_load_unreadable_body_includes_path_and_os_text() {
        let path = Path::new("/music/chosen.m3u");
        assert_eq!(
            command_load_unreadable_body(path, "файл не найден"),
            "Файл /music/chosen.m3u не удалось прочитать (файл не найден). Текущий \
             плейлист не изменён."
        );
    }

    #[test]
    fn playlist_export_failed_body_includes_path_and_os_text() {
        let path = Path::new("/music/export.m3u");
        assert_eq!(
            playlist_export_failed_body(path, "нет места на диске"),
            "Файл /music/export.m3u не удалось записать (нет места на диске)."
        );
    }
}
