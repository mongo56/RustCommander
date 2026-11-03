//! Application state and update logic. Drawing lives in [`view`].

mod view;
mod button_focus;
mod archive_edit;
mod connect;
mod file_diff;
mod help;
mod hotlist;
mod palette;
mod find_files;
mod multi_rename;
mod settings;
mod thumbs;
mod viewer;

use crate::keymap;
use crate::select_range::{self, Part, SelectRange};
use crate::scrollbar;
use crate::session::{self, Session, SideState, TabState};
use crate::theme;
use crate::watch;
use iced::keyboard::key::Named;
use iced::widget::{image, text_input};
use iced::{event, keyboard, mouse, window, Event, Point, Size, Subscription, Task, Theme};
use rc_core::ops::{self, Conflict, Job, JobEvent, JobHandle, Outcome, Progress, Resolution};
use rc_core::compare::{CompareOptions, Differences};
use rc_core::entry::Entry;
use rc_core::{remote, vfs, Activated, Command, EntryKind, Nav, Panel, SortKey};
use rc_platform::{DragOutcome, MenuAction};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf, MAIN_SEPARATOR};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Width of the divider between the panels, and the narrowest a panel gets.
const SPLIT_W: f32 = 5.0;
const MIN_PANEL: f32 = 200.0;
/// Narrowest the Ext / Size / Modified columns get, and the narrowest the
/// name column is kept (columns that would squeeze it further are hidden).
const MIN_COLUMNS: [f32; 3] = [30.0, 50.0, 70.0];
const MAX_COLUMN: f32 = 400.0;
const MIN_NAME: f32 = 100.0;
const ROW_H: f32 = 22.0;
/// Approximate height of everything that isn't file rows (tabs, headers,
/// footers, function-key bar). Only used to size PageUp/PageDown jumps.
const CHROME_H: f32 = 206.0;
/// Height of the function-key buttons row, which `CHROME_H` includes.
const FKEYS_H: f32 = 30.0;
const JOB_ROW_H: f32 = 30.0;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How far the mouse must move with the button down before a drag starts.
const DRAG_THRESHOLD: f32 = 6.0;
/// Quick search closes after this long without typing.
const QUICK_SEARCH_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_SUGGESTIONS: usize = 12;

fn dialog_input_id() -> text_input::Id {
    text_input::Id::new("dialog-input")
}

fn path_input_id() -> text_input::Id {
    text_input::Id::new("path-input")
}

fn rename_input_id() -> text_input::Id {
    text_input::Id::new("rename-input")
}

pub struct App {
    /// Left and right side, each with its tabs.
    sides: [Side; 2],
    active: usize,
    drives: Vec<String>,
    status: Option<(String, Tone)>,
    window_width: f32,
    window_height: f32,
    last_click: Option<(usize, usize, Instant)>,
    /// Last click on the empty part of a tab bar (double-click opens a tab).
    last_tab_bar_click: Option<(usize, Instant)>,
    dialog: Option<Dialog>,
    /// Focused button of the open dialog (`None` = the default, the last one).
    dialog_focus: Option<usize>,
    /// Focused button of the overwrite question (`None` = none yet).
    conflict_focus: Option<usize>,
    /// Favourite folders (Ctrl+D), saved with the session.
    hotlist: Vec<crate::hotlist::HotItem>,
    /// Commands last run from the command palette, most recent first
    /// (saved with the session).
    recent_commands: Vec<Command>,
    jobs: Vec<RunningJob>,
    next_job_id: u64,
    /// Tasks started where none can be returned (an archive rename committed
    /// by `leave_edits`); `update` runs them.
    deferred: Vec<Task<Message>>,
    /// Folder size counts running (Space / Alt+Shift+Enter): the folder
    /// listed and the batch's cancel flag.
    counts: Vec<(PathBuf, Arc<AtomicBool>)>,
    /// Compare-folders options (Ctrl+Shift+F2), saved in `config.toml`
    /// `[compare]`.
    compare_opts: CompareOptions,
    /// Compare options that couldn't be written to `config.toml` (or moved
    /// there from an old session); saved with the session meanwhile.
    compare_unsaved: Option<crate::config::CompareState>,
    /// A compare running on a worker: its cancel flag and the folders
    /// compared (results for other folders are dropped).
    comparing: Option<(Arc<AtomicBool>, PathBuf, PathBuf)>,
    path_edit: Option<PathEdit>,
    rename: Option<InlineRename>,
    quick_search: Option<QuickSearch>,
    /// Side whose quick filter (Ctrl+S) is being typed.
    filter_edit: Option<usize>,
    drag: Option<Drag>,
    /// Files dropped in from other apps, waiting for the drop position.
    dropped: Vec<PathBuf>,
    /// Last known mouse position (window coordinates); tracked while a
    /// mouse button is down.
    mouse: Point,
    /// Share of the width the left panel gets (0.5 = equal).
    split: f32,
    /// The divider between the panels is being dragged.
    split_drag: bool,
    /// When the divider was last pressed (double click resets it).
    split_click: Option<Instant>,
    /// Widths of the Ext / Size / Modified columns, shared by all panels.
    columns: [f32; 3],
    /// A column header edge is being dragged.
    column_drag: Option<ColumnDrag>,
    /// Column edge last pressed and when (double click resets the width).
    column_click: Option<(usize, Instant)>,
    /// Row under the mouse while dragging: (side, index).
    hover: Option<(usize, usize)>,
    modifiers: keyboard::Modifiers,
    /// Whether the window has keyboard focus, as last reported. On Windows,
    /// winit synthesizes key-downs for keys still held when focus arrives and
    /// delivers them *before* the `Focused` event, so a key press seen while
    /// this is false was held in the window we just left (e.g. the Enter of a
    /// terminal's `exit`).
    focused: bool,
    /// The held Enter leaked in from another window; it and its auto-repeats
    /// are ignored until the key-up.
    leaked_enter: bool,
    /// Icon cache keyed by `rc_platform::icon_key`. Filled lazily from
    /// `view`, hence the RefCell.
    icons: RefCell<HashMap<String, Option<image::Handle>>>,
    /// Thumbnail view: decoded thumbnails, decodes running, grid sizes.
    thumbs: thumbs::Thumbs,
    /// Free/total space per volume root, refreshed every few seconds.
    disk: RefCell<HashMap<PathBuf, DiskEntry>>,
    /// Latest background listing requested per folder (`Message::Listed`).
    listings: HashMap<PathBuf, u64>,
    next_listing: u64,
    /// Folder change per side still waiting for its listing (slow or
    /// network folders); see `start_nav`.
    nav: [Option<PendingNav>; 2],
    /// Built-in viewer (F3), covering the whole window while open.
    viewer: Option<viewer::Viewer>,
    /// Multi-rename tool (Ctrl+M), covering the whole window while open.
    multi_rename: Option<multi_rename::MultiRename>,
    /// Compare files by content (Alt+F2), covering the whole window.
    file_diff: Option<file_diff::FileDiff>,
    /// Find files (Alt+F7), covering the whole window while open; the
    /// closed tool is kept (fields and results) for the next Alt+F7.
    find: Option<find_files::FindFiles>,
    find_last: Option<find_files::FindFiles>,
    /// Connect to a server (Ctrl+F), covering the whole window while open.
    connect: Option<connect::ConnectTool>,
    /// (folder, name): put the cursor on `name` once a background listing
    /// of `folder` arrives (after a job on a server).
    focus_after_listing: Vec<(PathBuf, String)>,
    help: Option<help::Help>,
    /// Settings view (F10), covering the whole window while open.
    settings: Option<settings::Settings>,
    /// Multi-rename settings, kept between uses.
    rename_opts: rc_core::rename::Options,
    /// `(new, old)` paths of the last multi-rename batch, kept after the
    /// tool closes so Ctrl+Z in a reopened tool still undoes it.
    rename_undo: Vec<(PathBuf, PathBuf)>,
    /// Last good `config.toml` and the key bindings built from it.
    config: crate::config::Config,
    keymap: keymap::Keymap,
    /// Modification time of `config.toml` when it was last read; `None` =
    /// not read yet.
    config_stamp: Option<Option<std::time::SystemTime>>,
    /// Window position from the last session or the last quit, saved with
    /// the session.
    placement: Option<rc_platform::Placement>,
    /// Files from zips open in the editor (F4), put back on save.
    archive_edits: archive_edit::ArchiveEdits,
}

/// A navigation whose target folder is being listed in the background.
struct PendingNav {
    seq: u64,
    /// Tab it was started in; switching tabs abandons it.
    tab: usize,
    nav: Nav,
    path: PathBuf,
}

/// How long navigation waits for a listing before showing the folder as
/// loading and finishing in the background. Local folders list well within
/// this, so keys typed right after Enter still apply to the new folder.
const SYNC_LISTING_WAIT: Duration = Duration::from_millis(150);

/// One side of the window: a set of tabs, one of them shown.
struct Side {
    tabs: Vec<Panel>,
    current: usize,
}

/// When the space was read, and (free, total) bytes.
type DiskEntry = (Instant, Option<(u64, u64)>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Info,
    Success,
    Error,
}

/// Modal dialogs opened by commands.
#[derive(Debug)]
enum Dialog {
    Transfer { is_move: bool, sources: Vec<PathBuf>, base: PathBuf, input: String },
    MakeDir { input: String },
    /// Alt+F5: the zip to pack `sources` into (`base`: for relative input).
    Pack { sources: Vec<PathBuf>, base: PathBuf, input: String },
    /// Alt+F9: the folder to unpack `archives` into.
    Unpack { archives: Vec<PathBuf>, base: PathBuf, input: String },
    /// The password of an encrypted archive; `retry`: a side to take there
    /// once it's accepted (its listing needed it).
    Password { archive: PathBuf, input: String, retry: Option<usize> },
    /// Shift+F4: name of the file to create (if missing) and edit.
    EditNew { input: String },
    Delete { paths: Vec<PathBuf>, to_trash: bool },
    /// Hotlist menu (Ctrl+D).
    Hotlist(hotlist::HotlistMenu),
    /// Folder history list (Alt+Down); `selected` indexes `Panel::history`.
    History { selected: usize },
    /// Command palette (Ctrl+Space).
    Palette(palette::Palette),
    /// Compare-folders options; Enter compares with them.
    Compare { opts: CompareOptions },
}

/// The path bar of one side being edited, with folder suggestions.
#[derive(Debug)]
struct PathEdit {
    side: usize,
    text: String,
    suggestions: Vec<PathBuf>,
    selected: Option<usize>,
    /// Last folder listed for suggestions, to avoid re-reading it per key;
    /// `None` names while it's being read in the background.
    cache: Option<(PathBuf, Option<Vec<String>>)>,
}

/// A file being renamed in place.
#[derive(Debug)]
struct InlineRename {
    side: usize,
    src: PathBuf,
    text: String,
    is_dir: bool,
    /// The selected part of the name and its `select_range` request id.
    part: Part,
    select: u64,
}

impl InlineRename {
    /// Selects `part` of the current text (applied by `SelectRange`).
    fn select(&mut self, part: Part) {
        self.part = part;
        self.select = select_range::next_generation();
    }

    /// The grapheme range to select.
    fn selection(&self) -> (usize, usize) {
        self.part.range(&self.text, self.is_dir).unwrap_or((0, 0))
    }
}

/// Type-to-jump in the active panel.
#[derive(Debug)]
struct QuickSearch {
    side: usize,
    text: String,
    /// Bumped on every keystroke so stale timeouts are ignored.
    seq: u64,
}

#[derive(Debug)]
/// Dragging the edge in front of a column of the panel header: `prior` is the
/// fixed column on its left that gives or takes the width (none: the name column does).
#[derive(Clone)]
struct ColumnDrag {
    side: usize,
    col: usize,
    prior: Option<usize>,
    /// Mouse x at the first move; widths are changed relative to it.
    x0: Option<f32>,
    start: [f32; 3],
}

fn clamp_columns(widths: [f32; 3]) -> [f32; 3] {
    let mut out = widths;
    for (i, w) in out.iter_mut().enumerate() {
        *w = if w.is_finite() { w.clamp(MIN_COLUMNS[i], MAX_COLUMN) } else { session::DEFAULT_COLUMNS[i] };
    }
    out
}

struct Drag {
    side: usize,
    /// Where the drag started; set by the first mouse move after the press.
    start: Option<Point>,
    /// Becomes true once the mouse moved past the threshold.
    active: bool,
    sources: Vec<PathBuf>,
    /// Started with the right button: a click shows the context menu, a
    /// drop shows the shell's drop menu.
    right: bool,
    /// Left press on a selected item: if no drag follows, the release
    /// leaves only that item selected.
    collapse: bool,
    /// Row the right button went down on.
    index: usize,
}

