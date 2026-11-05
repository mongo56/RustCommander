//! Synchronize folders (Alt+Shift+F2, TC's `cm_SyncDirs`): fills the window
//! while open. Compares the two panels' folders recursively on a worker
//! (`rc_core::sync::compare_trees`, through `vfs`, so archives and servers
//! work too) and lists every file that is on one side only or differs, with
//! a direction per row chosen by the mode (two-way, mirror, update right);
//! Space or a click on the direction changes it. Nothing is copied or
//! deleted before "Synchronize" and its confirmation, which counts the
//! copies, the overwrites of newer files and the deletions (to the Recycle
//! Bin for local folders). The plan then runs as one job per direction,
//! one after another (`App::sync_chain`), overwriting without asking.
//! The mode and options are kept in `config.toml` `[sync]`.

use super::*;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{button, checkbox, column, container, mouse_area, responsive, row, text, Column, Row, Space};
use iced::{Alignment, Color, Element, Length};
use rc_core::panel::visible_start;
use rc_core::sync::{self, Action, Comparison, Mode, State, Summary};

const ROW_H: f32 = 20.0;
const COL_SIZE: f32 = 80.0;
const COL_DATE: f32 = 120.0;
const COL_DIR: f32 = 64.0;
/// Height of the header, list header and footer (for paging estimates).
const SYNC_CHROME_H: f32 = 200.0;

