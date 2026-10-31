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
    SwitchPanel,
    /// Toggle selection on the cursor entry, optionally moving down.
    ToggleSelect { advance: bool },
    SelectAll,
    DeselectAll,
    Refresh,
    ToggleHidden,
    SortBy(SortKey),
    /// Make the other panel show the same folder.
    SyncOtherPanel,
    View,
    Edit,
    Copy,
    Move,
    /// Rename the cursor entry in place (Shift+F6).
    Rename,
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
            SwitchPanel => "cm_FocusSwap",
            ToggleSelect { .. } => "cm_Select",
            SelectAll => "cm_SelectAll",
            DeselectAll => "cm_ClearAll",
            Refresh => "cm_RereadSource",
            ToggleHidden => "cm_SwitchHidSys",
            SortBy(SortKey::Name) => "cm_SortByName",
            SortBy(SortKey::Ext) => "cm_SortByExt",
            SortBy(SortKey::Size) => "cm_SortBySize",
            SortBy(SortKey::Modified) => "cm_SortByDateTime",
            SyncOtherPanel => "cm_TransferPath",
            View => "cm_View",
            Edit => "cm_Edit",
            Copy => "cm_Copy",
            Move => "cm_RenMov",
            Rename => "cm_RenameOnly",
            MakeDir => "cm_MkDir",
            Delete => "cm_Delete",
            DeletePermanent => "cm_DeletePermanent",
            NewTab => "cm_OpenNewTab",
            CloseTab => "cm_CloseCurrentTab",
            NextTab => "cm_SwitchToNextTab",
            PrevTab => "cm_SwitchToPreviousTab",
            OpenInNewTab => "cm_OpenDirInNewTab",
            Quit => "cm_Exit",
        }
    }
}