struct RunningJob {
    id: u64,
    title: String,
    done_verb: &'static str,
    /// Side whose selection is cleared when the job succeeds.
    source_side: Option<usize>,
    handle: JobHandle,
    progress: Progress,
    conflict: Option<Conflict>,
    started: Instant,
    /// Name to put the cursor on in a side once the job is done.
    focus: Option<(usize, String)>,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Key press with the text it produced; `bool` is true if a widget
    /// (e.g. a text input) already used it.
    Key(keyboard::Key, keyboard::Modifiers, Option<String>, bool),
    /// A key was released (tracked so a leaked Enter's repeats can be dropped).
    KeyReleased(keyboard::Key),
    /// The window gained focus.
    WindowFocused,
    /// The window lost focus.
    WindowUnfocused,
    Modifiers(keyboard::Modifiers),
    Run(Command),
    Click(usize, usize),
    MiddleClick(usize, usize),
    RightPress(usize, usize),
    RightClickBackground(usize),
    ClickBackground(usize),
    ContextDone(usize, Result<MenuAction, String>, Option<PathBuf>),
    Hover(usize, usize),
    HoverEnd(usize),
    MouseMoved(Point),
    /// Press on the divider between the panels / double click on it.
    SplitPress,
    /// Press on the edge in front of a column (0 = Ext, 1 = Size, 2 = Modified) in a panel's header.
    ColumnPress(usize, usize),
    /// Mouse button released; `true` for the right button.
    MouseUp(bool),
    DragOutDone(usize, Result<DragOutcome, String>),
    /// Files put on the system clipboard: how many, and whether cut.
    ClipboardSet(Result<usize, String>, bool),
    DropMenuDone(usize, Result<DragOutcome, String>),
    FileDropped(PathBuf),
    DropPosition(Option<(f32, f32)>),
    Wheel(usize, f32),
    /// Scrollbar dragged/clicked: first row to show, rows on screen.
    ScrollTo(usize, usize, usize),
    Drive(usize, String),
    DriveMiddle(usize, String),
    DriveRight(usize, String),
    /// The set of drives changed (mount, eject, USB plug-in, ...).
    DrivesChanged(Vec<String>),
    Sort(usize, SortKey),
    Resized(Size),
    CloseRequested,
    /// Quitting, with the window position read just before.
    Quit(Option<rc_platform::Placement>),
    TabSelect(usize, usize),
    TabClose(usize, usize),
    TabNew(usize),
    TabBarClick(usize),
    EditPath(usize),
    PathInput(String),
    PathSubmit,
    PathPick(PathBuf),
    RenameInput(String),
    RenameSubmit,
    QuickSearchTimeout(u64),
    DialogInput(String),
    DialogSubmit,
    /// Enter in a dialog's text box: activates the focused button.
    DialogEnter,
    DialogCancel,
    HotlistPick(usize),
    /// Open hotlist entry `i` in a new tab (Shift+Enter / middle-click).
    HotlistNewTab(usize),
    HotlistBack,
    HistoryPick(usize),
    /// Run the palette's `n`th shown command (a click).
    PalettePick(usize),
    Job(u64, JobEvent),
    JobPause(u64),
    JobCancel(u64),
    Answer(u64, Resolution),
    /// A watched folder changed on disk.
    FsChanged(PathBuf),
    /// A background listing of a folder finished (`u64`: request number).
    Listed(PathBuf, u64, Result<Vec<Entry>, String>),
    ViewerLoaded(u64, Result<viewer::Loaded, String>),
    /// Search result in a large file: the window holding the match, the
    /// match, and whether the search wrapped around.
    ViewerFound(u64, Result<Option<viewer::Found>, String>, std::sync::Arc<std::sync::atomic::AtomicBool>),
    ViewerWheel(f32),
    /// Viewer scrollbar: the row (whole file) or byte (large file) to show at the top.
    ViewerSeek(f32),
    /// The zoomed image was scrolled (offset in pixels).
    ViewerPanned(iced::widget::scrollable::AbsoluteOffset),
    /// Pointer moved over (or left) the viewer body.
    ViewerPointer(Option<iced::Point>),
    /// Wheel anywhere while an image is shown (zooms when Ctrl is held).
    ViewerZoomWheel(f32),
    ViewerSearch(String),
    ViewerFind,
    /// Left button pressed while the viewer shows a document (starts a selection over the body).
    ViewerPress,
    MultiRename(multi_rename::MrMsg),
    FileDiff(file_diff::DiffMsg),
    Find(find_files::FindMsg),
    Connect(connect::ConnectMsg),
    Settings(settings::SettingsMsg),
    /// Background listing for a pending navigation: (side, seq, listing).
    Navigated(usize, u64, Listing),
    /// A file inside an archive was extracted to a temp copy to be opened.
    Extracted(Result<PathBuf, String>),
    /// A disk image finished mounting on a worker thread.
    Mounted { active: usize, result: Result<Option<String>, String> },
    /// F4 inside an archive: extracted, or time to check the edited copies.
    ArchiveEdit(archive_edit::EditMsg),
    /// Subfolders of a folder, for path bar suggestions.
    Subfolders(PathBuf, Vec<String>),
    /// A folder size: (folder listed, entry name, size).
    DirSize(PathBuf, String, u64),
    /// A background compare finished (`None` = cancelled).
    Compared(Arc<AtomicBool>, Option<Differences>),
    /// Compare dialog option clicked: 0 content, 1 ignore date, 2 subfolders.
    CompareToggle(u8),
    /// A thumbnail was decoded (`None`: the file has none).
    Thumb(thumbs::ThumbKey, Option<image::Handle>),
}

impl App {
    pub fn new(session: Option<Session>) -> (Self, Task<Message>) {
        let home = rc_platform::home_dir();
        let left = std::env::current_dir().ok().or_else(|| home.clone());
        let right = home.clone().or_else(|| left.clone());
        let restore = |state: Option<&SideState>, fallback: Option<PathBuf>| {
            let tabs: Vec<Panel> = state.map(|s| s.tabs.iter().filter_map(TabState::open).collect()).unwrap_or_default();
            if tabs.is_empty() {
                Side { tabs: vec![open_panel(fallback)], current: 0 }
            } else {
                let current = state.map_or(0, |s| s.current).min(tabs.len() - 1);
                Side { tabs, current }
            }
        };
        let (width, height) = session.as_ref().map_or((1280.0, 800.0), |s| (s.width, s.height));

        let mut app = Self {
            sides: [
                restore(session.as_ref().map(|s| &s.left), left),
                restore(session.as_ref().map(|s| &s.right), right),
            ],
            active: session.as_ref().map_or(0, |s| s.active.min(1)),
            drives: rc_platform::roots(),
            status: None,
            window_width: width,
            window_height: height,
            last_click: None,
            last_tab_bar_click: None,
            dialog: None,
            dialog_focus: None,
            conflict_focus: None,
            hotlist: session.as_ref().map(|s| s.hotlist.clone()).unwrap_or_default(),
            recent_commands: session
                .as_ref()
                .map(|s| s.recent_commands.iter().filter_map(|id| Command::from_id(id)).collect())
                .unwrap_or_default(),
            jobs: Vec::new(),
            next_job_id: 1,
            deferred: Vec::new(),
            counts: Vec::new(),
            // `config.toml` `[compare]` replaces these in `refresh_config`.
            compare_opts: session.as_ref().and_then(|s| s.compare).map(Into::into).unwrap_or_default(),
            compare_unsaved: session.as_ref().and_then(|s| s.compare),
            comparing: None,
            path_edit: None,
            rename: None,
            quick_search: None,
            filter_edit: None,
            drag: None,
            dropped: Vec::new(),
            mouse: Point::ORIGIN,
            split: session.as_ref().map_or(0.5, |s| s.split).clamp(0.0, 1.0),
            split_drag: false,
            split_click: None,
            columns: clamp_columns(session.as_ref().map_or(session::DEFAULT_COLUMNS, |s| s.columns)),
            column_drag: None,
            column_click: None,
            hover: None,
            modifiers: keyboard::Modifiers::default(),
            focused: true,
            leaked_enter: false,
            icons: RefCell::new(HashMap::new()),
            thumbs: thumbs::Thumbs::default(),
            disk: RefCell::new(HashMap::new()),
            listings: HashMap::new(),
            next_listing: 0,
            nav: [None, None],
            viewer: None,
            multi_rename: None,
            file_diff: None,
            find: None,
            find_last: None,
            connect: None,
            focus_after_listing: Vec::new(),
            help: None,
            settings: None,
            rename_opts: rc_core::rename::Options::default(),
            rename_undo: Vec::new(),
            config: crate::config::Config::default(),
            keymap: keymap::Keymap::default(),
            config_stamp: None,
            placement: session.as_ref().and_then(|s| s.window).map(Into::into),
            archive_edits: archive_edit::ArchiveEdits::default(),
        };
        app.refresh_config();
        // First start: hidden files as configured.
        if session.is_none() && app.config.show_hidden {
            for side in &mut app.sides {
                for tab in &mut side.tabs {
                    tab.show_hidden = true;
                    let _ = tab.reload();
                }
            }
        }
        let show = match app.placement {
            Some(p) if hidden_until_placed(&session) => restore_window(p),
            _ => Task::none(),
        };
        (app, show)
    }

