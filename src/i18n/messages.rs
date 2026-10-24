//! Текст окон сообщений (T1.0-B7, ТЗ-46, §6.8): подписи кнопок
//! [`MessageAction`] и заголовок сводного окна ошибок записи — перенесено
//! из `src/core/messages.rs`, у которого не было доступа к `i18n` (текст уже
//! был на русском, перенос без изменения содержания).

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
}
