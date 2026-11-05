use crate::panel::SortKey;

/// Builds `enum Command`, `id`, `description`, `short_label` and `ALL` from
/// one table. Each command is one row: `Variant => "cm_Id", "description",
/// "Label";`. A variant with data gives its fields after a colon and then
/// one row per value, written as the fields part of the value:
/// `SortBy: (SortKey)` then `(SortKey::Name) => ...;`. Rows under
/// `bindable` are listed in `ALL` in table order; `mouse_only` rows are not.
macro_rules! commands {
    (bindable { $($bindable:tt)* } mouse_only { $($mouse:tt)* }) => {
        commands!(@gen [$($bindable)* $($mouse)*] [$($bindable)*]);
    };
    (@gen [
        $(
            $(#[$doc:meta])*
            $v:ident $(: $decl:tt)?
            $( $(($($targ:tt)*))? $({$($sarg:tt)*})? => $id:literal, $desc:literal, $label:literal; )+
        )*
    ] [
        $(
            $(#[$bdoc:meta])*
            $bv:ident $(: $bdecl:tt)?
            $( $(($($btarg:tt)*))? $({$($bsarg:tt)*})? => $bid:literal, $bdesc:literal, $blabel:literal; )+
        )*
    ]) => {
        /// Every user action goes through a `Command`. Key bindings, the function-key
        /// bar, menus and the command palette all map onto these, the same idea
        /// as Total Commander's `cm_*` internal commands.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Command {
            $( $(#[$doc])* $v $($decl)?, )*
        }

        impl Command {
            /// Stable identifier, used for config files and the command palette.
            pub fn id(self) -> &'static str {
                match self {
                    $($( Command::$v $(($($targ)*))? $({$($sarg)*})? => $id, )+)*
                }
            }

            /// What the command does, for the F1 help list and the palette.
            pub fn description(self) -> &'static str {
                match self {
                    $($( Command::$v $(($($targ)*))? $({$($sarg)*})? => $desc, )+)*
                }
            }

            /// A one-word label for the function-key bar.
            pub fn short_label(self) -> &'static str {
                match self {
                    $($( Command::$v $(($($targ)*))? $({$($sarg)*})? => $label, )+)*
                }
            }

            /// Every command a key can be bound to (all but the mouse-only ones).
            pub const ALL: &'static [Command] = &[
                $($( Command::$bv $(($($btarg)*))? $({$($bsarg)*})?, )+)*
            ];
        }
    };
}

