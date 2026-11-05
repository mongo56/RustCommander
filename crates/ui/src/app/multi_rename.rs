//! Multi-rename tool (Ctrl+M, TC's `cm_MultiRenameFiles`): fills the window
//! while open. Masks, search & replace and a case change give every
//! selected file a new name, previewed live; Enter renames the batch, which
//! Ctrl+Z undoes (also after closing: the last batch's undo is kept and
//! offered when the tool is reopened). A new name can be typed by hand:
//! click it, or F2 then Up/Down (Enter/Esc finish; clear the box to go back
//! to the computed name). Name logic lives in `rc_core::rename`.

use super::*;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, checkbox, column, container, mouse_area, row, scrollable, text, text_input, Column, Row, Space,
};
use iced::{Alignment, Element, Length};
use rc_core::rename::{self, Case, Options, Preview};
use rc_core::undo::UndoStep;
use std::collections::{HashMap, HashSet};

/// Rows drawn in the preview list; the rest are summarised.
const MAX_ROWS: usize = 2000;
const LABEL_W: f32 = 76.0;

fn name_input_id() -> text_input::Id {
    text_input::Id::new("multi-rename-name")
}

fn edit_input_id() -> text_input::Id {
    text_input::Id::new("multi-rename-edit")
}

fn list_id() -> scrollable::Id {
    scrollable::Id::new("multi-rename-list")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Ext,
    Search,
    Replace,
    Start,
    Step,
    Digits,
}

pub(super) struct MultiRename {
    side: usize,
    dir: PathBuf,
    entries: Vec<Entry>,
    /// The panel had a selection when the tool opened: renamed files are
    /// selected again under their new names.
    had_selection: bool,
    /// Names typed by hand, by entry index (may be empty while typing).
    edits: HashMap<usize, String>,
    /// Row whose new name is being edited.
    editing: Option<usize>,
    /// Last row edited, where F2 resumes.
    last_edit: usize,
    /// [`rename::name_key`]s of the folder's other names.
    others: HashSet<String>,
    opts: Options,
    /// Counter fields as typed.
    start: String,
    step: String,
    digits: String,
    preview: Result<Vec<Preview>, String>,
    /// `(new, old)` paths of the last batch, for Ctrl+Z.
    undo: Vec<(PathBuf, PathBuf)>,
    note: Option<(String, Tone)>,
}

impl MultiRename {
    /// The cursor is the row whose name is edited by hand. Nothing waits for an answer here.
    pub(super) fn note_state(&mut self) -> NoteState<'_> {
        NoteState { cursor: (self.editing.map_or(0, |i| i + 1), 0), pending: false, note: &mut self.note }
    }

    fn refresh(&mut self) {
        let parse = |s: &str, what: &str| s.trim().parse::<i64>().map_err(|_| format!("Counter {what}: not a number"));
        let counter = (|| {
            Ok::<_, String>(rename::Counter {
                start: parse(&self.start, "start")?,
                step: parse(&self.step, "step")?,
                digits: parse(&self.digits, "digits")?.clamp(1, 20) as usize,
            })
        })();
        self.preview = counter.and_then(|c| {
            self.opts.counter = c;
            rename::preview(&self.entries, &self.opts, &self.others, &self.edits)
        });
    }

    fn read_others(&mut self) {
        let mine: HashSet<String> = self.entries.iter().map(|e| rename::name_key(&e.name)).collect();
        self.others = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| rename::name_key(&e.file_name().to_string_lossy()))
                    .filter(|k| !mine.contains(k))
                    .collect()
            })
            .unwrap_or_default();
    }

    /// Applies finished `(from, to)` renames to the entries.
    fn moved(&mut self, done: &[(PathBuf, PathBuf)]) {
        for (from, to) in done {
            if let Some(e) = self.entries.iter_mut().find(|e| &e.path == from) {
                e.path = to.clone();
                e.name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            }
        }
    }

    /// Renames `pairs`, updates the list and returns the undo pairs and a note.
    fn rename(&mut self, pairs: &[(PathBuf, PathBuf)], verb: &str) -> Vec<(PathBuf, PathBuf)> {
        let (done, failed) = rename::rename_all(pairs);
        self.moved(&done);
        // Hand-typed names were applied (or are stale now).
        self.edits.clear();
        self.editing = None;
        self.read_others();
        self.refresh();
        self.note = Some(match failed.first() {
            None => (format!("{verb} {}", plural(done.len(), "file")), Tone::Success),
            Some(f) => (
                format!(
                    "{verb} {}, {} failed: {}: {}",
                    done.len(),
                    failed.len(),
                    f.from.file_name().unwrap_or_default().to_string_lossy(),
                    f.error
                ),
                Tone::Error,
            ),
        });
        done.into_iter().map(|(from, to)| (to, from)).collect()
    }
}

