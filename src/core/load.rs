//! Запуск загрузки плейлиста при старте (ADR-16, §6.12, ТЗ-21, ТЗ-47).
//!
//! Стартовая загрузка `playlist.m3u` идёт в потоке `apap-playlist`
//! (`PlaylistLoader`, `crate::playlist::load`): задание несёт поколение
//! `LoadGen`, итог приходит через канал `LoadOutcome`. Итог чужого
//! (устаревшего) поколения или итог при отсутствии активной загрузки
//! отбрасывается молча (И-Т9) — он пришёл от задания, результат которого
//! уже не ждут.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};

use super::messages::{Message, MessageButtons, MessageLevel};
use super::AppCore;
use crate::journal::JournalRecord;
use crate::persist::writer::WriterCmd;
use crate::persist::WorkFile;
use crate::playlist::load::{LoadGen, LoadJob, LoadOutcome, LoadSource};
use crate::playlist::model::SortKey;

/// Состояние загрузки плейлиста (ADR-16, §6.12). Команда «открыть файл»
/// (`Command`) — отдельный вариант будущего шага, здесь не вводится.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LoadState {
    #[default]
    Idle,
    Startup { gen: LoadGen },
}

impl LoadState {
    /// Нет активной загрузки.
    pub fn is_idle(&self) -> bool {
        matches!(self, LoadState::Idle)
    }
}

/// Плейлист заменён результатом завершённой загрузки — UI обязан
/// перечитать строки таблицы (ADR-16, §6.12).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadApplied {
    Replaced,
}

/// Итог обработки результата ТЕКУЩЕЙ загрузки за один вызов `poll_load`
/// (ADR-16, §6.12): что применилось к плейлисту и какое сообщение
/// показать пользователю, если оно есть.
pub struct LoadReport {
    pub applied: Option<LoadApplied>,
    pub message: Option<Message>,
}

/// Управление загрузкой плейлиста (ADR-16, §6.12): последнее выданное
/// поколение и состояние активной загрузки, канал результатов потока
/// `apap-playlist` — отправитель клонируется в каждое задание загрузчика,
/// получатель живёт здесь и опрашивается `poll_load`. `sort` — ключ,
/// с которым запущено активное задание: строки итога отсортированы по нему,
/// и `Playlist::replace` получает его же, даже если ключ сменился за время
/// загрузки (ADR-16, §6.12).
pub struct LoadCtl {
    state: LoadState,
    gen: LoadGen,
    sort: Option<SortKey>,
    tx: Sender<LoadOutcome>,
    rx: Receiver<LoadOutcome>,
}

impl LoadCtl {
    /// Канал создаётся один раз при построении `AppCore` (ADR-16).
    pub fn new() -> LoadCtl {
        let (tx, rx) = mpsc::channel();
        LoadCtl { state: LoadState::Idle, gen: LoadGen::default(), sort: None, tx, rx }
    }
}

impl AppCore {
    /// Запускает стартовую загрузку `playlist.m3u` (ADR-16, §6.12, ТЗ-21,
    /// ТЗ-47): новое поколение, задание уходит в поток `apap-playlist` через
    /// `deps.loader`. Без `deps` (мост) — без эффекта.
    pub fn start_startup_load(&mut self) {
        let Some(deps) = &self.deps else { return };
        let gen = self.load.gen.next();
        self.load.gen = gen;
        self.load.state = LoadState::Startup { gen };
        self.load.sort = self.state.sort();
        let job = LoadJob {
            gen,
            source: LoadSource::Startup(deps.paths.playlist.clone()),
            sort: self.load.sort,
        };
        deps.loader.start(job, deps.reader.clone(), self.load.tx.clone());
    }

    /// Текущее состояние загрузки плейлиста (§6.12).
    pub fn load_state(&self) -> &LoadState {
        &self.load.state
    }

