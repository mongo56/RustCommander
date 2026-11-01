//! Find files (Alt+F7, TC's `cm_SearchFor`): fills the window while open.
//! Name masks or a regex, optionally a text the files must contain (plain
//! or regex, match case), in a folder and its subfolders. The search runs
//! on a worker thread (`rc_core::search::search`) and hits appear as they
//! are found; Esc stops it. Go to a hit (Enter in the list, double-click,
//! Ctrl+G) to show it in its folder, or feed all hits to the panel
//! (Ctrl+L, TC's "feed to listbox": `Panel::show_results`). Closing keeps
//! the fields and the last results for the next Alt+F7.

use super::*;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{button, checkbox, column, container, mouse_area, responsive, row, text, text_input, Column, Row, Space};
use iced::{Alignment, Element, Length};
use rc_core::panel::visible_start;
use rc_core::search::{self, Batch, Query, Summary};

const ROW_H: f32 = 20.0;
const LABEL_W: f32 = 96.0;
const COL_SIZE: f32 = 90.0;
const COL_DATE: f32 = 130.0;
/// Height of the form, list header and footer (for paging estimates).
const FIND_CHROME_H: f32 = 230.0;

fn name_input_id() -> text_input::Id {
    text_input::Id::new("find-name")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Root,
    Name,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    NameRegex,
    TextRegex,
    MatchCase,
    Subfolders,
    Hidden,
}

#[derive(Debug, Clone)]
pub enum Progress {
    Batch(Batch),
    Done(Result<Summary, String>),
}

#[derive(Debug, Clone)]
pub enum FindMsg {
    Input(Field, String),
    Toggle(Opt),
    Start,
    Stop,
    Progress(u64, Progress),
    Click(usize),
    GoTo,
    Feed,
    Close,
    Wheel(f32),
}

pub(super) struct FindFiles {
    /// Panel the tool was opened from (go to / feed act on it).
    side: usize,
    /// "Search in" as typed.
    root: String,
    query: Query,
    /// Hits of the last (or running) search, relative to `searched`.
    results: Vec<Entry>,
    /// Root and label of the search the results belong to.
    searched: Option<(PathBuf, String)>,
    cursor: usize,
    top: usize,
    /// Cancel flag of the running search.
    running: Option<Arc<AtomicBool>>,
    seq: u64,
    /// Folder the running search is in, and items looked at so far.
    folder: PathBuf,
    checked: u64,
    summary: Option<Summary>,
    error: Option<String>,
    last_click: Option<(usize, Instant)>,
}

impl FindFiles {
    fn stop(&mut self) {
        if let Some(cancel) = self.running.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn set_option(&mut self, opt: Opt) {
        let q = &mut self.query;
        let flag = match opt {
            Opt::NameRegex => &mut q.name_regex,
            Opt::TextRegex => &mut q.text_regex,
            Opt::MatchCase => &mut q.match_case,
            Opt::Subfolders => &mut q.subfolders,
            Opt::Hidden => &mut q.hidden,
        };
        *flag = !*flag;
    }
}

/// "*.rs containing "fn main"" — what a search looked for.
fn label(q: &Query) -> String {
    let name = if q.name.trim().is_empty() { "*" } else { q.name.trim() };
    if q.text.is_empty() {
        name.to_owned()
    } else {
        format!("{name} containing \"{}\"", q.text)
    }
}

fn summary_text(s: &Summary) -> String {
    let mut t = format!(
        "{} found \u{b7} {} items in {} folders",
        s.hits, s.files, s.folders
    );
    if s.errors > 0 {
        t.push_str(&format!(" \u{b7} {} unreadable", s.errors));
    }
    if s.truncated {
        t.push_str(&format!(" \u{b7} stopped at {} hits", search::MAX_HITS));
    } else if s.cancelled {
        t.push_str(" \u{b7} stopped");
    }
    t
}

impl App {
    pub(super) fn open_find(&mut self) -> Task<Message> {
        let side = self.active;
        self.leave_edits();
        let panel = self.panel(side);
        let (root, hidden) = (panel.path.display().to_string(), panel.show_hidden);
        let mut find = self.find_last.take().unwrap_or_else(|| FindFiles {
            side,
            root: String::new(),
            query: Query { subfolders: true, ..Query::default() },
            results: Vec::new(),
            searched: None,
            cursor: 0,
            top: 0,
            running: None,
            seq: 0,
            folder: PathBuf::new(),
            checked: 0,
            summary: None,
            error: None,
            last_click: None,
        });
        // Hits deleted or renamed since (e.g. after going to one) go away.
        if find.running.is_none() {
            find.results.retain(|e| e.path.symlink_metadata().is_ok());
            find.cursor = find.cursor.min(find.results.len().saturating_sub(1));
        }
        find.side = side;
        find.root = root;
        find.query.hidden = hidden;
        self.find = Some(find);
        Task::batch([text_input::focus(name_input_id()), text_input::select_all(name_input_id())])
    }