    pub fn theme(&self) -> Theme {
        theme::theme()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            event::listen_with(|event, status, _window| match event {
                Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, text, .. }) => Some(
                    Message::Key(key, modifiers, text.map(|t| t.to_string()), status == event::Status::Captured),
                ),
                Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) => Some(Message::KeyReleased(key)),
                Event::Window(window::Event::Focused) => Some(Message::WindowFocused),
                Event::Window(window::Event::Unfocused) => Some(Message::WindowUnfocused),
                Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => Some(Message::Modifiers(m)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => Some(Message::MouseUp(false)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)) => Some(Message::MouseUp(true)),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Back)) => {
                    Some(Message::Run(Command::HistoryBack))
                }
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Forward)) => {
                    Some(Message::Run(Command::HistoryForward))
                }
                Event::Window(window::Event::FileDropped(path)) => Some(Message::FileDropped(path)),
                _ => None,
            }),
            window::resize_events().map(|(_id, size)| Message::Resized(size)),
            window::close_requests().map(|_id| Message::CloseRequested),
            watch::drives().map(Message::DrivesChanged),
        ];
        // Ctrl+wheel zooms images; the zoomed image's scrollable captures the
        // wheel, so listen globally instead of through the body's mouse_area.
        if self.viewer.as_ref().is_some_and(|v| v.is_image()) {
            subs.push(event::listen_with(|event, _status, _window| match event {
                Event::Mouse(mouse::Event::WheelScrolled { delta }) => Some(Message::ViewerZoomWheel(match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    // Touchpads: ~120 px per notch-equivalent.
                    mouse::ScrollDelta::Pixels { y, .. } => y / 120.0,
                })),
                _ => None,
            }));
        }
        // Mouse moves re-render the window, so only listen while a drag may
        // be starting or in progress, or the viewer shows a document (mouse
        // selection).
        if self.drag.is_some() || self.split_drag || self.column_drag.is_some() || self.viewer_tracks_mouse() {
            subs.push(event::listen_with(|event, _status, _window| match event {
                Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::MouseMoved(position)),
                _ => None,
            }));
        }
        // Viewer selections start on any left press over the body (seen
        // through `ViewerPointer`): its `mouse_area` doesn't get presses on
        // highlighted rows, since `rich_text` captures presses on its spans.
        if self.viewer_tracks_mouse() {
            subs.push(event::listen_with(|event, _status, _window| match event {
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => Some(Message::ViewerPress),
                _ => None,
            }));
        }
        // Watch the folders the visible tabs show, to refresh them on change.
        // (Archives aren't watched.)
        let mut dirs: Vec<PathBuf> =
            (0..2).map(|side| self.panel(side).path.clone()).filter(|p| !vfs::in_archive(p)).collect();
        dirs.dedup();
        // And the settings folder, to apply `config.toml` edits right away.
        if let Some(config) = crate::config::dir().filter(|d| !dirs.contains(d) && d.is_dir()) {
            dirs.push(config);
        }
        subs.push(watch::folders(dirs).map(Message::FsChanged));
        Subscription::batch(subs)
    }

    fn panel(&self, side: usize) -> &Panel {
        let s = &self.sides[side];
        &s.tabs[s.current]
    }

    fn panel_mut(&mut self, side: usize) -> &mut Panel {
        let s = &mut self.sides[side];
        &mut s.tabs[s.current]
    }

    // ---- update -----------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.handle(message);
        let thumbs = self.pump_thumbs();
        if self.deferred.is_empty() {
            return Task::batch([task, thumbs]);
        }
        Task::batch([task, thumbs].into_iter().chain(self.deferred.drain(..)))
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        if !self.counts.is_empty() {
            self.prune_counts();
        }
        // Grid layouts follow the last drawn size of each side.
        self.sync_grids();
        match message {
            Message::Thumb(key, handle) => self.thumb_done(key, handle),
            Message::Key(key, mods, text, captured) => return self.on_key(key, mods, text, captured),
            Message::KeyReleased(key) => {
                if let keyboard::Key::Named(Named::Enter) = key {
                    self.leaked_enter = false;
                }
            }
            Message::WindowFocused => self.focused = true,
            Message::WindowUnfocused => self.focused = false,
            Message::Modifiers(m) => self.modifiers = m,
            Message::Run(cmd) => return self.run(cmd),
            Message::Click(side, index) => {
                self.leave_edits();
                self.active = side;
                let (ctrl, shift) = (self.modifiers.control(), self.modifiers.shift());
                if ctrl || shift {
                    // Selecting: Ctrl+click toggles, Shift+click picks the
                    // range from the anchor, Ctrl+Shift+click adds it. No
                    // drag, no double click.
                    let panel = self.panel_mut(side);
                    if shift {
                        panel.select_range_to(index, ctrl);
                    } else {
                        panel.toggle_at(index);
                    }
                    self.drag = None;
                    self.last_click = None;
                    return Task::none();
                }
                let panel = self.panel_mut(side);
                // Explorer behaviour: a plain click drops the selection; on a
                // selected item that waits for the release, so the whole
                // selection can still be dragged.
                let on_selected = panel.click_at(index);
                self.drag = Some(Drag {
                    side,
                    start: None,
                    active: false,
                    sources: Vec::new(),
                    right: false,
                    collapse: on_selected,
                    index,
                });
                let now = Instant::now();
                let double = matches!(
                    self.last_click,
                    Some((s, i, t)) if s == side && i == index && now - t < DOUBLE_CLICK
                );
                if double {
                    self.last_click = None;
                    self.drag = None;
                    return self.run(Command::Open);
                }
                self.last_click = Some((side, index, now));
            }
            Message::MiddleClick(side, index) => {
                self.leave_edits();
                let entry = self.panel(side).entries.get(index);
                if let Some(path) = entry.filter(|e| e.is_dir()).map(|e| e.path.clone()) {
                    return self.open_tab(side, path, false);
                }
            }
            Message::RightPress(side, index) => {
                self.leave_edits();
                self.active = side;
                let panel = self.panel_mut(side);
                let Some(entry) = panel.entries.get(index) else { return Task::none() };
                // Explorer behaviour: right-clicking outside the selection
                // makes the clicked item the only target.
                if !entry.is_parent() && !panel.is_selected(entry) {
                    panel.deselect_all();
                    panel.set_cursor(index);
                }
                // The context menu opens on release, unless this turns into
                // a right-button drag.
                self.drag = Some(Drag {
                    side,
                    start: None,
                    active: false,
                    sources: Vec::new(),
                    right: true,
                    collapse: false,
                    index,
                });
            }
            Message::ClickBackground(side) => {
                self.leave_edits();
                self.active = side;
                self.panel_mut(side).deselect_all();
            }
            Message::RightClickBackground(side) => {
                self.leave_edits();
                self.active = side;
                return self.context_menu(side, true);
            }
            Message::ContextDone(side, result, made) => match result {
                Ok(MenuAction::Nothing) => {}
                Ok(MenuAction::Rename) => {
                    self.active = side;
                    return self.run(Command::Rename);
                }
                Ok(MenuAction::Delete) => {
                    self.active = side;
                    return self.run(Command::Delete);
                }
                Ok(MenuAction::Invoked(_verb)) => {
                    // "New > Folder" made "New folder" without asking: take
                    // it back (it is empty and was just created) and ask for
                    // the name like F7 does.
                    if made.is_some_and(|dir| std::fs::remove_dir(dir).is_ok()) {
                        self.active = side;
                        self.reload_panels();
                        return self.run(Command::MakeDir);
                    }
                    // Verbs that finish later (paste, extract, ...) are
                    // picked up by the folder watcher.
                    self.reload_panels();
                }
                Err(e) => self.status = Some((e, Tone::Error)),
            },
            Message::Hover(side, index) => self.hover = Some((side, index)),
            Message::HoverEnd(side) => {
                if self.hover.is_some_and(|(s, _)| s == side) {
                    self.hover = None;
                }
            }
            Message::MouseMoved(position) => {
                self.mouse = position;
                if self.split_drag {
                    let room = (self.window_width - SPLIT_W).max(1.0);
                    self.split = (position.x - SPLIT_W / 2.0) / room;
                    self.split = self.left_width() / room;
                    self.sync_grids();
                }
                self.column_dragged(position.x);
                let pan = self.viewer_mouse_moved();
                return Task::batch([pan, self.update_drag()]);
            }
            Message::SplitPress => {
                let now = Instant::now();
                if matches!(self.split_click, Some(t) if now - t < DOUBLE_CLICK) {
                    self.split_click = None;
                    self.split_drag = false;
                    self.split = 0.5;
                    self.sync_grids();
                } else {
                    self.split_click = Some(now);
                    self.split_drag = true;
                }
            }
            Message::ColumnPress(side, col) => {
                let now = Instant::now();
                if matches!(self.column_click, Some((c, t)) if c == col && now - t < DOUBLE_CLICK) {
                    self.column_click = None;
                    self.column_drag = None;
                    self.columns[col] = session::DEFAULT_COLUMNS[col];
                } else {
                    self.column_click = Some((col, now));
                    self.column_press(side, col);
                }
            }
            Message::MouseUp(right) => {
                self.split_drag = false;
                self.column_drag = None;
                if !right {
                    self.viewer_release();
                }
                return self.finish_drag(right);
            }
            Message::ClipboardSet(result, cut) => {
                self.status = Some(match result {
                    Ok(n) => {
                        let verb = if cut { "Cut" } else { "Copied" };
                        let what = if n == 1 { "1 item".to_owned() } else { format!("{n} items") };
                        (format!("{verb} {what} to the clipboard"), Tone::Success)
                    }
                    Err(e) => (format!("Can't use the clipboard: {e}"), Tone::Error),
                });
                return Task::none();
            }
            Message::DragOutDone(side, result) => {
                self.reload_panels();
                // Dropped back onto our own window: that drop is handled as
                // FileDropped, and the effect reported by the window's drop
                // target isn't meaningful.
                let dropped_on_self = !self.dropped.is_empty() || matches!(self.dialog, Some(Dialog::Transfer { .. }));
                if dropped_on_self {
                    return Task::none();
                }
                match result {
                    Ok(DragOutcome::Cancelled) => {}
                    Ok(outcome) => {
                        let verb = match outcome {
                            DragOutcome::Moved => "Moved",
                            DragOutcome::Linked => "Linked",
                            _ => "Copied",
                        };
                        self.status = Some((format!("{verb} to another app"), Tone::Success));
                        if outcome == DragOutcome::Moved {
                            self.panel_mut(side).deselect_all();
                        }
                    }
                    Err(e) => self.status = Some((e, Tone::Error)),
                }
            }
            Message::DropMenuDone(side, result) => {
                self.reload_panels();
                match result {
                    Ok(DragOutcome::Moved) => self.panel_mut(side).deselect_all(),
                    Ok(_) => {}
                    Err(e) => self.status = Some((e, Tone::Error)),
                }
            }
            Message::FileDropped(path) => {
                self.dropped.push(path);
                // Several events arrive for one drop; ask for the position once.
                if self.dropped.len() == 1 {
                    return window::get_oldest()
                        .and_then(|id| {
                            window::run_with_handle(id, |h| rc_platform::cursor_in_window(native_handle(&h)))
                        })
                        .map(Message::DropPosition);
                }
            }
            Message::DropPosition(position) => {
                let sources = std::mem::take(&mut self.dropped);
                self.status = None;
                let side = position.map(|(x, _)| usize::from(x >= self.split_x())).unwrap_or(self.active);
                self.active = side;
                let target = self.panel(side).path.clone();
                return self.open_transfer(false, sources, &target);
            }
            Message::Wheel(side, lines) => {
                let rows = self.page_rows();
                let panel = self.panel_mut(side);
                // Three lines a notch in the list, one row of thumbnails.
                let step = if panel.thumbnails { -lines } else { -lines * 3.0 };
                panel.scroll_by(step.round() as isize, rows);
            }
            Message::ScrollTo(side, start, rows) => self.panel_mut(side).scroll_to(start, rows),
            Message::Drive(side, drive) => {
                self.leave_edits();
                self.active = side;
                self.drives = rc_platform::roots();
                return self.start_nav(side, Nav::to(drive));
            }
            Message::DriveMiddle(side, drive) => {
                self.leave_edits();
                self.drives = rc_platform::roots();
                return self.open_tab(side, PathBuf::from(drive), false);
            }
            Message::DriveRight(side, drive) => {
                self.leave_edits();
                self.active = side;
                return self.drive_context_menu(side, drive);
            }
            Message::DrivesChanged(drives) => self.drives = drives,
            Message::Sort(side, key) => {
                self.active = side;
                self.panel_mut(side).sort_by(key);
            }
            Message::CloseRequested => return self.quit(),
            Message::Quit(placement) => {
                if placement.is_some() {
                    self.placement = placement;
                }
                self.archive_edits_flush();
                self.save_session();
                return iced::exit();
            }
            // Minimising reports a zero size; keep the last real one.
            Message::Resized(size) if size.width > 0.0 && size.height > 0.0 => {
                self.window_width = size.width;
                self.window_height = size.height;
            }
            Message::Resized(_) => {}
            Message::TabSelect(side, index) => {
                self.leave_edits();
                self.active = side;
                return self.select_tab(side, index);
            }
            Message::TabClose(side, index) => {
                self.leave_edits();
                return self.close_tab(side, index);
            }
            Message::TabBarClick(side) => {
                let now = Instant::now();
                let double = matches!(self.last_tab_bar_click, Some((s, t)) if s == side && now - t < DOUBLE_CLICK);
                self.last_tab_bar_click = (!double).then_some((side, now));
                if double {
                    return self.update(Message::TabNew(side));
                }
            }
            Message::TabNew(side) => {
                self.leave_edits();
                self.active = side;
                let path = self.panel(side).path.clone();
                return self.open_tab(side, path, true);
            }
            Message::EditPath(side) => {
                self.leave_edits();
                self.active = side;
                let text = remote::display(&self.panel(side).path);
                self.path_edit = Some(PathEdit { side, text, suggestions: Vec::new(), selected: None, cache: None });
                return Task::batch([text_input::focus(path_input_id()), text_input::select_all(path_input_id())]);
            }
            Message::PathInput(value) => {
                if let Some(edit) = &mut self.path_edit {
                    edit.text = value;
                }
                return self.refresh_suggestions();
            }
            Message::Compared(token, diff) => {
                if self.comparing.as_ref().is_some_and(|(c, ..)| Arc::ptr_eq(c, &token)) {
                    let (_, left, right) = self.comparing.take().unwrap();
                    match diff {
                        Some(diff) if self.panel(0).path == left && self.panel(1).path == right => {
                            self.apply_compare(diff)
                        }
                        _ => self.status = Some(("Compare cancelled".into(), Tone::Info)),
                    }
                }
            }
            Message::CompareToggle(i) => {
                if let Some(Dialog::Compare { opts }) = &mut self.dialog {
                    toggle_compare_option(opts, i);
                }
            }
            Message::DirSize(dir, name, size) => {
                for panel in self.sides.iter_mut().flat_map(|s| s.tabs.iter_mut()).filter(|p| p.path == dir) {
                    panel.set_dir_size(&name, size);
                }
            }
            Message::Subfolders(dir, names) => {
                if let Some(edit) = &mut self.path_edit {
                    if edit.cache.as_ref().is_some_and(|(cached, _)| *cached == dir) {
                        edit.cache = Some((dir, Some(names)));
                        return self.refresh_suggestions();
                    }
                }
            }
            Message::PathSubmit => {
                let picked = self
                    .path_edit
                    .as_ref()
                    .and_then(|e| e.selected.and_then(|i| e.suggestions.get(i).cloned()));
                return match picked {
                    Some(path) => self.go_to_path(path),
                    None => self.submit_path(),
                };
            }
            Message::PathPick(path) => return self.go_to_path(path),
            Message::RenameInput(value) => {
                if let Some(rename) = &mut self.rename {
                    rename.text = value;
                }
            }
            Message::RenameSubmit => self.commit_rename(),
            Message::QuickSearchTimeout(seq) => {
                if self.quick_search.as_ref().is_some_and(|q| q.seq == seq) {
                    self.quick_search = None;
                }
            }
            Message::DialogInput(value) => {
                if let Some(
                    Dialog::Transfer { input, .. }
                    | Dialog::MakeDir { input }
                    | Dialog::EditNew { input }
                    | Dialog::Pack { input, .. }
                    | Dialog::Unpack { input, .. }
                    | Dialog::Password { input, .. },
                ) = &mut self.dialog
                {
                    *input = value;
                }
            }
            Message::DialogSubmit => return self.submit_dialog(),
            Message::DialogEnter => return self.dialog_enter(),
            Message::DialogCancel => {
                self.dialog = None;
                self.dialog_focus = None;
            }
            Message::HotlistPick(i) => return self.hotlist_pick(i),
            Message::HotlistNewTab(i) => return self.hotlist_new_tab(i),
            Message::HotlistBack => self.hotlist_back(),
            Message::HistoryPick(i) => return self.history_pick(i),
            Message::PalettePick(i) => return self.palette_pick(i),
            Message::Job(id, event) => return self.on_job_event(id, event),
            Message::JobPause(id) => {
                if let Some(job) = self.job(id) {
                    let control = &job.handle.control;
                    control.set_paused(!control.is_paused());
                }
            }
            Message::JobCancel(id) => {
                if let Some(job) = self.job(id) {
                    job.handle.cancel();
                    job.conflict = None;
                }
            }
            Message::Answer(id, resolution) => {
                self.conflict_focus = None;
                if let Some(job) = self.job(id) {
                    job.conflict = None;
                    job.handle.answer(resolution);
                }
            }
            Message::FsChanged(path) => {
                if crate::config::dir().as_ref() == Some(&path) {
                    self.refresh_config();
                    if (0..2).all(|side| self.panel(side).path != path) {
                        return Task::none();
                    }
                }
                return self.list_in_background(path);
            }
            Message::Listed(path, seq, result) => self.apply_listing(path, seq, result),
            Message::Extracted(result) => {
                self.status = Some(match result.and_then(|p| launch(&p).map_err(|e| e.to_string())) {
                    Ok(msg) => (msg, Tone::Success),
                    Err(e) => (e, Tone::Error),
                });
            }
            Message::Mounted { active, result } => match result {
                Ok(new) => {
                    self.drives = rc_platform::roots();
                    if let Some(drive) = new {
                        self.status = Some((format!("Mounted as {drive}"), Tone::Success));
                        return self.start_nav(active, Nav::to(PathBuf::from(drive)));
                    }
                    // No new drive letter: already mounted, or still coming.
                    self.status = Some(("Mounted (no new drive letter appeared)".to_string(), Tone::Info));
                }
                Err(e) => self.status = Some((e, Tone::Error)),
            },
            Message::ViewerLoaded(seq, result) => return self.viewer_loaded(seq, result),
            Message::ViewerFound(seq, result, cancel) => self.viewer_found(seq, result, &cancel),
            Message::ViewerWheel(lines) => self.viewer_wheel(lines),
            Message::ViewerSeek(at) => self.viewer_seek(at),
            Message::ViewerPanned(offset) => self.viewer_panned(offset),
            Message::ViewerPointer(at) => self.viewer_pointer(at),
            Message::ViewerZoomWheel(lines) => return self.viewer_zoom_wheel(lines),
            Message::ViewerSearch(text) => self.viewer_search_input(text),
            Message::ViewerFind => return self.viewer_submit(),
            Message::ViewerPress => self.viewer_press(),
            Message::MultiRename(msg) => return self.multi_rename_update(msg),
            Message::FileDiff(msg) => return self.file_diff_update(msg),
            Message::Find(msg) => return self.find_update(msg),
            Message::Connect(msg) => return self.connect_update(msg),
            Message::Settings(msg) => return self.settings_update(msg),
            Message::ArchiveEdit(msg) => return self.archive_edit_update(msg),
            Message::Navigated(side, seq, result) => {
                let current = self.sides[side].current;
                let pending = self.nav[side].take_if(|p| p.seq == seq && p.tab == current);
                // Abandoned, superseded, or the history changed meanwhile.
                if let Some(p) = pending.filter(|p| self.panel(side).nav_target(&p.nav).as_ref() == Some(&p.path)) {
                    return self.finish_nav(side, p.nav, result);
                }
            }
        }
        Task::none()
    }

    /// Panel keys and mouse actions outside the keymap (`on_key`, mouse
    /// handling), for the F1 help. Keep in sync.
    const PANEL_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Letters, digits", "Quick search: jump to the next name starting with the typed text"),
        ("Up, Down (quick search)", "Previous / next match"),
        ("Backspace, Esc (quick search)", "Delete a character / end the quick search"),
        ("Typing, Backspace (quick filter)", "Edit the filter; Esc drops it, other keys keep it"),
        ("Esc", "Cancel a slow folder change, drop the filter, then clear the selection"),
        ("Dialogs: Left/Right, Tab", "Move the focus between the buttons (Left/Right only without a text box); Enter presses the focused one, Esc cancels"),
        ("F2 / Shift+F6 (renaming)", "Select the name without extension, the extension, the whole name; Enter renames, Esc cancels"),
        ("Click, double-click", "Move the cursor and drop the selection (a selected item keeps it while dragging), open"),
        ("Ctrl+click", "Select / unselect one item"),
        ("Shift+click", "Select the range from the last clicked item to this one"),
        ("Ctrl+Shift+click", "Add that range to the selection"),
        ("Shift+Up/Down/PgUp/PgDn/Home/End", "Move and select the range from the cursor's starting point"),
        ("Thumbnail view (Ctrl+Shift+F5)", "Arrows move in the grid (Shift+arrows select), PgUp/PgDn by a screen; Enter, Space/Ins, quick search, clicks and dragging work as in the list. Picture size: thumbnail_size in config.toml"),
        ("Middle-click a folder or drive", "Open it in a new tab"),
        ("Right-click a drive", "Windows menu for the drive"),
        ("Enter on a .iso, .img, .vhd, .vhdx", "Mount the disk image (Windows' Mount) and open the new drive"),
        ("Enter / Ctrl+PgDn on a .zip, .7z, .rar, .tar, .tar.gz, .gz, ...", "Browse the archive like a folder: view, copy out, Enter opens a temp copy; in a zip also copy/move in (also from other archives), F7, F2, F8 (others are read-only)"),
        ("Ctrl+F, then Enter", "Connect to an SFTP/FTP/FTPS server: browse, F3 view, F5/F6 download and upload, F7, F2, F8 work there; Ctrl+Shift+F disconnects"),
        ("Right-click", "Explorer context menu"),
        ("Drag and drop", "Copy (Shift = move) between panels and other apps; right drag = drop menu"),
        ("Mouse wheel", "Scroll the view (the cursor stays)"),
        ("Double-click the empty tab bar", "New tab"),
        ("Mouse back / forward", "Previous / next folder"),
        ("Click the path bar", "Edit the path: Tab completes, Up/Down pick a suggestion, Enter goes, Esc cancels"),
        ("Click a drive", "Change drive"),
        ("Tab bar: click, middle-click", "Switch tab, close tab"),
    ];

    fn on_key(
        &mut self,
        key: keyboard::Key,
        mods: keyboard::Modifiers,
        text: Option<String>,
        captured: bool,
    ) -> Task<Message> {
        self.refresh_config();
        let named = match &key {
            keyboard::Key::Named(n) => Some(*n),
            _ => None,
        };
        let escape = named == Some(Named::Escape);

        // A key press arriving while we still think we're unfocused is winit's
        // synthetic key-down for a key held in the window we just left (it
        // comes before the `Focused` event); ignore it, and for Enter also its
        // auto-repeats until the key-up, so a terminal's `exit` + Enter doesn't
        // open the selected file.
        if !self.focused {
            if named == Some(Named::Enter) {
                self.leaked_enter = true;
            }
            return Task::none();
        }
        if named == Some(Named::Enter) && self.leaked_enter {
            return Task::none();
        }

        // A pending overwrite question blocks everything else.
        if let Some(id) = self.jobs.iter().find(|j| j.conflict.is_some()).map(|j| j.id) {
            if escape {
                return self.update(Message::Answer(id, Resolution::Cancel));
            }
            let count = button_focus::CONFLICT_ANSWERS.len();
            match named {
                Some(Named::ArrowLeft) => self.conflict_focus = Some(button_focus::step(self.conflict_focus, count, true)),
                Some(Named::ArrowRight) => {
                    self.conflict_focus = Some(button_focus::step(self.conflict_focus, count, false));
                }
                Some(Named::Tab) => {
                    self.conflict_focus = Some(button_focus::step(self.conflict_focus, count, mods.shift()));
                }
                Some(Named::Enter) => {
                    if let Some(i) = self.conflict_focus {
                        return self.update(Message::Answer(id, button_focus::CONFLICT_ANSWERS[i].1));
                    }
                }
                _ => {}
            }
            return Task::none();
        }
        if self.help.is_some() {
            if named.is_some_and(is_modifier_key) {
                return Task::none();
            }
            return self.help_key(named, &key, mods, text.as_deref());
        }
        if self.settings.is_some() {
            if named.is_some_and(is_modifier_key) {
                return Task::none();
            }
            return self.settings_key(named, &key, mods, captured);
        }
        if self.multi_rename.is_some() {
            if named.is_some_and(is_modifier_key) {
                return Task::none();
            }
            return self.multi_rename_key(named, &key, mods, captured);
        }
        if self.find.is_some() {
            if named.is_some_and(is_modifier_key) {
                return Task::none();
            }
            return self.find_key(named, &key, mods, captured);
        }
        if self.connect.is_some() {
            if named.is_some_and(is_modifier_key) {
                return Task::none();
            }
            return self.connect_key(named, &key, mods, captured);
        }
        if self.file_diff.is_some() {
            if !named.is_some_and(is_modifier_key) {
                self.file_diff_key(named, &key, mods);
            }
            return Task::none();
        }
        if self.viewer.is_some() {
            // Keys the search box used must not act again: in a fast burst
            // its submit (closing the box) is handled before the typed
            // letters arrive here.
            if named.is_some_and(is_modifier_key) || captured && !escape {
                return Task::none();
            }
            return self.viewer_key(&key, mods);
        }
        if let Some(Dialog::Hotlist(_) | Dialog::History { .. } | Dialog::Palette(_)) = self.dialog {
            if named.is_some_and(is_modifier_key) {
                return Task::none();
            }
            let task = match self.dialog {
                Some(Dialog::History { .. }) => self.history_key(named, text.as_deref()),
                Some(Dialog::Palette(_)) => self.palette_key(named, &key, mods, text.as_deref()),
                _ => self.hotlist_key(named, text.as_deref(), mods),
            };
            return Task::batch([task, self.popup_snap()]);
        }
        if let Some(Dialog::Compare { opts }) = &mut self.dialog {
            let option = match text.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("c") => Some(0),
                Some("d") => Some(1),
                Some("s") => Some(2),
                _ => None,
            };
            if let Some(i) = option {
                toggle_compare_option(opts, i);
                return Task::none();
            }
        }
        if self.dialog.is_some() {
            // Dialogs with a text box keep Left/Right for its cursor.
            let has_input = !matches!(self.dialog, Some(Dialog::Delete { .. } | Dialog::Compare { .. }));
            let back = match named {
                Some(Named::Tab) => Some(mods.shift()),
                Some(Named::ArrowLeft) if !has_input => Some(true),
                Some(Named::ArrowRight) if !has_input => Some(false),
                _ => None,
            };
            if escape {
                self.dialog = None;
                self.dialog_focus = None;
            } else if let Some(back) = back {
                self.dialog_focus = Some(button_focus::step(Some(self.dialog_focus.unwrap_or(1)), 2, back));
            } else if named == Some(Named::Enter) && !captured {
                return self.dialog_enter();
            }
            return Task::none();
        }
        if let Some(rename) = &mut self.rename {
            if escape {
                self.rename = None;
            } else if self.keymap.command(&key, mods) == Some(Command::Rename) {
                // F2 / Shift+F6 again: stem → extension → whole name.
                rename.select(rename.part.next(&rename.text, rename.is_dir));
            }
            return Task::none();
        }
        if self.path_edit.is_some() {
            return self.path_edit_key(named);
        }
        if escape && self.nav[self.active].is_some() {
            self.nav[self.active] = None;
            self.status = Some(("Cancelled".into(), Tone::Info));
            return Task::none();
        }
        if escape && self.drag.as_ref().is_some_and(|d| d.active) {
            self.drag = None;
            return Task::none();
        }
        if captured {
            return Task::none();
        }

        // Modifier and lock keys on their own (e.g. Shift before a capital
        // letter) must not end a quick search or trigger anything.
        if named.is_some_and(is_modifier_key) {
            return Task::none();
        }

        // Quick search: plain typing jumps to matching names.
        let plain = !mods.control() && !mods.alt() && !mods.logo();
        let typed = text.filter(|t| plain && !t.is_empty() && !t.chars().any(char::is_control));
        if self.filter_edit == Some(self.active) {
            let mut filter = self.panel(self.active).filter().to_owned();
            if let Some(t) = &typed {
                filter.push_str(t);
                return self.set_filter(filter);
            }
            match named {
                Some(Named::Backspace) => {
                    filter.pop();
                    return self.set_filter(filter);
                }
                Some(Named::Escape) => {
                    self.filter_edit = None;
                    return self.set_filter(String::new());
                }
                // Moving the cursor keeps the filter box open.
                Some(
                    Named::ArrowUp | Named::ArrowDown | Named::PageUp | Named::PageDown | Named::Home | Named::End,
                ) if !mods.control() && !mods.alt() => {}
                _ => {
                    self.filter_edit = None;
                    // Ctrl+S again just closes the box, keeping the filter.
                    if self.keymap.command(&key, mods) == Some(Command::QuickFilter) {
                        return Task::none();
                    }
                }
            }
        } else if escape && !self.panel(self.active).filter().is_empty() {
            // Esc drops an active filter before it clears the selection.
            return self.set_filter(String::new());
        }

        let searching = self.quick_search.as_ref().is_some_and(|q| q.side == self.active);
        if let Some(t) = typed {
            if t != " " || searching {
                return self.quick_search_type(&t);
            }
        }
        if searching {
            match named {
                Some(Named::Backspace) => return self.quick_search_back(),
                Some(Named::Escape) => {
                    self.quick_search = None;
                    return Task::none();
                }
                Some(Named::ArrowDown) => return self.quick_search_step(true),
                Some(Named::ArrowUp) => return self.quick_search_step(false),
                _ => self.quick_search = None,
            }
        }

        match self.keymap.command(&key, mods) {
            Some(cmd) => self.run(cmd),
            None => Task::none(),
        }
    }

    #[allow(clippy::unit_arg)] // `Ok(panel.move_cursor(..))` reads better here
    fn run(&mut self, cmd: Command) -> Task<Message> {
        use Command::*;
        self.status = None;
        self.quick_search = None;
        let rows = self.page_rows();
        let page = rows.saturating_sub(1).max(1) as isize;
        let active = self.active;
        let other = 1 - active;

        // In archives: zips, tars and gz files can be changed (as jobs), others can't; nothing can
        // be edited in place. `bool` = allowed in a zip or tar.
        let refused = match cmd {
            // Files in archives are edited as temp copies (`archive_edit`).
            Edit | EditNew if remote::is_remote(&self.panel(active).path) => Some(("edit files", false)),
            Edit => None,
            // A new file in an archive is added, then edited as a temp copy.
            EditNew => Some(("create files", true)),
            MultiRename => Some(("multi-rename", false)),
            MakeDir => Some(("create folders", true)),
            Rename => Some(("rename", true)),
            Delete | DeletePermanent => Some(("delete", true)),
            _ => None,
        };
        if let Some((what, zip_ok)) = refused {
            if self.refuse_in_archive(active, what, zip_ok) {
                return Task::none();
            }
        }

        // Commands that touch more than the active panel or open dialogs.
        match cmd {
            Quit => return self.quit(),
            View => return self.open_viewer(),
            MultiRename => return self.open_multi_rename(),
            CompareFiles => return self.open_file_diff(),
            FindFiles => return self.open_find(),
            Connect => return self.open_connect(),
            Disconnect => return self.disconnect(),
            Help => return self.open_help(),
            Configuration => return self.open_settings(),
            CommandPalette => return self.open_palette(),
            Edit => {
                if let Some(e) = self.panel(active).current().filter(|e| e.kind == EntryKind::File) {
                    let (path, name) = (e.path.clone(), e.name.clone());
                    if vfs::is_virtual(&path) {
                        return self.edit_in_archive(path);
                    }
                    self.edit_file(&path, &name);
                }
                return Task::none();
            }
            EditConfig => {
                if let Err(e) = crate::config::create_if_missing("", Some(self.compare_opts.into())) {
                    self.status = Some((format!("Can't create config.toml: {e}"), Tone::Error));
                } else if let Some(path) = crate::config::file() {
                    self.edit_file(&path, "config.toml");
                }
                return Task::none();
            }
            EditNew => {
                let input = self
                    .panel(active)
                    .current()
                    .filter(|e| e.kind == EntryKind::File)
                    .map_or_else(String::new, |e| e.name.clone());
                return self.open_dialog(Dialog::EditNew { input });
            }
            SwitchPanel => {
                self.active = other;
                return Task::none();
            }
            SyncOtherPanel => {
                let path = self.panel(active).path.clone();
                return self.start_nav(other, Nav::to(path));
            }
            Copy | Move => {
                let sources = self.target_paths(active);
                if sources.is_empty() {
                    return Task::none();
                }
                let target = self.panel(other).path.clone();
                return self.open_transfer(cmd == Move, sources, &target);
            }
            CopyToClipboard | CutToClipboard => return self.clip_files(cmd == CutToClipboard),
            PasteFromClipboard => return self.paste_files(),
            Rename => return self.start_rename(),
            MakeDir => return self.open_dialog(Dialog::MakeDir { input: String::new() }),
            Pack => return self.open_pack(),
            Unpack => return self.open_unpack(),
            Terminal => {
                self.open_terminal();
                return Task::none();
            }
            Hotlist => {
                self.open_hotlist();
                return self.popup_snap();
            }
            DirHistory => {
                self.open_history();
                return self.popup_snap();
            }
            Delete | DeletePermanent => {
                let paths = self.target_paths(active);
                if paths.is_empty() {
                    return Task::none();
                }
                self.dialog = Some(Dialog::Delete { paths, to_trash: cmd == Delete });
                self.dialog_focus = None;
                return Task::none();
            }
            NewTab => {
                let path = self.panel(active).path.clone();
                return self.open_tab(active, path, true);
            }
            CloseTab => return self.close_tab(active, self.sides[active].current),
            NextTab | PrevTab => {
                let s = &self.sides[active];
                let n = s.tabs.len();
                let next = if cmd == NextTab { (s.current + 1) % n } else { (s.current + n - 1) % n };
                return self.select_tab(active, next);
            }
            OpenInNewTab => {
                let dir = self.panel(active).current().filter(|e| e.is_dir()).map(|e| e.path.clone());
                return dir.map_or(Task::none(), |path| self.open_tab(active, path, true));
            }
            Refresh => {
                self.drives = rc_platform::roots();
                self.disk.borrow_mut().clear();
                let path = self.panel(active).path.clone();
                return self.list_in_background(path);
            }
            QuickFilter => {
                if self.filter_edit == Some(active) {
                    self.filter_edit = None;
                } else {
                    self.leave_edits();
                    self.filter_edit = Some(active);
                }
                return Task::none();
            }
            ToggleSelect { advance } => {
                let panel = self.panel_mut(active);
                // Space also counts a folder's size, like in TC.
                let dir = panel.current().filter(|e| !advance && e.kind == EntryKind::Dir).map(|e| e.name.clone());
                panel.toggle_select_current();
                panel.mark_anchor();
                if advance {
                    panel.move_cursor(1);
                }
                panel.ensure_visible(rows);
                return self.count_dirs(active, dir.into_iter().collect());
            }
            CompareDirs => return self.compare_dirs(),
            CompareDirsOptions => {
                self.dialog = Some(Dialog::Compare { opts: self.compare_opts });
                self.dialog_focus = None;
                return Task::none();
            }
            CountDirSizes => {
                let names = self.panel(active).entries.iter().filter(|e| e.kind == EntryKind::Dir);
                let names = names.map(|e| e.name.clone()).collect();
                return self.count_dirs(active, names);
            }
            Thumbnails => {
                self.toggle_thumbnails();
                return Task::none();
            }
            ToggleHidden => {
                let panel = self.panel_mut(active);
                panel.show_hidden = !panel.show_hidden;
                let state = if panel.show_hidden { "shown" } else { "hidden" };
                self.status = Some((format!("Hidden files {state}"), Tone::Info));
                let path = self.panel(active).path.clone();
                return self.list_in_background(path);
            }
            _ => {}
        }

        // Folder changes list the target first, maybe in the background.
        let nav = match cmd {
            Open => match self.panel(active).activate() {
                Activated::Navigate(nav) => Some(nav),
                _ => None,
            },
            GoParent => self.panel(active).parent_nav(),
            GoRoot => self.panel(active).root_nav(),
            HistoryBack => Some(Nav::Back(1)),
            HistoryForward => Some(Nav::Forward(1)),
            _ => None,
        };
        if let Some(nav) = nav {
            return self.start_nav(active, nav);
        }

        if cmd == DeselectAll {
            if let Some((cancel, ..)) = self.comparing.take() {
                cancel.store(true, Ordering::Relaxed);
                self.status = Some(("Compare cancelled".into(), Tone::Info));
            }
            self.cancel_counts();
        }
        let extend = self.modifiers.shift();
        let panel = self.panel_mut(active);

        // Enter on a file: open it (or mount a disk image). Handled before
        // the shared panel work below because mounting needs &mut self for
        // the drive bar and navigation.
        if cmd == Open {
            if let Activated::File(path) = panel.activate() {
                // Inside an archive: extract to a temp copy first.
                if vfs::is_virtual(&path) {
                    return open_extracted(path);
                }
                // A disk image mounts instead of opening, like Explorer.
                if is_disk_image(&path) {
                    return mount_disk_image(path, active);
                }
                // Plain file: open with the default app.
                match launch(&path) {
                    Ok(msg) => self.status = Some((msg, Tone::Success)),
                    Err(e) => self.report(Err(e)),
                }
                return Task::none();
            }
            // Folder / `..` falls through to the shared navigation below.
        }

        let result: io::Result<()> = match cmd {
            // Left / Right only move in the thumbnail grid.
            CursorLeft | CursorRight if !panel.thumbnails => Ok(()),
            CursorUp | CursorDown | PageUp | PageDown | CursorHome | CursorEnd | CursorLeft | CursorRight => {
                let last = panel.entries.len().saturating_sub(1);
                let target = if panel.thumbnails {
                    let page = panel.grid().1.saturating_sub(1).max(1) as isize;
                    match cmd {
                        CursorUp => panel.grid_target(0, -1),
                        CursorDown => panel.grid_target(0, 1),
                        CursorLeft => panel.grid_target(-1, 0),
                        CursorRight => panel.grid_target(1, 0),
                        PageUp => panel.grid_target(0, -page),
                        PageDown => panel.grid_target(0, page),
                        CursorHome => 0,
                        _ => last,
                    }
                } else {
                    let to = match cmd {
                        CursorUp => -1,
                        CursorDown => 1,
                        PageUp => -page,
                        PageDown => page,
                        CursorHome => -(last as isize) - 1,
                        _ => last as isize + 1,
                    };
                    (panel.cursor as isize + to).clamp(0, last as isize) as usize
                };
                // Shift+cursor keys extend the selection from the anchor.
                if extend {
                    panel.select_range_to(target, false);
                } else {
                    panel.set_cursor(target);
                    panel.mark_anchor();
                }
                Ok(())
            }
            CursorBy(n) => Ok(panel.move_cursor(n as isize)),
            // At the root / no history: nothing to do.
            // Open on a non-file (or no current entry) is handled above.
            GoParent | GoRoot | HistoryBack | HistoryForward | Open => Ok(()),
            SelectAll => Ok(panel.select_all()),
            DeselectAll => Ok(panel.deselect_all()),
            SortBy(key) => Ok(panel.sort_by(key)),
            _ => unreachable!("handled above"),
        };
        panel.ensure_visible(rows);
        self.report(result);
        Task::none()
    }

    fn target_paths(&self, side: usize) -> Vec<PathBuf> {
        self.panel(side).targets().into_iter().map(|e| e.path.clone()).collect()
    }

    /// Ends an in-place rename (keeping the new name), path editing and
    /// quick search — e.g. when the user clicks elsewhere.
    fn leave_edits(&mut self) {
        self.path_edit = None;
        self.quick_search = None;
        self.filter_edit = None;
        if self.rename.is_some() {
            self.commit_rename();
        }
    }

    // ---- tabs -------------------------------------------------------------

    fn open_tab(&mut self, side: usize, path: PathBuf, switch: bool) -> Task<Message> {
        if !switch {
            // Opened in the background: list it now, as there's nowhere to
            // show progress.
            let mut panel = self.panel(side).clone();
            let result = if panel.path == path { panel.reload() } else { panel.navigate(path, None) };
            match result {
                Ok(()) => {
                    let s = &mut self.sides[side];
                    s.tabs.insert(s.current + 1, panel);
                }
                Err(e) => self.status = Some((e.to_string(), Tone::Error)),
            }
            return Task::none();
        }
        self.nav[side] = None;
        let panel = self.panel(side).clone();
        let same = panel.path == path;
        let s = &mut self.sides[side];
        s.tabs.insert(s.current + 1, panel);
        s.current += 1;
        self.active = side;
        if same { self.list_in_background(path) } else { self.start_nav(side, Nav::to(path)) }
    }

    fn close_tab(&mut self, side: usize, index: usize) -> Task<Message> {
        let s = &mut self.sides[side];
        if s.tabs.len() <= 1 || index >= s.tabs.len() {
            return Task::none();
        }
        s.tabs.remove(index);
        if s.current > index || s.current >= s.tabs.len() {
            s.current = s.current.saturating_sub(1);
        }
        self.nav[side] = None;
        let path = self.panel(side).path.clone();
        self.list_in_background(path)
    }

    fn select_tab(&mut self, side: usize, index: usize) -> Task<Message> {
        if index >= self.sides[side].tabs.len() {
            return Task::none();
        }
        self.sides[side].current = index;
        self.nav[side] = None;
        self.after_change();
        // Tabs in the background may be stale.
        let path = self.panel(side).path.clone();
        self.list_in_background(path)
    }

    // ---- quick search -----------------------------------------------------

    fn quick_search_type(&mut self, typed: &str) -> Task<Message> {
        let side = self.active;
        let mut text = match &self.quick_search {
            Some(q) if q.side == side => q.text.clone(),
            _ => String::new(),
        };
        text.push_str(typed);
        let panel = self.panel(side);
        // Stay on the current entry if it still matches.
        let Some(index) = panel.find_prefix(&text, panel.cursor, true) else {
            // No match: ignore the key but keep the box open.
            return match &self.quick_search {
                Some(q) => self.quick_search_timer(q.seq),
                None => Task::none(),
            };
        };
        let seq = self.quick_search.as_ref().map_or(0, |q| q.seq + 1);
        self.quick_search = Some(QuickSearch { side, text, seq });
        self.jump_to(side, index);
        self.quick_search_timer(seq)
    }

    fn quick_search_back(&mut self) -> Task<Message> {
        let Some(q) = &mut self.quick_search else { return Task::none() };
        q.text.pop();
        q.seq += 1;
        let (side, text, seq) = (q.side, q.text.clone(), q.seq);
        if text.is_empty() {
            self.quick_search = None;
            return Task::none();
        }
        if let Some(index) = self.panel(side).find_prefix(&text, 0, true) {
            self.jump_to(side, index);
        }
        self.quick_search_timer(seq)
    }

    fn quick_search_step(&mut self, forward: bool) -> Task<Message> {
        let Some(q) = &mut self.quick_search else { return Task::none() };
        q.seq += 1;
        let (side, text, seq) = (q.side, q.text.clone(), q.seq);
        let panel = self.panel(side);
        let len = panel.entries.len().max(1);
        let from = if forward { (panel.cursor + 1) % len } else { (panel.cursor + len - 1) % len };
        if let Some(index) = panel.find_prefix(&text, from, forward) {
            self.jump_to(side, index);
        }
        self.quick_search_timer(seq)
    }

    fn quick_search_timer(&self, seq: u64) -> Task<Message> {
        delayed(QUICK_SEARCH_TIMEOUT, Message::QuickSearchTimeout(seq))
    }

    // ---- quick filter -----------------------------------------------------

    /// Sets the active panel's quick filter and keeps the cursor in view.
    fn set_filter(&mut self, filter: String) -> Task<Message> {
        let rows = self.page_rows();
        let panel = self.panel_mut(self.active);
        panel.set_filter(&filter);
        panel.ensure_visible(rows);
        Task::none()
    }

    fn jump_to(&mut self, side: usize, index: usize) {
        let rows = self.page_rows();
        let panel = self.panel_mut(side);
        panel.set_cursor(index);
        panel.ensure_visible(rows);
    }

    // ---- inline rename ----------------------------------------------------

    fn start_rename(&mut self) -> Task<Message> {
        let side = self.active;
        let Some(entry) = self.panel(side).current().filter(|e| !e.is_parent()) else {
            return Task::none();
        };
        let is_dir = entry.is_dir();
        let mut rename = InlineRename {
            side,
            src: entry.path.clone(),
            text: entry.file_name().to_owned(),
            is_dir,
            part: Part::initial(is_dir),
            select: 0,
        };
        rename.select(Part::initial(is_dir));
        self.path_edit = None;
        self.rename = Some(rename);
        // `SelectRange` selects the stem once the input has focus.
        text_input::focus(rename_input_id())
    }

    fn commit_rename(&mut self) {
        let Some(InlineRename { side, src, text, .. }) = self.rename.take() else { return };
        let name = text.trim();
        let Some(parent) = src.parent() else { return };
        if name.is_empty() || src.file_name().is_some_and(|n| n == name) {
            return;
        }
        let dst = parent.join(name);
        if vfs::is_virtual(&src) {
            let title = format!("Renaming {} → {name}", src.file_name().unwrap_or_default().to_string_lossy());
            let task = self.start_job(Job::Rename { from: src, to: dst }, title);
            if let Some(job) = self.jobs.last_mut() {
                job.focus = Some((side, name.to_owned()));
            }
            self.deferred.push(task);
            return;
        }
        // Allow case-only renames, which "exist" on Windows.
        let result = if dst.exists() && !ops::same_path(&src, &dst) {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("'{name}' already exists")))
        } else {
            std::fs::rename(&src, &dst)
        };
        let ok = result.is_ok();
        self.report(result);
        // Search results show the hit under its new name (before the
        // refresh would drop the old path).
        let mut focus = name.to_owned();
        if ok {
            for s in 0..2 {
                if let Some(n) = self.panel_mut(s).result_renamed(&src, &dst) {
                    if s == side {
                        focus = n;
                    }
                }
            }
        }
        self.reload_panels();
        if ok {
            self.panel_mut(side).focus(&focus);
            self.after_change();
        }
    }

    // ---- path bar ---------------------------------------------------------

    fn path_edit_key(&mut self, named: Option<Named>) -> Task<Message> {
        let Some(edit) = &mut self.path_edit else { return Task::none() };
        let count = edit.suggestions.len();
        match named {
            Some(Named::Escape) => self.path_edit = None,
            Some(Named::ArrowDown) if count > 0 => {
                edit.selected = Some(edit.selected.map_or(0, |i| (i + 1) % count));
            }
            Some(Named::ArrowUp) if count > 0 => {
                edit.selected = Some(edit.selected.map_or(count - 1, |i| (i + count - 1) % count));
            }
            Some(Named::Tab) if count > 0 => {
                // Complete to the highlighted (or first) suggestion and keep going.
                let pick = &edit.suggestions[edit.selected.unwrap_or(0)];
                let mut text = pick.display().to_string();
                if !text.ends_with(MAIN_SEPARATOR) {
                    text.push(MAIN_SEPARATOR);
                }
                edit.text = text;
                return Task::batch([self.refresh_suggestions(), text_input::move_cursor_to_end(path_input_id())]);
            }
            _ => {}
        }
        Task::none()
    }

    /// Recomputes the folder suggestions for the typed path. Folders are
    /// read in the background (`Message::Subfolders`): typing a network
    /// path must not block.
    fn refresh_suggestions(&mut self) -> Task<Message> {
        let Some(side) = self.path_edit.as_ref().map(|e| e.side) else { return Task::none() };
        let base = self.panel(side).path.clone();
        let Some(edit) = &mut self.path_edit else { return Task::none() };
        edit.selected = None;
        edit.suggestions.clear();

        let raw = edit.text.trim().trim_matches('"');
        if raw.is_empty() {
            return Task::none();
        }
        let path = resolve_typed_path(raw, &base);
        let ends_with_sep = raw.ends_with(['\\', '/']) || (raw.len() == 2 && raw.ends_with(':'));
        let (dir, prefix) = if ends_with_sep {
            (path.clone(), String::new())
        } else {
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => (parent.to_path_buf(), name.to_string_lossy().to_lowercase()),
                _ => return Task::none(),
            }
        };

        if edit.cache.as_ref().is_none_or(|(cached, _)| *cached != dir) {
            edit.cache = Some((dir.clone(), None));
            return Task::perform(
                async move {
                    let (tx, rx) = iced::futures::channel::oneshot::channel();
                    let d = dir.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(list_subfolders(&d));
                    });
                    (dir, rx.await.unwrap_or_default())
                },
                |(dir, names)| Message::Subfolders(dir, names),
            );
        }
        let Some((_, Some(names))) = &edit.cache else { return Task::none() };
        edit.suggestions = names
            .iter()
            .filter(|n| n.to_lowercase().starts_with(&prefix))
            .take(MAX_SUGGESTIONS)
            .map(|n| dir.join(n))
            .collect();
        // Don't suggest exactly what's already typed.
        if edit.suggestions.len() == 1 && normalize_lexically(&edit.suggestions[0]) == normalize_lexically(&path) {
            edit.suggestions.clear();
        }
        Task::none()
    }

    fn submit_path(&mut self) -> Task<Message> {
        let Some(edit) = self.path_edit.as_ref() else { return Task::none() };
        let raw = edit.text.trim().trim_matches('"').to_string();
        if raw.is_empty() {
            self.path_edit = None;
            return Task::none();
        }
        let path = resolve_typed_path(&raw, &self.panel(edit.side).path);
        self.go_to_path(path)
    }

    /// Navigates the edited side to `path`: a folder, or a file to focus.
    fn go_to_path(&mut self, path: PathBuf) -> Task<Message> {
        let Some(edit) = self.path_edit.take() else { return Task::none() };
        self.start_nav_to(edit.side, Nav::to(normalize_lexically(&path)), true)
    }

    // ---- dialogs ----------------------------------------------------------

    fn open_transfer(&mut self, is_move: bool, sources: Vec<PathBuf>, target: &Path) -> Task<Message> {
        if sources.is_empty() {
            return Task::none();
        }
        if let Some(msg) = archive_transfer_problem(is_move, &sources, target) {
            self.status = Some((msg.into(), Tone::Info));
            return Task::none();
        }
        let mut input = remote::display(target);
        // A trailing separator says "into this folder" (on a server that
        // spares asking it whether the target is a folder).
        let sep = if remote::is_remote(target) { '/' } else { MAIN_SEPARATOR };
        if !input.ends_with(['/', '\\']) {
            input.push(sep);
        }
        let base = self.panel(self.active).path.clone();
        self.open_dialog(Dialog::Transfer { is_move, sources, base, input })
    }

    fn open_dialog(&mut self, dialog: Dialog) -> Task<Message> {
        self.leave_edits();
        self.dialog = Some(dialog);
        self.dialog_focus = None;
        Task::batch([text_input::focus(dialog_input_id()), text_input::select_all(dialog_input_id())])
    }

    /// Enter: the focused button (Cancel or the default one).
    fn dialog_enter(&mut self) -> Task<Message> {
        if self.dialog_focus == Some(0) {
            self.dialog = None;
            self.dialog_focus = None;
            return Task::none();
        }
        self.submit_dialog()
    }

    fn submit_dialog(&mut self) -> Task<Message> {
        self.dialog_focus = None;
        let Some(dialog) = self.dialog.take() else { return Task::none() };
        match dialog {
            Dialog::Transfer { is_move, sources, base, input } => {
                let target = resolve_typed_path(input.trim(), &base);
                if let Some(msg) = archive_transfer_problem(is_move, &sources, &target) {
                    self.status = Some((msg.into(), Tone::Info));
                    return Task::none();
                }
                match ops::plan_transfer(&sources, &target) {
                    Ok(transfers) => {
                        let title = format!(
                            "{} {} → {}",
                            if is_move { "Moving" } else { "Copying" },
                            describe(&sources),
                            target.display()
                        );
                        let job = if is_move { Job::Move(transfers) } else { Job::Copy(transfers) };
                        return self.start_job(job, title);
                    }
                    Err(msg) => self.status = Some((msg, Tone::Error)),
                }
            }
            Dialog::MakeDir { input } => {
                let name = input.trim();
                if name.is_empty() {
                    return Task::none();
                }
                let dir = self.panel(self.active).path.clone();
                if vfs::in_archive(&dir) {
                    let first = Path::new(name).components().next().map(|c| c.as_os_str().to_string_lossy().into_owned());
                    let task = self.start_job(Job::MakeDir(dir.join(name)), format!("Creating folder {name}"));
                    if let (Some(job), Some(first)) = (self.jobs.last_mut(), first) {
                        job.focus = Some((self.active, first));
                    }
                    return task;
                }
                let panel = self.panel_mut(self.active);
                let result = std::fs::create_dir_all(panel.path.join(name)).and_then(|_| panel.reload());
                // Focus the first component of what was created ("a\b" → "a").
                if let Some(first) = Path::new(name).components().next() {
                    panel.focus(&first.as_os_str().to_string_lossy());
                }
                self.after_change();
                self.report(result);
            }
            Dialog::EditNew { input } => {
                let name = input.trim();
                if name.is_empty() {
                    return Task::none();
                }
                let path = self.panel(self.active).path.join(name);
                if vfs::in_archive(&path) {
                    return self.new_in_archive(path, name);
                }
                let created = !path.exists();
                if created {
                    let made = path
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|_| std::fs::File::create_new(&path).map(drop));
                    if let Err(e) = made {
                        self.status = Some((format!("Can't create {name}: {e}"), Tone::Error));
                        return Task::none();
                    }
                    let panel = self.panel_mut(self.active);
                    let _ = panel.reload();
                    if let Some(first) = Path::new(name).components().next() {
                        panel.focus(&first.as_os_str().to_string_lossy());
                    }
                    self.after_change();
                }
                self.edit_file(&path, name);
            }
            Dialog::Pack { sources, base, input } => {
                let mut archive = resolve_typed_path(input.trim().trim_matches('"'), &base);
                if !archive.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip") || e.eq_ignore_ascii_case("jar")) {
                    let mut name = archive.file_name().unwrap_or_default().to_os_string();
                    name.push(".zip");
                    archive.set_file_name(name);
                }
                if archive.is_dir() || vfs::in_archive(archive.parent().unwrap_or(&archive)) {
                    self.status = Some((format!("Can't pack into {}", archive.display()), Tone::Error));
                    return Task::none();
                }
                let title = format!("Packing {} → {}", describe(&sources), archive.display());
                let other = 1 - self.active;
                let focus = (archive.parent() == Some(self.panel(other).path.as_path()))
                    .then(|| (other, archive.file_name().unwrap_or_default().to_string_lossy().into_owned()));
                let task = self.start_job(Job::Pack { sources, archive }, title);
                if let Some(job) = self.jobs.last_mut() {
                    job.focus = focus;
                }
                return task;
            }
            Dialog::Unpack { archives, base, input } => {
                let target = resolve_typed_path(input.trim().trim_matches('"'), &base);
                if vfs::in_archive(&target) {
                    self.status = Some(("Can't unpack into an archive".into(), Tone::Info));
                    return Task::none();
                }
                let title = format!("Unpacking {} → {}", describe(&archives), target.display());
                return self.start_job(Job::Unpack { archives, target }, title);
            }
            Dialog::Password { archive, input, retry } => match vfs::try_password(&archive, &input) {
                Ok(true) => {
                    self.status = Some(("Password accepted".into(), Tone::Success));
                    if let Some(side) = retry {
                        return self.start_nav(side, Nav::to(archive));
                    }
                }
                Ok(false) => {
                    self.status = Some(("Wrong password".into(), Tone::Error));
                    self.dialog = Some(Dialog::Password { archive, input: String::new(), retry });
                    return text_input::focus(dialog_input_id());
                }
                Err(e) => self.status = Some((format!("Can't open the archive: {e}"), Tone::Error)),
            },
            Dialog::Delete { paths, to_trash } => {
                // No recycle bin inside archives.
                let to_trash = to_trash && !paths.iter().any(|p| vfs::is_virtual(p));
                let verb = if to_trash { "Recycling" } else { "Deleting" };
                let title = format!("{verb} {}", describe(&paths));
                return self.start_job(Job::Delete { paths, to_trash }, title);
            }
            Dialog::Hotlist(menu) => {
                let selected = menu.selected;
                self.dialog = Some(Dialog::Hotlist(menu));
                return self.hotlist_pick(selected);
            }
            Dialog::History { selected } => return self.history_pick(selected),
            Dialog::Palette(palette) => {
                let selected = palette.selected;
                self.dialog = Some(Dialog::Palette(palette));
                return self.palette_pick(selected);
            }
            Dialog::Compare { opts } => {
                let changed = self.compare_opts != opts;
                self.compare_opts = opts;
                if changed {
                    if let Err(e) = self.save_compare() {
                        self.status = Some((format!("Compare options not saved: {e}"), Tone::Error));
                    }
                }
                return self.compare_dirs();
            }
        }
        Task::none()
    }

    // ---- drag & drop ------------------------------------------------------

    fn update_drag(&mut self) -> Task<Message> {
        let mouse = self.mouse;
        let (width, height) = (self.window_width, self.window_height);
        let Some(drag) = &mut self.drag else { return Task::none() };

        if drag.active {
            // Leaving the window hands the drag over to Windows, so it can
            // be dropped on Explorer, the desktop or another app.
            let outside = mouse.x < 0.0 || mouse.y < 0.0 || mouse.x > width || mouse.y > height;
            if outside {
                if let Some(drag) = self.drag.take() {
                    self.hover = None;
                    let Some((folder, items)) = self.shell_items_extracted(drag.side, drag.sources) else {
                        return Task::none();
                    };
                    return self.drag_out(drag.side, folder, items);
                }
            }
            return Task::none();
        }

        let start = *drag.start.get_or_insert(mouse);
        if mouse.distance(start) < DRAG_THRESHOLD {
            return Task::none();
        }
        let side = drag.side;
        let sources = self.target_paths(side);
        match &mut self.drag {
            Some(drag) if !sources.is_empty() => {
                drag.sources = sources;
                drag.active = true;
            }
            _ => self.drag = None, // e.g. dragging ".."
        }
        Task::none()
    }

    fn finish_drag(&mut self, right: bool) -> Task<Message> {
        if self.drag.as_ref().is_none_or(|d| d.right != right) {
            return Task::none();
        }
        let Some(drag) = self.drag.take() else { return Task::none() };
        self.hover = None;
        if !drag.active {
            if drag.collapse {
                let panel = self.panel_mut(drag.side);
                if drag.index < panel.entries.len() {
                    panel.deselect_all();
                }
            }
            if drag.right {
                let on_parent = self.panel(drag.side).entries.get(drag.index).is_none_or(|e| e.is_parent());
                return self.context_menu(drag.side, on_parent);
            }
            return Task::none();
        }
        let Some(target) = self.drop_target(&drag) else { return Task::none() };
        self.active = drag.side;
        // The shell's drop menu can't take files inside archives.
        let in_archive = vfs::in_archive(&self.panel(drag.side).path) || vfs::in_archive(&target);
        if drag.right && !in_archive {
            return self.drop_menu(drag, target);
        }
        self.open_transfer(self.modifiers.shift(), drag.sources, &target)
    }

    /// Right-button drop inside our window: let the shell show its drop
    /// menu for the target folder.
    fn drop_menu(&self, drag: Drag, target: PathBuf) -> Task<Message> {
        let side = drag.side;
        let (folder, items) = self.shell_items(side, drag.sources);
        window::get_oldest()
            .and_then(move |id| {
                let (folder, items, target) = (folder.clone(), items.clone(), target.clone());
                window::run_with_handle(id, move |handle| {
                    rc_platform::drop_menu(native_handle(&handle), &folder, &items, &target)
                })
            })
            .map(move |result| Message::DropMenuDone(side, result))
    }

    /// Ctrl+C / Ctrl+X: puts the active panel's targets on the system
    /// clipboard. Files in archives are copied as their temp copies; they
    /// (and files on servers) can't be cut.
    fn clip_files(&mut self, cut: bool) -> Task<Message> {
        let side = self.active;
        let mut items = self.target_paths(side);
        if items.is_empty() {
            return Task::none();
        }
        if items.iter().any(|p| vfs::is_virtual(p)) {
            if cut {
                self.status = Some(("Can't cut files in an archive or on a server: move them with F6".into(), Tone::Info));
                return Task::none();
            }
            match self.shell_items_extracted(side, items) {
                Some((_, local)) => items = local,
                None => return Task::none(),
            }
        }
        let n = items.len();
        window::get_oldest()
            .and_then(move |id| {
                let items = items.clone();
                window::run_with_handle(id, move |handle| {
                    rc_platform::set_clipboard_files(native_handle(&handle), &items, cut)
                })
            })
            .map(move |result| Message::ClipboardSet(result.map(|()| n), cut))
    }

    /// Ctrl+V: copies (or, if they were cut, moves) the files on the system
    /// clipboard into the active panel, through the usual transfer dialog.
    fn paste_files(&mut self) -> Task<Message> {
        let Some((sources, cut)) = rc_platform::clipboard_files() else {
            self.status = Some(("No files on the clipboard".into(), Tone::Info));
            return Task::none();
        };
        let target = self.panel(self.active).path.clone();
        self.open_transfer(cut, sources, &target)
    }

    fn drag_out(&self, side: usize, folder: PathBuf, items: Vec<PathBuf>) -> Task<Message> {
        window::get_oldest()
            .and_then(move |id| {
                let (folder, items) = (folder.clone(), items.clone());
                window::run_with_handle(id, move |handle| {
                    rc_platform::drag_out(native_handle(&handle), &folder, &items)
                })
            })
            .map(move |result| Message::DragOutDone(side, result))
    }

    /// Side the mouse is over (0 = left, 1 = right).
    fn side_under_mouse(&self) -> usize {
        usize::from(self.mouse.x >= self.split_x())
    }

    /// Where a drop at the current mouse position would go: the folder row
    /// under the mouse, else the other panel's folder. `None` means "no
    /// valid target" (e.g. back onto the same panel).
    fn drop_target(&self, drag: &Drag) -> Option<PathBuf> {
        let side = self.side_under_mouse();
        let panel = self.panel(side);
        if let Some((hover_side, index)) = self.hover {
            if hover_side == side {
                if let Some(entry) = panel.entries.get(index) {
                    if entry.is_dir() && !drag.sources.contains(&entry.path) {
                        return Some(entry.path.clone());
                    }
                }
            }
        }
        (side != drag.side).then(|| panel.path.clone())
    }

    /// The folder the shell resolves `items` in, and the items it can take:
    /// the panel's folder and all of them, except in search results, whose
    /// items may come from several folders: there the first item's folder
    /// and only the items in it.
    fn shell_items(&self, side: usize, mut items: Vec<PathBuf>) -> (PathBuf, Vec<PathBuf>) {
        let panel = self.panel(side);
        let parent = items.first().and_then(|p| p.parent()).map(Path::to_path_buf);
        match parent.filter(|_| panel.results().is_some()) {
            Some(parent) => {
                items.retain(|p| p.parent() == Some(parent.as_path()));
                (parent, items)
            }
            None => (panel.path.clone(), items),
        }
    }

    /// Like [`Self::shell_items`], but items inside an archive are first
    /// extracted to their temp copies (`vfs::extract_temp_all`), which the
    /// shell then gets. `None` (with a status) if that failed.
    fn shell_items_extracted(&mut self, side: usize, items: Vec<PathBuf>) -> Option<(PathBuf, Vec<PathBuf>)> {
        if !items.iter().any(|p| vfs::is_virtual(p)) {
            return Some(self.shell_items(side, items));
        }
        // Downloading here would block the window.
        if items.iter().any(|p| remote::is_remote(p)) {
            self.status = Some(("Not for files on a server: copy them to a local folder first (F5)".into(), Tone::Info));
            return None;
        }
        match vfs::extract_temp_all(&items) {
            Ok(local) => {
                let folder = local.first().and_then(|p| p.parent()).map(Path::to_path_buf)?;
                Some((folder, local))
            }
            Err(e) => {
                self.status = Some((format!("Can't extract from the archive: {e}"), Tone::Error));
                None
            }
        }
    }

    // ---- context menu -----------------------------------------------------

    fn context_menu(&mut self, side: usize, background: bool) -> Task<Message> {
        if background && remote::is_remote(&self.panel(side).path) {
            self.status = Some(("No context menu on a server".into(), Tone::Info));
            return Task::none();
        }
        if background && vfs::in_archive(&self.panel(side).path) {
            self.status = Some(("No context menu for the archive's folders; right-click a file".into(), Tone::Info));
            return Task::none();
        }
        let items = if background { Vec::new() } else { self.target_paths(side) };
        // Items in archives: the menu acts on temp copies.
        let Some((folder, items)) = self.shell_items_extracted(side, items) else { return Task::none() };
        let extended = self.modifiers.shift();
        let before = if background { subfolders(&folder) } else { Default::default() };

        window::get_oldest()
            .and_then(move |id| {
                let (folder, items, before) = (folder.clone(), items.clone(), before.clone());
                window::run_with_handle(id, move |handle| {
                    let result = rc_platform::show_context_menu(native_handle(&handle), &folder, &items, extended);
                    // A folder the menu created in the background (New > Folder).
                    let made = match (&result, items.is_empty()) {
                        (Ok(MenuAction::Invoked(_)), true) => {
                            let new: Vec<_> = subfolders(&folder).difference(&before).cloned().collect();
                            match new.as_slice() {
                                [one] => Some(folder.join(one)),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    (result, made)
                })
            })
            .map(move |(result, made)| Message::ContextDone(side, result, made))
    }

    /// The shell menu of a drive root (right-click on a drive button): the
    /// drive is an item of the "This PC" folder, which gives Properties,
    /// Eject, Format, ...
    fn drive_context_menu(&mut self, side: usize, drive: String) -> Task<Message> {
        const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
        let folder = PathBuf::from(THIS_PC);
        let items = vec![PathBuf::from(drive)];
        let extended = self.modifiers.shift();

        window::get_oldest()
            .and_then(move |id| {
                let (folder, items) = (folder.clone(), items.clone());
                window::run_with_handle(id, move |handle| {
                    rc_platform::show_context_menu(native_handle(&handle), &folder, &items, extended)
                })
            })
            .map(move |result| Message::ContextDone(side, result, None))
    }

    // ---- jobs -------------------------------------------------------------

    fn start_job(&mut self, job: Job, title: String) -> Task<Message> {
        let id = self.next_job_id;
        self.next_job_id += 1;
        let done_verb = job.done_verb();

        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        let handle = ops::spawn(job, move |event| {
            let _ = tx.unbounded_send(event);
        });
        self.jobs.push(RunningJob {
            id,
            title,
            done_verb,
            source_side: Some(self.active),
            handle,
            progress: Progress::default(),
            conflict: None,
            started: Instant::now(),
            focus: None,
        });
        Task::run(rx, move |event| Message::Job(id, event))
    }

    fn on_job_event(&mut self, id: u64, event: JobEvent) -> Task<Message> {
        match event {
            JobEvent::Progress(progress) => {
                if let Some(job) = self.job(id) {
                    job.progress = progress;
                }
            }
            JobEvent::Conflict(conflict) => {
                self.conflict_focus = None;
                if let Some(job) = self.job(id) {
                    job.conflict = Some(conflict);
                }
            }
            JobEvent::Finished(outcome) => {
                let Some(pos) = self.jobs.iter().position(|j| j.id == id) else { return Task::none() };
                let job = self.jobs.remove(pos);
                if let Some(side) = job.source_side.filter(|_| outcome.errors.is_empty() && !outcome.cancelled && outcome.skipped == 0) {
                    self.panel_mut(side).deselect_all();
                }
                self.disk.borrow_mut().clear();
                self.reload_panels();
                if let Some((side, name)) = job.focus.filter(|_| outcome.errors.is_empty()) {
                    if remote::is_remote(&self.panel(side).path) {
                        let path = self.panel(side).path.clone();
                        self.focus_after_listing.push((path, name));
                    } else {
                        self.panel_mut(side).focus(&name);
                    }
                }
                self.status = Some(summarize(job.done_verb, &outcome));
                let task = self.archive_edit_job_done(id, &outcome);
                self.after_change();
                return task;
            }
        }
        Task::none()
    }

    fn job(&mut self, id: u64) -> Option<&mut RunningJob> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    // ---- helpers ----------------------------------------------------------

    /// If `side` shows an archive (or a folder in one) where `what` can't be
    /// done, says so and returns true. `zip_ok`: it can be done in a zip
    /// (other archives are read-only).
    fn refuse_in_archive(&mut self, side: usize, what: &str, zip_ok: bool) -> bool {
        if remote::is_remote(&self.panel(side).path) {
            if !zip_ok {
                self.status = Some((format!("Can't {what} on a server: copy the files to a local folder first"), Tone::Info));
            }
            return !zip_ok;
        }
        let Some(writable) = vfs::archive_writable(&self.panel(side).path) else { return false };
        if zip_ok && writable {
            return false;
        }
        let msg = match zip_ok {
            true => format!("Read-only archive (only zip, tar and gz archives can be changed): can't {what} here"),
            false => format!("Can't {what} inside archives"),
        };
        self.status = Some((msg, Tone::Info));
        true
    }

    /// Asks for the password of the archive `path` is in (or is).
    fn ask_password(&mut self, path: PathBuf, retry: Option<usize>) -> Task<Message> {
        let archive = vfs::split_archive(&path).map_or(path, |(file, _)| file);
        self.open_dialog(Dialog::Password { archive, input: String::new(), retry })
    }

    /// Alt+F5: asks for the name of a zip to pack the selection into, in
    /// the other panel's folder.
    fn open_pack(&mut self) -> Task<Message> {
        let sources = self.target_paths(self.active);
        if sources.is_empty() {
            return Task::none();
        }
        if sources.iter().any(|p| vfs::is_virtual(p)) {
            self.status = Some(("Can't pack files inside archives or on servers: copy them out first".into(), Tone::Info));
            return Task::none();
        }
        let target = self.panel(1 - self.active).path.clone();
        let folder = if vfs::in_archive(&target) { self.panel(self.active).path.clone() } else { target };
        let stem = match &sources[..] {
            [one] if one.is_dir() => one.file_name().map(|n| n.to_string_lossy().into_owned()),
            [one] => one.file_stem().map(|n| n.to_string_lossy().into_owned()),
            _ => self.panel(self.active).path.file_name().map(|n| n.to_string_lossy().into_owned()),
        };
        let input = folder.join(format!("{}.zip", stem.unwrap_or_else(|| "archive".into()))).display().to_string();
        let base = self.panel(self.active).path.clone();
        self.open_dialog(Dialog::Pack { sources, base, input })
    }

    /// Alt+F9: asks where to unpack the selected archives (the other
    /// panel's folder).
    fn open_unpack(&mut self) -> Task<Message> {
        let archives: Vec<PathBuf> = self
            .panel(self.active)
            .targets()
            .into_iter()
            .filter(|e| e.kind == EntryKind::File && vfs::is_archive_name(&e.name))
            .map(|e| e.path.clone())
            .collect();
        if archives.is_empty() {
            self.status = Some(("Select archives (zip, 7z, rar, tar, ...) to unpack".into(), Tone::Info));
            return Task::none();
        }
        if archives.iter().any(|a| remote::is_remote(a)) {
            self.status = Some(("Can't unpack on a server: copy the archive to a local folder first".into(), Tone::Info));
            return Task::none();
        }
        let target = self.panel(1 - self.active).path.clone();
        if vfs::in_archive(&target) {
            self.status = Some(("Can't unpack into an archive or onto a server".into(), Tone::Info));
            return Task::none();
        }
        let mut input = target.display().to_string();
        if !input.ends_with(MAIN_SEPARATOR) {
            input.push(MAIN_SEPARATOR);
        }
        let base = self.panel(self.active).path.clone();
        self.open_dialog(Dialog::Unpack { archives, base, input })
    }

    /// Re-reads the visible tab on both sides after something changed.
    /// Servers are listed in the background (`deferred`).
    fn reload_panels(&mut self) {
        for side in 0..2 {
            let path = self.panel(side).path.clone();
            if remote::is_remote(&path) {
                if self.panel(side).results().is_none() && (side == 0 || self.panel(0).path != path) {
                    let task = self.list_in_background(path);
                    self.deferred.push(task);
                }
                continue;
            }
            if let Err(e) = self.panel_mut(side).reload() {
                self.folder_unreadable(side, e.to_string());
            }
        }
    }

    /// The folder of `side` can't be listed any more: it may be gone, so walk
    /// up to something that exists.
    fn folder_unreadable(&mut self, side: usize, error: String) {
        self.nav[side] = None;
        // On a server: a vanished folder goes up there; a lost connection
        // just says so.
        let path = self.panel(side).path.clone();
        if remote::is_remote(&path) {
            let gone = !error.contains("Not connected") && !error.contains("timed out") && !error.contains("in time");
            match self.panel(side).parent_nav().filter(|_| gone) {
                Some(nav) => {
                    let task = self.start_nav(side, nav);
                    self.deferred.push(task);
                }
                None => self.status = Some((error, Tone::Error)),
            }
            return;
        }
        let panel = self.panel_mut(side);
        let existing = panel.path.ancestors().skip(1).find(|p| p.is_dir()).map(Path::to_path_buf);
        match existing {
            Some(p) => {
                let _ = panel.navigate(p, None);
            }
            None => self.status = Some((error, Tone::Error)),
        }
    }

    /// Shift+F2: selects, in both panels, the entries missing from the other
    /// side or newer there (`rc_core::compare`, with `compare_opts`). With
    /// options that read the disk it runs on a worker (Esc cancels).
    fn compare_dirs(&mut self) -> Task<Message> {
        if (0..2).any(|side| self.panel(side).results().is_some()) {
            self.status = Some(("Can't compare search results".into(), Tone::Info));
            return Task::none();
        }
        if self.panel(0).path == self.panel(1).path {
            self.status = Some(("Both panels show the same folder".into(), Tone::Info));
            return Task::none();
        }
        if let Some((cancel, ..)) = self.comparing.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let opts = CompareOptions { show_hidden: self.panel(0).show_hidden && self.panel(1).show_hidden, ..self.compare_opts };
        if opts.reads_disk() && (0..2).any(|side| vfs::in_archive(&self.panel(side).path)) {
            let msg = "Compare by content or with subfolders doesn't work in archives yet";
            self.status = Some((msg.into(), Tone::Info));
            return Task::none();
        }
        if !opts.reads_disk() {
            let never = AtomicBool::new(false);
            let diff = rc_core::compare::compare_with(&self.panel(0).entries, &self.panel(1).entries, opts, &never);
            self.apply_compare(diff.unwrap_or_default());
            return Task::none();
        }
        let (left, right) = (self.panel(0).entries.clone(), self.panel(1).entries.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.comparing = Some((cancel.clone(), self.panel(0).path.clone(), self.panel(1).path.clone()));
        self.status = Some(("Comparing\u{2026} (Esc cancels)".into(), Tone::Info));
        let (tx, rx) = iced::futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let diff = rc_core::compare::compare_with(&left, &right, opts, &cancel);
            let _ = tx.send((cancel, diff));
        });
        Task::perform(rx, |r| match r {
            Ok((token, diff)) => Message::Compared(token, diff),
            Err(_) => Message::Compared(Arc::new(AtomicBool::new(true)), None),
        })
    }

    /// Selects a compare result in both panels and reports it.
    fn apply_compare(&mut self, diff: Differences) {
        let msg = if diff.is_empty() {
            ("Folders are identical".to_string(), Tone::Success)
        } else {
            (format!("Differences: {} left, {} right", diff.left.len(), diff.right.len()), Tone::Info)
        };
        self.panel_mut(0).set_selection(diff.left);
        self.panel_mut(1).set_selection(diff.right);
        self.status = Some(msg);
    }

    /// Counts the sizes of folders `names` of `side`'s folder on a worker
    /// thread, one `Message::DirSize` each. Folders already counted (or
    /// being counted) are skipped.
    fn count_dirs(&mut self, side: usize, names: Vec<String>) -> Task<Message> {
        let panel = self.panel_mut(side);
        let names: Vec<String> = names.into_iter().filter(|n| panel.start_dir_size(n)).collect();
        if names.is_empty() {
            return Task::none();
        }
        let dir = panel.path.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.counts.push((dir.clone(), cancel.clone()));
        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            for name in names {
                let path = dir.join(&name);
                let size = if vfs::is_virtual(&path) {
                    // Inside an archive: sum the sizes from its index.
                    vfs::open(&path).and_then(|v| v.walk(&path)).map_or(0, |items| items.iter().map(|e| e.size).sum())
                } else {
                    rc_core::fs::dir_size(&path, &cancel)
                };
                if cancel.load(Ordering::Relaxed) || tx.unbounded_send((dir.clone(), name, size)).is_err() {
                    break;
                }
            }
        });
        Task::run(rx, |(dir, name, size)| Message::DirSize(dir, name, size))
    }

    /// Stops counts whose folder no panel shows any more and forgets
    /// finished ones (their thread dropped its flag).
    fn prune_counts(&mut self) {
        let shown: Vec<&Path> = self.sides.iter().flat_map(|s| s.tabs.iter()).map(|p| p.path.as_path()).collect();
        self.counts.retain(|(dir, cancel)| {
            let keep = Arc::strong_count(cancel) > 1 && shown.contains(&dir.as_path());
            if !keep {
                cancel.store(true, Ordering::Relaxed);
            }
            keep
        });
    }

    /// Esc: stops all folder size counts.
    fn cancel_counts(&mut self) {
        for (_, cancel) in self.counts.drain(..) {
            cancel.store(true, Ordering::Relaxed);
        }
        for panel in self.sides.iter_mut().flat_map(|s| s.tabs.iter_mut()) {
            panel.drop_pending_dir_sizes();
        }
    }

    /// Lists `path` on a worker thread; the result updates every visible tab
    /// still showing it (`Message::Listed`).
    fn list_in_background(&mut self, path: PathBuf) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        self.listings.insert(path.clone(), seq);
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                let dir = path.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(vfs::list(&dir, true).map_err(|e| e.to_string()));
                });
                let result = rx.await.unwrap_or_else(|_| Err("listing thread failed".into()));
                (path, result)
            },
            move |(path, result)| Message::Listed(path, seq, result),
        )
    }

    /// Changes the folder of `side`. The target is listed on a worker
    /// thread; if that takes longer than `SYNC_LISTING_WAIT` the panel keeps
    /// showing the old folder (path bar says "reading") until
    /// `Message::Navigated` arrives. Esc or another navigation abandons it.
    fn start_nav(&mut self, side: usize, nav: Nav) -> Task<Message> {
        self.start_nav_to(side, nav, false)
    }

    /// `start_nav`; with `file_ok` a target that is a file opens its folder
    /// with the cursor on it.
    fn start_nav_to(&mut self, side: usize, nav: Nav, file_ok: bool) -> Task<Message> {
        use std::sync::mpsc;
        self.nav[side] = None;
        let Some(path) = self.panel(side).nav_target(&nav) else { return Task::none() };
        self.next_listing += 1;
        let seq = self.next_listing;
        let (tx, rx) = mpsc::channel();
        let dir = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(list_nav_target(&dir, file_ok));
        });
        let failed = || Err("listing thread failed".to_string());
        match rx.recv_timeout(SYNC_LISTING_WAIT) {
            Ok(result) => self.finish_nav(side, nav, result),
            Err(mpsc::RecvTimeoutError::Disconnected) => self.finish_nav(side, nav, failed()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.nav[side] = Some(PendingNav { seq, tab: self.sides[side].current, nav, path });
                Task::perform(
                    async move {
                        // Hand the blocking receive to a thread of its own.
                        let (tx, done) = iced::futures::channel::oneshot::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(rx.recv().unwrap_or_else(|_| failed()));
                        });
                        done.await.unwrap_or_else(|_| failed())
                    },
                    move |result| Message::Navigated(side, seq, result),
                )
            }
        }
    }

    fn finish_nav(&mut self, side: usize, nav: Nav, result: Listing) -> Task<Message> {
        match result {
            Ok(Listed { file: Some(name), entries }) => {
                // The target was a file: show its folder, cursor on it.
                let panel = self.panel_mut(side);
                if let Some(parent) = panel.nav_target(&nav).as_deref().and_then(Path::parent) {
                    let nav = Nav::To { path: parent.to_path_buf(), focus: Some(name) };
                    panel.arrive(&nav, entries);
                }
                self.after_change();
                Task::none()
            }
            Ok(Listed { file: None, entries }) => {
                self.panel_mut(side).arrive(&nav, entries);
                self.after_change();
                // Entering an encrypted archive asks for its password.
                let path = self.panel(side).path.clone();
                if self.dialog.is_none() && vfs::needs_password(&path) {
                    return self.ask_password(path, None);
                }
                Task::none()
            }
            Err(e) if e.contains(vfs::PASSWORD_REQUIRED) && self.dialog.is_none() => {
                match self.panel(side).nav_target(&nav) {
                    Some(target) => self.ask_password(target, Some(side)),
                    None => {
                        self.status = Some((e, Tone::Error));
                        Task::none()
                    }
                }
            }
            // A vanished history folder: try the one before it.
            Err(_) if self.panel_mut(side).forget_target(&nav) => self.start_nav(side, nav),
            Err(e) => {
                self.status = Some((e, Tone::Error));
                Task::none()
            }
        }
    }

    /// Target of the navigation `side` is waiting for, if any.
    pub(super) fn pending_nav(&self, side: usize) -> Option<&Path> {
        self.nav[side].as_ref().map(|p| p.path.as_path())
    }

    fn apply_listing(&mut self, path: PathBuf, seq: u64, result: Result<Vec<Entry>, String>) {
        // A newer listing of this folder is on its way.
        if self.listings.get(&path) != Some(&seq) {
            return;
        }
        self.listings.remove(&path);
        for side in 0..2 {
            if self.panel(side).path != path {
                continue;
            }
            match &result {
                Ok(entries) => self.panel_mut(side).apply_listing(entries.clone()),
                Err(e) => self.folder_unreadable(side, e.clone()),
            }
        }
        if result.is_ok() {
            let wanted: Vec<String> = self
                .focus_after_listing
                .iter()
                .filter(|(p, _)| *p == path)
                .map(|(_, n)| n.clone())
                .collect();
            self.focus_after_listing.retain(|(p, _)| *p != path);
            for name in wanted {
                for side in 0..2 {
                    if self.panel(side).path == path {
                        self.panel_mut(side).focus(&name);
                    }
                }
            }
        }
        self.after_change();
    }

    fn session(&self) -> Session {
        let side = |s: &Side| SideState { current: s.current, tabs: s.tabs.iter().map(TabState::from_panel).collect() };
        Session {
            width: self.window_width,
            height: self.window_height,
            active: self.active,
            left: side(&self.sides[0]),
            right: side(&self.sides[1]),
            hotlist: self.hotlist.clone(),
            compare: self.compare_unsaved,
            recent_commands: self.recent_commands.iter().map(|c| c.id().to_owned()).collect(),
            split: self.split,
            columns: self.columns,
            window: self.placement.map(Into::into),
            editor: String::new(),
        }
    }

    /// Re-reads `config.toml` if it changed since the last read and applies
    /// it (key bindings, colours). A broken file keeps the previous settings
    /// and reports the error.
    fn refresh_config(&mut self) {
        let stamp = crate::config::file().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());
        if self.config_stamp == Some(stamp) {
            return;
        }
        let first = self.config_stamp.is_none();
        self.config_stamp = Some(stamp);
        match self.apply_config() {
            Ok(()) if !first => self.status = Some(("Settings reloaded".into(), Tone::Info)),
            Ok(()) => {}
            Err(e) => self.status = Some((format!("Bad config: {e}"), Tone::Error)),
        }
    }

    /// Reads `config.toml` now (after the settings view wrote it) and
    /// applies it; `refresh_config` then sees it unchanged.
    fn reload_config(&mut self) -> Result<(), String> {
        let stamp = crate::config::file().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());
        self.config_stamp = Some(stamp);
        self.apply_config()
    }

    /// Loads `config.toml` and applies it (key bindings, colours); a broken
    /// file keeps the previous settings.
    fn apply_config(&mut self) -> Result<(), String> {
        let config = crate::config::load()?;
        let keymap = keymap::Keymap::new(&config.keys).map_err(|e| format!("config.toml [keys]: {e}"))?;
        let colors = config.colors().map_err(|e| format!("config.toml: {e}"))?;
        if let Some(compare) = config.compare {
            self.compare_opts = compare.into();
        }
        self.config = config;
        self.keymap = keymap;
        theme::set(colors);
        Ok(())
    }

    /// Opens `path` in the editor from `config.toml` (F4, Shift+F4).
    fn edit_file(&mut self, path: &Path, name: &str) {
        self.refresh_config();
        self.status = Some(match crate::editor::launch(&self.config.editor, path) {
            Ok(_) => (format!("Editing {name}"), Tone::Success),
            Err(e) => (format!("Can't start the editor: {e}"), Tone::Error),
        });
    }

    /// F9: a terminal in the active panel's folder (the nearest real one
    /// above an archive or server location).
    fn open_terminal(&mut self) {
        self.refresh_config();
        let dir = self.panel(self.active).path.ancestors().find(|p| !vfs::is_virtual(p) && p.is_dir()).map(Path::to_owned);
        self.status = Some(match dir {
            None => ("No folder on disk here to open a terminal in".into(), Tone::Error),
            Some(dir) => match crate::terminal::launch(&self.config.terminal, &dir) {
                Ok(()) => (format!("Terminal opened in {}", dir.display()), Tone::Success),
                Err(e) => (format!("Can't start the terminal: {e}"), Tone::Error),
            },
        });
    }

    /// Writes `compare_opts` to `config.toml` `[compare]`; when that fails
    /// they're kept in the session instead.
    fn save_compare(&mut self) -> Result<(), String> {
        let state = crate::config::CompareState::from(self.compare_opts);
        if let Err(e) = crate::config::write(&[crate::config::Edit::Compare(state)]) {
            self.compare_unsaved = Some(state);
            self.save_session();
            return Err(e);
        }
        if self.compare_unsaved.take().is_some() {
            self.save_session();
        }
        self.reload_config()
    }

    fn save_session(&self) {
        if let Err(e) = session::save(&self.session()) {
            eprintln!("could not save session: {e}");
        }
    }

    /// Reads the window position, then saves the session and exits
    /// (`Message::Quit`).
    fn quit(&self) -> Task<Message> {
        window::get_oldest()
            .then(|id| match id {
                Some(id) => window::run_with_handle(id, |h| rc_platform::window_placement(native_handle(&h))),
                None => Task::done(None),
            })
            .map(Message::Quit)
    }

    fn after_change(&mut self) {
        let rows = self.page_rows();
        for side in &mut self.sides {
            side.tabs[side.current].ensure_visible(rows);
        }
    }

    fn report(&mut self, result: io::Result<()>) {
        if let Err(e) = result {
            self.status = Some((e.to_string(), Tone::Error));
        }
    }

    fn page_rows(&self) -> usize {
        let bar = if self.config.show_fkeys { 0.0 } else { FKEYS_H };
        let chrome = CHROME_H - bar + self.jobs.len() as f32 * JOB_ROW_H;
        ((self.window_height - chrome) / ROW_H).floor().max(1.0) as usize
    }

    /// System icon for a path, cached.
    fn icon(&self, path: &Path, is_dir: bool) -> Option<image::Handle> {
        let key = rc_platform::icon_key(path, is_dir);
        if let Some(handle) = self.icons.borrow().get(&key) {
            return handle.clone();
        }
        let handle = rc_platform::file_icon(path, is_dir)
            .map(|icon| image::Handle::from_rgba(icon.width, icon.height, icon.rgba));
        self.icons.borrow_mut().insert(key, handle.clone());
        handle
    }

    /// (free, total) for the volume holding `path`, cached for a few seconds.
    fn disk_space(&self, path: &Path) -> Option<(u64, u64)> {
        let root = path.ancestors().last()?.to_path_buf();
        if let Some((at, space)) = self.disk.borrow().get(&root) {
            if at.elapsed() < Duration::from_secs(5) {
                return *space;
            }
        }
        let space = rc_platform::disk_space(&root);
        self.disk.borrow_mut().insert(root, (Instant::now(), space));
        space
    }
}

