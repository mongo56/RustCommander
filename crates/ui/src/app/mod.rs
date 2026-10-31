//! Application state and update logic. Drawing lives in [`view`].

mod view;

use crate::keymap;
use crate::theme;
use iced::keyboard::key::Named;
use iced::widget::{image, text_input};
use iced::{event, keyboard, mouse, window, Event, Point, Size, Subscription, Task, Theme};
use rc_core::ops::{self, Conflict, Job, JobEvent, JobHandle, Outcome, Progress, Resolution};
use rc_core::{Activated, Command, EntryKind, Panel, SortKey};
use rc_platform::MenuAction;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf, MAIN_SEPARATOR};
use std::time::{Duration, Instant};

const ROW_H: f32 = 22.0;
/// Approximate height of everything that isn't file rows (headers, footers,
/// function-key bar). Only used to size PageUp/PageDown jumps.
const CHROME_H: f32 = 180.0;
const JOB_ROW_H: f32 = 30.0;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How far the mouse must move with the button down before a drag starts.
const DRAG_THRESHOLD: f32 = 6.0;

fn dialog_input_id() -> text_input::Id {
    text_input::Id::new("dialog-input")
}

fn path_input_id() -> text_input::Id {
    text_input::Id::new("path-input")
}

pub struct App {
    panels: [Panel; 2],
    active: usize,
    drives: Vec<String>,
    status: Option<(String, Tone)>,
    window_width: f32,
    window_height: f32,
    last_click: Option<(usize, usize, Instant)>,
    dialog: Option<Dialog>,
    jobs: Vec<RunningJob>,
    next_job_id: u64,
    /// Panel whose path bar is being edited, and the text.
    path_edit: Option<(usize, String)>,
    drag: Option<Drag>,
    /// Last known mouse position (window coordinates).
    mouse: Point,
    /// Row under the mouse: (panel, index).
    hover: Option<(usize, usize)>,
    modifiers: keyboard::Modifiers,
    /// Icon cache keyed by `rc_platform::icon_key`. Filled lazily from
    /// `view`, hence the RefCell.
    icons: RefCell<HashMap<String, Option<image::Handle>>>,
    /// Free/total space per volume root, refreshed every few seconds.
    disk: RefCell<HashMap<PathBuf, DiskEntry>>,
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
    Rename { src: PathBuf, input: String },
    Delete { paths: Vec<PathBuf>, to_trash: bool },
}

#[derive(Debug)]
struct Drag {
    side: usize,
    start: Point,
    /// Becomes true once the mouse moved past the threshold.
    active: bool,
    sources: Vec<PathBuf>,
}

struct RunningJob {
    id: u64,
    title: String,
    done_verb: &'static str,
    /// Panel whose selection is cleared when the job succeeds.
    source_panel: usize,
    handle: JobHandle,
    progress: Progress,
    conflict: Option<Conflict>,
    started: Instant,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Key press; `bool` is true if a widget (e.g. a text input) used it.
    Key(keyboard::Key, keyboard::Modifiers, bool),
    Modifiers(keyboard::Modifiers),
    Run(Command),
    Click(usize, usize),
    RightClick(usize, usize),
    RightClickBackground(usize),
    ContextDone(usize, Result<MenuAction, String>),
    Hover(usize, usize),
    HoverEnd(usize),
    MouseMoved(Point),
    MouseUp,
    Wheel(usize, f32),
    Drive(usize, String),
    Sort(usize, SortKey),
    Resized(Size),
    EditPath(usize),
    PathInput(String),
    PathSubmit,
    DialogInput(String),
    DialogSubmit,
    DialogCancel,
    Job(u64, JobEvent),
    JobPause(u64),
    JobCancel(u64),
    Answer(u64, Resolution),
    ReloadPanels,
}

impl App {
    pub fn new() -> (Self, Task<Message>) {
        let home = rc_platform::home_dir();
        let left = std::env::current_dir().ok().or_else(|| home.clone());
        let right = home.clone().or_else(|| left.clone());

        let app = Self {
            panels: [open_panel(left), open_panel(right)],
            active: 0,
            drives: rc_platform::roots(),
            status: None,
            window_width: 1280.0,
            window_height: 800.0,
            last_click: None,
            dialog: None,
            jobs: Vec::new(),
            next_job_id: 1,
            path_edit: None,
            drag: None,
            mouse: Point::ORIGIN,
            hover: None,
            modifiers: keyboard::Modifiers::default(),
            icons: RefCell::new(HashMap::new()),
            disk: RefCell::new(HashMap::new()),
        };
        (app, Task::none())
    }

