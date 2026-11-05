//! The full-window tool that covers the panels (`App::screen`): only one is
//! open at a time. `on_key` and `view` dispatch on it with one `match`.
//!
//! Every tool is opened from the panels (a command reached through the
//! keymap, the F-key bar or the command palette) and closing it returns to
//! the panels, so no tool opens over another and a single slot is enough.

use super::{connect, delayed, disk_usage, file_diff, find_files, help, multi_rename, settings, sync, viewer};
use super::{App, Message, Tone};
use iced::Task;
use std::mem::{discriminant, Discriminant};
use std::time::Duration;

/// `Term` names the built-in terminal shown (`App::terms` holds them all,
/// shown or not).
pub(super) enum Screen {
    Help(help::Help),
    Settings(Box<settings::Settings>),
    Find(Box<find_files::FindFiles>),
    Sync(Box<sync::SyncTool>),
    DiskUsage(Box<disk_usage::DiskUsage>),
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
    fn sync_mut(&mut self) -> Option<&mut sync::SyncTool>;
    fn take_sync(&mut self) -> Option<sync::SyncTool>;
    fn disk_usage(&self) -> Option<&disk_usage::DiskUsage>;
    fn disk_usage_mut(&mut self) -> Option<&mut disk_usage::DiskUsage>;
    fn take_disk_usage(&mut self) -> Option<disk_usage::DiskUsage>;
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
    get_mut!(sync_mut, Sync, sync::SyncTool);
    take!(take_sync, Sync, sync::SyncTool);
    get!(disk_usage, DiskUsage, disk_usage::DiskUsage);
    get_mut!(disk_usage_mut, DiskUsage, disk_usage::DiskUsage);
    take!(take_disk_usage, DiskUsage, disk_usage::DiskUsage);
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

/// What the footer-note timer needs from a tool with a note (`Screen::note_state`). A new note fades out
/// after a while (errors stay longer); moving the cursor clears an old one (not an error). A note asking
/// for an answer, or the only sign of a job running, stays while `pending`, and times out once that ends.
pub(super) struct NoteState<'a> {
    pub(super) note: &'a mut Option<(String, Tone)>,
    /// Where the cursor or selection is (row, page, ..): a change clears a non-error note.
    pub(super) cursor: (usize, usize),
    pub(super) pending: bool,
}

/// A tool's note (or the panels' status line), cursor and pending state before a message; see
/// `App::note_after`.
pub(super) struct NoteMark {
    /// The tool; `None`: the status line.
    screen: Option<Discriminant<Screen>>,
    note: Option<(String, Tone)>,
    cursor: (usize, usize),
    pending: bool,
}

/// What happens to the note after a message.
#[derive(Debug, PartialEq)]
enum NoteStep {
    Keep,
    Clear,
    /// A timer (seconds) for the note.
    Expire(u64),
}

fn note_step(
    before: Option<&NoteMark>,
    note: Option<&(String, Tone)>,
    cursor: (usize, usize),
    pending: bool,
) -> NoteStep {
    let Some((_, tone)) = note else { return NoteStep::Keep };
    let secs = if *tone == Tone::Error { 8 } else { 4 };
    match before {
        // A new or changed note (or one the tool opened with).
        None => NoteStep::Expire(secs),
        Some(b) if b.note.as_ref() != note => NoteStep::Expire(secs),
        // The wait ended: the note times out now.
        Some(b) if b.pending && !pending => NoteStep::Expire(secs),
        // Errors stay until their timer runs out, so a failed save isn't missed.
        Some(b) if *tone != Tone::Error && !pending && b.cursor != cursor => NoteStep::Clear,
        _ => NoteStep::Keep,
    }
}

impl Screen {
    /// The footer note of tools that have one.
    fn note_state(&mut self) -> Option<NoteState<'_>> {
        match self {
            Screen::Settings(t) => Some(t.note_state()),
            Screen::Sync(t) => Some(t.note_state()),
            Screen::DiskUsage(t) => Some(t.note_state()),
            Screen::Connect(t) => Some(t.note_state()),
            Screen::MultiRename(t) => Some(t.note_state()),
            Screen::Viewer(t) => Some(t.note_state()),
            Screen::Help(_) | Screen::Find(_) | Screen::FileDiff(_) | Screen::Term(_) => None,
        }
    }
}

/// The open tool's note and the status line before a message.
pub(super) struct NoteMarks {
    tool: Option<NoteMark>,
    status: NoteMark,
}

impl App {
    /// The status line stands for an operation still running (branch listing, comparing, or a message with "…"
    /// such as "Extracting x…"), so it doesn't time out yet. It has no cursor: commands clear it (`App::run`).
    fn status_pending(&self) -> bool {
        self.branching.is_some()
            || self.comparing.is_some()
            || self.status.as_ref().is_some_and(|(m, _)| m.contains('…'))
    }