/// Names of the folders directly inside `dir`.
fn subfolders(dir: &Path) -> std::collections::HashSet<std::ffi::OsString> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Default::default() };
    rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.file_name()).collect()
}

fn is_modifier_key(key: Named) -> bool {
    matches!(
        key,
        Named::Shift
            | Named::Control
            | Named::Alt
            | Named::AltGraph
            | Named::Super
            | Named::Meta
            | Named::Hyper
            | Named::Fn
            | Named::FnLock
            | Named::CapsLock
            | Named::NumLock
            | Named::ScrollLock
            | Named::Symbol
            | Named::SymbolLock
    )
}

/// Transfers involving archives or servers that can't be done: into a
/// read-only archive, moving out of one, between servers, ...
fn archive_transfer_problem(is_move: bool, sources: &[PathBuf], target: &Path) -> Option<&'static str> {
    let server = |p: &Path| remote::split(p).map(|(key, _)| key.to_lowercase());
    let from_server = sources.iter().find_map(|p| server(p));
    match (from_server, server(target)) {
        (Some(a), Some(b)) if a != b || !is_move => {
            return Some("Can't copy between or within servers: copy to a local folder first (F6 moves on one server)");
        }
        (Some(_), None) if vfs::in_archive(target) => return Some("Can't copy from a server into an archive: copy to a folder first"),
        (None, Some(_)) if sources.iter().any(|p| vfs::is_virtual(p)) => {
            return Some("Can't copy from an archive onto a server: copy out to a folder first");
        }
        _ => {}
    }
    let from = sources.iter().find_map(|p| vfs::split_archive(p).filter(|(_, inner)| !inner.as_os_str().is_empty()));
    let into = vfs::split_archive(target);
    match (&from, &into) {
        (_, Some((file, _))) if vfs::archive_writable(file) == Some(false) => {
            Some("Read-only archive (only zip, tar and gz archives can be changed)")
        }
        // Into another (writable) archive is fine: extracted, then packed.
        (Some((a, _)), _) if is_move && vfs::archive_writable(a) == Some(false) => {
            Some("This archive is read-only (only zip, tar and gz archives can be changed): copy out with F5 instead")
        }
        _ => None,
    }
}

