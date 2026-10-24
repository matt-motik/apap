//! Текст окон сообщений (T1.0-B7, ТЗ-46, §6.8): подписи кнопок
//! [`MessageAction`] и заголовок сводного окна ошибок записи — перенесено
//! из `src/core/messages.rs`, у которого не было доступа к `i18n` (текст уже
//! был на русском, перенос без изменения содержания).
//!
//! Сообщения о загрузке/экспорте плейлиста (ADR-16, ADR-17, §6.12, §6.18,
//! ОВ-8, ТЗ-13 а/б, ТЗ-20, ТЗ-21, ТЗ-48, T1.0-B8) — перенесены из
//! `src/core/load.rs` байт-в-байт.
//!
//! Заголовки/тела окон сообщений, строка состояния и подсказка
//! переключателя режимов (ТЗ-52, ТЗ-86, ТЗ-110, ТЗ-111, ОВ-7, §6.2 п. 5,
//! §6.18, §6.20, T1.0-B8) — перенесены из `src/app/mod.rs` байт-в-байт.
//!
//! Строка состояния захвата устройства, строка состояния воспроизведения и
//! окно «Замок открыт» (ТЗ-119, ТЗ-125, §6.19, T1.0-B8) — перенесены из
//! `src/app/playback_manager.rs` байт-в-байт.

use std::path::Path;

use crate::core::messages::MessageAction;

use super::format_named;

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

/// Заголовок окна — аудио-движок не поднялся при старте приложения
/// (ТЗ-88, T1.0-B8): показывается сразу, не дожидаясь первого обращения к
/// звуку.
pub const ENGINE_NOT_STARTED_TITLE: &str = "Аудио-движок не запущен";

/// Короткая причина недоступности Оптимального/Строгого режима в
/// переключателе режимов top_panel (ТЗ-110, T1.0-B8) — дословно по тексту
/// ТЗ.
pub const MODE_SWITCH_UNAVAILABLE_REASON: &str = "монопольный доступ на этой платформе пока не поддерживается";

/// Заголовок уведомления о недоступном на платформе активном режиме
/// (ТЗ-111, §6.2 п. 5, ADR-13, T1.0-B8): ставится при старте и при отказе
/// открытия трека `OpenError::ModeUnavailable`.
pub const MODE_UNAVAILABLE_NOTICE_TITLE: &str = "Режим недоступен";

/// Тело уведомления, см. [`MODE_UNAVAILABLE_NOTICE_TITLE`].
pub const MODE_UNAVAILABLE_NOTICE_BODY: &str =
    "на этой платформе доступен только Совместимый режим (Shared), для воспроизведения переключите режим";

/// Короткий текст отказа отправки команды открытия трека, когда поток
/// движка уже не запущен (T1.0-B8, `src/app/mod.rs::open_track`) —
/// подстрока тела окна [`PLAYBACK_FAILED_TITLE`] (`on_open_failed`).
pub const ENGINE_NOT_RUNNING_SHORT: &str = "движок не запущен";

/// Заголовок предупреждения о падении загрузки стартовой темы (§6.1,
/// ТЗ-52, ОВ-7, T1.0-B8): тема для текущего запуска заменена на светлую,
/// значение в `settings.toml` не перезаписывается.
pub const THEME_NOT_LOADED_TITLE: &str = "Тема не загружена";

/// Тело предупреждения, см. [`THEME_NOT_LOADED_TITLE`]; `theme_name` —
/// имя темы из настроек, которую не удалось загрузить.
pub fn theme_not_loaded_body(theme_name: &str) -> String {
    format!("Тема \"{theme_name}\" не загружена, применена светлая тема")
}

/// Имя темы в диалоге настроек, когда файл темы есть, но не читается/не
/// разбирается (§5.2/§5.3, T1.0-B8).
pub const THEME_META_UNREADABLE: &str = "(не удалось загрузить метаданные)";

/// Имя темы в диалоге настроек, когда файл темы не найден в `themes/`
/// (§5.2/§5.3, T1.0-B8).
pub const THEME_NOT_FOUND: &str = "(тема не найдена)";

/// Строка состояния при автопропуске повреждённого/нечитаемого трека
/// (ТЗ-52, ТЗ-86, ТЗ-87, ОВ-7, T1.0-B8): `name` — имя файла, `reason` —
/// вывод `describe_skip_reason`.
pub fn track_skipped_status(name: &str, reason: &str) -> String {
    format!("Пропущен трек «{name}»: {reason}")
}

/// Строка состояния — вся библиотека пропущена подряд без успешного
/// открытия (защита от бесконечного пропуска при `RepeatMode::All`,
/// T1.0-B8).
pub const ALL_TRACKS_SKIPPED: &str = "Все треки пропущены";

/// Заголовок окна ошибки явного действия пользователя — трек не
/// воспроизвёлся (ТЗ-52, ОВ-7, T1.0-B8).
pub const PLAYBACK_FAILED_TITLE: &str = "Не удалось воспроизвести трек";