/// Which rows the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    LeftOnly,
    RightOnly,
    /// On both sides and different (newer on one side, or a conflict).
    Changed,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::LeftOnly, Filter::RightOnly, Filter::Changed];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::LeftOnly => "Left only",
            Filter::RightOnly => "Right only",
            Filter::Changed => "Changed",
        }
    }

    fn shows(self, state: State, show_equal: bool) -> bool {
        match self {
            Filter::All => show_equal || state != State::Equal,
            Filter::LeftOnly => state == State::LeftOnly,
            Filter::RightOnly => state == State::RightOnly,
            Filter::Changed => matches!(state, State::LeftNewer | State::RightNewer | State::Different),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    ByContent,
    IgnoreDate,
    ShowEqual,
}

#[derive(Debug, Clone)]
pub enum SyncProgress {
    /// Folder being compared, folders done.
    Folder(PathBuf, u64),
    Done(Result<Option<Comparison>, String>),
}

#[derive(Debug, Clone)]
pub enum SyncMsg {
    Mode(Mode),
    Toggle(Opt),
    Filter(Filter),
    Compare,
    Stop,
    Progress(u64, SyncProgress),
    /// Click on a row (index into the visible rows).
    Click(usize),
    /// Click on a row's direction: the next allowed action.
    Cycle(usize),
    /// The cursor row's action, if the row allows it.
    Set(Action),
    /// Every visible row back to the mode's default.
    Defaults,
    /// Every visible row skipped.
    SkipAll,
    /// Show the confirmation.
    Synchronize,
    /// Confirmed: start the jobs.
    Confirm,
    /// Back from the confirmation.
    Back,
    Close,
    Wheel(f32),
    ScrollTo(usize, usize),
}

pub(super) struct SyncTool {
    left: PathBuf,
    right: PathBuf,
    mode: Mode,
    by_content: bool,
    ignore_date: bool,
    show_equal: bool,
    filter: Filter,
    /// The last comparison and the chosen action of each row.
    rows: Vec<sync::Row>,
    actions: Vec<Action>,
    /// Indices into `rows` that the filter shows.
    visible: Vec<usize>,
    cursor: usize,
    top: usize,
    compared: bool,
    /// Folders that couldn't be read.
    errors: Vec<String>,
    running: Option<Arc<AtomicBool>>,
    seq: u64,
    folder: PathBuf,
    folders: u64,
    note: Option<(String, Tone)>,
    confirm: bool,
}

impl SyncTool {
    fn stop(&mut self) {
        if let Some(cancel) = self.running.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn refilter(&mut self) {
        let current = self.visible.get(self.cursor).copied();
        let (filter, equal) = (self.filter, self.show_equal);
        self.visible = (0..self.rows.len()).filter(|&i| filter.shows(self.rows[i].state, equal)).collect();
        self.cursor = current.and_then(|c| self.visible.iter().position(|&i| i >= c)).unwrap_or(0);
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
        self.top = self.top.min(self.cursor);
    }

    fn defaults(&mut self, only_visible: bool) {
        let mode = self.mode;
        let all: Vec<usize> = if only_visible { self.visible.clone() } else { (0..self.rows.len()).collect() };
        for i in all {
            self.actions[i] = sync::default_action(&self.rows[i], mode);
        }
    }

    fn summary(&self) -> Summary {
        sync::summarize(&self.rows, &self.actions)
    }

    fn state(&self) -> crate::config::SyncState {
        crate::config::SyncState {
            mode: self.mode.id().to_owned(),
            by_content: self.by_content,
            ignore_date: self.ignore_date,
            show_equal: self.show_equal,
        }
    }
}

/// Whether one folder is inside the other (or they're the same).
fn nested(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| {
        let s = p.to_string_lossy().replace('/', "\\").to_lowercase();
        let s = s.trim_end_matches('\\').to_owned();
        s + "\\"
    };
    let (a, b) = (key(a), key(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// "3 files, 1.2 MB".
fn files_text((files, bytes): (u64, u64)) -> String {
    format!("{files} file{} ({})", if files == 1 { "" } else { "s" }, rc_core::format::size(bytes))
}

fn summary_line(s: &Summary) -> String {
    let mut parts = vec![
        format!("\u{2192} {}", files_text(s.to_right)),
        format!("\u{2190} {}", files_text(s.to_left)),
    ];
    if s.deletions() > 0 {
        parts.push(format!("{} to delete ({} files)", s.deletions(), s.delete_right.1 + s.delete_left.1));
    }
    if s.overwrite_newer > 0 {
        parts.push(format!("{} newer file(s) overwritten", s.overwrite_newer));
    }
    parts.join(" \u{b7} ")
}

/// Whether deleting under `root` is permanent (archives and servers have
/// no Recycle Bin).
fn deletes_permanently(root: &Path) -> bool {
    vfs::in_archive(root)
}

impl App {
    pub(super) fn open_sync(&mut self) -> Task<Message> {
        let (left, right) = (self.panel(0).path.clone(), self.panel(1).path.clone());
        if (0..2).any(|side| self.panel(side).results().is_some()) {
            self.info("Can't synchronize search results");
            return Task::none();
        }
        if nested(&left, &right) {
            self.info("One panel's folder is inside the other's: can't synchronize them");
            return Task::none();
        }
        self.leave_edits();
        let saved = self.config.sync.clone().unwrap_or_default();
        let tool = SyncTool {
            left,
            right,
            mode: saved.mode(),
            by_content: saved.by_content,
            ignore_date: saved.ignore_date,
            show_equal: saved.show_equal,
            filter: Filter::All,
            rows: Vec::new(),
            actions: Vec::new(),
            visible: Vec::new(),
            cursor: 0,
            top: 0,
            compared: false,
            errors: Vec::new(),
            running: None,
            seq: 0,
            folder: PathBuf::new(),
            folders: 0,
            note: None,
            confirm: false,
        };
        self.screen = Some(Screen::Sync(Box::new(tool)));
        self.start_sync_compare()
    }

    fn close_sync(&mut self) {
        if let Some(mut t) = self.screen.take_sync() {
            t.stop();
        }
    }

    /// Writes the tool's mode and options to `config.toml` `[sync]`.
    fn save_sync_options(&mut self) {
        let Some(t) = self.screen.sync_mut() else { return };
        let state = t.state();
        let result = crate::config::write(&[crate::config::Edit::Sync(state)]).and_then(|()| self.reload_config());
        if let (Err(e), Some(t)) = (result, self.screen.sync_mut()) {
            t.note = Some((format!("Options not saved: {e}"), Tone::Error));
        }
    }

    fn sync_rows(&self) -> usize {
        ((self.window_height - SYNC_CHROME_H) / ROW_H).floor().max(1.0) as usize
    }

    fn sync_move(&mut self, delta: isize) {
        let rows = self.sync_rows();
        let Some(t) = self.screen.sync_mut() else { return };
        let max = t.visible.len().saturating_sub(1);
        t.cursor = t.cursor.saturating_add_signed(delta).min(max);
        t.top = visible_start(t.top, t.cursor, rows, t.visible.len());
    }

    fn start_sync_compare(&mut self) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        let hidden = self.panel(0).show_hidden && self.panel(1).show_hidden;
        let Some(t) = self.screen.sync_mut() else { return Task::none() };
        t.stop();
        let cancel = Arc::new(AtomicBool::new(false));
        t.running = Some(cancel.clone());
        t.seq = seq;
        t.confirm = false;
        t.note = None;
        t.folders = 0;
        t.folder = t.left.clone();
        let opts = rc_core::compare::CompareOptions {
            by_content: t.by_content,
            ignore_date: t.ignore_date,
            subfolders: true,
            show_hidden: hidden,
        };
        let (left, right) = (t.left.clone(), t.right.clone());
        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let mut last = Instant::now();
            let mut done = 0;
            let result = sync::compare_trees(&left, &right, &opts, &sync::AnySource, &cancel, &mut |folder| {
                done += 1;
                if last.elapsed() >= Duration::from_millis(80) {
                    last = Instant::now();
                    let _ = tx.unbounded_send(SyncProgress::Folder(folder.to_path_buf(), done));
                }
            });
            let _ = tx.unbounded_send(SyncProgress::Done(result));
        });
        Task::run(rx, move |p| Message::Sync(SyncMsg::Progress(seq, p)))
    }

    pub(super) fn sync_update(&mut self, msg: SyncMsg) -> Task<Message> {
        let Some(t) = self.screen.sync_mut() else { return Task::none() };
        // While the confirmation shows, only its answers count.
        if t.confirm && !matches!(msg, SyncMsg::Confirm | SyncMsg::Back | SyncMsg::Close | SyncMsg::Progress(..)) {
            return Task::none();
        }
        match msg {
            SyncMsg::Mode(mode) => {
                if t.mode != mode {
                    t.mode = mode;
                    t.defaults(false);
                    self.save_sync_options();
                }
            }
            SyncMsg::Toggle(opt) => {
                match opt {
                    Opt::ByContent => t.by_content = !t.by_content,
                    Opt::IgnoreDate => t.ignore_date = !t.ignore_date,
                    Opt::ShowEqual => {
                        t.show_equal = !t.show_equal;
                        t.refilter();
                    }
                }
                self.save_sync_options();
                // What counts as different changed: compare again.
                if opt != Opt::ShowEqual {
                    return self.start_sync_compare();
                }
            }
            SyncMsg::Filter(filter) => {
                t.filter = filter;
                t.refilter();
            }
            SyncMsg::Compare => return self.start_sync_compare(),
            SyncMsg::Stop => {
                t.stop();
                t.note = Some(("Comparison stopped".into(), Tone::Info));
            }
            SyncMsg::Progress(seq, p) => {
                if seq != t.seq {
                    return Task::none();
                }
                match p {
                    SyncProgress::Folder(folder, done) => {
                        t.folder = folder;
                        t.folders = done;
                    }
                    SyncProgress::Done(result) => {
                        t.running = None;
                        match result {
                            Ok(Some(c)) => {
                                t.actions = c.rows.iter().map(|r| sync::default_action(r, t.mode)).collect();
                                t.rows = c.rows;
                                t.errors = c.errors;
                                t.folders = c.folders;
                                t.compared = true;
                                t.cursor = 0;
                                t.top = 0;
                                t.visible.clear();
                                t.refilter();
                                let differ = t.rows.iter().filter(|r| r.state != State::Equal).count();
                                t.note = Some(if differ == 0 && t.errors.is_empty() {
                                    ("The folders are identical".into(), Tone::Success)
                                } else if t.errors.is_empty() {
                                    (format!("{differ} difference(s) in {} folders", t.folders), Tone::Info)
                                } else {
                                    let first = &t.errors[0];
                                    (format!("{differ} difference(s); {} folder(s) unreadable, e.g. {first}", t.errors.len()), Tone::Error)
                                });
                            }
                            Ok(None) => t.note = Some(("Comparison stopped".into(), Tone::Info)),
                            Err(e) => t.note = Some((e, Tone::Error)),
                        }
                    }
                }
            }
            SyncMsg::Click(i) => {
                t.cursor = i.min(t.visible.len().saturating_sub(1));
            }
            SyncMsg::Cycle(i) => {
                t.cursor = i.min(t.visible.len().saturating_sub(1));
                if let Some(&r) = t.visible.get(i) {
                    t.actions[r] = t.rows[r].next_action(t.actions[r]);
                }
            }
            SyncMsg::Set(action) => {
                if let Some(&r) = t.visible.get(t.cursor) {
                    if t.rows[r].actions().contains(&action) {
                        t.actions[r] = action;
                    }
                }
                self.sync_move(1);
            }
            SyncMsg::Defaults => t.defaults(true),
            SyncMsg::SkipAll => {
                for &r in &t.visible {
                    t.actions[r] = Action::Skip;
                }
            }
            SyncMsg::Synchronize => {
                if t.running.is_some() {
                    return Task::none();
                }
                if t.summary().is_empty() {
                    t.note = Some(("Nothing to do: every row is skipped".into(), Tone::Info));
                } else {
                    t.confirm = true;
                }
            }
            SyncMsg::Back => t.confirm = false,
            SyncMsg::Confirm => return self.sync_run(),
            SyncMsg::Close => self.close_sync(),
            SyncMsg::Wheel(lines) => self.sync_move((-lines * 3.0).round() as isize),
            SyncMsg::ScrollTo(start, shown) => {
                let len = t.visible.len();
                let start = start.min(len.saturating_sub(shown));
                t.cursor = t.cursor.clamp(start, (start + shown).saturating_sub(1).min(len.saturating_sub(1)));
                t.top = start;
            }
        }
        Task::none()
    }

    /// Starts the confirmed plan: copies to the right, copies to the left,
    /// then deletions on the right and on the left, one job after another;
    /// a job that is cancelled or has errors stops the rest.
    fn sync_run(&mut self) -> Task<Message> {
        if self.sync_chain.is_some() {
            if let Some(t) = self.screen.sync_mut() {
                t.confirm = false;
                t.note = Some(("A synchronization is still running: wait for it to finish".into(), Tone::Error));
            }
            return Task::none();
        }
        let Some(t) = self.screen.take_sync() else { return Task::none() };
        let plan = sync::plan(&t.rows, &t.actions);
        let (left, right) = (t.left.clone(), t.right.clone());
        // Deletions stay inside the synchronized folders.
        let inside = |paths: Vec<PathBuf>, root: &Path| -> Vec<PathBuf> {
            paths.into_iter().filter(|p| p.starts_with(root) && p != root).collect()
        };
        let delete_right = inside(plan.delete_right, &right);
        let delete_left = inside(plan.delete_left, &left);
        let mut jobs: Vec<(Job, String)> = Vec::new();
        let show = |p: &Path| remote::display(p);
        if !plan.to_right.is_empty() {
            let title = format!("Synchronizing: {} item(s) to {}", plan.to_right.len(), show(&right));
            jobs.push((Job::Copy(plan.to_right), title));
        }
        if !plan.to_left.is_empty() {
            let title = format!("Synchronizing: {} item(s) to {}", plan.to_left.len(), show(&left));
            jobs.push((Job::Copy(plan.to_left), title));
        }
        for (paths, root) in [(delete_right, &right), (delete_left, &left)] {
            if !paths.is_empty() {
                let title = format!("Synchronizing: deleting {} item(s) in {}", paths.len(), show(root));
                jobs.push((Job::Delete { paths, to_trash: !deletes_permanently(root) }, title));
            }
        }
        if jobs.is_empty() {
            self.info("Nothing to synchronize");
            return Task::none();
        }
        let steps = jobs.len();
        let (job, title) = jobs.remove(0);
        let id = self.next_job_id;
        let task = self.start_job_with(job, title, true);
        self.sync_chain = Some((id, jobs));
        self.info(format!("Synchronizing in the background ({steps} step(s))"));
        task
    }

    /// After job `id`: starts the next step of a synchronization, unless
    /// that job was cancelled or failed (then the rest is dropped).
    pub(super) fn sync_job_done(&mut self, id: u64, outcome: &Outcome) -> Task<Message> {
        let Some((running, mut rest)) = self.sync_chain.take() else { return Task::none() };
        if running != id {
            self.sync_chain = Some((running, rest));
            return Task::none();
        }
        if rest.is_empty() {
            return Task::none();
        }
        if outcome.cancelled || !outcome.errors.is_empty() {
            let msg = self.status.as_ref().map(|(m, _)| m.clone()).unwrap_or_default();
            let why = if outcome.cancelled { "cancelled" } else { "errors" };
            let text = format!("{msg} \u{b7} synchronization stopped ({why}): {} later step(s) not run", rest.len());
            self.status = Some((text, Tone::Error));
            return Task::none();
        }
        let (job, title) = rest.remove(0);
        let next = self.next_job_id;
        let task = self.start_job_with(job, title, true);
        self.sync_chain = Some((next, rest));
        task
    }

    /// Keys of `sync_key`, for the F1 help. Keep in sync.
    pub(super) const SYNC_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Esc", "Stop the comparison / back from the confirmation / close"),
        ("Up, Down, PgUp, PgDn, Ctrl+Home, Ctrl+End", "Pick a row"),
        ("Space, click on the direction", "Next direction for the row (\u{2192}, skip, \u{2190}, delete where it fits)"),
        ("Right, Left", "Copy the row to the right / to the left"),
        ("Delete", "Skip the row (Shift+Delete: delete it on the side where it is alone)"),
        ("Ctrl+R, F2", "Compare again"),
        ("Ctrl+1 .. Ctrl+4", "Show all / left only / right only / changed rows"),
        ("Ctrl+M", "Next mode: two-way, mirror left \u{2192} right, update right"),
        ("Ctrl+B", "Compare by content on / off"),
        ("Ctrl+I", "Ignore dates on / off"),
        ("Ctrl+E", "Show equal files on / off"),
        ("Ctrl+D", "Shown rows back to the mode's default"),
        ("Ctrl+K", "Skip all shown rows"),
        ("Ctrl+S, Enter", "Synchronize: shows what will be done, Enter confirms"),
    ];

    pub(super) fn sync_key(&mut self, named: Option<Named>, key: &keyboard::Key, mods: keyboard::Modifiers) -> Task<Message> {
        let page = self.sync_rows().saturating_sub(1).max(1) as isize;
        let Some(t) = self.screen.sync_mut() else { return Task::none() };
        if t.confirm {
            let msg = match named {
                Some(Named::Escape) => SyncMsg::Back,
                Some(Named::Enter) => SyncMsg::Confirm,
                _ => return Task::none(),
            };
            return self.sync_update(msg);
        }
        let running = t.running.is_some();
        let current = t.visible.get(t.cursor).copied();
        let msg = match named {
            Some(Named::Escape) if running => SyncMsg::Stop,
            Some(Named::Escape) => SyncMsg::Close,
            Some(Named::Enter) => SyncMsg::Synchronize,
            Some(Named::F2) => SyncMsg::Compare,
            Some(Named::Space) => match current {
                Some(r) => {
                    t.actions[r] = t.rows[r].next_action(t.actions[r]);
                    return Task::none();
                }
                None => return Task::none(),
            },
            Some(Named::ArrowRight) => SyncMsg::Set(Action::ToRight),
            Some(Named::ArrowLeft) => SyncMsg::Set(Action::ToLeft),
            Some(Named::Delete) if mods.shift() => {
                let Some(r) = current else { return Task::none() };
                match t.rows[r].state {
                    State::LeftOnly => SyncMsg::Set(Action::DeleteLeft),
                    State::RightOnly => SyncMsg::Set(Action::DeleteRight),
                    _ => return Task::none(),
                }
            }
            Some(Named::Delete) => SyncMsg::Set(Action::Skip),
            Some(Named::ArrowDown) => {
                self.sync_move(1);
                return Task::none();
            }
            Some(Named::ArrowUp) => {
                self.sync_move(-1);
                return Task::none();
            }
            Some(Named::PageDown) => {
                self.sync_move(page);
                return Task::none();
            }
            Some(Named::PageUp) => {
                self.sync_move(-page);
                return Task::none();
            }
            Some(Named::Home) => {
                self.sync_move(isize::MIN / 2);
                return Task::none();
            }
            Some(Named::End) => {
                self.sync_move(isize::MAX / 2);
                return Task::none();
            }
            _ => {
                let keyboard::Key::Character(c) = key else { return Task::none() };
                if !mods.control() || mods.alt() {
                    return Task::none();
                }
                match c.to_ascii_lowercase().as_str() {
                    "r" => SyncMsg::Compare,
                    "s" => SyncMsg::Synchronize,
                    "1" => SyncMsg::Filter(Filter::All),
                    "2" => SyncMsg::Filter(Filter::LeftOnly),
                    "3" => SyncMsg::Filter(Filter::RightOnly),
                    "4" => SyncMsg::Filter(Filter::Changed),
                    "m" => {
                        let i = Mode::ALL.iter().position(|&m| m == t.mode).unwrap_or(0);
                        SyncMsg::Mode(Mode::ALL[(i + 1) % Mode::ALL.len()])
                    }
                    "b" => SyncMsg::Toggle(Opt::ByContent),
                    "i" => SyncMsg::Toggle(Opt::IgnoreDate),
                    "e" => SyncMsg::Toggle(Opt::ShowEqual),
                    "d" => SyncMsg::Defaults,
                    "k" => SyncMsg::SkipAll,
                    _ => return Task::none(),
                }
            }
        };
        self.sync_update(msg)
    }

    pub(super) fn sync_view<'a>(&'a self, t: &'a SyncTool) -> Element<'a, Message> {
        let colors = theme::c();
        let action = |label: &'a str, msg: SyncMsg, primary: bool| {
            button(text(label).size(13))
                .on_press(Message::Sync(msg))
                .padding([6, 14])
                .style(if primary { theme::primary } else { theme::fkey })
        };
        let choice = |label: &'a str, on: bool, msg: SyncMsg| {
            button(text(label).size(13)).on_press(Message::Sync(msg)).padding([4, 10]).style(theme::tab(on))
        };
        let toggle = |label: &'a str, on: bool, opt: Opt| {
            checkbox(label, on).on_toggle(move |_| Message::Sync(SyncMsg::Toggle(opt))).size(14).text_size(13)
        };
        let muted = |s: String| text(s).size(13).color(colors.muted);

        let roots = row![
            container(text(remote::display(&t.left)).size(13).wrapping(Wrapping::None)).width(Length::Fill).clip(true),
            text("\u{21c4}").size(14).color(colors.accent),
            container(text(remote::display(&t.right)).size(13).wrapping(Wrapping::None)).width(Length::Fill).clip(true),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let mut modes = Row::new().spacing(6).align_y(Alignment::Center).push(muted("Mode".into()));
        for mode in Mode::ALL {
            modes = modes.push(choice(mode.label(), t.mode == mode, SyncMsg::Mode(mode)));
        }
        modes = modes
            .push(Space::with_width(Length::Fixed(16.0)))
            .push(toggle("By content", t.by_content, Opt::ByContent))
            .push(toggle("Ignore dates", t.ignore_date, Opt::IgnoreDate))
            .push(toggle("Equal files", t.show_equal, Opt::ShowEqual));
        let mut filters = Row::new().spacing(6).align_y(Alignment::Center).push(muted("Show".into()));
        for f in Filter::ALL {
            filters = filters.push(choice(f.label(), t.filter == f, SyncMsg::Filter(f)));
        }
        filters = filters
            .push(Space::with_width(Length::Fill))
            .push(action("Defaults", SyncMsg::Defaults, false))
            .push(action("Skip all", SyncMsg::SkipAll, false));
        let header = container(
            column![row![text("Synchronize folders").size(16)], roots, modes, filters].spacing(8),
        )
        .padding(14)
        .style(filled(colors.header));

        let head_cell = |s: &'a str, w: Length| container(text(s).size(12).color(colors.muted)).width(w);
        let list_header = container(
            row![
                head_cell("Left", Length::Fill),
                head_cell("Size", Length::Fixed(COL_SIZE)),
                head_cell("Date", Length::Fixed(COL_DATE)),
                head_cell("Action", Length::Fixed(COL_DIR)),
                head_cell("Right", Length::Fill),
                head_cell("Size", Length::Fixed(COL_SIZE)),
                head_cell("Date", Length::Fixed(COL_DATE)),
            ]
            .spacing(8),
        )
        .padding([3, 14])
        .style(filled(colors.header));

        let body: Element<'a, Message> = if t.confirm {
            self.sync_confirm_view(t)
        } else {
            let list = responsive(move |size| {
                let rows = (size.height / ROW_H).floor().max(1.0) as usize;
                let len = t.visible.len();
                let start = visible_start(t.top, t.cursor, rows, len);
                let end = (start + rows).min(len);
                let lines = (start..end).map(|i| {
                    let r = &t.rows[t.visible[i]];
                    let act = t.actions[t.visible[i]];
                    let color = match r.state {
                        State::Equal => colors.muted,
                        State::Different => colors.error,
                        _ => colors.text,
                    };
                    let side = |item: &Option<sync::Item>, newer: bool| -> [String; 3] {
                        match item {
                            None => [String::new(), String::new(), String::new()],
                            Some(it) => {
                                let size = match it.kind {
                                    EntryKind::Dir if it.files > 0 || it.size > 0 => {
                                        format!("{} ({})", rc_core::format::size(it.size), it.files)
                                    }
                                    EntryKind::Dir => "<DIR>".to_owned(),
                                    _ => rc_core::format::size(it.size),
                                };
                                let mark = if newer { " *" } else { "" };
                                [r.rel.clone(), size, format!("{}{mark}", rc_core::format::date(&it.modified))]
                            }
                        }
                    };
                    let [ln, ls, ld] = side(&r.left, r.state == State::LeftNewer);
                    let [rn, rs, rd] = side(&r.right, r.state == State::RightNewer);
                    let is_dir = |item: &Option<sync::Item>| item.as_ref().is_some_and(|i| i.kind == EntryKind::Dir);
                    let cell = |s: String, w: Length, c: Color| {
                        container(text(s).size(13).color(c).wrapping(Wrapping::None)).width(w).clip(true)
                    };
                    let name_color = |dir: bool| if dir && r.state != State::Equal { colors.dir } else { color };
                    let act_color = match act {
                        Action::Skip => colors.muted,
                        Action::ToRight | Action::ToLeft => colors.success,
                        Action::DeleteRight | Action::DeleteLeft => colors.error,
                    };
                    let glyph = if act == Action::Skip && r.conflict() { "?" } else { act.glyph() };
                    let dir_cell = mouse_area(
                        container(text(glyph).size(13).color(act_color))
                            .width(Length::Fixed(COL_DIR))
                            .align_x(Alignment::Center),
                    )
                    .on_press(Message::Sync(SyncMsg::Cycle(i)));
                    let line = row![
                        cell(ln, Length::Fill, name_color(is_dir(&r.left))),
                        cell(ls, Length::Fixed(COL_SIZE), color),
                        cell(ld, Length::Fixed(COL_DATE), color),
                        dir_cell,
                        cell(rn, Length::Fill, name_color(is_dir(&r.right))),
                        cell(rs, Length::Fixed(COL_SIZE), color),
                        cell(rd, Length::Fixed(COL_DATE), color),
                    ]
                    .spacing(8);
                    let bg = if i == t.cursor {
                        colors.cursor
                    } else if i % 2 == 1 {
                        colors.zebra
                    } else {
                        colors.panel
                    };
                    mouse_area(container(line).height(Length::Fixed(ROW_H)).padding([1, 14]).style(filled(bg)))
                        .on_press(Message::Sync(SyncMsg::Click(i)))
                        .into()
                });
                let lines = container(Column::with_children(lines).width(Length::Fill))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .clip(true);
                scrollbar::attach(lines, start as f32, rows as f32, len as f32, move |at| {
                    Message::Sync(SyncMsg::ScrollTo(at.round() as usize, rows))
                })
            });
            mouse_area(container(list).width(Length::Fill).height(Length::Fill).style(filled(colors.panel)))
                .on_scroll(|delta| {
                    Message::Sync(SyncMsg::Wheel(match delta {
                        mouse::ScrollDelta::Lines { y, .. } => y,
                        mouse::ScrollDelta::Pixels { y, .. } => y / ROW_H / 3.0,
                    }))
                })
                .into()
        };

        let (note, color) = if t.running.is_some() {
            (format!("Comparing\u{2026} {} folders \u{b7} {}", t.folders, remote::display(&t.folder)), colors.text)
        } else if let Some((n, tone)) = &t.note {
            let c = match tone {
                Tone::Error => colors.error,
                Tone::Success => colors.success,
                Tone::Info => colors.text,
            };
            let n = if t.compared && *tone != Tone::Error { format!("{n} \u{b7} {}", summary_line(&t.summary())) } else { n.clone() };
            (n, c)
        } else if t.compared {
            (summary_line(&t.summary()), colors.text)
        } else {
            ("Esc close \u{b7} Ctrl+R compare".to_owned(), colors.muted)
        };
        let mut buttons = Row::new().spacing(8);
        buttons = buttons.push(action("Close", SyncMsg::Close, false));
        buttons = if t.running.is_some() {
            buttons.push(action("Stop", SyncMsg::Stop, true))
        } else {
            buttons.push(action("Compare", SyncMsg::Compare, false)).push(action("Synchronize\u{2026}", SyncMsg::Synchronize, true))
        };
        let footer = container(
            row![
                container(text(note).size(12).color(color).wrapping(Wrapping::None)).width(Length::Fill).clip(true),
                Space::with_width(Length::Fixed(8.0)),
                buttons
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(filled(colors.header));

        container(column![header, list_header, body, footer])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.bg))
            .into()
    }

    /// What "Synchronize" will do, with the deletions spelled out.
    fn sync_confirm_view<'a>(&'a self, t: &'a SyncTool) -> Element<'a, Message> {
        let colors = theme::c();
        let s = t.summary();
        let line = |s: String, c: Color| text(s).size(14).color(c);
        let mut col = Column::new().spacing(8).push(text("Synchronize now?").size(16));
        let show = |p: &Path| remote::display(p);
        if s.to_right.0 > 0 {
            col = col.push(line(format!("Copy {} to {}", files_text(s.to_right), show(&t.right)), colors.text));
        }
        if s.to_left.0 > 0 {
            col = col.push(line(format!("Copy {} to {}", files_text(s.to_left), show(&t.left)), colors.text));
        }
        if s.overwrite_newer > 0 {
            col = col.push(line(
                format!("{} newer file(s) will be overwritten with older ones", s.overwrite_newer),
                colors.error,
            ));
        }
        col = col.push(line("Existing files are replaced without asking.".into(), colors.muted));
        for ((rows, files), root) in [(s.delete_right, &t.right), (s.delete_left, &t.left)] {
            if rows == 0 {
                continue;
            }
            let what = format!("{rows} item(s), {files} file(s) in {}", show(root));
            let (msg, c) = if deletes_permanently(root) {
                (format!("{what} will be DELETED PERMANENTLY (no Recycle Bin here)"), colors.error)
            } else {
                (format!("{what} will be moved to the Recycle Bin"), colors.error)
            };
            col = col.push(line(msg, c));
        }
        let go = if s.deletions() > 0 { format!("Synchronize and delete {}", s.deletions()) } else { "Synchronize".to_owned() };
        let buttons = row![
            button(text(go).size(13)).on_press(Message::Sync(SyncMsg::Confirm)).padding([6, 14]).style(theme::primary),
            button(text("Back").size(13)).on_press(Message::Sync(SyncMsg::Back)).padding([6, 14]).style(theme::fkey),
        ]
        .spacing(8);
        col = col.push(Space::with_height(Length::Fixed(6.0))).push(buttons);
        container(container(col).padding(20).max_width(720.0).style(theme::dialog))
            .center(Length::Fill)
            .style(filled(colors.panel))
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_folders_are_refused() {
        assert!(nested(Path::new(r"C:\a"), Path::new(r"C:\a\b")));
        assert!(nested(Path::new(r"C:\A\"), Path::new(r"c:\a")));
        assert!(!nested(Path::new(r"C:\a"), Path::new(r"C:\ab")));
        assert!(!nested(Path::new(r"C:\a"), Path::new(r"D:\a")));
    }

    #[test]
    fn filters() {
        assert!(!Filter::All.shows(State::Equal, false));
        assert!(Filter::All.shows(State::Equal, true));
        assert!(Filter::Changed.shows(State::Different, false));
        assert!(!Filter::Changed.shows(State::LeftOnly, false));
        assert!(Filter::RightOnly.shows(State::RightOnly, false));
    }
}