/// True for files Windows mounts as a drive (iso, vhd, ...).
fn is_disk_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "iso" | "img" | "vhd" | "vhdx"))
}

/// Enter on a disk image: mount it (the shell "Mount" verb) on a worker
/// thread, then navigate to the new drive (`Message::Mounted`). The drive
/// letter shows up a moment after the verb returns, so the worker waits
/// (up to ~5 s) for a new root and reports it.
fn mount_disk_image(path: PathBuf, active: usize) -> Task<Message> {
    Task::perform(
        async move {
            let (tx, rx) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let before = rc_platform::roots();
                let result = rc_platform::mount_disk_image(&path).map(|()| {
                    for _ in 0..50 {
                        let now = rc_platform::roots();
                        if let Some(new) = now.into_iter().find(|d| !before.contains(d)) {
                            return Some(new);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    None
                });
                let _ = tx.send(result);
            });
            rx.await.unwrap_or_else(|_| Err("mount thread failed".into()))
        },
        move |result| Message::Mounted { active, result },
    )
}

/// Enter on a file inside an archive: extracts it to a temp copy on a
/// worker thread, then opens that (`Message::Extracted`).
fn open_extracted(path: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let (tx, rx) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(vfs::extract_temp(&path).map_err(|e| format!("Can't extract {}: {e}", path.display())));
            });
            rx.await.unwrap_or_else(|_| Err("extract thread failed".into()))
        },
        Message::Extracted,
    )
}

