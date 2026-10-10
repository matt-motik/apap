//! Запуск загрузки плейлиста при старте и по команде (ADR-16, §6.12,
//! ТЗ-21, ТЗ-47, ТЗ-13 б, ТЗ-48).
//!
//! Стартовая (`Startup`) и командная (`Command`, «Загрузить плейлист» —
//! выбор файла пользователем) загрузки идут в потоке `apap-playlist`
//! (`PlaylistLoader`, `crate::playlist::load`): задание несёт поколение
//! `LoadGen`, итог приходит через канал `LoadOutcome`. Итог чужого
//! (устаревшего) поколения или итог при отсутствии активной загрузки
//! отбрасывается молча (И-Т9) — он пришёл от задания, результат которого
//! уже не ждут; это покрывает и случай, когда команда запускает новую
//! загрузку поверх ещё активной (стартовой или командной) — старый итог
//! просто отбрасывается.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use super::messages::{Message, MessageButtons, MessageLevel};
use super::AppCore;
use crate::journal::JournalRecord;
use crate::persist::writer::WriterCmd;
use crate::persist::WorkFile;
use crate::playlist::load::{LoadGen, LoadJob, LoadOutcome, LoadSource};
use crate::playlist::model::{SortKey, TrackId};

/// Состояние загрузки плейлиста (ADR-16, §6.12): стартовая загрузка или
/// командная загрузка выбранного пользователем файла (ТЗ-13 б, ТЗ-48).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum LoadState {
    #[default]
    Idle,
    Startup { gen: LoadGen },
    Command { gen: LoadGen, path: PathBuf },
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
    /// Стартовая загрузка: плейлист заменён, воспроизведение не трогаем.
    Replaced,
    /// Командная загрузка (ТЗ-13 б, ТЗ-48): плейлист заменён, и его нужно
    /// немедленно заиграть с трека `first` — остановку прежнего трека и
    /// открытие нового (с переводом поколения `Open`) выполняет
    /// `MusicApp` через аудио-фасад, не `core`. `None` — новый плейлист
    /// пуст, воспроизведение останавливается.
    PlayNow { first: Option<TrackId> },
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

impl Default for LoadCtl {
    fn default() -> LoadCtl {
        LoadCtl::new()
    }
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

    /// Запускает загрузку плейлиста, выбранного командой «Загрузить
    /// плейлист» (ADR-16, §6.12, ТЗ-13 б, ТЗ-48): новое поколение заменяет
    /// активную загрузку (стартовую или командную) — её итог придёт с
    /// устаревшим поколением и будет отброшен (И-Т9). Без `deps` (мост) —
    /// без эффекта.
    pub fn start_command_load(&mut self, path: PathBuf) {
        let Some(deps) = &self.deps else { return };
        let gen = self.load.gen.next();
        self.load.gen = gen;
        self.load.sort = self.state.sort();
        self.load.state = LoadState::Command { gen, path: path.clone() };
        let job = LoadJob { gen, source: LoadSource::Command(path), sort: self.load.sort };
        deps.loader.start(job, deps.reader.clone(), self.load.tx.clone());
    }

    /// Текущее состояние загрузки плейлиста (§6.12).
    pub fn load_state(&self) -> &LoadState {
        &self.load.state
    }

