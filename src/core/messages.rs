//! Центр сообщений (ADR-13, §6.15, §6.8, ТЗ-52, ТЗ-20). UI-поток, без Slint.
//!
//! `MessageCenter` не владеет `UiGate`: вызывающий код сам взводит причину
//! блокировки `BlockReason::Message`, когда эффект несёт `show`, и снимает
//! её, когда эффект несёт `hide` (это и есть `gate.block`/`unblock` из §6.15
//! на стороне вызывающего).

use std::collections::{BTreeMap, VecDeque};

use crate::audio::error::CaptureFailure;
use crate::persist::WorkFile;
use crate::platform::fs::WriteErrorClass;
use crate::platform::lifecycle::PlatformCaps;
use crate::platform::notify::Notification;

/// Уровень сообщения (ADR-13). `Info` в текущем объёме не используется.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageLevel {
    Info,
    Warning,
    Error,
}

/// Действие кнопки окна сообщения (ADR-13, §6.15, ТЗ-46, ТЗ-144). Одна модель
/// и для фиксированных окон (`Message.actions`, составляется прямо на
/// месте вызова `push_message`/`Message { .. }`), и для переменного набора
/// окна ошибки замка: `lock_failure_actions`
/// возвращает переменное по длине и составу упорядоченное подмножество в
/// зависимости от причины отказа захвата. Конвенция порядка — общая для
/// обоих источников и для `ui/message_window.slint`: первый элемент —
/// основная кнопка окна (Enter), последний — закрывающая (Esc); если
/// элемент один, он и основная, и закрывающая. Полная проводка окна ошибки
/// замка (`LockState::Failed` → `push_message`) — следующий шаг (AM1.0-B14).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageAction {
    /// Повторить попытку захвата устройства (ТЗ-42). Отсутствует для причин, при
    /// которых повтор заведомо бессмыслен (`NoDeviceSelected`, `PlatformUnsupported`).
    RetryCapture,
    /// Открыть диалог настроек на вкладке «Аудио» (ТЗ-46, ТЗ-144).
    OpenAudioSettings,
    /// Перейти в Совместимый режим (ТЗ-46, ТЗ-111).
    SwitchToCompatible,
    /// Закрыть окно без действия. Всегда присутствует и всегда последняя в
    /// наборе — привязана к Esc (см. `lock_failure_actions`).
    Cancel,
    /// Единственная кнопка обычного информационного/предупреждающего
    /// сообщения.
    Ok,
    /// Повторить запись рабочих файлов — основная кнопка сводного окна
    /// ошибок записи (§6.8).
    Retry,
    /// Захватить устройство и начать воспроизведение — основная кнопка окна
    /// «Замок открыт» (ТЗ-125).
    CaptureAndPlay,
}

impl MessageAction {
    /// Русская подпись кнопки (ТЗ-46).
    pub fn label(self) -> &'static str {
        match self {
            MessageAction::RetryCapture => "Повторить захват",
            MessageAction::OpenAudioSettings => "Настройки аудио",
            MessageAction::SwitchToCompatible => "В Совместимый",
            MessageAction::Cancel => "Отмена",
            MessageAction::Ok => "OK",
            MessageAction::Retry => "Повторить",
            MessageAction::CaptureAndPlay => "Захватить и играть",
        }
    }
}

/// Набор кнопок окна ошибки захвата по причине (ТЗ-46, ТЗ-144). Порядок в
/// возвращённом `Vec` — порядок кнопок слева направо; первый элемент —
/// основная кнопка окна (Enter), последний элемент — всегда `Cancel`,
/// привязанная к Esc (см. `MessageAction` про общую конвенцию порядка).
///
/// Группы по таблице решения пользователя 2026-10-23:
/// - `NoDeviceSelected`, `PlatformUnsupported` — без повтора: «Настройки
///   аудио» (основная) · «Перейти в Совместимый» · «Отмена»;
/// - все прочие причины (устройство отключено/занято — с владельцем или в
///   обход протокола, отказ драйвера, владелец не отвечает, резерв потерян,
///   нет сессионной шины, отказ при повторном открытии) — с повтором:
///   «Повторить захват» (основная) · «Настройки аудио» · «Перейти в
///   Совместимый» · «Отмена».
pub fn lock_failure_actions(reason: &CaptureFailure) -> Vec<MessageAction> {
    use MessageAction::{Cancel, OpenAudioSettings, RetryCapture, SwitchToCompatible};
    match reason {
        CaptureFailure::NoDeviceSelected | CaptureFailure::PlatformUnsupported => {
            vec![OpenAudioSettings, SwitchToCompatible, Cancel]
        }
        CaptureFailure::DeviceDisconnected
        | CaptureFailure::ReservationDenied { .. }
        | CaptureFailure::BusyOutsideProtocol { .. }
        | CaptureFailure::DriverRefused { .. }
        | CaptureFailure::OwnerNotResponding
        | CaptureFailure::ReservationLost
        | CaptureFailure::LostOnReopen
        | CaptureFailure::NoSessionBus => {
            vec![RetryCapture, OpenAudioSettings, SwitchToCompatible, Cancel]
        }
    }
}