    /// Taken before every message: the open tool's note, cursor and pending state, and the status line.
    pub(super) fn note_mark(&mut self) -> NoteMarks {
        let pending = self.status_pending();
        let status = NoteMark { screen: None, note: self.status.clone(), cursor: (0, 0), pending };
        let tool = self.screen.as_mut().and_then(|screen| {
            let kind = discriminant(&*screen);
            let s = screen.note_state()?;
            Some(NoteMark { screen: Some(kind), note: s.note.clone(), cursor: s.cursor, pending: s.pending })
        });
        NoteMarks { tool, status }
    }

    /// After every message: starts the timer of a new note or status, or clears an old note when the cursor
    /// moved.
    pub(super) fn note_after(&mut self, marks: NoteMarks, task: Task<Message>) -> Task<Message> {
        let pending = self.status_pending();
        let task = match note_step(Some(&marks.status), self.status.as_ref(), (0, 0), pending) {
            NoteStep::Expire(secs) => {
                self.status_gen = self.status_gen.wrapping_add(1);
                Task::batch([task, delayed(Duration::from_secs(secs), Message::StatusExpired(self.status_gen))])
            }
            NoteStep::Keep | NoteStep::Clear => task,
        };
        let Some(screen) = self.screen.as_mut() else { return task };
        let kind = discriminant(&*screen);
        let Some(s) = screen.note_state() else { return task };
        // Another tool was open before: its note counts as no note.
        let before = marks.tool.filter(|m| m.screen == Some(kind));
        match note_step(before.as_ref(), s.note.as_ref(), s.cursor, s.pending) {
            NoteStep::Keep => task,
            NoteStep::Clear => {
                *s.note = None;
                task
            }
            NoteStep::Expire(secs) => {
                self.note_gen = self.note_gen.wrapping_add(1);
                Task::batch([task, delayed(Duration::from_secs(secs), Message::NoteExpired(self.note_gen))])
            }
        }
    }

    /// The timer of note `gen` ran out: clears it if it is still the latest and nothing waits for it.
    pub(super) fn note_expired(&mut self, gen: u32) {
        if gen != self.note_gen {
            return;
        }
        if let Some(s) = self.screen.as_mut().and_then(Screen::note_state) {
            if !s.pending {
                *s.note = None;
            }
        }
    }

    /// The timer of status `gen` ran out: clears the status line if it is still the latest status and no
    /// operation waits on it.
    pub(super) fn status_expired(&mut self, gen: u32) {
        if gen == self.status_gen && !self.status_pending() {
            self.status = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(note: Option<(&str, Tone)>, cursor: (usize, usize), pending: bool) -> NoteMark {
        NoteMark {
            screen: Some(discriminant(&Screen::Term(0))),
            note: note.map(|(t, tone)| (t.to_owned(), tone)),
            cursor,
            pending,
        }
    }

    #[test]
    fn new_notes_expire_errors_later() {
        let info = ("Saved".to_owned(), Tone::Info);
        let error = ("Can't save".to_owned(), Tone::Error);
        assert_eq!(note_step(None, Some(&info), (0, 0), false), NoteStep::Expire(4));
        assert_eq!(note_step(Some(&mark(None, (0, 0), false)), Some(&error), (0, 0), false), NoteStep::Expire(8));
        // A changed note gets its own timer, even while pending.
        let b = mark(Some(("Press Del again", Tone::Info)), (0, 0), true);
        assert_eq!(note_step(Some(&b), Some(&info), (0, 0), true), NoteStep::Expire(4));
        // No note: nothing to do.
        assert_eq!(note_step(Some(&b), None, (3, 0), false), NoteStep::Keep);
    }

    #[test]
    fn cursor_moves_clear_all_but_errors_and_pending() {
        let info = ("Saved".to_owned(), Tone::Info);
        let error = ("Can't save".to_owned(), Tone::Error);
        let b = mark(Some(("Saved", Tone::Info)), (0, 0), false);
        assert_eq!(note_step(Some(&b), Some(&info), (0, 0), false), NoteStep::Keep);
        assert_eq!(note_step(Some(&b), Some(&info), (1, 0), false), NoteStep::Clear);
        assert_eq!(note_step(Some(&b), Some(&info), (0, 1), false), NoteStep::Clear);
        let b = mark(Some(("Can't save", Tone::Error)), (0, 0), false);
        assert_eq!(note_step(Some(&b), Some(&error), (1, 0), false), NoteStep::Keep);
        let b = mark(Some(("Saved", Tone::Info)), (0, 0), true);
        assert_eq!(note_step(Some(&b), Some(&info), (1, 0), true), NoteStep::Keep);
    }

    #[test]
    fn a_note_times_out_when_its_wait_ends() {
        let info = ("Connecting".to_owned(), Tone::Info);
        let b = mark(Some(("Connecting", Tone::Info)), (0, 0), true);
        assert_eq!(note_step(Some(&b), Some(&info), (0, 0), true), NoteStep::Keep);
        assert_eq!(note_step(Some(&b), Some(&info), (0, 0), false), NoteStep::Expire(4));
    }
}
