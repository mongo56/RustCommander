//! Application state and update logic. Drawing lives in [`view`].

mod view;
mod multi_rename;
mod viewer;

use crate::keymap;
use crate::session::{self, Session, SideState, TabState};
use crate::theme;
use crate::watch;
use iced::keyboard::key::Named;
use iced::widget::{image, text_input};
use iced::{event, keyboard, mouse, window, Event, Point, Size, Subscription, Task, Theme};
use rc_core::ops::{self, Conflict, Job, JobEvent, JobHandle, Outcome, Progress, Resolution};
use rc_core::entry::Entry;
use rc_core::{Activated, Command, EntryKind, Nav, Panel, SortKey};
use rc_platform::{DragOutcome, MenuAction};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf, MAIN_SEPARATOR};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ROW_H: f32 = 22.0;
/// Approximate height of everything that isn't file rows (tabs, headers,
/// footers, function-key bar). Only used to size PageUp/PageDown jumps.
const CHROME_H: f32 = 206.0;
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
    /// Favourite folders (Ctrl+D), saved with the session.
    hotlist: Vec<PathBuf>,
    jobs: Vec<RunningJob>,
    next_job_id: u64,
    /// Folder size counts running (Space / Alt+Shift+Enter): the folder
    /// listed and the batch's cancel flag.
    counts: Vec<(PathBuf, Arc<AtomicBool>)>,
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
    /// Row under the mouse while dragging: (side, index).
    hover: Option<(usize, usize)>,
    modifiers: keyboard::Modifiers,
    /// Icon cache keyed by `rc_platform::icon_key`. Filled lazily from
    /// `view`, hence the RefCell.
    icons: RefCell<HashMap<String, Option<image::Handle>>>,
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
    /// Its settings, kept between uses.
    rename_opts: rc_core::rename::Options,
    /// Last good `config.toml` and the key bindings built from it.
    config: crate::config::Config,
    keymap: keymap::Keymap,
    /// Modification time of `config.toml` when it was last read; `None` =
    /// not read yet.
    config_stamp: Option<Option<std::time::SystemTime>>,
    /// Window position from the last session or the last quit, saved with
    /// the session.
    placement: Option<rc_platform::Placement>,
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
    /// Shift+F4: name of the file to create (if missing) and edit.
    EditNew { input: String },
    Delete { paths: Vec<PathBuf>, to_trash: bool },
    /// Hotlist menu; `selected` indexes the entries, then the add/remove item.
    Hotlist { selected: usize },
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
    /// Row the right button went down on.
    index: usize,
}

