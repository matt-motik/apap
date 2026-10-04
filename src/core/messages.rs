//! Центр сообщений (ADR-13, §6.15, §6.8, ТЗ-52, ТЗ-20). UI-поток, без Slint.
//!
//! `MessageCenter` не владеет `UiGate`: вызывающий код сам взводит причину
//! блокировки `BlockReason::Message`, когда эффект несёт `show`, и снимает
//! её, когда эффект несёт `hide` (это и есть `gate.block`/`unblock` из §6.15
//! на стороне вызывающего).

use std::collections::{BTreeMap, VecDeque};

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

/// Набор кнопок окна сообщения (ADR-13).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageButtons {
    Ok,
    RetryOk,
}

/// Нажатая кнопка: `Primary` — основная (Enter), `Close` — закрыть (Esc) (§6.15).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageButton {
    Primary,
    Close,
}

/// Содержимое окна сообщения (ADR-13, §6.15).
#[derive(Clone, PartialEq, Debug)]
pub struct Message {
    pub level: MessageLevel,
    pub title: Box<str>,
    pub body: Box<str>,
    pub buttons: MessageButtons,
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

    /// Нажатие кнопки показанного окна (Enter → `Primary`, Esc → `Close`).
    /// Для `Ok`-окна обе кнопки закрывают его (§6.15).
    pub fn press(&mut self, b: MessageButton) -> (CloseEffect, MsgEffect) {
        let mut effect = MsgEffect::default();
        match self.shown.take() {
            None => (CloseEffect::Closed, effect),
            Some(Queued::Plain(_)) => {
                effect.hide = true;
                self.show_next(&mut effect);
                (CloseEffect::Closed, effect)
            }
            Some(Queued::WriteErrors) => match b {
                // «Повторить»: окно остаётся открытым, пока список не пуст (§6.8).
                MessageButton::Primary => {
                    let files: Vec<WorkFile> = self.write_errors.keys().copied().collect();
                    self.shown = Some(Queued::WriteErrors);
                    (CloseEffect::Retry(files), effect)
                }
                // «OK»: окно закрыто, запись по отсчёту для этих файлов остаётся
                // остановленной — это заботит вызывающий код (ТЗ-20).
                MessageButton::Close => {
                    self.write_errors.clear();
                    effect.hide = true;
                    self.show_next(&mut effect);
                    (CloseEffect::Closed, effect)
                }
            },
        }
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
            buttons: MessageButtons::RetryOk,
        }
    }
}