/// Содержимое окна сообщения (ADR-13, §6.15).
#[derive(Clone, PartialEq, Debug)]
pub struct Message {
    pub level: MessageLevel,
    pub title: Box<str>,
    pub body: Box<str>,
    /// Набор кнопок слева направо; первый элемент — основная кнопка окна
    /// (Enter), последний — закрывающая (Esc); если элемент один, он и
    /// основная, и закрывающая (см. `MessageAction`, `lock_failure_actions`).
    pub actions: Vec<MessageAction>,
}

/// Запись очереди: обычное сообщение или сводная запись ошибок записи (§6.8).
#[derive(Clone, PartialEq, Debug)]
enum Queued {
    Plain(Message),
    WriteErrors,
}

/// Что сделать с окном и уведомлением после вызова `MessageCenter` (ADR-13).
#[derive(Default, PartialEq, Debug)]
pub struct MsgEffect {
    pub show: Option<Message>,
    pub hide: bool,
    pub notify: Option<Notification>,
}

/// Итог нажатия кнопки для вызывающего кода (§6.8, ТЗ-20).
#[derive(Clone, PartialEq, Debug)]
pub enum CloseEffect {
    Closed,
    /// «Повторить»: список файлов для немедленной записи (§6.8).
    Retry(Vec<WorkFile>),
    /// «Переключить в Совместимый режим»: вызывающий код выполняет тот же
    /// путь, что выбор Совместимого режима в переключателе (ТЗ-111, §6.2 п. 5).
    SwitchToCompatible,
    /// «Захватить и играть»: вызывающий код выполняет захват устройства, как
    /// при ручном закрытии замка, и при успехе начинает воспроизведение
    /// (ТЗ-125).
    CaptureAndPlay,
}

/// Очередь сообщений (ADR-13). Хранит до первого показа главного окна,
/// показывает по одному, сводит ошибки записи рабочих файлов в одно окно
/// (§6.8) и уведомляет, если главное окно скрыто в трей (§6.15).
#[derive(Default)]
pub struct MessageCenter {
    queue: VecDeque<Queued>,
    shown: Option<Queued>,
    window_shown: bool,
    in_tray: bool,
    write_errors: BTreeMap<WorkFile, WriteErrorClass>,
}

impl MessageCenter {
    /// Ставит обычное сообщение в очередь (ТЗ-7, ТЗ-52). Уведомление — если
    /// главное окно скрыто в трей и трей умеет уведомления (§6.15).
    pub fn push(&mut self, m: Message, caps: PlatformCaps) -> MsgEffect {
        let mut effect = MsgEffect::default();
        if self.in_tray && caps.notifications {
            effect.notify = Some(Notification {
                level: m.level,
                title: m.title.clone(),
                body: m.body.clone(),
            });
        }
        self.queue.push_back(Queued::Plain(m));
        self.show_next(&mut effect);
        effect
    }

    /// Ошибка синхронной записи рабочего файла: файл добавляется/обновляется
    /// в сводной записи; новое окно открывается не чаще одного раза на
    /// появление записи — одно уведомление на новую запись (§6.8, ТЗ-20).
    pub fn write_failed(&mut self, f: WorkFile, class: WriteErrorClass, caps: PlatformCaps) -> MsgEffect {
        let mut effect = MsgEffect::default();
        self.write_errors.insert(f, class);
        if matches!(self.shown, Some(Queued::WriteErrors)) {
            effect.show = Some(self.write_errors_text());
        } else if self.write_errors_queued() {
            // Сводная запись уже в очереди, ждёт показа — ничего больше.
        } else {
            self.queue.push_back(Queued::WriteErrors);
            if self.in_tray && caps.notifications {
                let text = self.write_errors_text();
                effect.notify = Some(Notification {
                    level: text.level,
                    title: text.title,
                    body: text.body,
                });
            }
            self.show_next(&mut effect);
        }
        effect
    }