/// Opens or runs a file, returning a status line describing what happened.
fn launch(path: &Path) -> io::Result<String> {
    rc_platform::open_default(path)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    Ok(if rc_platform::is_executable(path) {
        format!("Launched {name}")
    } else {
        format!("Opened {name} with its default app")
    })
}

/// Turns what the user typed in the path bar into a path: `~` is the home
/// folder, `D:` the root of D:, relative paths start at `base`.
fn resolve_typed_path(raw: &str, base: &Path) -> PathBuf {
    let mut path = match raw.strip_prefix('~') {
        Some(rest) => rc_platform::home_dir()
            .map(|h| h.join(rest.trim_start_matches(['/', '\\'])))
            .unwrap_or_else(|| PathBuf::from(raw)),
        None => PathBuf::from(raw),
    };
    if raw.len() == 2 && raw.ends_with(':') {
        path.push(MAIN_SEPARATOR.to_string());
    }
    if path.is_relative() && !remote::is_remote(&path) {
        path = base.join(path);
    }
    path
}

/// Names of the sub-folders of `dir`, sorted case-insensitively.
/// Result of listing a navigation target (`App::start_nav`).
type Listing = Result<Listed, String>;

#[derive(Debug, Clone)]
pub struct Listed {
    /// Set when the target was a file: `entries` are its folder's.
    file: Option<String>,
    entries: Vec<Entry>,
}