/// Тело окна ошибки «устройство не запустило воспроизведение», когда
/// известен буфер устройства (AM1.0-B34 шаг 4, §6.7 п. 2, §6.20, T1.0-B8).
pub fn device_buffer_stalled_body(ms: &str, period_frames: &str) -> String {
    format!("Буфер устройства {ms} мс (период {period_frames} кадров) — драйвер не начал воспроизведение.")
}

/// Тело того же окна, когда буфер устройства неизвестен, см.
/// [`device_buffer_stalled_body`].
pub const DRIVER_NOT_STARTED: &str = "Драйвер не начал воспроизведение.";

/// Заголовок окна ошибки «устройство не запустило воспроизведение»
/// (AM1.0-B34 шаг 4, §6.7 п. 2, §6.20, T1.0-B8): поток открылся, но
/// hw-указатель не двинулся — другая по смыслу ошибка, чем потеря уже
/// игравшего устройства ([`DEVICE_LOST_TITLE`]).
pub const DEVICE_NOT_STARTED_TITLE: &str = "Устройство не запустило воспроизведение";

/// Заголовок окна ошибки реальной потери устройства вывода во время игры
/// (ТЗ-51, ADR-14, T1.0-B8).
pub const DEVICE_LOST_TITLE: &str = "Устройство вывода потеряно";

/// Тело того же окна, см. [`DEVICE_LOST_TITLE`].
pub const DEVICE_LOST_BODY: &str = "Воспроизведение остановлено (ТЗ-51, ADR-14).";

/// Строка состояния при фоновом переключении на устройство по умолчанию
/// после потери текущего устройства (`Reaction::FollowSystemDefault`,
/// ТЗ-52, ОВ-7, T1.0-B8) — не явное действие пользователя, поэтому строка
/// состояния, не окно.
pub const OUTPUT_SWITCHED_TO_DEFAULT_STATUS: &str = "Вывод переключён на устройство по умолчанию";

/// Строка состояния при фоновом сбое перечисления устройств вывода
/// (ТЗ-104, ТЗ-105, ADR-16, T1.0-B8); `msg` — текст
/// `BackendError::Unavailable`.
pub fn devices_unavailable_status(msg: &str) -> String {
    format!("Не удалось получить список устройств вывода: {msg}")
}

/// Строка состояния, пока замок устройства захватывает устройство (ТЗ-119,
/// §6.19, AM1.0-B9, T1.0-B8): показывается вместо обычного текста
/// `self.status` (`src/app/playback_manager.rs::sync_playback_state_to_ui`).
pub const ACQUIRING_DEVICE_STATUS: &str = "Захват устройства\u{2026}";

/// Строка состояния текущего воспроизведения
/// (`src/app/playback_manager.rs::playing_status`, T1.0-B8); `artist` —
/// исполнитель (может быть пустой строкой), `title` — название трека.
pub fn now_playing_status(artist: &str, title: &str) -> String {
    format_named("Воспроизведение: {artist}\u{2014}{title}", &[("artist", artist), ("title", title)])
}

/// Заголовок окна «замок открыт — воспроизведение заблокировано» (ТЗ-125,
/// §6.19, T1.0-B8, `src/app/playback_manager.rs::lock_blocks_play`).
pub const LOCK_OPEN_BLOCKS_PLAY_TITLE: &str = "Замок открыт";

/// Тело того же окна, см. [`LOCK_OPEN_BLOCKS_PLAY_TITLE`].
pub const LOCK_OPEN_BLOCKS_PLAY_BODY: &str =
    "устройство не захвачено; нажмите «Захватить и играть», чтобы захватить устройство и начать воспроизведение";

/// Заголовок окна Error при непрочитанном `settings.toml` (ОВС-6 в, §2.13,
/// T1.0-B8, `src/core/mod.rs::settings_read_failed_message`).
pub const SETTINGS_READ_FAILED_TITLE: &str = "Файл настроек не прочитан";

/// Тело того же окна, см. [`SETTINGS_READ_FAILED_TITLE`]; `os_text` —
/// системный текст ошибки чтения.
pub fn settings_read_failed_body(os_text: &str) -> String {
    format!(
        "Файл настроек не удалось прочитать ({os_text}). Используются значения по умолчанию. \
         Автоматическое сохранение отключено до перезапуска. Кнопка «Сохранить» в окне \
         настроек заменит файл."
    )
}

/// Постоянный текст рядом с «Сохранить» в диалоге настроек, пока действует
/// запрет автозаписи `settings.toml` (ОВС-6 в, §2.13, T1.0-B8,
/// `src/core/mod.rs::settings_save_notice`).
pub const SETTINGS_SAVE_NOTICE: &str =
    "Файл настроек не прочитан — «Сохранить» заменит его значениями из этого окна";

/// Подпись режима «Спектрограмма» в плейсхолдере недоступности для DSD
/// (§6.3, T1.0-B8; перенесено из `src/app/fulltrack_manager.rs::drain_fulltrack`
/// байт-в-байт), см. [`fulltrack_disabled_for_dsd`].
pub const FULLTRACK_LABEL_SPECTROGRAM: &str = "Спектрограмма";
/// Подпись режима «Осциллограмма», см. [`FULLTRACK_LABEL_SPECTROGRAM`].
pub const FULLTRACK_LABEL_OSCILLOGRAM: &str = "Осциллограмма";