    /// Файл успешно записан (по «Повторить» или по отсчёту): убирает его из
    /// сводной записи; опустевшая запись закрывает окно или снимается из
    /// очереди (§6.8).
    pub fn write_succeeded(&mut self, f: WorkFile) -> MsgEffect {
        let mut effect = MsgEffect::default();
        self.write_errors.remove(&f);
        if self.write_errors.is_empty() {
            if matches!(self.shown, Some(Queued::WriteErrors)) {
                self.shown = None;
                effect.hide = true;
                self.show_next(&mut effect);
            } else {
                self.queue.retain(|q| !matches!(q, Queued::WriteErrors));
            }
        } else if matches!(self.shown, Some(Queued::WriteErrors)) {
            effect.show = Some(self.write_errors_text());
        }
        effect
    }

    /// Главное окно показано первый раз: до этого момента сообщения только
    /// копились (ТЗ-7, ТЗ-52).
    pub fn window_shown(&mut self) -> MsgEffect {
        let mut effect = MsgEffect::default();
        self.window_shown = true;
        self.show_next(&mut effect);
        effect
    }

    /// Признак «главное окно скрыто в трей». При открытии окна копившиеся
    /// сообщения показываются (§6.15).
    pub fn set_in_tray(&mut self, hidden: bool) -> MsgEffect {
        let mut effect = MsgEffect::default();
        self.in_tray = hidden;
        if !hidden {
            self.show_next(&mut effect);
        }
        effect
    }

    /// Нажатие кнопки показанного окна: `action` — действие нажатой кнопки
    /// (`Message.actions`/`lock_failure_actions`; Enter —
    /// действие с индексом 0, Esc — с последним индексом, см.
    /// `MessageAction`). Для `Ok`-окна любое действие закрывает его (§6.15).
    pub fn press(&mut self, action: MessageAction) -> (CloseEffect, MsgEffect) {
        let mut effect = MsgEffect::default();
        match self.shown.take() {
            None => (CloseEffect::Closed, effect),
            Some(Queued::Plain(_)) => {
                effect.hide = true;
                self.show_next(&mut effect);
                match action {
                    MessageAction::SwitchToCompatible => (CloseEffect::SwitchToCompatible, effect),
                    MessageAction::CaptureAndPlay => (CloseEffect::CaptureAndPlay, effect),
                    _ => (CloseEffect::Closed, effect),
                }
            }
            Some(Queued::WriteErrors) => match action {
                // «Повторить»: окно остаётся открытым, пока список не пуст (§6.8).
                MessageAction::Retry => {
                    let files: Vec<WorkFile> = self.write_errors.keys().copied().collect();
                    self.shown = Some(Queued::WriteErrors);
                    (CloseEffect::Retry(files), effect)
                }
                // «OK» (или любое другое действие): окно закрыто, запись по
                // отсчёту для этих файлов остаётся остановленной — это
                // заботит вызывающий код (ТЗ-20).
                _ => {
                    self.write_errors.clear();
                    effect.hide = true;
                    self.show_next(&mut effect);
                    (CloseEffect::Closed, effect)
                }
            },
        }
    }

    /// Убирает сообщение `m` программно — без нажатия кнопки (AM1.0-B14,
    /// ТЗ-46, ТЗ-126): окно ошибки замка вызывает это при выходе из
    /// `Failed` (автозахват вернувшегося устройства) и при замене причины
    /// отказа другой. Если `m` сейчас показано — закрывает его и показывает
    /// следующее из очереди (как `press`); если оно только стоит в
    /// очереди — снимает его оттуда без эффекта. `m` ищется по значению, не
    /// по указателю — несколько равных по содержимому сообщений не
    /// различаются (в текущем использовании «моё» сообщение всегда одно).
    pub fn retract(&mut self, m: &Message) -> MsgEffect {
        let mut effect = MsgEffect::default();
        if matches!(&self.shown, Some(Queued::Plain(shown)) if shown == m) {
            self.shown = None;
            effect.hide = true;
            self.show_next(&mut effect);
            return effect;
        }
        self.queue.retain(|q| !matches!(q, Queued::Plain(q) if q == m));
        effect
    }

    /// Путь выхода: закрыть показанное и очистить очередь без вопроса (ТЗ-52).
    pub fn dismiss_for_exit(&mut self) {
        self.shown = None;
        self.queue.clear();
    }

