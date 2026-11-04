//! The full-window tool that covers the panels (`App::screen`): only one is
//! open at a time. `on_key` and `view` dispatch on it with one `match`.
//!
//! Every tool is opened from the panels (a command reached through the
//! keymap, the F-key bar or the command palette) and closing it returns to
//! the panels, so no tool opens over another and a single slot is enough.

use super::{connect, file_diff, find_files, help, multi_rename, settings, viewer};

/// `Term` names the built-in terminal shown (`App::terms` holds them all,
/// shown or not).
pub(super) enum Screen {
    Help(help::Help),
    Settings(Box<settings::Settings>),
    Find(Box<find_files::FindFiles>),
    Connect(Box<connect::ConnectTool>),
    MultiRename(Box<multi_rename::MultiRename>),
    FileDiff(Box<file_diff::FileDiff>),
    Viewer(Box<viewer::Viewer>),
    Term(u64),
}

/// Typed access to the open tool, on the `App::screen` field itself (not on
/// `App`), so a borrow of the tool doesn't lock the rest of `App`:
/// `let Some(v) = self.screen.viewer_mut() else { .. }; .. self.sides ..`.
pub(super) trait ScreenSlot {
    fn help_mut(&mut self) -> Option<&mut help::Help>;
    fn settings(&self) -> Option<&settings::Settings>;
    fn settings_mut(&mut self) -> Option<&mut settings::Settings>;
    fn find(&self) -> Option<&find_files::FindFiles>;
    fn find_mut(&mut self) -> Option<&mut find_files::FindFiles>;
    fn take_find(&mut self) -> Option<find_files::FindFiles>;
    fn connect_mut(&mut self) -> Option<&mut connect::ConnectTool>;
    fn multi_rename(&self) -> Option<&multi_rename::MultiRename>;
    fn multi_rename_mut(&mut self) -> Option<&mut multi_rename::MultiRename>;
    fn take_multi_rename(&mut self) -> Option<multi_rename::MultiRename>;
    fn file_diff(&self) -> Option<&file_diff::FileDiff>;
    fn file_diff_mut(&mut self) -> Option<&mut file_diff::FileDiff>;
    fn take_file_diff(&mut self) -> Option<file_diff::FileDiff>;
    fn viewer(&self) -> Option<&viewer::Viewer>;
    fn viewer_mut(&mut self) -> Option<&mut viewer::Viewer>;
    fn take_viewer(&mut self) -> Option<viewer::Viewer>;
    fn term(&self) -> Option<&u64>;
}

macro_rules! get {
    ($name:ident, $variant:ident, $ty:ty) => {
        fn $name(&self) -> Option<&$ty> {
            match self {
                Some(Screen::$variant(t)) => Some(t),
                _ => None,
            }
        }
    };
}

macro_rules! get_mut {
    ($name_mut:ident, $variant:ident, $ty:ty) => {
        fn $name_mut(&mut self) -> Option<&mut $ty> {
            match self {
                Some(Screen::$variant(t)) => Some(t),
                _ => None,
            }
        }
    };
}

/// Closes the tool if it is the open one and returns it.
macro_rules! take {
    ($name:ident, $variant:ident, $ty:ty) => {
        fn $name(&mut self) -> Option<$ty> {
            match self.take() {
                Some(Screen::$variant(t)) => Some(*t),
                other => {
                    *self = other;
                    None
                }
            }
        }
    };
}

impl ScreenSlot for Option<Screen> {
    get_mut!(help_mut, Help, help::Help);
    get!(settings, Settings, settings::Settings);
    get_mut!(settings_mut, Settings, settings::Settings);
    get!(find, Find, find_files::FindFiles);
    get_mut!(find_mut, Find, find_files::FindFiles);
    take!(take_find, Find, find_files::FindFiles);
    get_mut!(connect_mut, Connect, connect::ConnectTool);
    get!(multi_rename, MultiRename, multi_rename::MultiRename);
    get_mut!(multi_rename_mut, MultiRename, multi_rename::MultiRename);
    take!(take_multi_rename, MultiRename, multi_rename::MultiRename);
    get!(file_diff, FileDiff, file_diff::FileDiff);
    get_mut!(file_diff_mut, FileDiff, file_diff::FileDiff);
    take!(take_file_diff, FileDiff, file_diff::FileDiff);
    get!(viewer, Viewer, viewer::Viewer);
    get_mut!(viewer_mut, Viewer, viewer::Viewer);
    take!(take_viewer, Viewer, viewer::Viewer);
    get!(term, Term, u64);
}
