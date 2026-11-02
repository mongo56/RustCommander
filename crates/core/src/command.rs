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
    /// Previous / next entry in the thumbnail view (Left / Right).
    CursorLeft,
    CursorRight,
    /// Scroll the cursor by n rows (mouse wheel).
    CursorBy(i32),
    Open,
    GoParent,
    GoRoot,
    /// Previous / next folder in the panel's history (Alt+Left/Right).
    HistoryBack,
    HistoryForward,
    /// The panel's folder history as a popup list (Alt+Down).
    DirHistory,
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
    /// Saved server connections; connect one in this panel (Ctrl+F).
    Connect,
    /// Close the connection of the panel's server (Ctrl+Shift+F).
    Disconnect,
    SelectAll,
    DeselectAll,
    Refresh,
    ToggleHidden,
    /// Thumbnail view on / off for the tab (Ctrl+Shift+F5).
    Thumbnails,
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
    /// Put the selected files on the system clipboard (Ctrl+C / Ctrl+X),
    /// for pasting here or in Explorer.
    CopyToClipboard,
    CutToClipboard,
    /// Copy or move the files on the clipboard into the panel (Ctrl+V).
    PasteFromClipboard,
    /// Rename the cursor entry in place (Shift+F6).
    Rename,
    /// Rename the selected files by mask (Ctrl+M).
    MultiRename,
    MakeDir,
    /// Pack the selected files into a zip in the other panel (Alt+F5).
    Pack,
    /// Unpack the selected archives into the other panel (Alt+F9).
    Unpack,
    /// Open a terminal in the active panel's folder (F9).
    Terminal,
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
    /// Settings view: preferences, key bindings, colours (F10).
    Configuration,
    /// Popup to find a command by name, id or key and run it (Ctrl+Space).
    CommandPalette,
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
            CursorLeft => "cm_CursorLeft",
            CursorRight => "cm_CursorRight",
            CursorBy(_) => "cm_Scroll",
            Open => "cm_Return",
            GoParent => "cm_GoToParent",
            GoRoot => "cm_GoToRoot",
            HistoryBack => "cm_PrevDir",
            HistoryForward => "cm_NextDir",
            DirHistory => "cm_DirectoryHistory",
            Hotlist => "cm_DirectoryHotlist",
            SwitchPanel => "cm_FocusSwap",
            ToggleSelect { advance: true } => "cm_Select",
            ToggleSelect { advance: false } => "cm_SelectNoMove",
            CountDirSizes => "cm_CountDirContent",
            CompareDirs => "cm_CompareDirs",
            CompareDirsOptions => "cm_CompareDirsOptions",
            CompareFiles => "cm_CompareFilesByContent",
            FindFiles => "cm_SearchFor",
            Connect => "cm_FtpConnect",
            Disconnect => "cm_FtpDisconnect",
            SelectAll => "cm_SelectAll",
            DeselectAll => "cm_ClearAll",
            Refresh => "cm_RereadSource",
            ToggleHidden => "cm_SwitchHidSys",
            Thumbnails => "cm_SrcThumbs",
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
            CopyToClipboard => "cm_CopyToClipboard",
            CutToClipboard => "cm_CutToClipboard",
            PasteFromClipboard => "cm_PasteFromClipboard",
            Rename => "cm_RenameOnly",
            MultiRename => "cm_MultiRenameFiles",
            MakeDir => "cm_MkDir",
            Pack => "cm_PackFiles",
            Unpack => "cm_UnpackFiles",
            Terminal => "cm_ExecuteDOS",
            Delete => "cm_Delete",
            DeletePermanent => "cm_DeletePermanent",
            NewTab => "cm_OpenNewTab",
            CloseTab => "cm_CloseCurrentTab",
            NextTab => "cm_SwitchToNextTab",
            PrevTab => "cm_SwitchToPreviousTab",
            OpenInNewTab => "cm_OpenDirInNewTab",
            EditConfig => "cm_EditConfig",
            Configuration => "cm_Configuration",
            CommandPalette => "cm_CommandPalette",
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
            CursorLeft => "Cursor to the previous entry (thumbnail view)",
            CursorRight => "Cursor to the next entry (thumbnail view)",
            CursorBy(_) => "Scroll the cursor (mouse wheel)",
            Open => "Open the folder, archive (zip, 7z, tar, gz: browse like a folder) or file under the cursor",
            GoParent => "Go to the parent folder",
            GoRoot => "Go to the root of the drive",
            HistoryBack => "Back to the previous folder",
            HistoryForward => "Forward to the next folder",
            DirHistory => "Folder history list: jump back or forward to any folder visited in this tab",
            Hotlist => "Directory hotlist: favourite folders, with submenus",
            SwitchPanel => "Switch to the other panel",
            ToggleSelect { advance: true } => "Select / deselect the entry and move down",
            ToggleSelect { advance: false } => "Select / deselect the entry",
            CountDirSizes => "Compute the size of every folder in the panel",
            CompareDirs => "Compare folders: select the entries that differ from the other panel",
            CompareDirsOptions => "Compare folders with options (by content, ignore dates, subfolders)",
            CompareFiles => "Compare two files by content, side by side",
            FindFiles => "Find files by name and contents",
            Connect => "Connect to a server (SFTP, FTP, FTPS): saved connections, new, edit, delete",
            Disconnect => "Close the connection of the server shown in the panel",
            SelectAll => "Select all entries",
            DeselectAll => "Clear the selection",
            Refresh => "Reread the folder",
            ToggleHidden => "Show / hide hidden and system files",
            Thumbnails => "Thumbnail view on / off: pictures, videos and documents as a grid of previews",
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
            CopyToClipboard => "Copy the files to the clipboard",
            CutToClipboard => "Cut the files to the clipboard (paste moves them)",
            PasteFromClipboard => "Paste files from the clipboard into this panel",
            Rename => "Rename in place",
            MultiRename => "Multi-rename the selected files by mask",
            MakeDir => "Create a folder",
            Pack => "Pack the selected files into a zip archive in the other panel",
            Unpack => "Unpack the selected archives (zip, 7z, tar, gz) into the other panel",
            Terminal => "Open a terminal in this folder (the \"terminal\" setting)",
            Delete => "Delete to the recycle bin",
            DeletePermanent => "Delete permanently",
            NewTab => "New tab",
            CloseTab => "Close the tab",
            NextTab => "Next tab",
            PrevTab => "Previous tab",
            OpenInNewTab => "Open the folder under the cursor in a new tab",
            EditConfig => "Edit config.toml (editor, [keys], [colors])",
            Configuration => "Settings: preferences, key bindings, colour themes",
            CommandPalette => "Command palette: find a command by name, id or key and run it",
            Help => "This help: all keys and features",
            Quit => "Exit",
        }
    }

    /// A one-word label for the function-key bar.
    pub fn short_label(self) -> &'static str {
        use Command::*;
        match self {
            Help => "Help",
            Rename => "Rename",
            View => "View",
            Edit => "Edit",
            EditNew => "NewFile",
            Copy => "Copy",
            Move => "Move",
            MakeDir => "MkDir",
            Delete => "Delete",
            DeletePermanent => "DelPerm",
            Terminal => "Term",
            Configuration => "Settings",
            Quit => "Exit",
            Open => "Open",
            Refresh => "Refresh",
            Pack => "Pack",
            Unpack => "Unpack",
            FindFiles => "Find",
            MultiRename => "MultiRen",
            CommandPalette => "Palette",
            NewTab => "NewTab",
            CloseTab => "CloseTab",
            Hotlist => "Hotlist",
            Connect => "Connect",
            Disconnect => "Disconn",
            SelectAll => "SelAll",
            DeselectAll => "SelNone",
            ToggleHidden => "Hidden",
            Thumbnails => "Thumbs",
            QuickFilter => "Filter",
            CompareDirs | CompareDirsOptions => "CmpDirs",
            CompareFiles => "CmpFiles",
            SortBy(SortKey::Name) => "ByName",
            SortBy(SortKey::Ext) => "ByExt",
            SortBy(SortKey::Size) => "BySize",
            SortBy(SortKey::Modified) => "ByDate",
            SwitchPanel => "Switch",
            GoParent => "Up",
            GoRoot => "Root",
            EditConfig => "Config",
            CopyToClipboard => "ClipCopy",
            CutToClipboard => "ClipCut",
            PasteFromClipboard => "Paste",
            _ => {
                let id = self.id();
                id.strip_prefix("cm_").unwrap_or(id)
            }
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
            CursorLeft,
            CursorRight,
            Open,
            GoParent,
            GoRoot,
            HistoryBack,
            HistoryForward,
            DirHistory,
            Hotlist,
            SwitchPanel,
            ToggleSelect { advance: true },
            ToggleSelect { advance: false },
            CountDirSizes,
            CompareDirs,
            CompareDirsOptions,
            CompareFiles,
            FindFiles,
            Connect,
            Disconnect,
            SelectAll,
            DeselectAll,
            Refresh,
            ToggleHidden,
            Thumbnails,
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
            CopyToClipboard,
            CutToClipboard,
            PasteFromClipboard,
            Rename,
            MultiRename,
            MakeDir,
            Pack,
            Unpack,
            Terminal,
            Delete,
            DeletePermanent,
            NewTab,
            CloseTab,
            NextTab,
            PrevTab,
            OpenInNewTab,
            EditConfig,
            Configuration,
            CommandPalette,
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