    /// Разбирает результаты загрузчика плейлиста, накопленные в канале
    /// (ADR-16, §6.12). Итог чужого поколения или итог при отсутствии
    /// активной загрузки отбрасывается молча (И-Т9). Итог ТЕКУЩЕЙ загрузки
    /// завершает её (`state` -> `Idle`) и возвращается отчётом.
    pub fn poll_load(&mut self) -> Option<LoadReport> {
        let mut report = None;
        while let Ok(outcome) = self.load.rx.try_recv() {
            let LoadState::Startup { gen } = self.load.state else { continue };
            if outcome.gen() != gen {
                continue;
            }
            report = Some(self.on_load_outcome(outcome));
            self.load.state = LoadState::Idle;
        }
        report
    }

    /// Обрабатывает итог ТЕКУЩЕЙ стартовой загрузки по случаям ОВ-8/ТЗ-21
    /// (ADR-16, §6.12).
    fn on_load_outcome(&mut self, outcome: LoadOutcome) -> LoadReport {
        match outcome {
            // Случай 1 (ТЗ-21): файла нет — плейлист остаётся пустым, без
            // сообщения.
            LoadOutcome::StartupAbsent { .. } => LoadReport { applied: None, message: None },
            // Случай 2 (ТЗ-21): файл повреждён — в карантин через писателя;
            // сообщение придёт позже ответом `Quarantined`/`QuarantineFailed`
            // (обрабатывается в `AppCore::tick`).
            LoadOutcome::StartupCorrupt { reason, .. } => {
                if let Some(deps) = &self.deps {
                    deps.journal.record(JournalRecord::PlaylistCorrupt { reason });
                    deps.writer.send(WriterCmd::QuarantinePlaylist);
                }
                LoadReport { applied: None, message: None }
            }
            // Случай 3 (ТЗ-21): файл не прочитан — запись `playlist.m3u`
            // запрещена до перезапуска.
            LoadOutcome::ReadFailed { err, .. } => {
                self.forbid_playlist();
                let message = playlist_unreadable_message(&err.path, &err.os_text);
                if let Some(deps) = &self.deps {
                    deps.journal.record(JournalRecord::ReadFailed { file: WorkFile::Playlist, err });
                }
                LoadReport { applied: None, message: Some(message) }
            }
            LoadOutcome::Loaded { rows, visible, .. } => {
                self.playlist_replace(rows, visible, self.load.sort);
                LoadReport { applied: Some(LoadApplied::Replaced), message: None }
            }
        }
    }
}

/// Случай 2 ОВ-8/ТЗ-21: повреждённый `playlist.m3u` успешно перемещён в
/// карантин писателем (ответ `WriterReply::Quarantined`) — плейлист пуст.
pub(super) fn quarantined_message(path: &Path) -> Message {
    Message {
        level: MessageLevel::Warning,
        title: "Плейлист повреждён".into(),
        body: format!(
            "Файл плейлиста повреждён и не может быть прочитан. Копия повреждённого \
             файла сохранена: {}. Плейлист пуст.",
            path.display()
        )
        .into(),
        buttons: MessageButtons::Ok,
    }
}

/// Случай 3 ОВ-8/ТЗ-21: плейлист недоступен в этом сеансе — файл не
/// прочитан (`LoadOutcome::ReadFailed`) или копия в карантин не записалась
/// (`WriterReply::QuarantineFailed`); запись `playlist.m3u` запрещена до
/// перезапуска (`forbid_playlist`).
pub(super) fn playlist_unreadable_message(path: &Path, os_text: &str) -> Message {
    Message {
        level: MessageLevel::Error,
        title: "Плейлист не прочитан".into(),
        body: format!(
            "Файл плейлиста {} не удалось прочитать ({}). Плейлист пуст. Файл \
             playlist.m3u не будет записан в этом сеансе — следующий запуск повторит \
             попытку.",
            path.display(),
            os_text
        )
        .into(),
        buttons: MessageButtons::Ok,
    }
}
