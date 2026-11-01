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
    /// Find files by name and contents (Alt+F7).
    FindFiles,
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
    /// Searchable list of keys and features (F1).
    Help,
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
            FindFiles => "cm_SearchFor",
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
            Help => "cm_HelpIndex",
            Quit => "cm_Exit",
        }
    }

    /// What the command does, for the F1 help list. A new command must add
    /// one here (the match is exhaustive).
    pub fn description(self) -> &'static str {
        use Command::*;
        match self {
            CursorUp => "Cursor up",
            CursorDown => "Cursor down",
            PageUp => "Cursor one page up",
            PageDown => "Cursor one page down",
            CursorHome => "Cursor to the first entry",
            CursorEnd => "Cursor to the last entry",
            CursorBy(_) => "Scroll the cursor (mouse wheel)",
            Open => "Open the folder, archive (zip, 7z: browse like a folder) or file under the cursor",
            GoParent => "Go to the parent folder",
            GoRoot => "Go to the root of the drive",
            HistoryBack => "Back to the previous folder",
            HistoryForward => "Forward to the next folder",
            Hotlist => "Directory hotlist: favourite folders",
            SwitchPanel => "Switch to the other panel",
            ToggleSelect { advance: true } => "Select / deselect the entry and move down",
            ToggleSelect { advance: false } => "Select / deselect the entry",
            CountDirSizes => "Compute the size of every folder in the panel",
            CompareDirs => "Compare folders: select the entries that differ from the other panel",
            CompareDirsOptions => "Compare folders with options (by content, ignore dates, subfolders)",
            CompareFiles => "Compare two files by content, side by side",
            FindFiles => "Find files by name and contents",
            SelectAll => "Select all entries",
            DeselectAll => "Clear the selection",
            Refresh => "Reread the folder",
            ToggleHidden => "Show / hide hidden and system files",
            QuickFilter => "Quick filter: show only entries matching typed text",
            SortBy(SortKey::Name) => "Sort by name",
            SortBy(SortKey::Ext) => "Sort by extension",
            SortBy(SortKey::Size) => "Sort by size",
            SortBy(SortKey::Modified) => "Sort by date",
            SyncOtherPanel => "Show this folder in the other panel too",
            View => "View the file (text, hex, images)",
            Edit => "Edit the file in the configured editor",
            EditNew => "Create a new file and edit it",
            Copy => "Copy to the other panel",
            Move => "Move / rename to the other panel",
            Rename => "Rename in place",
            MultiRename => "Multi-rename the selected files by mask",
            MakeDir => "Create a folder",
            Delete => "Delete to the recycle bin",
            DeletePermanent => "Delete permanently",
            NewTab => "New tab",
            CloseTab => "Close the tab",
            NextTab => "Next tab",
            PrevTab => "Previous tab",
            OpenInNewTab => "Open the folder under the cursor in a new tab",
            EditConfig => "Edit config.toml (editor, [keys], [colors])",
            Help => "This help: all keys and features",
            Quit => "Exit",
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
            FindFiles,
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
            Help,
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

    #[test]
    fn every_command_has_a_description() {
        for &c in Command::ALL {
            assert!(!c.description().trim().is_empty(), "{}", c.id());
        }
        assert!(!Command::CursorBy(1).description().is_empty());
    }
}
