use crate::keymap;
use crate::theme::{self, filled};
use iced::keyboard::key::Named;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, center, column, container, mouse_area, opaque, pick_list, progress_bar, responsive,
    row, stack, text, text_input, Column, Row, Space,
};
use iced::{
    event, keyboard, mouse, window, Alignment, Element, Event, Length, Size, Subscription, Task,
    Theme,
};
use rc_core::ops::{self, Conflict, Job, JobEvent, JobHandle, Outcome, Progress, Resolution};
use rc_core::panel::visible_start;
use rc_core::{format, Activated, Command, EntryKind, Panel, SortKey};
use std::io;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::time::{Duration, Instant};

const ROW_H: f32 = 22.0;
/// Approximate height of everything that isn't file rows (headers, footers,
/// function-key bar). Only used to size PageUp/PageDown jumps.
const CHROME_H: f32 = 150.0;
const JOB_ROW_H: f32 = 30.0;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

const COL_EXT: f32 = 60.0;
const COL_SIZE: f32 = 80.0;
const COL_DATE: f32 = 120.0;

fn input_id() -> text_input::Id {
    text_input::Id::new("dialog-input")
}

pub struct App {
    panels: [Panel; 2],
    active: usize,
    drives: Vec<String>,
    /// (message, is_error)
    status: Option<(String, bool)>,
    window_width: f32,
    window_height: f32,
    last_click: Option<(usize, usize, Instant)>,
    dialog: Option<Dialog>,
    jobs: Vec<RunningJob>,
    next_job_id: u64,
}