/// Lists a navigation target (with hidden entries); runs on a worker
/// thread. With `file_ok`, a file lists its parent folder instead.
fn list_nav_target(path: &Path, file_ok: bool) -> Listing {
    match vfs::list(path, true) {
        Ok(entries) => Ok(Listed { file: None, entries }),
        Err(_) if file_ok && (path.is_file() || vfs::is_virtual(path)) => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let parent = path.parent().ok_or("No parent folder")?;
            let entries = vfs::list(parent, true).map_err(|e| e.to_string())?;
            Ok(Listed { file: name, entries })
        }
        Err(e) if file_ok && e.kind() == io::ErrorKind::NotFound => Err(format!("Path not found: {}", path.display())),
        Err(e) => Err(e.to_string()),
    }
}

fn list_subfolders(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = read
        .flatten()
        .filter(|e| std::fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Resolves `.` and `..` without touching the disk (unlike canonicalize,
/// which on Windows produces `\\?\` paths).
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn native_handle(handle: &raw_window_handle::WindowHandle<'_>) -> isize {
    match handle.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(h) => h.hwnd.get(),
        _ => 0,
    }
}

/// A task that yields `message` after `delay`, without blocking the UI.
fn delayed(delay: Duration, message: Message) -> Task<Message> {
    Task::perform(
        async move {
            let (tx, rx) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let _ = tx.send(());
            });
            let _ = rx.await;
        },
        move |_| message.clone(),
    )
}