    pub fn theme(&self) -> Theme {
        theme::theme()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            event::listen_with(|event, status, _window| match event {
                Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                    Some(Message::Key(key, modifiers, status == event::Status::Captured))
                }
                Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => Some(Message::Modifiers(m)),
                Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::MouseMoved(position)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => Some(Message::MouseUp),
                _ => None,
            }),
            window::resize_events().map(|(_id, size)| Message::Resized(size)),
        ])
    }

    // ---- update -----------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Key(key, mods, captured) => return self.on_key(key, mods, captured),
            Message::Modifiers(m) => self.modifiers = m,
            Message::Run(cmd) => return self.run(cmd),
            Message::Click(side, index) => {
                self.path_edit = None;
                self.active = side;
                self.panels[side].set_cursor(index);
                self.drag = Some(Drag { side, start: self.mouse, active: false, sources: Vec::new() });
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
            Message::RightClick(side, index) => {
                self.path_edit = None;
                self.active = side;
                let panel = &mut self.panels[side];
                let Some(entry) = panel.entries.get(index) else { return Task::none() };
                if entry.is_parent() {
                    return self.context_menu(side, true);
                }
                // Explorer behaviour: right-clicking outside the selection
                // makes the clicked item the only target.
                if !panel.is_selected(entry) {
                    panel.deselect_all();
                    panel.set_cursor(index);
                }
                return self.context_menu(side, false);
            }
            Message::RightClickBackground(side) => {
                self.path_edit = None;
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
                    self.reload_panels();
                    // Some shell verbs (paste, extract, ...) finish a moment later.
                    return delayed(Duration::from_millis(800), Message::ReloadPanels);
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
                self.update_drag();
            }
            Message::MouseUp => return self.finish_drag(),
            Message::Wheel(side, lines) => {
                let rows = self.page_rows();
                let panel = &mut self.panels[side];
                panel.move_cursor((-lines * 3.0).round() as isize);
                panel.ensure_visible(rows);
            }
            Message::Drive(side, drive) => {
                self.active = side;
                self.path_edit = None;
                self.drives = rc_platform::roots();
                let result = self.panels[side].navigate(PathBuf::from(drive), None);
                self.report(result);
            }
            Message::Sort(side, key) => {
                self.active = side;
                self.panels[side].sort_by(key);
            }
            Message::Resized(size) => {
                self.window_width = size.width;
                self.window_height = size.height;
            }
            Message::EditPath(side) => {
                self.active = side;
                self.path_edit = Some((side, self.panels[side].path.display().to_string()));
                return Task::batch([text_input::focus(path_input_id()), text_input::select_all(path_input_id())]);
            }
            Message::PathInput(value) => {
                if let Some((_, text)) = &mut self.path_edit {
                    *text = value;
                }
            }
            Message::PathSubmit => self.submit_path(),
            Message::DialogInput(value) => {
                if let Some(
                    Dialog::Transfer { input, .. }
                    | Dialog::MakeDir { input }
                    | Dialog::Rename { input, .. },
                ) = &mut self.dialog
                {
                    *input = value;
                }
            }
            Message::DialogSubmit => return self.submit_dialog(),
            Message::DialogCancel => self.dialog = None,
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
            Message::ReloadPanels => self.reload_panels(),
        }
        Task::none()
    }

    fn on_key(&mut self, key: keyboard::Key, mods: keyboard::Modifiers, captured: bool) -> Task<Message> {
        let escape = key == keyboard::Key::Named(Named::Escape);
        let enter = key == keyboard::Key::Named(Named::Enter);

        // A pending overwrite question blocks everything else.
        if let Some(id) = self.jobs.iter().find(|j| j.conflict.is_some()).map(|j| j.id) {
            if escape {
                return self.update(Message::Answer(id, Resolution::Cancel));
            }
            return Task::none();
        }
        if self.dialog.is_some() {
            if escape {
                self.dialog = None;
            } else if enter && !captured {
                return self.submit_dialog();
            }
            return Task::none();
        }
        if self.path_edit.is_some() {
            if escape {
                self.path_edit = None;
            }
            return Task::none();
        }
        if escape && self.drag.as_ref().is_some_and(|d| d.active) {
            self.drag = None;
            return Task::none();
        }
        if captured {
            return Task::none();
        }
        match keymap::command_for(&key, mods) {
            Some(cmd) => self.run(cmd),
            None => Task::none(),
        }
    }

    #[allow(clippy::unit_arg)] // `Ok(panel.move_cursor(..))` reads better here
    fn run(&mut self, cmd: Command) -> Task<Message> {
        use Command::*;
        self.status = None;
        let rows = self.page_rows();
        let page = rows.saturating_sub(1).max(1) as isize;
        let other = 1 - self.active;

        // Commands that touch more than the active panel or open dialogs.
        match cmd {
            Quit => return iced::exit(),
            SwitchPanel => {
                self.active = other;
                return Task::none();
            }
            SyncOtherPanel => {
                let path = self.panels[self.active].path.clone();
                let result = self.panels[other].navigate(path, None);
                self.report(result);
                return Task::none();
            }
            Copy | Move => {
                let sources = self.target_paths(self.active);
                if sources.is_empty() {
                    return Task::none();
                }
                let target = self.panels[other].path.clone();
                return self.open_transfer(cmd == Move, sources, &target);
            }
            Rename => {
                let Some(entry) = self.panels[self.active].current().filter(|e| !e.is_parent()) else {
                    return Task::none();
                };
                let dialog = Dialog::Rename { src: entry.path.clone(), input: entry.name.clone() };
                return self.open_dialog(dialog);
            }
            MakeDir => return self.open_dialog(Dialog::MakeDir { input: String::new() }),
            Delete | DeletePermanent => {
                let paths = self.target_paths(self.active);
                if paths.is_empty() {
                    return Task::none();
                }
                self.dialog = Some(Dialog::Delete { paths, to_trash: cmd == Delete });
                return Task::none();
            }
            Refresh => {
                self.drives = rc_platform::roots();
                self.disk.borrow_mut().clear();
            }
            _ => {}
        }

        let panel = &mut self.panels[self.active];
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
                Ok(Activated::File(path)) => launch(&path).map(|msg| info = Some(msg)),
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            },
            GoParent => panel.go_parent(),
            GoRoot => panel.go_root(),
            ToggleSelect { advance } => {
                panel.toggle_select_current();
                if advance {
                    panel.move_cursor(1);
                }
                Ok(())
            }
            SelectAll => Ok(panel.select_all()),
            DeselectAll => Ok(panel.deselect_all()),
            Refresh => panel.reload(),
            ToggleHidden => {
                let r = panel.toggle_hidden();
                let state = if panel.show_hidden { "shown" } else { "hidden" };
                info = Some(format!("Hidden files {state}"));
                r
            }
            SortBy(key) => Ok(panel.sort_by(key)),
            // Until the built-in viewer/editor exist, hand off to the OS.
            View | Edit => match panel.current() {
                Some(e) if e.kind == EntryKind::File => {
                    rc_platform::open_default(&e.path).map(|_| info = Some(format!("Opened {}", e.name)))
                }
                _ => Ok(()),
            },
            Quit | SwitchPanel | SyncOtherPanel | Copy | Move | Rename | MakeDir | Delete
            | DeletePermanent => unreachable!(),
        };
        panel.ensure_visible(rows);
        self.report(result);
        if let Some(msg) = info {
            let tone = if cmd == Open || cmd == View || cmd == Edit { Tone::Success } else { Tone::Info };
            self.status = Some((msg, tone));
        }
        Task::none()
    }

    fn target_paths(&self, side: usize) -> Vec<PathBuf> {
        self.panels[side].targets().into_iter().map(|e| e.path.clone()).collect()
    }

    fn open_transfer(&mut self, is_move: bool, sources: Vec<PathBuf>, target: &Path) -> Task<Message> {
        let mut input = target.display().to_string();
        if !input.ends_with(MAIN_SEPARATOR) {
            input.push(MAIN_SEPARATOR);
        }
        let base = self.panels[self.active].path.clone();
        self.open_dialog(Dialog::Transfer { is_move, sources, base, input })
    }

    fn open_dialog(&mut self, dialog: Dialog) -> Task<Message> {
        self.path_edit = None;
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
                let panel = &mut self.panels[self.active];
                let result = std::fs::create_dir_all(panel.path.join(name)).and_then(|_| panel.reload());
                // Focus the first component of what was created ("a\b" → "a").
                if let Some(first) = Path::new(name).components().next() {
                    panel.focus(&first.as_os_str().to_string_lossy());
                }
                self.after_change();
                self.report(result);
            }
            Dialog::Rename { src, input } => {
                let name = input.trim();
                let Some(parent) = src.parent() else { return Task::none() };
                if name.is_empty() || src.file_name().is_some_and(|n| n == name) {
                    return Task::none();
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
                    self.panels[self.active].focus(name);
                    self.after_change();
                }
            }
            Dialog::Delete { paths, to_trash } => {
                let verb = if to_trash { "Recycling" } else { "Deleting" };
                let title = format!("{verb} {}", describe(&paths));
                return self.start_job(Job::Delete { paths, to_trash }, title);
            }
        }
        Task::none()
    }

    fn submit_path(&mut self) {
        let Some((side, input)) = self.path_edit.take() else { return };
        let raw = input.trim().trim_matches('"');
        if raw.is_empty() {
            return;
        }
        let panel = &mut self.panels[side];
        let mut path = match raw.strip_prefix('~') {
            Some(rest) => rc_platform::home_dir()
                .map(|h| h.join(rest.trim_start_matches(['/', '\\'])))
                .unwrap_or_else(|| PathBuf::from(raw)),
            None => PathBuf::from(raw),
        };
        // "D:" means the root of D:, not D:'s current directory.
        if raw.len() == 2 && raw.ends_with(':') {
            path.push(MAIN_SEPARATOR.to_string());
        }
        if path.is_relative() {
            path = panel.path.join(path);
        }
        let path = normalize_lexically(&path);

        let result = if path.is_dir() {
            panel.navigate(path, None)
        } else if path.is_file() {
            // Jump to the folder and put the cursor on the file.
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            match path.parent() {
                Some(parent) => panel.navigate(parent.to_path_buf(), name.as_deref()),
                None => Ok(()),
            }
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, format!("Path not found: {}", path.display())))
        };
        self.after_change();
        self.report(result);
    }

    // ---- drag & drop ------------------------------------------------------

    fn update_drag(&mut self) {
        let Some(drag) = &self.drag else { return };
        if drag.active {
            return;
        }
        let distance = self.mouse.distance(drag.start);
        if distance < DRAG_THRESHOLD {
            return;
        }
        let sources = self.target_paths(drag.side);
        if sources.is_empty() {
            self.drag = None; // e.g. dragging ".."
        } else if let Some(drag) = &mut self.drag {
            drag.sources = sources;
            drag.active = true;
        }
    }

    fn finish_drag(&mut self) -> Task<Message> {
        let Some(drag) = self.drag.take() else { return Task::none() };
        if !drag.active {
            return Task::none();
        }
        let Some(target) = self.drop_target(&drag) else { return Task::none() };
        self.active = drag.side;
        self.open_transfer(self.modifiers.shift(), drag.sources, &target)
    }

    /// Panel the mouse is over (0 = left, 1 = right).
    fn panel_under_mouse(&self) -> usize {
        usize::from(self.mouse.x >= self.window_width / 2.0)
    }

    /// Where a drop at the current mouse position would go: the folder row
    /// under the mouse, else the other panel's folder. `None` means "no
    /// valid target" (e.g. back onto the same panel).
    fn drop_target(&self, drag: &Drag) -> Option<PathBuf> {
        let side = self.panel_under_mouse();
        let panel = &self.panels[side];
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
        let folder = self.panels[side].path.clone();
        let items = if background { Vec::new() } else { self.target_paths(side) };
        let extended = self.modifiers.shift();

        window::get_oldest()
            .and_then(move |id| {
                let (folder, items) = (folder.clone(), items.clone());
                window::run_with_handle(id, move |handle| {
                    let hwnd = native_handle(&handle);
                    rc_platform::show_context_menu(hwnd, &folder, &items, extended)
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
            source_panel: self.active,
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
                    self.panels[job.source_panel].deselect_all();
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

    /// Re-reads both panels after something changed on disk.
    fn reload_panels(&mut self) {
        for panel in &mut self.panels {
            if let Err(e) = panel.reload() {
                // The folder itself may be gone: walk up to something that exists.
                let existing = panel.path.ancestors().skip(1).find(|p| p.is_dir()).map(Path::to_path_buf);
                match existing {
                    Some(p) => {
                        let _ = panel.navigate(p, None);
                    }
                    None => self.status = Some((e.to_string(), Tone::Error)),
                }
            }
        }
    }

    fn after_change(&mut self) {
        let rows = self.page_rows();
        for panel in &mut self.panels {
            panel.ensure_visible(rows);
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