/// Плейсхолдер полнотрековой визуализации (осциллограмма/спектрограмма),
/// пропущенной для DSD-трека (§6.3, T1.0-B8; перенесено из
/// `src/app/fulltrack_manager.rs::drain_fulltrack` байт-в-байт): `label` —
/// [`FULLTRACK_LABEL_SPECTROGRAM`] или [`FULLTRACK_LABEL_OSCILLOGRAM`].
pub fn fulltrack_disabled_for_dsd(label: &str) -> String {
    format!("{label} недоступна для DSD")
}

/// Причина отказа построения полнотрековой визуализации: нулевая длина
/// трека (T1.0-B8; перенесено из `src/app/fulltrack_manager.rs` байт-в-байт,
/// два места вызова `fail()`).
pub const FULLTRACK_FAIL_ZERO_LENGTH: &str = "не удалось определить длину трека (num_frames == 0)";

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

    #[test]
    fn theme_not_loaded_body_substitutes_theme_name() {
        assert_eq!(theme_not_loaded_body("dark"), "Тема \"dark\" не загружена, применена светлая тема");
    }

    #[test]
    fn track_skipped_status_substitutes_name_and_reason() {
        assert_eq!(
            track_skipped_status("intro.flac", "файл повреждён: повреждён заголовок файла"),
            "Пропущен трек «intro.flac»: файл повреждён: повреждён заголовок файла"
        );
    }

    #[test]
    fn device_buffer_stalled_body_substitutes_ms_and_period() {
        assert_eq!(
            device_buffer_stalled_body("20", "882"),
            "Буфер устройства 20 мс (период 882 кадров) — драйвер не начал воспроизведение."
        );
    }

    #[test]
    fn devices_unavailable_status_substitutes_msg() {
        assert_eq!(
            devices_unavailable_status("D-Bus недоступен"),
            "Не удалось получить список устройств вывода: D-Bus недоступен"
        );
    }

    #[test]
    fn app_message_constants_are_russian() {
        assert_eq!(ENGINE_NOT_STARTED_TITLE, "Аудио-движок не запущен");
        assert_eq!(MODE_SWITCH_UNAVAILABLE_REASON, "монопольный доступ на этой платформе пока не поддерживается");
        assert_eq!(MODE_UNAVAILABLE_NOTICE_TITLE, "Режим недоступен");
        assert_eq!(
            MODE_UNAVAILABLE_NOTICE_BODY,
            "на этой платформе доступен только Совместимый режим (Shared), для воспроизведения переключите режим"
        );
        assert_eq!(ENGINE_NOT_RUNNING_SHORT, "движок не запущен");
        assert_eq!(THEME_NOT_LOADED_TITLE, "Тема не загружена");
        assert_eq!(THEME_META_UNREADABLE, "(не удалось загрузить метаданные)");
        assert_eq!(THEME_NOT_FOUND, "(тема не найдена)");
        assert_eq!(ALL_TRACKS_SKIPPED, "Все треки пропущены");
        assert_eq!(PLAYBACK_FAILED_TITLE, "Не удалось воспроизвести трек");
        assert_eq!(DRIVER_NOT_STARTED, "Драйвер не начал воспроизведение.");
        assert_eq!(DEVICE_NOT_STARTED_TITLE, "Устройство не запустило воспроизведение");
        assert_eq!(DEVICE_LOST_TITLE, "Устройство вывода потеряно");
        assert_eq!(DEVICE_LOST_BODY, "Воспроизведение остановлено (ТЗ-51, ADR-14).");
        assert_eq!(OUTPUT_SWITCHED_TO_DEFAULT_STATUS, "Вывод переключён на устройство по умолчанию");
        assert_eq!(ACQUIRING_DEVICE_STATUS, "Захват устройства\u{2026}");
        assert_eq!(LOCK_OPEN_BLOCKS_PLAY_TITLE, "Замок открыт");
        assert_eq!(
            LOCK_OPEN_BLOCKS_PLAY_BODY,
            "устройство не захвачено; нажмите «Захватить и играть», чтобы захватить устройство и начать воспроизведение"
        );
    }

    #[test]
    fn now_playing_status_substitutes_artist_and_title() {
        assert_eq!(now_playing_status("Daft Punk", "Harder"), "Воспроизведение: Daft Punk\u{2014}Harder");
    }

    #[test]
    fn fulltrack_disabled_for_dsd_substitutes_label() {
        assert_eq!(
            fulltrack_disabled_for_dsd(FULLTRACK_LABEL_SPECTROGRAM),
            "Спектрограмма недоступна для DSD"
        );
        assert_eq!(
            fulltrack_disabled_for_dsd(FULLTRACK_LABEL_OSCILLOGRAM),
            "Осциллограмма недоступна для DSD"
        );
    }

    #[test]
    fn fulltrack_fail_zero_length_is_russian() {
        assert_eq!(FULLTRACK_FAIL_ZERO_LENGTH, "не удалось определить длину трека (num_frames == 0)");
    }
}