impl MultiRename {
    /// Ends editing a row; an emptied box goes back to the computed name.
    fn finish_edit(&mut self) {
        if let Some(i) = self.editing.take() {
            if self.edits.get(&i).is_some_and(String::is_empty) {
                self.edits.remove(&i);
            }
        }
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

impl App {
    pub(super) fn open_multi_rename(&mut self) -> Task<Message> {
        let side = self.active;
        let panel = self.panel(side);
        if panel.results().is_some() {
            self.info("Multi-rename doesn't work on search results yet");
            return Task::none();
        }
        let entries: Vec<Entry> = panel.targets().into_iter().cloned().collect();
        if entries.is_empty() {
            return Task::none();
        }
        let dir = panel.path.clone();
        let had_selection = panel.selected_count() > 0;
        self.leave_edits();
        let undo = std::mem::take(&mut self.rename_undo);
        let note = (!undo.is_empty())
            .then(|| (format!("Ctrl+Z undoes the last batch ({})", plural(undo.len(), "file")), Tone::Info));
        let c = self.rename_opts.counter;
        let mut mr = MultiRename {
            side,
            dir,
            entries,
            had_selection,
            edits: HashMap::new(),
            editing: None,
            last_edit: 0,
            others: HashSet::new(),
            opts: self.rename_opts.clone(),
            start: c.start.to_string(),
            step: c.step.to_string(),
            digits: c.digits.to_string(),
            preview: Ok(Vec::new()),
            undo,
            note,
        };
        mr.read_others();
        mr.refresh();
        self.screen = Some(Screen::MultiRename(Box::new(mr)));
        Task::batch([text_input::focus(name_input_id()), text_input::move_cursor_to_end(name_input_id())])
    }

    fn close_multi_rename(&mut self) {
        self.sync_rename_panel();
        if let Some(mr) = self.screen.take_multi_rename() {
            self.rename_opts = mr.opts;
            self.rename_undo = mr.undo;
        }
    }

    /// After a batch (the panels were re-read): selects the tool's files in
    /// their panel under their current names (if they were selected when it
    /// opened) and puts the cursor on the first.
    fn sync_rename_panel(&mut self) {
        let Some(mr) = self.screen.multi_rename() else {
            return;
        };
        if self.panel(mr.side).path != mr.dir {
            return;
        }
        let names: Vec<String> = mr.entries.iter().map(|e| e.name.clone()).collect();
        let (side, had_selection) = (mr.side, mr.had_selection);
        let panel = self.panel_mut(side);
        if had_selection {
            panel.set_selection(names.iter().cloned());
        }
        if let Some(first) = names.first() {
            panel.focus(first);
        }
    }

    /// Starts editing the new name of row `i` (clamped), focusing its box.
    fn edit_row(&mut self, i: usize, scroll: bool) -> Task<Message> {
        let Some(mr) = self.screen.multi_rename_mut() else {
            return Task::none();
        };
        let n = mr.entries.len().min(MAX_ROWS);
        if n == 0 {
            return Task::none();
        }
        let i = i.min(n - 1);
        mr.finish_edit();
        mr.refresh();
        mr.editing = Some(i);
        mr.last_edit = i;
        let focus = Task::batch([text_input::focus(edit_input_id()), text_input::move_cursor_to_end(edit_input_id())]);
        if !scroll {
            return focus;
        }
        // Rows are about the same height, so the row's share of the list
        // keeps it in view.
        let y = if n > 1 { i as f32 / (n - 1) as f32 } else { 0.0 };
        Task::batch([focus, scrollable::snap_to(list_id(), scrollable::RelativeOffset { x: 0.0, y })])
    }

    pub(super) fn multi_rename_update(&mut self, msg: MrMsg) -> Task<Message> {
        let Some(mr) = self.screen.multi_rename_mut() else {
            return Task::none();
        };
        match msg {
            MrMsg::Input(field, value) => {
                match field {
                    Field::Name => mr.opts.name_mask = value,
                    Field::Ext => mr.opts.ext_mask = value,
                    Field::Search => mr.opts.search = value,
                    Field::Replace => mr.opts.replace = value,
                    Field::Start => mr.start = value,
                    Field::Step => mr.step = value,
                    Field::Digits => mr.digits = value,
                }
                mr.refresh();
            }
            MrMsg::Regex(on) => {
                mr.opts.regex = on;
                mr.refresh();
            }
            MrMsg::MatchCase(on) => {
                mr.opts.match_case = on;
                mr.refresh();
            }
            MrMsg::Case(case) => {
                mr.opts.case = case;
                mr.refresh();
            }
            MrMsg::Edit(i) => return self.edit_row(i, false),
            MrMsg::EditInput(value) => {
                if let Some(i) = mr.editing {
                    mr.edits.insert(i, value);
                    mr.refresh();
                }
            }
            MrMsg::EditDone => {
                mr.finish_edit();
                mr.refresh();
                return Task::batch([text_input::focus(name_input_id()), text_input::move_cursor_to_end(name_input_id())]);
            }
            MrMsg::Run => {
                mr.finish_edit();
                mr.refresh();
                let previews = match &mr.preview {
                    Err(e) => {
                        mr.note = Some((e.clone(), Tone::Error));
                        return Task::none();
                    }
                    Ok(p) => p,
                };
                let bad = previews.iter().filter(|p| p.problem.is_some()).count();
                if bad > 0 {
                    mr.note = Some((format!("Can't rename: {} with problems", plural(bad, "name")), Tone::Error));
                    return Task::none();
                }
                let pairs: Vec<(PathBuf, PathBuf)> = mr
                    .entries
                    .iter()
                    .zip(previews)
                    .filter(|(e, p)| !p.unchanged(&e.name))
                    .map(|(e, p)| (e.path.clone(), mr.dir.join(&p.new_name)))
                    .collect();
                if pairs.is_empty() {
                    mr.note = Some(("Nothing to rename".into(), Tone::Info));
                    return Task::none();
                }
                mr.undo = mr.rename(&pairs, "Renamed");
                // Ctrl+Z on the panels can undo it too (after closing).
                let done = mr.undo.iter().map(|(to, from)| (from.clone(), to.clone())).collect();
                self.push_undo(UndoStep::multi_renamed(done));
                self.reload_panels();
                self.sync_rename_panel();
            }
            MrMsg::Undo => {
                if mr.undo.is_empty() {
                    mr.note = Some(("Nothing to undo".into(), Tone::Info));
                    return Task::none();
                }
                let pairs = std::mem::take(&mut mr.undo);
                mr.rename(&pairs, "Undone:");
                // Undone here: the panels' step for it is done with.
                let last_is_this = matches!(self.undo.last(), Some(UndoStep::MultiRename(v))
                    if v.len() == pairs.len() && v.iter().zip(&pairs).all(|(m, (to, from))| m.to == *to && m.from == *from));
                if last_is_this {
                    self.undo.pop();
                }
                self.reload_panels();
                self.sync_rename_panel();
            }
            MrMsg::Close => self.close_multi_rename(),
        }
        Task::none()
    }

    /// Keys of `multi_rename_key`, for the F1 help. Keep in sync.
    pub(super) const RENAME_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Enter", "Rename the files"),
        ("Esc", "Close (or stop editing a name)"),
        ("Tab, Shift+Tab", "Next / previous field"),
        ("Ctrl+Z", "Undo the last rename (also after closing)"),
        ("Ctrl+R", "Regex search on / off"),
        ("Ctrl+E", "Match case on / off"),
        ("Ctrl+U", "Next upper / lower case mode"),
        ("F2", "Edit a new name by hand (click a row too)"),
        ("Up, Down", "Edit the previous / next name"),
        ("[N] [E] [C] [P] [YMD] ...", "Mask placeholders: name, extension, counter, folder, date (see the form)"),
    ];

    pub(super) fn multi_rename_key(
        &mut self,
        named: Option<Named>,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
        captured: bool,
    ) -> Task<Message> {
        let editing = self.screen.multi_rename().and_then(|mr| mr.editing);
        let msg = match named {
            Some(Named::Escape) if editing.is_some() => MrMsg::EditDone,
            Some(Named::ArrowUp) if editing.is_some() => {
                return self.edit_row(editing.unwrap_or(0).saturating_sub(1), true);
            }
            Some(Named::ArrowDown) if editing.is_some() => return self.edit_row(editing.unwrap_or(0) + 1, true),
            Some(Named::F2) => {
                let last = self.screen.multi_rename().map_or(0, |mr| mr.last_edit);
                return self.edit_row(last, true);
            }
            Some(Named::Escape) => MrMsg::Close,
            Some(Named::Tab) if mods.shift() => return iced::widget::focus_previous(),
            Some(Named::Tab) => return iced::widget::focus_next(),
            Some(Named::Enter) if !captured => MrMsg::Run,
            _ => {
                let keyboard::Key::Character(c) = key else {
                    return Task::none();
                };
                if !mods.control() || mods.alt() {
                    return Task::none();
                }
                let Some(mr) = self.screen.multi_rename() else {
                    return Task::none();
                };
                match c.to_ascii_lowercase().as_str() {
                    "z" => MrMsg::Undo,
                    "r" => MrMsg::Regex(!mr.opts.regex),
                    "e" => MrMsg::MatchCase(!mr.opts.match_case),
                    "u" => {
                        let i = Case::ALL.iter().position(|&c| c == mr.opts.case).unwrap_or(0);
                        MrMsg::Case(Case::ALL[(i + 1) % Case::ALL.len()])
                    }
                    _ => return Task::none(),
                }
            }
        };
        self.multi_rename_update(msg)
    }

    pub(super) fn multi_rename_view<'a>(&'a self, mr: &'a MultiRename) -> Element<'a, Message> {
        let colors = theme::c();
        let label = |s: &'a str| text(s).size(13).color(colors.muted).width(Length::Fixed(LABEL_W));
        let input = |field: Field, value: &'a str| {
            let mut t = text_input("", value)
                .on_input(move |v| Message::MultiRename(MrMsg::Input(field, v)))
                .on_submit(Message::MultiRename(MrMsg::Run))
                .padding(6)
                .size(13)
                .style(theme::input);
            if field == Field::Name {
                t = t.id(name_input_id());
            }
            t
        };
        let small = |field: Field, value: &'a str| input(field, value).width(Length::Fixed(56.0));

        let header = row![
            text("Multi-rename").size(16),
            Space::with_width(Length::Fill),
            text(format!("{} in {}", plural(mr.entries.len(), "item"), mr.dir.display()))
                .size(12)
                .color(colors.muted)
                .wrapping(Wrapping::None),
        ]
        .align_y(Alignment::Center);

        let masks = row![
            label("Name"),
            input(Field::Name, &mr.opts.name_mask),
            text("Extension").size(13).color(colors.muted),
            input(Field::Ext, &mr.opts.ext_mask).width(Length::Fixed(160.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let replace = row![
            label("Search"),
            input(Field::Search, &mr.opts.search),
            text("Replace").size(13).color(colors.muted),
            input(Field::Replace, &mr.opts.replace),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut cases = Row::new().spacing(4);
        for case in Case::ALL {
            cases = cases.push(
                button(text(case.label()).size(12))
                    .on_press(Message::MultiRename(MrMsg::Case(case)))
                    .padding([3, 8])
                    .style(theme::tab(case == mr.opts.case)),
            );
        }
        let toggles = row![
            label("Options"),
            checkbox("Regex", mr.opts.regex).on_toggle(|b| Message::MultiRename(MrMsg::Regex(b))).size(14).text_size(13),
            checkbox("Match case", mr.opts.match_case)
                .on_toggle(|b| Message::MultiRename(MrMsg::MatchCase(b)))
                .size(14)
                .text_size(13),
            Space::with_width(Length::Fixed(12.0)),
            cases,
            Space::with_width(Length::Fill),
            text("Counter").size(13).color(colors.muted),
            small(Field::Start, &mr.start),
            text("step").size(12).color(colors.muted),
            small(Field::Step, &mr.step),
            text("digits").size(12).color(colors.muted),
            small(Field::Digits, &mr.digits),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let help = text(
            "[N] name  [E] extension  [N2-5] [N3-] [N2,3] [N-3-] characters  [C] counter  [C10+5:3] start+step:digits  \
             [P] folder  [YMD] [hms] [Y] [M] [D] date  [[ ]] brackets",
        )
        .size(11)
        .color(colors.muted);

        let form = container(column![header, masks, replace, toggles, help].spacing(10))
            .padding(14)
            .style(filled(colors.header));

        let list: Element<'a, Message> = match &mr.preview {
            Err(e) => container(text(e.as_str()).size(13).color(colors.error)).padding(14).into(),
            Ok(previews) => {
                let cell = |s: String, color| {
                    container(text(s).size(13).color(color).wrapping(Wrapping::None))
                        .width(Length::FillPortion(1))
                        .clip(true)
                };
                let mut rows = Column::new();
                rows = rows.push(
                    row![cell("Old name".into(), colors.muted), cell("New name".into(), colors.muted)]
                        .spacing(16)
                        .padding([4, 14]),
                );
                for (i, (e, p)) in mr.entries.iter().zip(previews).enumerate().take(MAX_ROWS) {
                    let old_color = if e.is_dir() { colors.dir } else { colors.text };
                    let (new, color) = match &p.problem {
                        Some(why) => (format!("{}   ({why})", p.new_name), colors.error),
                        None if p.unchanged(&e.name) => (p.new_name.clone(), colors.muted),
                        None => (p.new_name.clone(), colors.selected),
                    };
                    let new = if p.edited && p.problem.is_none() { format!("{new}   (edited)") } else { new };
                    let bg = if i % 2 == 1 { colors.zebra } else { colors.panel };
                    let new_cell: Element<'a, Message> = if mr.editing == Some(i) {
                        let value = mr.edits.get(&i).map_or(p.new_name.as_str(), String::as_str);
                        text_input("", value)
                            .id(edit_input_id())
                            .on_input(|v| Message::MultiRename(MrMsg::EditInput(v)))
                            .on_submit(Message::MultiRename(MrMsg::EditDone))
                            .padding([0, 4])
                            .size(13)
                            .width(Length::FillPortion(1))
                            .style(theme::input)
                            .into()
                    } else {
                        mouse_area(cell(new, color)).on_press(Message::MultiRename(MrMsg::Edit(i))).into()
                    };
                    rows = rows.push(
                        container(row![cell(e.name.clone(), old_color), new_cell].spacing(16))
                            .padding([2, 14])
                            .style(filled(bg)),
                    );
                }
                if mr.entries.len() > MAX_ROWS {
                    rows = rows.push(
                        container(text(format!("… and {} more", mr.entries.len() - MAX_ROWS)).size(12).color(colors.muted))
                            .padding([4, 14]),
                    );
                }
                scrollbar::thin(rows).id(list_id()).height(Length::Fill).into()
            }
        };

        let note: Element<'a, Message> = match &mr.note {
            Some((msg, tone)) => {
                let color = match tone {
                    Tone::Info => colors.text,
                    Tone::Success => colors.success,
                    Tone::Error => colors.error,
                };
                text(msg.as_str()).size(12).color(color).wrapping(Wrapping::None).into()
            }
            None => {
                const HINTS: &[theme::Hint] = &[
                    (&["Enter"], "rename"),
                    (&["Ctrl+Z"], "undo"),
                    (&["F2"], "or click a new name to edit it"),
                    (&["Ctrl+R"], "regex"),
                    (&["Ctrl+E"], "match case"),
                    (&["Ctrl+U"], "case"),
                    (&["Tab"], "next field"),
                    (&["Esc"], "close"),
                ];
                theme::key_hints_row(HINTS, self.window_width - 360.0)
            }
        };
        let action = |label: &'a str, msg: MrMsg, primary: bool| {
            button(text(label).size(13))
                .on_press(Message::MultiRename(msg))
                .padding([6, 14])
                .style(if primary { theme::primary } else { theme::fkey })
        };
        let mut buttons = Row::new().spacing(8);
        if !mr.undo.is_empty() {
            buttons = buttons.push(action("Undo", MrMsg::Undo, false));
        }
        buttons = buttons.push(action("Close", MrMsg::Close, false)).push(action("Rename", MrMsg::Run, true));
        let footer = container(
            row![container(note).width(Length::Fill).clip(true), buttons].spacing(12).align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(filled(colors.header));

        container(column![form, container(list).height(Length::Fill).style(filled(colors.panel)), footer])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.bg))
            .into()
    }
}

#[derive(Debug, Clone)]
pub enum MrMsg {
    Input(Field, String),
    Regex(bool),
    MatchCase(bool),
    Case(Case),
    /// Edit the new name of a row by hand.
    Edit(usize),
    EditInput(String),
    EditDone,
    Run,
    Undo,
    Close,
}