    /// Текст показанного сейчас сообщения, если оно есть.
    pub fn shown(&self) -> Option<Message> {
        self.shown.as_ref().map(|q| self.render(q))
    }

    /// `true`, если окно сообщения сейчас показано.
    pub fn is_shown(&self) -> bool {
        self.shown.is_some()
    }

    /// Показывает следующее сообщение из очереди, если окно уже показывалось,
    /// не скрыто в трей и сейчас ничего не показано (§6.15).
    fn show_next(&mut self, effect: &mut MsgEffect) {
        if !self.window_shown || self.in_tray || self.shown.is_some() {
            return;
        }
        let Some(q) = self.queue.pop_front() else { return };
        effect.show = Some(self.render(&q));
        self.shown = Some(q);
    }

    fn render(&self, q: &Queued) -> Message {
        match q {
            Queued::Plain(m) => m.clone(),
            Queued::WriteErrors => self.write_errors_text(),
        }
    }

    fn write_errors_queued(&self) -> bool {
        self.queue.iter().any(|q| matches!(q, Queued::WriteErrors))
    }

    /// Текст сводного окна ошибок записи: по одной строке на файл, в порядке
    /// `BTreeMap` (§6.8, ТЗ-20).
    fn write_errors_text(&self) -> Message {
        let body = self
            .write_errors
            .iter()
            .map(|(f, c)| format!("{} — {}", f.file_name(), c.text()))
            .collect::<Vec<_>>()
            .join("\n");
        Message {
            level: MessageLevel::Error,
            title: "Не удалось сохранить данные".into(),
            body: body.into(),
            actions: vec![MessageAction::Retry, MessageAction::Ok],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_msg(title: &str) -> Message {
        Message {
            level: MessageLevel::Warning,
            title: title.into(),
            body: "body".into(),
            actions: vec![MessageAction::Ok],
        }
    }

    #[test]
    fn messages_before_show_queued_in_order() {
        let mut mc = MessageCenter::default();
        let caps = PlatformCaps::default();
        let m1 = plain_msg("A");
        let m2 = plain_msg("B");

        assert_eq!(mc.push(m1.clone(), caps).show, None);
        assert_eq!(mc.push(m2.clone(), caps).show, None);

        let shown = mc.window_shown();
        assert_eq!(shown.show, Some(m1));

        let (close, effect) = mc.press(MessageAction::Ok);
        assert_eq!(close, CloseEffect::Closed);
        assert!(effect.hide);
        assert_eq!(effect.show, Some(m2));
    }

    #[test]
    fn tray_hidden_error_notifies_once() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps {
            tray: true,
            notifications: true,
        };
        mc.set_in_tray(true);

        let e1 = mc.write_failed(WorkFile::State, WriteErrorClass::Io, caps);
        assert!(e1.notify.is_some());
        assert_eq!(e1.show, None);

        let e2 = mc.write_failed(WorkFile::Settings, WriteErrorClass::NoSpace, caps);
        assert!(e2.notify.is_none());
        assert_eq!(e2.show, None);

        let e3 = mc.set_in_tray(false);
        let shown = e3.show.expect("окно ошибок записи показано при открытии");
        assert_eq!(shown.level, MessageLevel::Error);
        assert!(shown.body.contains("state.toml"));
        assert!(shown.body.contains("settings.toml"));
    }

    #[test]
    fn no_tray_error_shows_window() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps::default();

        let e = mc.write_failed(WorkFile::Playlist, WriteErrorClass::ReadOnlyFs, caps);
        assert!(e.notify.is_none());
        let shown = e.show.expect("окно ошибки записи показано без трея");
        assert_eq!(shown.level, MessageLevel::Error);
        assert_eq!(shown.actions, vec![MessageAction::Retry, MessageAction::Ok]);
    }

    #[test]
    fn enter_esc_buttons() {
        let caps = PlatformCaps::default();

        let mut primary_closes = MessageCenter::default();
        primary_closes.window_shown();
        primary_closes.push(plain_msg("A"), caps);
        let (close, _) = primary_closes.press(MessageAction::Ok);
        assert_eq!(close, CloseEffect::Closed);

        let mut close_closes = MessageCenter::default();
        close_closes.window_shown();
        close_closes.push(plain_msg("A"), caps);
        let (close, _) = close_closes.press(MessageAction::Ok);
        assert_eq!(close, CloseEffect::Closed);

        let mut retry = MessageCenter::default();
        retry.window_shown();
        retry.write_failed(WorkFile::State, WriteErrorClass::Io, caps);
        let (close, _) = retry.press(MessageAction::Retry);
        assert_eq!(close, CloseEffect::Retry(vec![WorkFile::State]));
        assert!(retry.is_shown());

        let (close, effect) = retry.press(MessageAction::Ok);
        assert_eq!(close, CloseEffect::Closed);
        assert!(effect.hide);
        assert!(!retry.is_shown());
    }