/// Modal dialogs opened by commands.
#[derive(Debug)]
enum Dialog {
    Transfer { is_move: bool, sources: Vec<PathBuf>, base: PathBuf, input: String },
    MakeDir { input: String },
    Rename { src: PathBuf, input: String },
    Delete { paths: Vec<PathBuf>, to_trash: bool },
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
    Run(Command),
    Click(usize, usize),
    Wheel(usize, f32),
    Drive(usize, String),
    Sort(usize, SortKey),
    Resized(Size),
    DialogInput(String),
    DialogSubmit,
    DialogCancel,
    Job(u64, JobEvent),
    JobPause(u64),
    JobCancel(u64),
    Answer(u64, Resolution),
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
                _ => None,
            }),
            window::resize_events().map(|(_id, size)| Message::Resized(size)),
        ])
    }

    // ---- update -----------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Key(key, mods, captured) => return self.on_key(key, mods, captured),
            Message::Run(cmd) => return self.run(cmd),
            Message::Click(side, index) => {
                self.active = side;
                self.panels[side].set_cursor(index);
                let now = Instant::now();
                let double = matches!(
                    self.last_click,
                    Some((s, i, t)) if s == side && i == index && now - t < DOUBLE_CLICK
                );
                if double {
                    self.last_click = None;
                    return self.run(Command::Open);
                }
                self.last_click = Some((side, index, now));
            }
            Message::Wheel(side, lines) => {
                let rows = self.page_rows();
                let panel = &mut self.panels[side];
                panel.move_cursor((-lines * 3.0).round() as isize);
                panel.ensure_visible(rows);
            }
            Message::Drive(side, drive) => {
                self.active = side;
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
                let sources = self.target_paths();
                if sources.is_empty() {
                    return Task::none();
                }
                let mut input = self.panels[other].path.display().to_string();
                if !input.ends_with(MAIN_SEPARATOR) {
                    input.push(MAIN_SEPARATOR);
                }
                let base = self.panels[self.active].path.clone();
                return self.open_dialog(Dialog::Transfer { is_move: cmd == Move, sources, base, input });
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
                let paths = self.target_paths();
                if paths.is_empty() {
                    return Task::none();
                }
                self.dialog = Some(Dialog::Delete { paths, to_trash: cmd == Delete });
                return Task::none();
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
                Ok(Activated::File(path)) => rc_platform::open_default(&path),
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
                info = Some(format!(
                    "Hidden files {}",
                    if panel.show_hidden { "shown" } else { "hidden" }
                ));
                r
            }
            SortBy(key) => Ok(panel.sort_by(key)),
            // Until the built-in viewer/editor exist, hand off to the OS.
            View | Edit => match panel.current() {
                Some(e) if e.kind == EntryKind::File => rc_platform::open_default(&e.path),
                _ => Ok(()),
            },
            Quit | SwitchPanel | SyncOtherPanel | Copy | Move | Rename | MakeDir | Delete
            | DeletePermanent => unreachable!(),
        };
        panel.ensure_visible(rows);
        self.report(result);
        if let Some(msg) = info {
            self.status = Some((msg, false));
        }
        Task::none()
    }

    fn target_paths(&self) -> Vec<PathBuf> {
        self.panels[self.active].targets().into_iter().map(|e| e.path.clone()).collect()
    }

    fn open_dialog(&mut self, dialog: Dialog) -> Task<Message> {
        self.dialog = Some(dialog);
        Task::batch([text_input::focus(input_id()), text_input::select_all(input_id())])
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
                    Err(msg) => self.status = Some((msg, true)),
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
                let title = format!(
                    "{} {}",
                    if to_trash { "Recycling" } else { "Deleting" },
                    describe(&paths)
                );
                return self.start_job(Job::Delete { paths, to_trash }, title);
            }
        }
        Task::none()
    }

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
                self.reload_panels();
                self.status = Some(summarize(job.done_verb, &outcome));
                self.after_change();
            }
        }
    }

    fn job(&mut self, id: u64) -> Option<&mut RunningJob> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

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
                    None => self.status = Some((e.to_string(), true)),
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
            self.status = Some((e.to_string(), true));
        }
    }

    /// Roughly how many path characters fit in a panel header.
    fn path_chars(&self) -> usize {
        ((self.window_width / 2.0 - 110.0) / 7.0).max(10.0) as usize
    }

    fn page_rows(&self) -> usize {
        let chrome = CHROME_H + self.jobs.len() as f32 * JOB_ROW_H;
        ((self.window_height - chrome) / ROW_H).floor().max(1.0) as usize
    }

    // ---- view -------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        let panels = row![self.panel_view(0), vline(), self.panel_view(1)].height(Length::Fill);

        let base = container(column![panels, hline(), self.jobs_view(), self.bottom_bar()])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(theme::BG));

        let modal = self
            .jobs
            .iter()
            .find_map(|j| j.conflict.as_ref().map(|c| self.conflict_view(j.id, c)))
            .or_else(|| self.dialog.as_ref().map(|d| self.dialog_view(d)));

        match modal {
            Some(card) => stack![base, opaque(center(card).style(theme::backdrop))].into(),
            None => base.into(),
        }
    }

    fn panel_view(&self, side: usize) -> Element<'_, Message> {
        let panel = &self.panels[side];
        let active = side == self.active;

        // Thin accent strip marks the active panel.
        let accent = container(Space::new(Length::Fill, Length::Fixed(2.0)))
            .style(filled(if active { theme::ACCENT } else { theme::HEADER }));

        let drive = self
            .drives
            .iter()
            .find(|d| panel.path.starts_with(d.as_str()))
            .cloned();
        let header = container(
            row![
                pick_list(self.drives.as_slice(), drive, move |d| Message::Drive(side, d))
                    .text_size(12)
                    .padding([2, 8]),
                text(elide_left(&panel.path.display().to_string(), self.path_chars()))
                    .size(13)
                    .color(if active { theme::TEXT } else { theme::MUTED })
                    .wrapping(Wrapping::None),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([6, 10])
        .width(Length::Fill)
        .clip(true)
        .style(filled(theme::HEADER));

        let sort_button = |label: &str, key: SortKey, width: Length, right: bool| {
            let arrow = match (panel.sort.key == key, panel.sort.descending) {
                (true, false) => " ↑",
                (true, true) => " ↓",
                _ => "",
            };
            let label = text(format!("{label}{arrow}")).size(12).width(Length::Fill);
            let label = if right { label.align_x(Alignment::End) } else { label };
            button(label)
                .on_press(Message::Sort(side, key))
                .padding(0)
                .width(width)
                .style(theme::flat)
        };
        let columns = container(
            row![
                sort_button("Name", SortKey::Name, Length::Fill, false),
                sort_button("Ext", SortKey::Ext, Length::Fixed(COL_EXT), false),
                sort_button("Size", SortKey::Size, Length::Fixed(COL_SIZE), true),
                sort_button("Modified", SortKey::Modified, Length::Fixed(COL_DATE), false),
            ]
            .spacing(10),
        )
        .padding([4, 10])
        .style(filled(theme::PANEL));

        // Virtualised list: only the rows that fit on screen are built.
        let list = responsive(move |size| {
            let rows = ((size.height / ROW_H).floor() as usize).max(1);
            let len = panel.entries.len();
            let start = visible_start(panel.scroll, panel.cursor, rows, len);
            let end = (start + rows).min(len);
            container(Column::with_children(
                (start..end).map(|i| self.entry_row(side, i, active)),
            ))
            .width(Length::Fill)
            .height(Length::Fill)
            .clip(true)
            .style(filled(theme::PANEL))
            .into()
        });
        let list = mouse_area(list).on_scroll(move |delta| {
            let lines = match delta {
                mouse::ScrollDelta::Lines { y, .. } => y,
                mouse::ScrollDelta::Pixels { y, .. } => y / ROW_H,
            };
            Message::Wheel(side, lines)
        });

        let (sel, total, sel_bytes, total_bytes) = panel.summary();
        let footer = container(
            text(format!(
                "{sel} of {total} selected  ·  {} of {}",
                format::size(sel_bytes),
                format::size(total_bytes)
            ))
            .size(12)
            .color(theme::MUTED),
        )
        .padding([4, 10])
        .width(Length::Fill)
        .style(filled(theme::HEADER));

        column![accent, header, columns, list, footer]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn entry_row(&self, side: usize, index: usize, active: bool) -> Element<'_, Message> {
        let panel = &self.panels[side];
        let entry = &panel.entries[index];

        let color = if panel.is_selected(entry) {
            theme::SELECTED
        } else if entry.is_dir() {
            theme::DIR
        } else if entry.hidden {
            theme::MUTED
        } else {
            theme::TEXT
        };
        let cell = |s: String, width: Length| {
            text(s).size(13).color(color).wrapping(Wrapping::None).width(width)
        };

        let (stem, ext) = entry.split_name();
        let size = match entry.kind {
            EntryKind::Parent => String::new(),
            EntryKind::Dir => "<DIR>".into(),
            EntryKind::File => format::size(entry.size),
        };

        let content = row![
            cell(stem.to_string(), Length::Fill),
            cell(ext.to_string(), Length::Fixed(COL_EXT)),
            cell(size, Length::Fixed(COL_SIZE)).align_x(Alignment::End),
            cell(format::date(&entry.modified), Length::Fixed(COL_DATE)).color(theme::MUTED),
        ]
        .spacing(10)
        .padding([0, 10])
        .height(ROW_H)
        .align_y(Alignment::Center);

        let background = if index == panel.cursor {
            if active { theme::CURSOR } else { theme::CURSOR_INACTIVE }
        } else if index % 2 == 1 {
            theme::ZEBRA
        } else {
            theme::PANEL
        };

        mouse_area(
            container(content)
                .width(Length::Fill)
                .height(ROW_H)
                .clip(true)
                .style(filled(background)),
        )
        .on_press(Message::Click(side, index))
        .into()
    }

    fn jobs_view(&self) -> Element<'_, Message> {
        Column::with_children(self.jobs.iter().map(|job| {
            let p = &job.progress;
            let elapsed = job.started.elapsed().as_secs_f64().max(0.001);
            let speed = if p.total_bytes > 0 {
                format!("{}/s", format::size((p.done_bytes as f64 / elapsed) as u64))
            } else {
                format!("{} of {}", p.done_items, p.total_items)
            };
            let paused = job.handle.control.is_paused();
            let current = p
                .current
                .as_ref()
                .and_then(|c| c.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            container(
                row![
                    column![
                        text(&job.title).size(12).wrapping(Wrapping::None),
                        text(current).size(11).color(theme::MUTED).wrapping(Wrapping::None),
                    ]
                    .width(Length::Fill)
                    .clip(true),
                    progress_bar(0.0..=1.0, p.fraction())
                        .width(Length::Fixed(220.0))
                        .height(Length::Fixed(6.0))
                        .style(theme::progress),
                    text(format!("{:>3.0}%", p.fraction() * 100.0)).size(12).width(Length::Fixed(40.0)),
                    text(speed).size(12).color(theme::MUTED).width(Length::Fixed(90.0)),
                    small_button(if paused { "Resume" } else { "Pause" }, Message::JobPause(job.id)),
                    small_button("Cancel", Message::JobCancel(job.id)),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .padding([2, 10])
            .height(JOB_ROW_H)
            .style(filled(theme::HEADER))
            .into()
        }))
        .into()
    }

    fn dialog_view<'a>(&'a self, dialog: &'a Dialog) -> Element<'a, Message> {
        let input_field = |value: &'a str| {
            text_input("", value)
                .id(input_id())
                .on_input(Message::DialogInput)
                .on_submit(Message::DialogSubmit)
                .padding(8)
                .size(13)
                .style(theme::input)
        };

        let (title, body, field, ok): (String, String, Option<_>, &str) = match dialog {
            Dialog::Transfer { is_move, sources, input, .. } => (
                if *is_move { "Move" } else { "Copy" }.into(),
                format!("{} {} to:", if *is_move { "Move" } else { "Copy" }, describe(sources)),
                Some(input_field(input)),
                if *is_move { "Move" } else { "Copy" },
            ),
            Dialog::MakeDir { input } => (
                "New folder".into(),
                format!("Create folder in {}:", self.panels[self.active].path.display()),
                Some(input_field(input)),
                "Create",
            ),
            Dialog::Rename { src, input } => (
                "Rename".into(),
                format!("Rename '{}' to:", src.file_name().unwrap_or_default().to_string_lossy()),
                Some(input_field(input)),
                "Rename",
            ),
            Dialog::Delete { paths, to_trash } => (
                if *to_trash { "Delete" } else { "Delete permanently" }.into(),
                if *to_trash {
                    format!("Move {} to the Recycle Bin?", describe(paths))
                } else {
                    format!("Permanently delete {}? This cannot be undone.", describe(paths))
                },
                None,
                "Delete",
            ),
        };

        let mut content = column![
            text(title).size(16),
            text(body).size(13).color(theme::MUTED),
        ]
        .spacing(12);
        if let Some(field) = field {
            content = content.push(field);
        }
        content = content.push(
            row![
                Space::with_width(Length::Fill),
                dialog_button("Cancel", Message::DialogCancel, false),
                dialog_button(ok, Message::DialogSubmit, true),
            ]
            .spacing(8),
        );

        container(content).padding(20).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }

    fn conflict_view<'a>(&'a self, id: u64, c: &'a Conflict) -> Element<'a, Message> {
        let name = c.dst.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let line = |label: &'a str, size: u64, date: &Option<_>| {
            row![
                text(label).size(13).color(theme::MUTED).width(Length::Fixed(80.0)),
                text(format::size(size)).size(13).width(Length::Fixed(90.0)),
                text(format::date(date)).size(13),
            ]
        };
        let answer = |label, r, primary| dialog_button(label, Message::Answer(id, r), primary);

        let content = column![
            text("File already exists").size(16),
            text(name).size(13).color(theme::SELECTED),
            text(c.dst.display().to_string()).size(12).color(theme::MUTED),
            column![
                line("New", c.src_size, &c.src_modified),
                line("Existing", c.dst_size, &c.dst_modified),
            ]
            .spacing(4),
            row![
                answer("Overwrite", Resolution::Overwrite, true),
                answer("Overwrite all", Resolution::OverwriteAll, false),
                answer("Overwrite older", Resolution::OverwriteOlder, false),
            ]
            .spacing(8),
            row![
                answer("Skip", Resolution::Skip, false),
                answer("Skip all", Resolution::SkipAll, false),
                Space::with_width(Length::Fill),
                answer("Cancel job", Resolution::Cancel, false),
            ]
            .spacing(8),
        ]
        .spacing(12);

        container(content).padding(20).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }

    fn bottom_bar(&self) -> Element<'_, Message> {
        let status = match &self.status {
            Some((msg, is_error)) => text(msg.as_str())
                .size(12)
                .color(if *is_error { theme::ERROR } else { theme::MUTED }),
            None => text("Tab switch panel · Enter open · Backspace up · Ins/Space select · Shift+F6 rename · Ctrl+H hidden · Alt+O sync")
                .size(12)
                .color(theme::MUTED),
        };

        let keys = [
            ("F3", "View", Command::View),
            ("F4", "Edit", Command::Edit),
            ("F5", "Copy", Command::Copy),
            ("F6", "Move", Command::Move),
            ("F7", "MkDir", Command::MakeDir),
            ("F8", "Delete", Command::Delete),
            ("Alt+F4", "Exit", Command::Quit),
        ];
        let fkeys = Row::with_children(keys.into_iter().map(|(key, label, cmd)| {
            button(
                container(
                    row![text(key).size(12).color(theme::ACCENT), text(label).size(12)].spacing(6),
                )
                .center_x(Length::Fill),
            )
            .on_press(Message::Run(cmd))
            .padding([4, 10])
            .width(Length::Fill)
            .style(theme::fkey)
            .into()
        }))
        .spacing(6);

        column![
            container(status.wrapping(Wrapping::None)).padding([3, 10]).clip(true),
            container(fkeys).padding(iced::Padding { top: 0.0, right: 8.0, bottom: 8.0, left: 8.0 }),
        ]
        .into()
    }
}

