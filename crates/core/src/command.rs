use crate::panel::SortKey;

/// Every user action goes through a `Command`. Key bindings, the function-key
/// bar, menus and (later) a command palette all map onto these, the same idea
/// as Total Commander's `cm_*` internal commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    CursorUp,
    CursorDown,
    PageUp,
    PageDown,
    CursorHome,
    CursorEnd,
    /// Scroll the cursor by n rows (mouse wheel).
    CursorBy(i32),
    Open,
    GoParent,
    GoRoot,
    /// Previous / next folder in the panel's history (Alt+Left/Right).
    HistoryBack,
    HistoryForward,
    /// Directory hotlist popup (Ctrl+D).
    Hotlist,
    SwitchPanel,
    /// Toggle selection on the cursor entry, optionally moving down.
    ToggleSelect { advance: bool },
    /// Compute the size of every folder in the panel (Alt+Shift+Enter).
    CountDirSizes,
    /// Select the entries that differ from the other panel (Shift+F2).
    CompareDirs,
    /// Compare-folders options, then compare (Ctrl+Shift+F2).
    CompareDirsOptions,
    /// Side-by-side diff of two files (Alt+F2).
    CompareFiles,
    SelectAll,
    DeselectAll,
    Refresh,
    ToggleHidden,
    /// Show only entries matching a typed filter (Ctrl+S).
    QuickFilter,
    SortBy(SortKey),
    /// Make the other panel show the same folder.
    SyncOtherPanel,
    View,
    Edit,
    /// Ask for a file name, create it if missing and edit it (Shift+F4).
    EditNew,
    Copy,
    Move,
    /// Rename the cursor entry in place (Shift+F6).
    Rename,
    /// Rename the selected files by mask (Ctrl+M).
    MultiRename,
    MakeDir,
    /// Delete to the recycle bin.
    Delete,
    /// Delete permanently (Shift+F8 / Shift+Del).
    DeletePermanent,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    /// Open the folder under the cursor in a new tab.
    OpenInNewTab,
    /// Open `config.toml` in the editor (Ctrl+,).
    EditConfig,
    Quit,
}

impl Command {
    /// Stable identifier, used for config files and the command palette.
    pub fn id(self) -> &'static str {
        use Command::*;
        match self {
            CursorUp => "cm_CursorUp",
            CursorDown => "cm_CursorDown",
            PageUp => "cm_PageUp",
            PageDown => "cm_PageDown",
            CursorHome => "cm_GoToFirstEntry",
            CursorEnd => "cm_GoToLastEntry",
            CursorBy(_) => "cm_Scroll",
            Open => "cm_Return",
            GoParent => "cm_GoToParent",
            GoRoot => "cm_GoToRoot",
            HistoryBack => "cm_PrevDir",
            HistoryForward => "cm_NextDir",
            Hotlist => "cm_DirectoryHotlist",
            SwitchPanel => "cm_FocusSwap",
            ToggleSelect { advance: true } => "cm_Select",
            ToggleSelect { advance: false } => "cm_SelectNoMove",
            CountDirSizes => "cm_CountDirContent",
            CompareDirs => "cm_CompareDirs",
            CompareDirsOptions => "cm_CompareDirsOptions",
            CompareFiles => "cm_CompareFilesByContent",
            SelectAll => "cm_SelectAll",
            DeselectAll => "cm_ClearAll",
            Refresh => "cm_RereadSource",
            ToggleHidden => "cm_SwitchHidSys",
            QuickFilter => "cm_QuickFilter",
            SortBy(SortKey::Name) => "cm_SortByName",
            SortBy(SortKey::Ext) => "cm_SortByExt",
            SortBy(SortKey::Size) => "cm_SortBySize",
            SortBy(SortKey::Modified) => "cm_SortByDateTime",
            SyncOtherPanel => "cm_TransferPath",
            View => "cm_View",
            Edit => "cm_Edit",
            EditNew => "cm_EditNewFile",
            Copy => "cm_Copy",
            Move => "cm_RenMov",
            Rename => "cm_RenameOnly",
            MultiRename => "cm_MultiRenameFiles",
            MakeDir => "cm_MkDir",
            Delete => "cm_Delete",
            DeletePermanent => "cm_DeletePermanent",
            NewTab => "cm_OpenNewTab",
            CloseTab => "cm_CloseCurrentTab",
            NextTab => "cm_SwitchToNextTab",
            PrevTab => "cm_SwitchToPreviousTab",
            OpenInNewTab => "cm_OpenDirInNewTab",
            EditConfig => "cm_EditConfig",
            Quit => "cm_Exit",
        }
    }

    /// The command with this id (case-insensitive), for config files.
    pub fn from_id(id: &str) -> Option<Command> {
        Self::ALL.iter().copied().find(|c| c.id().eq_ignore_ascii_case(id))
    }

    /// Every command a key can be bound to (all but the mouse-only ones).
    pub const ALL: &'static [Command] = {
        use Command::*;
        &[
            CursorUp,
            CursorDown,
            PageUp,
            PageDown,
            CursorHome,
            CursorEnd,
            Open,
            GoParent,
            GoRoot,
            HistoryBack,
            HistoryForward,
            Hotlist,
            SwitchPanel,
            ToggleSelect { advance: true },
            ToggleSelect { advance: false },
            CountDirSizes,
            CompareDirs,
            CompareDirsOptions,
            CompareFiles,
            SelectAll,
            DeselectAll,
            Refresh,
            ToggleHidden,
            QuickFilter,
            SortBy(SortKey::Name),
            SortBy(SortKey::Ext),
            SortBy(SortKey::Size),
            SortBy(SortKey::Modified),
            SyncOtherPanel,
            View,
            Edit,
            EditNew,
            Copy,
            Move,
            Rename,
            MultiRename,
            MakeDir,
            Delete,
            DeletePermanent,
            NewTab,
            CloseTab,
            NextTab,
            PrevTab,
            OpenInNewTab,
            EditConfig,
            Quit,
        ]
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip() {
        let mut ids: Vec<_> = Command::ALL.iter().map(|c| c.id()).collect();
        for &c in Command::ALL {
            assert_eq!(Command::from_id(c.id()), Some(c));
        }
        assert_eq!(Command::from_id("CM_COPY"), Some(Command::Copy));
        assert_eq!(Command::from_id("cm_Nope"), None);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Command::ALL.len());
    }
}