    /// Closes the tool (stopping its search), keeping it for the next open.
    fn close_find(&mut self) {
        if let Some(mut f) = self.find.take() {
            f.stop();
            self.find_last = Some(f);
        }
    }

    fn find_rows(&self) -> usize {
        ((self.window_height - FIND_CHROME_H) / ROW_H).floor().max(1.0) as usize
    }

    fn find_move(&mut self, delta: isize) {
        let rows = self.find_rows();
        let Some(f) = &mut self.find else { return };
        let max = f.results.len().saturating_sub(1);
        f.cursor = f.cursor.saturating_add_signed(delta).min(max);
        f.top = visible_start(f.top, f.cursor, rows, f.results.len());
    }

    fn start_find(&mut self) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        let Some(f) = &mut self.find else { return Task::none() };
        f.stop();
        let root = resolve_typed_path(f.root.trim(), &self.sides[f.side].tabs[self.sides[f.side].current].path);
        if !root.is_dir() {
            f.error = Some(format!("Not a folder: {}", root.display()));
            return Task::none();
        }
        let query = Query { root: root.clone(), ..f.query.clone() };
        let cancel = Arc::new(AtomicBool::new(false));
        f.running = Some(cancel.clone());
        f.seq = seq;
        f.results.clear();
        f.searched = Some((root.clone(), label(&query)));
        f.cursor = 0;
        f.top = 0;
        f.folder = root;
        f.checked = 0;
        f.summary = None;
        f.error = None;
        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let result = search::search(&query, &cancel, |b| tx.unbounded_send(Progress::Batch(b)).is_ok());
            let _ = tx.unbounded_send(Progress::Done(result));
        });
        Task::run(rx, move |p| Message::Find(FindMsg::Progress(seq, p)))
    }

    /// Shows the hit under the cursor in its folder, in the tool's panel.
    fn find_go_to(&mut self) -> Task<Message> {
        let Some(f) = &self.find else { return Task::none() };
        let Some(hit) = f.results.get(f.cursor) else { return Task::none() };
        let Some(folder) = hit.path.parent().map(Path::to_path_buf) else { return Task::none() };
        let focus = hit.path.file_name().map(|n| n.to_string_lossy().into_owned());
        let side = f.side;
        self.close_find();
        self.active = side;
        self.start_nav(side, Nav::To { path: folder, focus })
    }

    /// Puts all hits into the tool's panel (TC's "feed to listbox").
    fn find_feed(&mut self) -> Task<Message> {
        let rows = self.page_rows();
        let Some(f) = &mut self.find else { return Task::none() };
        let Some((root, label)) = f.searched.clone().filter(|_| !f.results.is_empty()) else {
            f.error = Some("Nothing to feed to the panel".into());
            return Task::none();
        };
        f.stop();
        let (side, hits, cursor) = (f.side, f.results.clone(), f.cursor);
        self.close_find();
        self.active = side;
        self.nav[side] = None;
        let panel = self.panel_mut(side);
        // The cursor goes to the hit that was under it in the list.
        let name = hits.get(cursor).map(|e| e.name.clone());
        panel.show_results(root, label, hits);
        if let Some(name) = name {
            panel.focus(&name);
        }
        panel.ensure_visible(rows);
        self.status = Some(("Search results in the panel; Backspace or .. returns to the folder".into(), Tone::Info));
        Task::none()
    }

    pub(super) fn find_update(&mut self, msg: FindMsg) -> Task<Message> {
        let Some(f) = &mut self.find else { return Task::none() };
        match msg {
            FindMsg::Input(field, value) => match field {
                Field::Root => f.root = value,
                Field::Name => f.query.name = value,
                Field::Text => f.query.text = value,
            },
            FindMsg::Toggle(opt) => f.set_option(opt),
            FindMsg::Start => return self.start_find(),
            FindMsg::Stop => f.stop(),
            FindMsg::Progress(seq, p) => {
                if seq != f.seq {
                    return Task::none();
                }
                match p {
                    Progress::Batch(b) => {
                        f.results.extend(b.hits);
                        f.folder = b.folder;
                        f.checked = b.checked;
                    }
                    Progress::Done(result) => {
                        f.running = None;
                        match result {
                            Ok(s) => f.summary = Some(s),
                            Err(e) => f.error = Some(e),
                        }
                    }
                }
            }
            FindMsg::Click(i) => {
                let now = Instant::now();
                let double = matches!(f.last_click, Some((j, t)) if j == i && now - t < DOUBLE_CLICK);
                f.cursor = i.min(f.results.len().saturating_sub(1));
                f.last_click = Some((i, now));
                if double {
                    f.last_click = None;
                    return self.find_go_to();
                }
            }
            FindMsg::GoTo => return self.find_go_to(),
            FindMsg::Feed => return self.find_feed(),
            FindMsg::Close => self.close_find(),
            FindMsg::Wheel(lines) => self.find_move((-lines * 3.0).round() as isize),
        }
        Task::none()
    }

    /// Keys of `find_key`, for the F1 help. Keep in sync.
    pub(super) const FIND_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Enter", "Search (in a box) / go to the hit (in the list)"),
        ("Esc", "Stop the search, then close"),
        ("Tab, Shift+Tab", "Next / previous field"),
        ("Up, Down, PgUp, PgDn, Ctrl+Home, Ctrl+End", "Pick a hit"),
        ("Ctrl+G, double-click", "Go to the file: the panel opens its folder"),
        ("Ctrl+L", "Feed the hits to the panel"),
        ("Ctrl+R", "Name regex on / off"),
        ("Ctrl+T", "Text regex on / off"),
        ("Ctrl+E", "Match case on / off"),
        ("Ctrl+S", "Search subfolders on / off"),
        ("Ctrl+H", "Search hidden files on / off"),
        ("*.txt;*.md|skip*", "Name masks: ; separates, | starts exclusions"),
    ];

    pub(super) fn find_key(
        &mut self,
        named: Option<Named>,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
        captured: bool,
    ) -> Task<Message> {
        let Some(f) = &self.find else { return Task::none() };
        let page = self.find_rows().saturating_sub(1).max(1) as isize;
        let msg = match named {
            Some(Named::Escape) if f.running.is_some() => FindMsg::Stop,
            Some(Named::Escape) => FindMsg::Close,
            Some(Named::Tab) if mods.shift() => return iced::widget::focus_previous(),
            Some(Named::Tab) => return iced::widget::focus_next(),
            // Enter in a text box (captured) starts the search through its
            // submit; elsewhere it goes to the hit, if there is one.
            Some(Named::Enter) if !captured => {
                if f.results.is_empty() { FindMsg::Start } else { FindMsg::GoTo }
            }
            Some(Named::ArrowDown) => {
                self.find_move(1);
                return Task::none();
            }
            Some(Named::ArrowUp) => {
                self.find_move(-1);
                return Task::none();
            }
            Some(Named::PageDown) => {
                self.find_move(page);
                return Task::none();
            }
            Some(Named::PageUp) => {
                self.find_move(-page);
                return Task::none();
            }
            Some(Named::Home) if mods.control() => {
                self.find_move(isize::MIN / 2);
                return Task::none();
            }
            Some(Named::End) if mods.control() => {
                self.find_move(isize::MAX / 2);
                return Task::none();
            }
            _ => {
                let keyboard::Key::Character(c) = key else { return Task::none() };
                if !mods.control() || mods.alt() {
                    return Task::none();
                }
                match c.to_ascii_lowercase().as_str() {
                    "g" => FindMsg::GoTo,
                    "l" => FindMsg::Feed,
                    "r" => FindMsg::Toggle(Opt::NameRegex),
                    "t" => FindMsg::Toggle(Opt::TextRegex),
                    "e" => FindMsg::Toggle(Opt::MatchCase),
                    "s" => FindMsg::Toggle(Opt::Subfolders),
                    "h" => FindMsg::Toggle(Opt::Hidden),
                    _ => return Task::none(),
                }
            }
        };
        self.find_update(msg)
    }

    pub(super) fn find_view<'a>(&'a self, f: &'a FindFiles) -> Element<'a, Message> {
        let colors = theme::c();
        let label_w = |s: &'a str| text(s).size(13).color(colors.muted).width(Length::Fixed(LABEL_W));
        let input = |field: Field, placeholder: &'a str, value: &'a str| {
            let mut t = text_input(placeholder, value)
                .on_input(move |v| Message::Find(FindMsg::Input(field, v)))
                .on_submit(Message::Find(FindMsg::Start))
                .padding(6)
                .size(13)
                .style(theme::input);
            if field == Field::Name {
                t = t.id(name_input_id());
            }
            t
        };
        let toggle = |label: &'a str, on: bool, opt: Opt| {
            checkbox(label, on).on_toggle(move |_| Message::Find(FindMsg::Toggle(opt))).size(14).text_size(13)
        };
        let q = &f.query;

        let header = row![text("Find files").size(16)].align_y(Alignment::Center);
        let root = row![label_w("Search in"), input(Field::Root, "Folder", &f.root)].spacing(8).align_y(Alignment::Center);
        let name = row![
            label_w("Search for"),
            input(Field::Name, "*.txt;*.md|skip*   (empty = all)", &q.name),
            toggle("Regex", q.name_regex, Opt::NameRegex),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let contents = row![
            label_w("Containing"),
            input(Field::Text, "Text inside files (empty = names only)", &q.text),
            toggle("Regex", q.text_regex, Opt::TextRegex),
            toggle("Match case", q.match_case, Opt::MatchCase),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let options = row![
            label_w("Options"),
            toggle("Subfolders", q.subfolders, Opt::Subfolders),
            toggle("Hidden files", q.hidden, Opt::Hidden),
        ]
        .spacing(16)
        .align_y(Alignment::Center);
        let form = container(column![header, root, name, contents, options].spacing(10))
            .padding(14)
            .style(filled(colors.header));

        let head_cell = |s: &'a str, w: Length| container(text(s).size(12).color(colors.muted)).width(w);
        let list_header = container(
            row![
                head_cell("Name", Length::Fill),
                head_cell("Size", Length::Fixed(COL_SIZE)),
                head_cell("Date", Length::Fixed(COL_DATE)),
            ]
            .spacing(8),
        )
        .padding([3, 14])
        .style(filled(colors.header));

        let list = responsive(move |size| {
            let rows = (size.height / ROW_H).floor().max(1.0) as usize;
            let start = visible_start(f.top, f.cursor, rows, f.results.len());
            let end = (start + rows).min(f.results.len());
            let lines = (start..end).map(|i| {
                let e = &f.results[i];
                let cursor = i == f.cursor;
                let color = if e.is_dir() {
                    colors.dir
                } else {
                    colors.text
                };
                let size = if e.is_dir() { "<DIR>".to_owned() } else { rc_core::format::size(e.size) };
                let cell = |s: String, w: Length| {
                    container(text(s).size(13).color(color).wrapping(Wrapping::None)).width(w).clip(true)
                };
                let bg = if cursor {
                    colors.cursor
                } else if i % 2 == 1 {
                    colors.zebra
                } else {
                    colors.panel
                };
                mouse_area(
                    container(
                        row![
                            cell(e.name.clone(), Length::Fill),
                            cell(size, Length::Fixed(COL_SIZE)),
                            cell(rc_core::format::date(&e.modified), Length::Fixed(COL_DATE)),
                        ]
                        .spacing(8),
                    )
                    .height(Length::Fixed(ROW_H))
                    .padding([1, 14])
                    .style(filled(bg)),
                )
                .on_press(Message::Find(FindMsg::Click(i)))
                .into()
            });
            container(Column::with_children(lines).width(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true)
                .into()
        });
        let list = mouse_area(container(list).width(Length::Fill).height(Length::Fill).style(filled(colors.panel)))
            .on_scroll(|delta| {
                Message::Find(FindMsg::Wheel(match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / ROW_H / 3.0,
                }))
            });

        let (note, color) = if let Some(e) = &f.error {
            (e.clone(), colors.error)
        } else if f.running.is_some() {
            (
                format!("Searching\u{2026} {} found \u{b7} {} checked \u{b7} {}", f.results.len(), f.checked, f.folder.display()),
                colors.text,
            )
        } else if let Some(s) = &f.summary {
            (summary_text(s), if s.hits > 0 { colors.success } else { colors.text })
        } else {
            (
                "Enter search \u{b7} Esc stop/close \u{b7} \u{2191}\u{2193} pick a hit, Enter/double-click/Ctrl+G go to it \u{b7} \
                 Ctrl+L feed to panel \u{b7} Ctrl+R/T regex \u{b7} Ctrl+E case \u{b7} Ctrl+S subfolders \u{b7} Ctrl+H hidden"
                    .to_owned(),
                colors.muted,
            )
        };
        let action = |label: &'a str, msg: FindMsg, primary: bool| {
            button(text(label).size(13))
                .on_press(Message::Find(msg))
                .padding([6, 14])
                .style(if primary { theme::primary } else { theme::fkey })
        };
        let mut buttons = Row::new().spacing(8);
        if !f.results.is_empty() {
            buttons = buttons.push(action("Go to file", FindMsg::GoTo, false)).push(action("Feed to panel", FindMsg::Feed, false));
        }
        buttons = buttons.push(action("Close", FindMsg::Close, false));
        buttons = if f.running.is_some() {
            buttons.push(action("Stop", FindMsg::Stop, true))
        } else {
            buttons.push(action("Search", FindMsg::Start, true))
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

        container(column![form, list_header, list, footer])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.bg))
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_describe_the_search() {
        let q = Query { name: " *.rs ".into(), ..Query::default() };
        assert_eq!(label(&q), "*.rs");
        let q = Query { text: "fn main".into(), ..Query::default() };
        assert_eq!(label(&q), "* containing \"fn main\"");
    }
}