/// Keeps the end of a long path: `…\sandbox\B`.
fn elide_left(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count - max_chars + 1).collect();
    format!("…{tail}")
}

/// "file.txt" or "3 items".
fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!("'{}'", one.file_name().unwrap_or_default().to_string_lossy()),
        many => format!("{} items", many.len()),
    }
}

fn summarize(verb: &str, o: &Outcome) -> (String, bool) {
    let mut msg = format!("{verb} {} item(s)", o.done_items);
    if o.done_bytes > 0 {
        msg.push_str(&format!(" ({})", format::size(o.done_bytes)));
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
            (msg, true)
        }
        None => (msg, false),
    }
}

fn dialog_button(label: &str, message: Message, primary: bool) -> Element<'_, Message> {
    button(text(label).size(13))
        .on_press(message)
        .padding([6, 14])
        .style(if primary { theme::primary } else { theme::fkey })
        .into()
}

fn small_button(label: &str, message: Message) -> Element<'_, Message> {
    button(text(label).size(12)).on_press(message).padding([2, 10]).style(theme::fkey).into()
}

fn open_panel(path: Option<PathBuf>) -> Panel {
    path.and_then(|p| Panel::open(p).ok())
        .or_else(|| rc_platform::home_dir().and_then(|p| Panel::open(p).ok()))
        .or_else(|| Panel::open(if cfg!(windows) { "C:\\" } else { "/" }).ok())
        .expect("could not open any starting folder")
}

fn vline<'a>() -> Element<'a, Message> {
    container(Space::new(Length::Fixed(1.0), Length::Fill))
        .style(filled(theme::BORDER))
        .into()
}

fn hline<'a>() -> Element<'a, Message> {
    container(Space::new(Length::Fill, Length::Fixed(1.0)))
        .style(filled(theme::BORDER))
        .into()
}