    #[test]
    fn write_errors_merge_into_one_window() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps::default();

        let e1 = mc.write_failed(WorkFile::State, WriteErrorClass::Io, caps);
        assert!(e1.show.is_some());

        let e2 = mc.write_failed(WorkFile::Settings, WriteErrorClass::NoSpace, caps);
        let shown = e2.show.expect("сводный текст обновлён");
        assert!(shown.body.contains("state.toml"));
        assert!(shown.body.contains("settings.toml"));

        let (close, _) = mc.press(MessageAction::Retry);
        assert_eq!(close, CloseEffect::Retry(vec![WorkFile::State, WorkFile::Settings]));
    }

    #[test]
    fn retry_success_closes_window() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps::default();
        mc.write_failed(WorkFile::State, WriteErrorClass::Io, caps);
        mc.write_failed(WorkFile::Settings, WriteErrorClass::NoSpace, caps);
        assert!(mc.is_shown());

        let e1 = mc.write_succeeded(WorkFile::State);
        assert!(!e1.hide);
        assert!(mc.is_shown());

        let e2 = mc.write_succeeded(WorkFile::Settings);
        assert!(e2.hide);
        assert!(!mc.is_shown());
    }

    #[test]
    fn switch_compatible_primary_returns_switch_effect() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps::default();
        mc.push(
            Message {
                level: MessageLevel::Warning,
                title: "Сохранённый режим недоступен".into(),
                body: "body".into(),
                actions: vec![MessageAction::SwitchToCompatible, MessageAction::Ok],
            },
            caps,
        );

        let (close, effect) = mc.press(MessageAction::SwitchToCompatible);
        assert_eq!(close, CloseEffect::SwitchToCompatible);
        assert!(effect.hide);
    }

    #[test]
    fn switch_compatible_close_returns_closed() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps::default();
        mc.push(
            Message {
                level: MessageLevel::Warning,
                title: "Сохранённый режим недоступен".into(),
                body: "body".into(),
                actions: vec![MessageAction::SwitchToCompatible, MessageAction::Ok],
            },
            caps,
        );

        let (close, _) = mc.press(MessageAction::Ok);
        assert_eq!(close, CloseEffect::Closed);
    }

    #[test]
    fn capture_and_play_primary_returns_capture_effect() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps::default();
        mc.push(
            Message {
                level: MessageLevel::Warning,
                title: "Замок открыт".into(),
                body: "body".into(),
                actions: vec![MessageAction::CaptureAndPlay, MessageAction::Ok],
            },
            caps,
        );

        let (close, effect) = mc.press(MessageAction::CaptureAndPlay);
        assert_eq!(close, CloseEffect::CaptureAndPlay);
        assert!(effect.hide);
    }

    #[test]
    fn capture_and_play_close_returns_closed() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps::default();
        mc.push(
            Message {
                level: MessageLevel::Warning,
                title: "Замок открыт".into(),
                body: "body".into(),
                actions: vec![MessageAction::CaptureAndPlay, MessageAction::Ok],
            },
            caps,
        );

        let (close, _) = mc.press(MessageAction::Ok);
        assert_eq!(close, CloseEffect::Closed);
    }

    #[test]
    fn plain_ok_primary_still_closes() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        mc.push(plain_msg("A"), PlatformCaps::default());

        let (close, _) = mc.press(MessageAction::Ok);
        assert_eq!(close, CloseEffect::Closed);
    }

    #[test]
    fn dismiss_for_exit_clears_everything() {
        let mut mc = MessageCenter::default();
        mc.push(plain_msg("A"), PlatformCaps::default());
        mc.window_shown();
        mc.push(plain_msg("B"), PlatformCaps::default());
        assert!(mc.is_shown());

        mc.dismiss_for_exit();
        assert!(!mc.is_shown());

        let e = mc.window_shown();
        assert_eq!(e.show, None, "очередь тоже очищена — показывать нечего");
    }

    /// `lock_failure_actions` (ТЗ-144): «устройство не выбрано» — без повтора,
    /// «Настройки аудио» основная.
    #[test]
    fn no_device_selected_actions() {
        let actions = lock_failure_actions(&CaptureFailure::NoDeviceSelected);
        assert_eq!(
            actions,
            vec![MessageAction::OpenAudioSettings, MessageAction::SwitchToCompatible, MessageAction::Cancel]
        );
    }

    /// `lock_failure_actions` (ТЗ-46): «платформа не поддерживает» — тот же
    /// набор, что «устройство не выбрано» (без повтора).
    #[test]
    fn platform_unsupported_actions() {
        let actions = lock_failure_actions(&CaptureFailure::PlatformUnsupported);
        assert_eq!(
            actions,
            vec![MessageAction::OpenAudioSettings, MessageAction::SwitchToCompatible, MessageAction::Cancel]
        );
    }

    /// `lock_failure_actions` (ТЗ-46, ТЗ-122): все причины, допускающие повтор
    /// захвата, дают один и тот же набор с «Повторить захват» первой кнопкой.
    #[test]
    fn retryable_reasons_get_retry_first() {
        let reasons = [
            CaptureFailure::DeviceDisconnected,
            CaptureFailure::ReservationDenied { owner: None },
            CaptureFailure::BusyOutsideProtocol { owner: None },
            CaptureFailure::DriverRefused { errno: 0 },
            CaptureFailure::OwnerNotResponding,
            CaptureFailure::ReservationLost,
            CaptureFailure::LostOnReopen,
            CaptureFailure::NoSessionBus,
        ];
        let expected = vec![
            MessageAction::RetryCapture,
            MessageAction::OpenAudioSettings,
            MessageAction::SwitchToCompatible,
            MessageAction::Cancel,
        ];
        for reason in &reasons {
            assert_eq!(lock_failure_actions(reason), expected, "reason={reason:?}");
        }
    }

    /// `lock_failure_actions`: «Отмена» и «Настройки аудио» присутствуют для
    /// любой причины отказа захвата.
    #[test]
    fn cancel_and_audio_settings_always_present() {
        let reasons = [
            CaptureFailure::NoDeviceSelected,
            CaptureFailure::PlatformUnsupported,
            CaptureFailure::DeviceDisconnected,
            CaptureFailure::ReservationDenied { owner: None },
            CaptureFailure::BusyOutsideProtocol { owner: None },
            CaptureFailure::DriverRefused { errno: 0 },
            CaptureFailure::OwnerNotResponding,
            CaptureFailure::ReservationLost,
            CaptureFailure::LostOnReopen,
            CaptureFailure::NoSessionBus,
        ];
        for reason in &reasons {
            let actions = lock_failure_actions(reason);
            assert!(actions.contains(&MessageAction::Cancel), "reason={reason:?}");
            assert!(actions.contains(&MessageAction::OpenAudioSettings), "reason={reason:?}");
            assert_eq!(actions.last(), Some(&MessageAction::Cancel), "Cancel не последняя: reason={reason:?}");
        }
    }

    /// `lock_failure_actions`: «Повторить захват» отсутствует ровно для
    /// `NoDeviceSelected`/`PlatformUnsupported` — там повтор бессмыслен.
    #[test]
    fn retry_absent_for_no_device_and_platform_unsupported() {
        for reason in [CaptureFailure::NoDeviceSelected, CaptureFailure::PlatformUnsupported] {
            let actions = lock_failure_actions(&reason);
            assert!(!actions.contains(&MessageAction::RetryCapture), "reason={reason:?}");
        }
    }

    /// `retract` (AM1.0-B14 шаг 5): показанное сообщение закрывается и
    /// освобождает место следующему в очереди; сообщение, которое ещё не
    /// показывалось, снимается из очереди без эффекта.
    #[test]
    fn retract_shown_closes_and_queued_removes_without_effect() {
        let mut mc = MessageCenter::default();
        mc.window_shown();
        let caps = PlatformCaps::default();
        let a = plain_msg("A");
        let b = plain_msg("B");

        mc.push(a.clone(), caps);
        assert_eq!(mc.shown(), Some(a.clone()));
        mc.push(b.clone(), caps);

        // "B" ещё в очереди, не показано — retract его убирает молча.
        let effect = mc.retract(&b);
        assert!(!effect.hide);
        assert_eq!(effect.show, None);

        // "A" показано — retract закрывает его; очередь пуста ("B" снято), окно пропадает.
        let effect = mc.retract(&a);
        assert!(effect.hide);
        assert_eq!(effect.show, None);
        assert!(!mc.is_shown());
    }
}