struct RunningJob {
    id: u64,
    title: String,
    done_verb: &'static str,
    /// Side whose selection is cleared when the job succeeds.
    source_side: usize,
    handle: JobHandle,
    progress: Progress,
    conflict: Option<Conflict>,
    started: Instant,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Key press with the text it produced; `bool` is true if a widget
    /// (e.g. a text input) already used it.
    Key(keyboard::Key, keyboard::Modifiers, Option<String>, bool),
    Modifiers(keyboard::Modifiers),
    Run(Command),
    Click(usize, usize),
    MiddleClick(usize, usize),
    RightPress(usize, usize),
    RightClickBackground(usize),
    ContextDone(usize, Result<MenuAction, String>),
    Hover(usize, usize),
    HoverEnd(usize),
    MouseMoved(Point),
    /// Mouse button released; `true` for the right button.
    MouseUp(bool),
    DragOutDone(usize, Result<DragOutcome, String>),
    DropMenuDone(usize, Result<DragOutcome, String>),
    FileDropped(PathBuf),
    DropPosition(Option<(f32, f32)>),
    Wheel(usize, f32),
    Drive(usize, String),
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
    DialogCancel,
    HotlistPick(usize),
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
    /// The zoomed image was scrolled (offset in pixels).
    ViewerPanned(iced::widget::scrollable::AbsoluteOffset),
    /// Pointer moved over (or left) the viewer body.
    ViewerPointer(Option<iced::Point>),
    /// Wheel anywhere while an image is shown (zooms when Ctrl is held).
    ViewerZoomWheel(f32),
    ViewerSearch(String),
    ViewerFind,
    MultiRename(multi_rename::MrMsg),
    /// Background listing for a pending navigation: (side, seq, listing).
    Navigated(usize, u64, Listing),
    /// Subfolders of a folder, for path bar suggestions.
    Subfolders(PathBuf, Vec<String>),
    /// A folder size: (folder listed, entry name, size).
    DirSize(PathBuf, String, u64),
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
            hotlist: session.as_ref().map(|s| s.hotlist.clone()).unwrap_or_default(),
            jobs: Vec::new(),
            next_job_id: 1,
            counts: Vec::new(),
            path_edit: None,
            rename: None,
            quick_search: None,
            filter_edit: None,
            drag: None,
            dropped: Vec::new(),
            mouse: Point::ORIGIN,
            hover: None,
            modifiers: keyboard::Modifiers::default(),
            icons: RefCell::new(HashMap::new()),
            disk: RefCell::new(HashMap::new()),
            listings: HashMap::new(),
            next_listing: 0,
            nav: [None, None],
            viewer: None,
            multi_rename: None,
            rename_opts: rc_core::rename::Options::default(),
            config: crate::config::Config::default(),
            keymap: keymap::Keymap::default(),
            config_stamp: None,
            placement: session.as_ref().and_then(|s| s.window).map(Into::into),
        };
        app.refresh_config();
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
        // be starting or in progress.
        if self.drag.is_some() {
            subs.push(event::listen_with(|event, _status, _window| match event {
                Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::MouseMoved(position)),
                _ => None,
            }));
        }
        // Watch the folders the visible tabs show, to refresh them on change.
        let mut dirs: Vec<PathBuf> = (0..2).map(|side| self.panel(side).path.clone()).collect();
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
        if !self.counts.is_empty() {
            self.prune_counts();
        }
        match message {
            Message::Key(key, mods, text, captured) => return self.on_key(key, mods, text, captured),
            Message::Modifiers(m) => self.modifiers = m,
            Message::Run(cmd) => return self.run(cmd),
            Message::Click(side, index) => {
                self.leave_edits();
                self.active = side;
                self.panel_mut(side).set_cursor(index);
                self.drag = Some(Drag { side, start: None, active: false, sources: Vec::new(), right: false, index });
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
                self.drag = Some(Drag { side, start: None, active: false, sources: Vec::new(), right: true, index });
            }
            Message::RightClickBackground(side) => {
                self.leave_edits();
                self.active = side;
                return self.context_menu(side, true);
            }
            Message::ContextDone(side, result) => match result {
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
                return self.update_drag();
            }
            Message::MouseUp(right) => return self.finish_drag(right),
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
                let side = position.map(|(x, _)| usize::from(x >= self.window_width / 2.0)).unwrap_or(self.active);
                self.active = side;
                let target = self.panel(side).path.clone();
                return self.open_transfer(false, sources, &target);
            }
            Message::Wheel(side, lines) => {
                let rows = self.page_rows();
                self.panel_mut(side).scroll_by((-lines * 3.0).round() as isize, rows);
            }
            Message::Drive(side, drive) => {
                self.leave_edits();
                self.active = side;
                self.drives = rc_platform::roots();
                return self.start_nav(side, Nav::to(drive));
            }
            Message::Sort(side, key) => {
                self.active = side;
                self.panel_mut(side).sort_by(key);
            }
            Message::CloseRequested => return self.quit(),
            Message::Quit(placement) => {
                if placement.is_some() {
                    self.placement = placement;
                }
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
                let text = self.panel(side).path.display().to_string();
                self.path_edit = Some(PathEdit { side, text, suggestions: Vec::new(), selected: None, cache: None });
                return Task::batch([text_input::focus(path_input_id()), text_input::select_all(path_input_id())]);
            }
            Message::PathInput(value) => {
                if let Some(edit) = &mut self.path_edit {
                    edit.text = value;
                }
                return self.refresh_suggestions();
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
                if let Some(Dialog::Transfer { input, .. } | Dialog::MakeDir { input } | Dialog::EditNew { input }) = &mut self.dialog {
                    *input = value;
                }
            }
            Message::DialogSubmit => return self.submit_dialog(),
            Message::DialogCancel => self.dialog = None,
            Message::HotlistPick(i) => return self.hotlist_pick(i),
            Message::Job(id, event) => self.on_job_event(id, event),
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
            Message::ViewerLoaded(seq, result) => return self.viewer_loaded(seq, result),
            Message::ViewerFound(seq, result, cancel) => self.viewer_found(seq, result, &cancel),
            Message::ViewerWheel(lines) => self.viewer_wheel(lines),
            Message::ViewerPanned(offset) => self.viewer_panned(offset),
            Message::ViewerPointer(at) => self.viewer_pointer(at),
            Message::ViewerZoomWheel(lines) => return self.viewer_zoom_wheel(lines),
            Message::ViewerSearch(text) => self.viewer_search_input(text),
            Message::ViewerFind => return self.viewer_submit(),
            Message::MultiRename(msg) => return self.multi_rename_update(msg),
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

        // A pending overwrite question blocks everything else.
        if let Some(id) = self.jobs.iter().find(|j| j.conflict.is_some()).map(|j| j.id) {
            if escape {
                return self.update(Message::Answer(id, Resolution::Cancel));
            }
            return Task::none();
        }
        if self.multi_rename.is_some() {
            if named.is_some_and(is_modifier_key) {
                return Task::none();
            }
            return self.multi_rename_key(named, &key, mods, captured);
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
        if let Some(Dialog::Hotlist { selected }) = self.dialog {
            if !named.is_some_and(is_modifier_key) {
                return self.hotlist_key(selected, named, text.as_deref());
            }
            return Task::none();
        }
        if self.dialog.is_some() {
            if escape {
                self.dialog = None;
            } else if named == Some(Named::Enter) && !captured {
                return self.submit_dialog();
            }
            return Task::none();
        }
        if self.rename.is_some() {
            if escape {
                self.rename = None;
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

        // Commands that touch more than the active panel or open dialogs.
        match cmd {
            Quit => return self.quit(),
            View => return self.open_viewer(),
            MultiRename => return self.open_multi_rename(),
            Edit => {
                if let Some(e) = self.panel(active).current().filter(|e| e.kind == EntryKind::File) {
                    let (path, name) = (e.path.clone(), e.name.clone());
                    self.edit_file(&path, &name);
                }
                return Task::none();
            }
            EditConfig => {
                if let Err(e) = crate::config::create_if_missing("") {
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
            Rename => return self.start_rename(),
            MakeDir => return self.open_dialog(Dialog::MakeDir { input: String::new() }),
            Hotlist => {
                self.leave_edits();
                // Start on the current folder if it is in the list.
                let here = &self.panel(active).path;
                let selected = self.hotlist.iter().position(|p| p == here).unwrap_or(0);
                self.dialog = Some(Dialog::Hotlist { selected });
                return Task::none();
            }
            Delete | DeletePermanent => {
                let paths = self.target_paths(active);
                if paths.is_empty() {
                    return Task::none();
                }
                self.dialog = Some(Dialog::Delete { paths, to_trash: cmd == Delete });
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
                if advance {
                    panel.move_cursor(1);
                }
                panel.ensure_visible(rows);
                return self.count_dirs(active, dir.into_iter().collect());
            }
            CountDirSizes => {
                let names = self.panel(active).entries.iter().filter(|e| e.kind == EntryKind::Dir);
                let names = names.map(|e| e.name.clone()).collect();
                return self.count_dirs(active, names);
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
            HistoryBack => Some(Nav::Back),
            HistoryForward => Some(Nav::Forward),
            _ => None,
        };
        if let Some(nav) = nav {
            return self.start_nav(active, nav);
        }

        if cmd == DeselectAll {
            self.cancel_counts();
        }
        let panel = self.panel_mut(active);
        let mut info = None;
        let result: io::Result<()> = match cmd {
            CursorUp => Ok(panel.move_cursor(-1)),
            CursorDown => Ok(panel.move_cursor(1)),
            PageUp => Ok(panel.move_cursor(-page)),
            PageDown => Ok(panel.move_cursor(page)),
            CursorHome => Ok(panel.cursor_home()),
            CursorEnd => Ok(panel.cursor_end()),
            CursorBy(n) => Ok(panel.move_cursor(n as isize)),
            Open => match panel.activate() {
                Activated::File(path) => launch(&path).map(|msg| info = Some(msg)),
                _ => Ok(()),
            },
            // At the root / no history: nothing to do.
            GoParent | GoRoot | HistoryBack | HistoryForward => Ok(()),
            SelectAll => Ok(panel.select_all()),
            DeselectAll => Ok(panel.deselect_all()),
            SortBy(key) => Ok(panel.sort_by(key)),
            _ => unreachable!("handled above"),
        };
        panel.ensure_visible(rows);
        self.report(result);
        if let Some(msg) = info {
            let tone = if matches!(cmd, Open | View | Edit) { Tone::Success } else { Tone::Info };
            self.status = Some((msg, tone));
        }
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
        let rename = InlineRename { side, src: entry.path.clone(), text: entry.name.clone() };
        self.path_edit = None;
        self.rename = Some(rename);
        Task::batch([text_input::focus(rename_input_id()), text_input::select_all(rename_input_id())])
    }

    fn commit_rename(&mut self) {
        let Some(InlineRename { side, src, text }) = self.rename.take() else { return };
        let name = text.trim();
        let Some(parent) = src.parent() else { return };
        if name.is_empty() || src.file_name().is_some_and(|n| n == name) {
            return;
        }
        let dst = parent.join(name);
        // Allow case-only renames, which "exist" on Windows.
        let result = if dst.exists() && !ops::same_path(&src, &dst) {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("'{name}' already exists")))
        } else {
            std::fs::rename(&src, &dst)
        };
        let ok = result.is_ok();
        self.report(result);
        self.reload_panels();
        if ok {
            self.panel_mut(side).focus(name);
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

    // ---- hotlist ----------------------------------------------------------

    fn hotlist_key(&mut self, selected: usize, named: Option<Named>, text: Option<&str>) -> Task<Message> {
        let count = self.hotlist.len() + 1;
        let mut select = |i: usize| self.dialog = Some(Dialog::Hotlist { selected: i });
        match named {
            Some(Named::Escape) => self.dialog = None,
            Some(Named::ArrowUp) => select((selected + count - 1) % count),
            Some(Named::ArrowDown) => select((selected + 1) % count),
            Some(Named::Home | Named::PageUp) => select(0),
            Some(Named::End | Named::PageDown) => select(count - 1),
            Some(Named::Enter) => return self.hotlist_pick(selected),
            Some(Named::Delete) if selected < self.hotlist.len() => {
                self.hotlist.remove(selected);
                self.dialog = Some(Dialog::Hotlist { selected: selected.min(self.hotlist.len()) });
                self.save_session();
            }
            _ => match text.map(|t| t.to_ascii_lowercase()).as_deref() {
                // 1-9 open the matching entry directly.
                Some(d @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")) => {
                    let i = d.parse::<usize>().unwrap_or(1) - 1;
                    if i < self.hotlist.len() {
                        return self.hotlist_pick(i);
                    }
                }
                Some("a" | "+") => return self.hotlist_pick(self.hotlist.len()),
                _ => {}
            },
        }
        Task::none()
    }

    /// Opens hotlist entry `i`; the index past the entries adds or removes
    /// the active panel's folder.
    fn hotlist_pick(&mut self, i: usize) -> Task<Message> {
        self.dialog = None;
        let active = self.active;
        if let Some(path) = self.hotlist.get(i).cloned() {
            return self.start_nav(active, Nav::to(path));
        }
        let here = self.panel(active).path.clone();
        let msg = if let Some(pos) = self.hotlist.iter().position(|p| *p == here) {
            self.hotlist.remove(pos);
            format!("Removed {} from the hotlist", here.display())
        } else {
            self.hotlist.push(here.clone());
            format!("Added {} to the hotlist", here.display())
        };
        self.status = Some((msg, Tone::Info));
        self.save_session();
        Task::none()
    }

    // ---- dialogs ----------------------------------------------------------

    fn open_transfer(&mut self, is_move: bool, sources: Vec<PathBuf>, target: &Path) -> Task<Message> {
        if sources.is_empty() {
            return Task::none();
        }
        let mut input = target.display().to_string();
        if !input.ends_with(MAIN_SEPARATOR) {
            input.push(MAIN_SEPARATOR);
        }
        let base = self.panel(self.active).path.clone();
        self.open_dialog(Dialog::Transfer { is_move, sources, base, input })
    }

    fn open_dialog(&mut self, dialog: Dialog) -> Task<Message> {
        self.leave_edits();
        self.dialog = Some(dialog);
        Task::batch([text_input::focus(dialog_input_id()), text_input::select_all(dialog_input_id())])
    }

    fn submit_dialog(&mut self) -> Task<Message> {
        let Some(dialog) = self.dialog.take() else { return Task::none() };
        match dialog {
            Dialog::Transfer { is_move, sources, base, input } => {
                let target = PathBuf::from(input.trim());
                let target = if target.is_relative() { base.join(target) } else { target };
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
            Dialog::Delete { paths, to_trash } => {
                let verb = if to_trash { "Recycling" } else { "Deleting" };
                let title = format!("{verb} {}", describe(&paths));
                return self.start_job(Job::Delete { paths, to_trash }, title);
            }
            Dialog::Hotlist { selected } => return self.hotlist_pick(selected),
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
                    return self.drag_out(drag);
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
            if drag.right {
                let on_parent = self.panel(drag.side).entries.get(drag.index).is_none_or(|e| e.is_parent());
                return self.context_menu(drag.side, on_parent);
            }
            return Task::none();
        }
        let Some(target) = self.drop_target(&drag) else { return Task::none() };
        self.active = drag.side;
        if drag.right {
            return self.drop_menu(drag, target);
        }
        self.open_transfer(self.modifiers.shift(), drag.sources, &target)
    }

    /// Right-button drop inside our window: let the shell show its drop
    /// menu for the target folder.
    fn drop_menu(&self, drag: Drag, target: PathBuf) -> Task<Message> {
        let side = drag.side;
        let folder = self.panel(side).path.clone();
        let items = drag.sources;
        window::get_oldest()
            .and_then(move |id| {
                let (folder, items, target) = (folder.clone(), items.clone(), target.clone());
                window::run_with_handle(id, move |handle| {
                    rc_platform::drop_menu(native_handle(&handle), &folder, &items, &target)
                })
            })
            .map(move |result| Message::DropMenuDone(side, result))
    }

    fn drag_out(&self, drag: Drag) -> Task<Message> {
        let side = drag.side;
        let folder = self.panel(side).path.clone();
        let items = drag.sources;
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
        usize::from(self.mouse.x >= self.window_width / 2.0)
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

    // ---- context menu -----------------------------------------------------

    fn context_menu(&self, side: usize, background: bool) -> Task<Message> {
        let folder = self.panel(side).path.clone();
        let items = if background { Vec::new() } else { self.target_paths(side) };
        let extended = self.modifiers.shift();

        window::get_oldest()
            .and_then(move |id| {
                let (folder, items) = (folder.clone(), items.clone());
                window::run_with_handle(id, move |handle| {
                    rc_platform::show_context_menu(native_handle(&handle), &folder, &items, extended)
                })
            })
            .map(move |result| Message::ContextDone(side, result))
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
            source_side: self.active,
            handle,
            progress: Progress::default(),
            conflict: None,
            started: Instant::now(),
        });
        Task::run(rx, move |event| Message::Job(id, event))
    }

    fn on_job_event(&mut self, id: u64, event: JobEvent) {
        match event {
            JobEvent::Progress(progress) => {
                if let Some(job) = self.job(id) {
                    job.progress = progress;
                }
            }
            JobEvent::Conflict(conflict) => {
                if let Some(job) = self.job(id) {
                    job.conflict = Some(conflict);
                }
            }
            JobEvent::Finished(outcome) => {
                let Some(pos) = self.jobs.iter().position(|j| j.id == id) else { return };
                let job = self.jobs.remove(pos);
                if outcome.errors.is_empty() && !outcome.cancelled && outcome.skipped == 0 {
                    self.panel_mut(job.source_side).deselect_all();
                }
                self.disk.borrow_mut().clear();
                self.reload_panels();
                self.status = Some(summarize(job.done_verb, &outcome));
                self.after_change();
            }
        }
    }

    fn job(&mut self, id: u64) -> Option<&mut RunningJob> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    // ---- helpers ----------------------------------------------------------

    /// Re-reads the visible tab on both sides after something changed.
    fn reload_panels(&mut self) {
        for side in 0..2 {
            if let Err(e) = self.panel_mut(side).reload() {
                self.folder_unreadable(side, e.to_string());
            }
        }
    }

    /// The folder of `side` can't be listed any more: it may be gone, so walk
    /// up to something that exists.
    fn folder_unreadable(&mut self, side: usize, error: String) {
        self.nav[side] = None;
        let panel = self.panel_mut(side);
        let existing = panel.path.ancestors().skip(1).find(|p| p.is_dir()).map(Path::to_path_buf);
        match existing {
            Some(p) => {
                let _ = panel.navigate(p, None);
            }
            None => self.status = Some((error, Tone::Error)),
        }
    }

    /// Lists `path` on a worker thread; the result updates every visible tab
    /// still showing it (`Message::Listed`).
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
                let size = rc_core::fs::dir_size(&dir.join(&name), &cancel);
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

    fn list_in_background(&mut self, path: PathBuf) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        self.listings.insert(path.clone(), seq);
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                let dir = path.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(rc_core::fs::list_dir(&dir, true).map_err(|e| e.to_string()));
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
                Task::none()
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
        let loaded = crate::config::load().and_then(|config| {
            let keymap = keymap::Keymap::new(&config.keys).map_err(|e| format!("config.toml [keys]: {e}"))?;
            let colors = theme::DARK.with(&config.colors).map_err(|e| format!("config.toml [colors]: {e}"))?;
            Ok((config, keymap, colors))
        });
        match loaded {
            Ok((config, keymap, colors)) => {
                self.config = config;
                self.keymap = keymap;
                theme::set(colors);
                if !first {
                    self.status = Some(("Settings reloaded".into(), Tone::Info));
                }
            }
            Err(e) => self.status = Some((format!("Bad config: {e}"), Tone::Error)),
        }
    }

    /// Opens `path` in the editor from `config.toml` (F4, Shift+F4).
    fn edit_file(&mut self, path: &Path, name: &str) {
        self.refresh_config();
        self.status = Some(match crate::editor::launch(&self.config.editor, path) {
            Ok(()) => (format!("Editing {name}"), Tone::Success),
            Err(e) => (format!("Can't start the editor: {e}"), Tone::Error),
        });
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
        let chrome = CHROME_H + self.jobs.len() as f32 * JOB_ROW_H;
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
    if path.is_relative() {
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
    match rc_core::fs::list_dir(path, true) {
        Ok(entries) => Ok(Listed { file: None, entries }),
        Err(_) if file_ok && path.is_file() => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let parent = path.parent().ok_or("No parent folder")?;
            let entries = rc_core::fs::list_dir(parent, true).map_err(|e| e.to_string())?;
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