commands! {
    bindable {
        CursorUp => "cm_CursorUp", "Cursor up", "CursorUp";
        CursorDown => "cm_CursorDown", "Cursor down", "CursorDown";
        PageUp => "cm_PageUp", "Cursor one page up", "PageUp";
        PageDown => "cm_PageDown", "Cursor one page down", "PageDown";
        CursorHome => "cm_GoToFirstEntry", "Cursor to the first entry", "GoToFirstEntry";
        CursorEnd => "cm_GoToLastEntry", "Cursor to the last entry", "GoToLastEntry";
        /// Previous / next entry in the thumbnail view (Left / Right).
        CursorLeft => "cm_CursorLeft", "Cursor to the previous entry (thumbnail view)", "CursorLeft";
        CursorRight => "cm_CursorRight", "Cursor to the next entry (thumbnail view)", "CursorRight";
        Open => "cm_Return",
            "Open the folder, archive (zip, 7z, tar, gz: browse like a folder) or file under the cursor", "Open";
        GoParent => "cm_GoToParent", "Go to the parent folder", "Up";
        GoRoot => "cm_GoToRoot", "Go to the root of the drive", "Root";
        /// Previous / next folder in the panel's history (Alt+Left/Right).
        HistoryBack => "cm_PrevDir", "Back to the previous folder", "PrevDir";
        HistoryForward => "cm_NextDir", "Forward to the next folder", "NextDir";
        /// The panel's folder history as a popup list (Alt+Down).
        DirHistory => "cm_DirectoryHistory",
            "Folder history list: jump back or forward to any folder visited in this tab", "DirectoryHistory";
        /// Directory hotlist popup (Ctrl+D).
        Hotlist => "cm_DirectoryHotlist", "Directory hotlist: favourite folders, with submenus", "Hotlist";
        /// Show / hide the hotlist tree left of the panels
        /// (Ctrl+Shift+D; needs the `hotlist_tree` setting).
        HotlistTree => "cm_HotlistTree",
            "Hotlist tree: show or hide the hotlist as a folder tree left of the panels", "Tree";
        /// With the hotlist tree shown: hide / show the left panel, leaving
        /// the tree and the right panel.
        HotlistTreeHideLeft => "cm_HotlistTreeHideLeft",
            "Hotlist tree: hide or show the left panel while the tree is shown", "HideLeft";
        SwitchPanel => "cm_FocusSwap", "Switch to the other panel", "Switch";
        /// Toggle selection on the cursor entry, optionally moving down.
        ToggleSelect: { advance: bool }
            { advance: true } => "cm_Select", "Select / deselect the entry and move down", "Select";
            { advance: false } => "cm_SelectNoMove", "Select / deselect the entry", "SelectNoMove";
        /// Compute the size of every folder in the panel (Alt+Shift+Enter).
        CountDirSizes => "cm_CountDirContent", "Compute the size of every folder in the panel", "CountDirContent";
        /// Select the entries that differ from the other panel (Shift+F2).
        CompareDirs => "cm_CompareDirs",
            "Compare folders: select the entries that differ from the other panel", "CmpDirs";
        /// Compare-folders options, then compare (Ctrl+Shift+F2).
        CompareDirsOptions => "cm_CompareDirsOptions",
            "Compare folders with options (by content, ignore dates, subfolders)", "CmpDirs";
        /// Side-by-side diff of two files (Alt+F2).
        CompareFiles => "cm_CompareFilesByContent", "Compare two files by content, side by side", "CmpFiles";
        /// SHA-256 of the selected files, with a dialog to copy or save them.
        CalcChecksum => "cm_CalcChecksum", "Calculate the SHA-256 checksum of the selected files (copy or save as .sha256)", "Checksum";
        /// Check the files listed in the .sha256 file under the cursor.
        VerifyChecksum => "cm_VerifyChecksum", "Verify the files listed in the .sha256 file under the cursor", "Verify";
        /// Cut the file under the cursor into parts (name.001, name.002, ...) in the other panel.
        SplitFile => "cm_SplitFile", "Split the file under the cursor into parts (name.001, ...) in the other panel's folder", "Split";
        /// Join the parts name.001, name.002, ... into the other panel's folder.
        CombineFiles => "cm_CombineFiles", "Join the parts (name.001, name.002, ...) from the file under the cursor into the other panel's folder", "Combine";
        /// Find files by name and contents (Ctrl+F).
        FindFiles => "cm_SearchFor", "Find files by name and contents", "Find";
        /// What takes up space in a folder, as a tree (Alt+Shift+F7).
        DiskUsage => "cm_DiskUsage",
            "Show what takes up space in the folder under the cursor (or the current folder)", "DiskUse";
        /// Saved server connections; connect one in this panel (Ctrl+F7).
        Connect => "cm_FtpConnect",
            "Connect to a server (SFTP, FTP, FTPS): saved connections, new, edit, delete", "Connect";
        /// Close the connection of the panel's server (Ctrl+Shift+F).
        Disconnect => "cm_FtpDisconnect", "Close the connection of the server shown in the panel", "Disconn";
        SelectAll => "cm_SelectAll", "Select all entries", "SelAll";
        DeselectAll => "cm_ClearAll", "Clear the selection", "SelNone";
        Refresh => "cm_RereadSource", "Reread the folder", "Refresh";
        ToggleHidden => "cm_SwitchHidSys", "Show / hide hidden and system files", "Hidden";
        /// Thumbnail view on / off for the tab (Ctrl+Shift+F5).
        Thumbnails => "cm_SrcThumbs",
            "Thumbnail view on / off: pictures, videos and documents as a grid of previews", "Thumbs";
        /// Show only entries matching a typed filter (Ctrl+S).
        QuickFilter => "cm_QuickFilter", "Quick filter: show only entries matching typed text", "Filter";
        SortBy: (SortKey)
            (SortKey::Name) => "cm_SortByName", "Sort by name", "ByName";
            (SortKey::Ext) => "cm_SortByExt", "Sort by extension", "ByExt";
            (SortKey::Size) => "cm_SortBySize", "Sort by size", "BySize";
            (SortKey::Modified) => "cm_SortByDateTime", "Sort by date", "ByDate";
        /// Make the other panel show the same folder.
        SyncOtherPanel => "cm_TransferPath", "Show this folder in the other panel too", "TransferPath";
        View => "cm_View", "View the file (text, hex, images, rendered Markdown)", "View";
        Edit => "cm_Edit", "Edit the file in the configured editor", "Edit";
        /// Ask for a file name, create it if missing and edit it (Shift+F4).
        EditNew => "cm_EditNewFile", "Create a new file and edit it", "NewFile";
        Copy => "cm_Copy", "Copy to the other panel", "Copy";
        Move => "cm_RenMov", "Move / rename to the other panel", "Move";
        /// Put the selected files on the system clipboard (Ctrl+C / Ctrl+X),
        /// for pasting here or in Explorer.
        CopyToClipboard => "cm_CopyToClipboard", "Copy the files to the clipboard", "ClipCopy";
        CutToClipboard => "cm_CutToClipboard", "Cut the files to the clipboard (paste moves them)", "ClipCut";
        /// Copy or move the files on the clipboard into the panel (Ctrl+V).
        PasteFromClipboard => "cm_PasteFromClipboard", "Paste files from the clipboard into this panel", "Paste";
        /// Rename the cursor entry in place (Shift+F6).
        Rename => "cm_RenameOnly", "Rename in place", "Rename";
        /// Rename the selected files by mask (Ctrl+M).
        MultiRename => "cm_MultiRenameFiles", "Multi-rename the selected files by mask", "MultiRen";
        MakeDir => "cm_MkDir", "Create a folder", "MkDir";
        /// Pack the selected files into a zip in the other panel (Alt+F5).
        Pack => "cm_PackFiles", "Pack the selected files into a zip archive in the other panel", "Pack";
        /// Unpack the selected archives into the other panel (Alt+F9).
        Unpack => "cm_UnpackFiles", "Unpack the selected archives (zip, 7z, tar, gz) into the other panel", "Unpack";
        /// Open the built-in terminal in the active panel's folder (F9), or
        /// show the one already running.
        InternalTerminal => "cm_InternalTerminal", "Open a built-in terminal in this folder (the \"builtin_shell\" setting)", "Term";
        /// Switch between the panels and the built-in terminal (Ctrl+Shift+F9);
        /// the one key the terminal doesn't pass on to the shell.
        ToggleTerminal => "cm_ToggleTerminal", "Switch between the panels and the built-in terminal (it keeps running)", "Switch";
        /// Open another built-in terminal in the active panel's folder (the
        /// "+" of the sidebar; Ctrl+Shift+T).
        NewTerminal => "cm_NewTerminal", "Open another built-in terminal in this folder", "NewTerm";
        /// Next entry of the terminal sidebar: Files, terminal 1, 2, ... and
        /// round again (Ctrl+Shift+Down).
        NextTerminal => "cm_NextTerminal", "Show the next entry of the terminal sidebar (Files, terminal 1, 2, ...)", "NextTerm";
        /// Previous entry of the terminal sidebar (Ctrl+Shift+Up).
        PrevTerminal => "cm_PrevTerminal", "Show the previous entry of the terminal sidebar", "PrevTerm";
        /// End the shown built-in terminal and close it (Ctrl+Shift+W).
        CloseTerminal => "cm_CloseTerminal", "End the shown built-in terminal and close it", "CloseTerm";
        /// Show the folder the shown (or last used) built-in terminal is in in the active panel and return to the panels.
        TerminalFolderToPanel => "cm_TerminalFolderToPanel", "Go to the built-in terminal's current folder in the active panel", "TermDir";
        /// Open an external terminal in the active panel's folder (Ctrl+F9).
        Terminal => "cm_ExecuteTerm", "Open an external terminal in this folder (the \"terminal\" setting)", "Term";
        /// Delete to the recycle bin.
        Delete => "cm_Delete", "Delete to the recycle bin", "Delete";
        /// Delete permanently (Shift+F8 / Shift+Del).
        DeletePermanent => "cm_DeletePermanent", "Delete permanently", "DelPerm";
        NewTab => "cm_OpenNewTab", "New tab", "NewTab";
        CloseTab => "cm_CloseCurrentTab", "Close the tab", "CloseTab";
        NextTab => "cm_SwitchToNextTab", "Next tab", "SwitchToNextTab";
        PrevTab => "cm_SwitchToPreviousTab", "Previous tab", "SwitchToPreviousTab";
        /// Open the folder under the cursor in a new tab.
        OpenInNewTab => "cm_OpenDirInNewTab", "Open the folder under the cursor in a new tab", "OpenDirInNewTab";
        /// Open `config.toml` in the editor (Ctrl+,).
        EditConfig => "cm_EditConfig", "Edit config.toml (editor, [keys], [colors])", "Config";
        /// Settings view: preferences, key bindings, colours (F10).
        Configuration => "cm_Configuration", "Settings: preferences, key bindings, colour themes", "Settings";
        /// Popup to find a command by name, id or key and run it (Ctrl+Space).
        CommandPalette => "cm_CommandPalette",
            "Command palette: find a command by name, id or key and run it", "Palette";
        /// Searchable list of keys and features (F1).
        Help => "cm_HelpIndex", "This help: all keys and features", "Help";
        /// Look for a newer release on GitHub; download and install it.
        CheckForUpdates => "cm_CheckForUpdates",
            "Check for updates: look for a newer release on GitHub, download and install it", "Update";
        Quit => "cm_Exit", "Exit", "Exit";
    }
    mouse_only {
        /// Scroll the cursor by n rows (mouse wheel).
        CursorBy: (i32)
            (_) => "cm_Scroll", "Scroll the cursor (mouse wheel)", "Scroll";
    }
}

/// Ids that were renamed: old config files may still use them, so `from_id` accepts them.
/// Saving writes the new id (`Command::id`). Keep this table small; add a row only on a rename.
const OLD_IDS: &[(&str, Command)] = &[("cm_ExecuteDOS", Command::Terminal)];

impl Command {
    /// The command with this id (case-insensitive), for config files. Also accepts old ids (`OLD_IDS`).
    pub fn from_id(id: &str) -> Option<Command> {
        Self::ALL
            .iter()
            .copied()
            .find(|c| c.id().eq_ignore_ascii_case(id))
            .or_else(|| OLD_IDS.iter().find(|(old, _)| old.eq_ignore_ascii_case(id)).map(|&(_, c)| c))
    }
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
        assert_eq!(Command::from_id("cm_ExecuteTerm"), Some(Command::Terminal));
        assert_eq!(Command::from_id("cm_executedos"), Some(Command::Terminal));
        assert_eq!(Command::Terminal.id(), "cm_ExecuteTerm");
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