/// "file.txt" or "3 items".
fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!("'{}'", one.file_name().unwrap_or_default().to_string_lossy()),
        many => format!("{} items", many.len()),
    }
}

fn summarize(verb: &str, o: &Outcome) -> (String, Tone) {
    let mut msg = format!("{verb} {} item(s)", o.done_items);
    if o.done_bytes > 0 {
        msg.push_str(&format!(" ({})", rc_core::format::size(o.done_bytes)));
    }
    if o.skipped > 0 {
        msg.push_str(&format!(", skipped {}", o.skipped));
    }
    if o.cancelled {
        msg.push_str(" — cancelled");
    }
    match o.errors.first() {
        Some((path, err)) => {
            msg.push_str(&format!(" — {} error(s), first: {}: {err}", o.errors.len(), path.display()));
            (msg, Tone::Error)
        }
        None => (msg, if o.cancelled { Tone::Info } else { Tone::Success }),
    }
}

fn open_panel(path: Option<PathBuf>) -> Panel {
    path.and_then(|p| Panel::open(p).ok())
        .or_else(|| rc_platform::home_dir().and_then(|p| Panel::open(p).ok()))
        .or_else(|| Panel::open(if cfg!(windows) { "C:\\" } else { "/" }).ok())
        .expect("could not open any starting folder")
}

/// Whether `main` creates the window hidden, for [`restore_window`] to move
/// it into place before it's first shown (no jump from the default spot).
pub fn hidden_until_placed(session: &Option<Session>) -> bool {
    cfg!(windows) && session.as_ref().is_some_and(|s| s.window.is_some())
}

/// Moves the (hidden) window to its saved place and shows it.
fn restore_window(p: rc_platform::Placement) -> Task<Message> {
    window::get_oldest().then(move |id| match id {
        Some(id) => window::run_with_handle(id, move |h| rc_platform::restore_placement(native_handle(&h), &p))
            // Let winit know it's visible (and show it if placing failed).
            .chain(window::change_mode(id, window::Mode::Windowed)),
        None => Task::none(),
    })
    .discard()
}

/// Flips compare option `i`: 0 by content, 1 ignore date, 2 subfolders.
fn toggle_compare_option(opts: &mut CompareOptions, i: u8) {
    match i {
        0 => opts.by_content ^= true,
        1 => opts.ignore_date ^= true,
        _ => opts.subfolders ^= true,
    }
}