    /// Разбирает результаты загрузчика плейлиста, накопленные в канале
    /// (ADR-16, §6.12). Итог чужого поколения или итог при отсутствии
    /// активной загрузки отбрасывается молча (И-Т9). Итог ТЕКУЩЕЙ загрузки
    /// завершает её (`state` -> `Idle`) и возвращается отчётом — по
    /// случаям стартовой загрузки (ОВ-8/ТЗ-21) или по случаям командной
    /// загрузки (ТЗ-13 б, ТЗ-48), в зависимости от того, какая из них
    /// активна.
    pub fn poll_load(&mut self) -> Option<LoadReport> {
        let mut report = None;
        while let Ok(outcome) = self.load.rx.try_recv() {
            let gen = match &self.load.state {
                LoadState::Idle => continue,
                LoadState::Startup { gen } | LoadState::Command { gen, .. } => *gen,
            };
            if outcome.gen() != gen {
                continue;
            }
            let is_command = matches!(self.load.state, LoadState::Command { .. });
            report = Some(if is_command {
                self.on_command_outcome(outcome)
            } else {
                self.on_load_outcome(outcome)
            });
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

    /// Обрабатывает итог ТЕКУЩЕЙ командной загрузки (ТЗ-13 б, ТЗ-48,
    /// ADR-16, §6.12): файл выбран командой «Загрузить плейлист», загрузчик
    /// (`run_load`, `crate::playlist::load`) для источника `Command` отдаёт
    /// только `Loaded` или `ReadFailed`.
    fn on_command_outcome(&mut self, outcome: LoadOutcome) -> LoadReport {
        match outcome {
            // Файл не прочитан: плейлист, флаг и воспроизведение не
            // меняются — в отличие от стартовой загрузки, запись
            // `playlist.m3u` не запрещается и в журнал ничего не пишется
            // (это не ошибка чтения рабочего файла, а отказ пользовательской
            // команды).
            LoadOutcome::ReadFailed { err, .. } => {
                let message = command_unreadable_message(&err.path, &err.os_text);
                LoadReport { applied: None, message: Some(message) }
            }
            // Файл прочитан: плейлист заменён с флагом «изменён» (ТЗ-13 б),
            // и его нужно заиграть немедленно — первый трек с учётом
            // Shuffle; остановку прежнего трека и открытие нового выполняет
            // `MusicApp`.
            LoadOutcome::Loaded { rows, visible, .. } => {
                self.playlist_replace(rows, visible, self.load.sort);
                self.playlist_changed();
                let first = if self.state.shuffle() {
                    self.shuffle_first()
                } else {
                    self.playlist().first_visible()
                };
                LoadReport { applied: Some(LoadApplied::PlayNow { first }), message: None }
            }
            // `Command` не порождает случаи стартовой загрузки (`run_load`,
            // `crate::playlist::load`) — на практике недостижимо, но матч
            // обязан быть исчерпывающим без паники.
            LoadOutcome::StartupAbsent { .. } | LoadOutcome::StartupCorrupt { .. } => {
                LoadReport { applied: None, message: None }
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

/// Командная загрузка (ТЗ-13 б, ТЗ-48, §6.12): файл, выбранный в диалоге
/// «Загрузить плейлист», не удалось прочитать — текущий плейлист,
/// флаг «изменён» и воспроизведение не меняются.
pub(super) fn command_unreadable_message(path: &Path, os_text: &str) -> Message {
    Message {
        level: MessageLevel::Error,
        title: "Не удалось загрузить плейлист".into(),
        body: format!(
            "Файл {} не удалось прочитать ({}). Текущий плейлист не изменён.",
            path.display(),
            os_text
        )
        .into(),
        buttons: MessageButtons::Ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testing::Harness;
    use crate::platform::fs::{ReadError, ReadErrorClass};
    use crate::playlist::compare::CompareKeys;
    use crate::playlist::Track;

    fn row(name: &str) -> (Track, CompareKeys) {
        let t = Track { path: PathBuf::from(format!("/music/{name}.flac")), title: name.into(), ..Track::default() };
        let keys = CompareKeys::from_track(&t);
        (t, keys)
    }

    fn boot() -> (Harness, AppCore) {
        let h = Harness::new();
        h.put_settings(b"");
        h.put_state(b"");
        let core = h.boot();
        (h, core)
    }

    /// Командная загрузка завершена (ТЗ-13 б, ТЗ-48): плейлист заменён
    /// (без явного флага «изменён» здесь — он взводится `playlist_changed`
    /// внутри `on_command_outcome`), отчёт — `PlayNow` с первым видимым
    /// треком.
    #[test]
    fn command_load_loaded_replaces_playlist_and_reports_play_now() {
        let (h, mut core) = boot();
        core.start_command_load(PathBuf::from("/music/chosen.m3u"));
        let gen = h.loader.jobs()[0].gen;
        h.loader.finish(LoadOutcome::Loaded { gen, rows: vec![row("a"), row("b")], visible: vec![0, 1] });

        let report = core.poll_load().expect("отчёт загрузки");
        let first_id = core.playlist().visible()[0];
        assert_eq!(report.applied, Some(LoadApplied::PlayNow { first: Some(first_id) }));
        assert!(report.message.is_none());
        assert!(core.load_state().is_idle());
    }

    /// Файл, выбранный командой, не прочитан (ТЗ-13 б, ТЗ-48): плейлист не
    /// меняется, запись `playlist.m3u` не запрещается, в журнал ничего не
    /// пишется (в отличие от стартовой загрузки).
    #[test]
    fn command_load_read_failed_keeps_playlist_and_reports_message() {
        let (h, mut core) = boot();
        core.start_command_load(PathBuf::from("/music/missing.m3u"));
        let gen = h.loader.jobs()[0].gen;
        let journal_before = h.journal().len();
        let err = ReadError {
            class: ReadErrorClass::NoAccess,
            os_code: None,
            os_text: "отказано".into(),
            path: PathBuf::from("/music/missing.m3u"),
        };
        h.loader.finish(LoadOutcome::ReadFailed { gen, err });

        let report = core.poll_load().expect("отчёт загрузки");
        assert!(report.applied.is_none());
        let message = report.message.expect("сообщение об ошибке");
        assert_eq!(message.title.as_ref(), "Не удалось загрузить плейлист");
        assert!(core.playlist().visible().is_empty());
        assert_eq!(h.journal().len(), journal_before);
    }

    /// Новая командная загрузка поверх активной (И-Т9): итог устаревшего
    /// поколения отбрасывается молча, применяется только итог последнего.
    #[test]
    fn command_load_supersedes_previous_load_stale_outcome_dropped() {
        let (h, mut core) = boot();
        core.start_command_load(PathBuf::from("/music/first.m3u"));
        let stale_gen = h.loader.jobs()[0].gen;

        core.start_command_load(PathBuf::from("/music/second.m3u"));
        let fresh_gen = h.loader.jobs().last().expect("второе задание").gen;
        assert_ne!(stale_gen, fresh_gen);

        h.loader.finish(LoadOutcome::Loaded { gen: stale_gen, rows: vec![row("stale")], visible: vec![0] });
        assert!(core.poll_load().is_none());

        h.loader.finish(LoadOutcome::Loaded { gen: fresh_gen, rows: vec![row("fresh")], visible: vec![0] });
        let report = core.poll_load().expect("отчёт загрузки");
        assert!(matches!(report.applied, Some(LoadApplied::PlayNow { .. })));
    }
}
