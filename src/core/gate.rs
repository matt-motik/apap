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

#[cfg(test)]
mod tests {
    use super::*;

    /// Исчерпывающий перечень `MainCmd`, включая один вариант `SortBy`.
    const ALL_CMDS: &[MainCmd] = &[
        MainCmd::AddFiles,
        MainCmd::AddFolder,
        MainCmd::LoadPlaylist,
        MainCmd::SavePlaylist,
        MainCmd::RemoveCurrent,
        MainCmd::ClearPlaylist,
        MainCmd::SortBy(ColumnId::Title),
        MainCmd::DropFiles,
        MainCmd::Next,
        MainCmd::Prev,
        MainCmd::PlayPause,
        MainCmd::Stop,
        MainCmd::Seek,
        MainCmd::PlayRow,
        MainCmd::Volume,
        MainCmd::ToggleMute,
        MainCmd::SwitchMode,
        MainCmd::Repeat,
        MainCmd::Shuffle,
        MainCmd::VizCycle,
        MainCmd::ColumnResize,
        MainCmd::OpenSettings,
    ];

    /// Список команд, недоступных по ТЗ-48 во время загрузки плейлиста.
    const LOAD_BLOCKED_CMDS: &[MainCmd] = &[
        MainCmd::AddFiles,
        MainCmd::AddFolder,
        MainCmd::LoadPlaylist,
        MainCmd::RemoveCurrent,
        MainCmd::ClearPlaylist,
        MainCmd::SavePlaylist,
        MainCmd::SortBy(ColumnId::Title),
        MainCmd::DropFiles,
    ];

    #[test]
    fn menu_inactive_while_dialog_open_gate() {
        let mut gate = UiGate::default();
        gate.block(BlockReason::Dialog);
        for &cmd in ALL_CMDS {
            assert!(!gate.allows(cmd), "{cmd:?} must be denied while dialog is open");
        }
        assert!(gate.window_blocked());
    }

    #[test]
    fn menu_inactive_while_message_open_gate() {
        let mut gate = UiGate::default();
        gate.block(BlockReason::Message);
        for &cmd in ALL_CMDS {
            assert!(!gate.allows(cmd), "{cmd:?} must be denied while message window is open");
        }
        assert!(gate.window_blocked());
    }

    #[test]
    fn menu_inactive_while_file_picker_open_gate() {
        let mut gate = UiGate::default();
        gate.block(BlockReason::FilePicker);
        for &cmd in ALL_CMDS {
            assert!(!gate.allows(cmd), "{cmd:?} must be denied while file picker is open");
        }
        assert!(gate.window_blocked());
    }

    #[test]
    fn unblocking_one_of_two_reasons_keeps_blocked() {
        let mut gate = UiGate::default();
        gate.block(BlockReason::Dialog);
        gate.block(BlockReason::Message);
        gate.unblock(BlockReason::Dialog);
        assert!(gate.window_blocked());
        assert!(!gate.allows(MainCmd::PlayPause));
    }

    #[test]
    fn unblocking_all_reasons_allows_everything() {
        let mut gate = UiGate::default();
        gate.block(BlockReason::Dialog);
        gate.block(BlockReason::Message);
        gate.block(BlockReason::FilePicker);
        gate.unblock(BlockReason::Dialog);
        gate.unblock(BlockReason::Message);
        gate.unblock(BlockReason::FilePicker);
        assert!(!gate.window_blocked());
        for &cmd in ALL_CMDS {
            assert!(gate.allows(cmd), "{cmd:?} must be allowed once all reasons are cleared");
        }
    }

    #[test]
    fn loading_startup_denies_tz48_list_only() {
        let mut gate = UiGate::default();
        gate.set_loading(Some(LoadKind::Startup));
        for &cmd in LOAD_BLOCKED_CMDS {
            assert!(!gate.allows(cmd), "{cmd:?} must be denied during startup load (ТЗ-48)");
        }
        for cmd in [
            MainCmd::Next,
            MainCmd::Prev,
            MainCmd::PlayPause,
            MainCmd::Stop,
            MainCmd::Seek,
            MainCmd::Volume,
            MainCmd::ToggleMute,
        ] {
            assert!(gate.allows(cmd), "{cmd:?} must stay allowed during startup load");
        }
    }

    #[test]
    fn loading_command_additionally_denies_next_prev() {
        let mut gate = UiGate::default();
        gate.set_loading(Some(LoadKind::Command));
        for &cmd in LOAD_BLOCKED_CMDS {
            assert!(!gate.allows(cmd), "{cmd:?} must be denied during command load (ТЗ-48)");
        }
        assert!(!gate.allows(MainCmd::Next));
        assert!(!gate.allows(MainCmd::Prev));
        for cmd in [
            MainCmd::PlayPause,
            MainCmd::Stop,
            MainCmd::Seek,
            MainCmd::Volume,
            MainCmd::ToggleMute,
        ] {
            assert!(gate.allows(cmd), "{cmd:?} must stay allowed during command load");
        }
    }

    #[test]
    fn window_blocked_only_with_block_reason() {
        let mut gate = UiGate::default();
        assert!(!gate.window_blocked());
        gate.set_loading(Some(LoadKind::Command));
        assert!(!gate.window_blocked(), "loading alone must not block the window");
        gate.block(BlockReason::Message);
        assert!(gate.window_blocked());
    }
}
