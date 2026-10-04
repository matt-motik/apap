//! Шлюз недоступности главного окна (ADR-12, ТЗ-23, ТЗ-48, И-Р8, §2.11).

use crate::settings::ColumnId;

/// Причина недоступности главного окна (ADR-12).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockReason {
    Dialog,
    Message,
    FilePicker,
}

/// Вид идущей загрузки плейлиста (ТЗ-48).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadKind {
    Startup,
    Command,
}

/// Команда главного окна: меню, клавиши, элементы окна, перенос файлов (§2.11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MainCmd {
    AddFiles,
    AddFolder,
    LoadPlaylist,
    SavePlaylist,
    RemoveCurrent,
    ClearPlaylist,
    SortBy(ColumnId),
    DropFiles,
    Next,
    Prev,
    PlayPause,
    Stop,
    Seek,
    PlayRow,
    Volume,
    ToggleMute,
    SwitchMode,
    Repeat,
    Shuffle,
    VizCycle,
    ColumnResize,
    OpenSettings,
}

/// Шлюз главного окна (UI-поток, ADR-12). Хранит множество причин блокировки
/// окна {диалог настроек, окно сообщения, выбор файла} и отдельно признак
/// «идёт загрузка плейлиста» (ТЗ-48).
#[derive(Default)]
pub struct UiGate {
    blocked: [bool; 3],
    loading: Option<LoadKind>,
}

const fn reason_index(r: BlockReason) -> usize {
    match r {
        BlockReason::Dialog => 0,
        BlockReason::Message => 1,
        BlockReason::FilePicker => 2,
    }
}

/// Команды, недоступные во время загрузки плейлиста (ТЗ-48).
fn blocked_during_load(cmd: MainCmd) -> bool {
    matches!(
        cmd,
        MainCmd::AddFiles
            | MainCmd::AddFolder
            | MainCmd::LoadPlaylist
            | MainCmd::RemoveCurrent
            | MainCmd::ClearPlaylist
            | MainCmd::SavePlaylist
            | MainCmd::SortBy(_)
            | MainCmd::DropFiles
    )
}

impl UiGate {
    /// Взводит причину блокировки окна.
    pub fn block(&mut self, r: BlockReason) {
        self.blocked[reason_index(r)] = true;
    }

    /// Снимает причину блокировки окна.
    pub fn unblock(&mut self, r: BlockReason) {
        self.blocked[reason_index(r)] = false;
    }

    /// Устанавливает/снимает признак идущей загрузки плейлиста.
    pub fn set_loading(&mut self, k: Option<LoadKind>) {
        self.loading = k;
    }

    /// Текущий вид загрузки плейлиста, если она идёт.
    pub fn loading(&self) -> Option<LoadKind> {
        self.loading
    }

    /// Любая причина блокировки окна → `false` для всех `MainCmd`; во время
    /// загрузки плейлиста → `false` для перечня ТЗ-48 (+ `Next`, `Prev` при
    /// `LoadKind::Command`).
    pub fn allows(&self, cmd: MainCmd) -> bool {
        if self.window_blocked() {
            return false;
        }
        match self.loading {
            None => true,
            Some(kind) => {
                if blocked_during_load(cmd) {
                    return false;
                }
                if kind == LoadKind::Command && matches!(cmd, MainCmd::Next | MainCmd::Prev) {
                    return false;
                }
                true
            }
        }
    }

    /// `true`, если взведена хотя бы одна причина блокировки окна.
    pub fn window_blocked(&self) -> bool {
        self.blocked.iter().any(|b| *b)
    }
}
