//! Change attributes and dates (`cm_SetAttrib`, Ctrl+Shift+A): a dialog with
//! four tri-state flags (Read-only, Hidden, System, Archive: leave / set /
//! clear), the modified and created date and time (empty: leave), and
//! "Include subfolders". OK starts a `Job::SetAttrib` (see `rc_core::attrib`
//! for the logic). With exactly one target the dialog starts from its
//! current values.

use super::*;
use iced::widget::{button, column, container, row, text, text_input, Space};
use iced::{Alignment, Element, Length};
use rc_core::attrib::{self as at, Tri};

#[derive(Debug, Clone)]
pub enum AttribMsg {
    /// Click on a toggle: focus it and cycle (flags) or flip it.
    Toggle(Control),
    Modified(String),
    Created(String),
    Ok,
    Cancel,
}

/// The things in the dialog the keyboard focus visits (Tab).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Flag(usize),
    Subfolders,
    Modified,
    Created,
    Cancel,
    Ok,
}

#[derive(Debug)]
pub(super) struct AttribDialog {
    paths: Vec<PathBuf>,
    flags: [Tri; 4],
    /// The date boxes, and what they started with (an unchanged box leaves
    /// the date alone, so seconds' fractions aren't cut off).
    modified: String,
    created: String,
    start: (String, String),
    has_dirs: bool,
    subfolders: bool,
    focus: Control,
}

impl AttribDialog {
    /// The controls in Tab order.
    fn controls(&self) -> Vec<Control> {
        let mut v: Vec<Control> = (0..4).map(Control::Flag).collect();
        if self.has_dirs {
            v.push(Control::Subfolders);
        }
        v.extend([Control::Modified, Control::Created, Control::Cancel, Control::Ok]);
        v
    }

    fn step(&mut self, back: bool) {
        let all = self.controls();
        let at = all.iter().position(|&c| c == self.focus).unwrap_or(0);
        let next = if back { (at + all.len() - 1) % all.len() } else { (at + 1) % all.len() };
        self.focus = all[next];
    }

    /// Cycles the flag / flips "subfolders" if `c` is one of them.
    fn toggle(&mut self, c: Control) {
        match c {
            Control::Flag(i) => self.flags[i] = self.flags[i].next(),
            Control::Subfolders => self.subfolders = !self.subfolders,
            _ => {}
        }
    }

    /// The change the dialog asks for, or what is wrong with a date.
    fn change(&self) -> Result<at::Change, String> {
        let date = |text: &str, start: &str, what: &str| {
            if text.trim() == start.trim() {
                return Ok(None);
            }
            at::parse_time(text).map_err(|e| format!("{what}: {e}"))
        };
        Ok(at::Change {
            flags: self.flags,
            modified: date(&self.modified, &self.start.0, "Modified")?,
            created: date(&self.created, &self.start.1, "Created")?,
        })
    }
}

/// Moves the text focus away from every box (for focus on a button).
pub(super) fn unfocus_boxes() -> Task<Message> {
    use iced::advanced::widget::{self, operation};
    widget::operate(operation::focusable::focus::<()>(widget::Id::new("attrib-none"))).discard()
}

/// The task that puts the text focus where `c` is.
fn focus_task(c: Control) -> Task<Message> {
    match c {
        Control::Modified => Task::batch([text_input::focus(dialog_input_id()), text_input::select_all(dialog_input_id())]),
        Control::Created => Task::batch([text_input::focus(dialog_comment_id()), text_input::select_all(dialog_comment_id())]),
        _ => unfocus_boxes(),
    }
}

impl App {
    fn attrib_dialog(&mut self) -> Option<&mut AttribDialog> {
        match &mut self.dialog {
            Some(Dialog::Attrib(d)) => Some(d),
            _ => None,
        }
    }

    /// `cm_SetAttrib`: opens the dialog for the selected files (or the cursor entry).
    pub(super) fn open_attrib(&mut self) -> Task<Message> {
        let panel = self.panel(self.active);
        if vfs::is_virtual(&panel.path) {
            self.info("Can only change attributes of local files, not in an archive or on a server");
            return Task::none();
        }
        let targets = panel.targets();
        if targets.is_empty() {
            self.info("Nothing to change: put the cursor on a file or select some");
            return Task::none();
        }
        let has_dirs = targets.iter().any(|e| e.is_dir());
        let paths: Vec<PathBuf> = targets.iter().map(|e| e.path.clone()).collect();
        let mut dialog = AttribDialog {
            flags: [Tri::Leave; 4],
            modified: String::new(),
            created: String::new(),
            start: (String::new(), String::new()),
            has_dirs,
            subfolders: false,
            focus: Control::Flag(0),
            paths,
        };
        if let [path] = dialog.paths.as_slice() {
            if let Ok(attrs) = at::get_attributes(path) {
                dialog.flags = at::flags_of(attrs);
            }
            if let Ok(meta) = std::fs::symlink_metadata(path) {
                let time = |t: io::Result<std::time::SystemTime>| t.ok().map(at::format_time).unwrap_or_default();
                dialog.modified = time(meta.modified());
                dialog.created = time(meta.created());
                dialog.start = (dialog.modified.clone(), dialog.created.clone());
            }
        }
        self.leave_edits();
        self.dialog = Some(Dialog::Attrib(Box::new(dialog)));
        self.dialog_focus = None;
        unfocus_boxes()
    }

    pub(super) fn attrib_msg(&mut self, msg: AttribMsg) -> Task<Message> {
        match msg {
            AttribMsg::Toggle(c) => {
                if let Some(d) = self.attrib_dialog() {
                    d.focus = c;
                    d.toggle(c);
                }
                unfocus_boxes()
            }
            AttribMsg::Modified(s) => {
                if let Some(d) = self.attrib_dialog() {
                    d.modified = s;
                    d.focus = Control::Modified;
                }
                Task::none()
            }
            AttribMsg::Created(s) => {
                if let Some(d) = self.attrib_dialog() {
                    d.created = s;
                    d.focus = Control::Created;
                }
                Task::none()
            }
            AttribMsg::Cancel => {
                self.dialog = None;
                Task::none()
            }
            AttribMsg::Ok => self.attrib_ok(),
        }
    }

    /// OK: starts the job. A date that can't be read keeps the dialog open.
    fn attrib_ok(&mut self) -> Task<Message> {
        let Some(Dialog::Attrib(d)) = self.dialog.take() else { return Task::none() };
        let change = match d.change() {
            Ok(c) => c,
            Err(e) => {
                let focus = if e.starts_with("Created") { Control::Created } else { Control::Modified };
                self.error(e);
                let mut d = d;
                d.focus = focus;
                self.dialog = Some(Dialog::Attrib(d));
                return focus_task(focus);
            }
        };
        let AttribDialog { paths, subfolders, .. } = *d;
        if change.is_empty() {
            self.info("Nothing to change");
            return Task::none();
        }
        let title = format!("Changing attributes of {}", describe(&paths));
        self.start_job(Job::SetAttrib { paths, change, recursive: subfolders }, title)
    }

    /// Keys while the dialog is open (`captured`: a box used the key).
    pub(super) fn attrib_key(&mut self, named: Option<Named>, shift: bool, captured: bool) -> Task<Message> {
        let Some(d) = self.attrib_dialog() else { return Task::none() };
        let focus = d.focus;
        let in_box = matches!(focus, Control::Modified | Control::Created);
        match named {
            Some(Named::Escape) => {
                self.dialog = None;
                Task::none()
            }
            Some(Named::Tab) => {
                d.step(shift);
                focus_task(d.focus)
            }
            Some(Named::ArrowUp | Named::ArrowDown) if !in_box || !captured => {
                d.step(named == Some(Named::ArrowUp));
                focus_task(d.focus)
            }
            // A box sends its own Enter (as OK), and keeps its Space.
            _ if captured => Task::none(),
            Some(Named::Space) => match focus {
                Control::Cancel => self.attrib_msg(AttribMsg::Cancel),
                Control::Ok => self.attrib_msg(AttribMsg::Ok),
                Control::Flag(_) | Control::Subfolders => {
                    d.toggle(focus);
                    Task::none()
                }
                _ => Task::none(),
            },
            Some(Named::Enter) => match focus {
                Control::Cancel => self.attrib_msg(AttribMsg::Cancel),
                _ => self.attrib_msg(AttribMsg::Ok),
            },
            _ => Task::none(),
        }
    }

    pub(super) fn attrib_view<'a>(&'a self, d: &'a AttribDialog) -> Element<'a, Message> {
        let c = theme::c();
        let toggle = |control: Control, mark: &'static str, label: &'static str, state: &'static str| {
            let content = row![
                text(mark).size(13).font(iced::Font::MONOSPACE).width(Length::Fixed(26.0)),
                text(label).size(13).width(Length::Fill),
                text(state).size(12).color(c.muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            button(content)
                .on_press(Message::Attrib(AttribMsg::Toggle(control)))
                .width(Length::Fill)
                .padding([4, 8])
                .style(theme::suggestion(d.focus == control))
        };
        let mut content = column![
            text("Attributes and dates").size(16),
            text(format!("Change {}. Space or a click cycles: leave, set, clear.", describe(&d.paths)))
                .size(13)
                .color(c.muted),
        ]
        .spacing(10);
        for (i, ((_, label), tri)) in at::FLAGS.iter().zip(d.flags).enumerate() {
            let (mark, state) = match tri {
                Tri::Leave => ("[-]", "leave"),
                Tri::Set => ("[x]", "set"),
                Tri::Clear => ("[ ]", "clear"),
            };
            content = content.push(toggle(Control::Flag(i), mark, label, state));
        }
        if d.has_dirs {
            let mark = if d.subfolders { "[x]" } else { "[ ]" };
            content = content.push(toggle(
                Control::Subfolders,
                mark,
                "Include subfolders (apply to everything inside the folders too)",
                "",
            ));
        }
        let field = |hint: &'static str, value: &'a str, id: iced::widget::text_input::Id, on_input: fn(String) -> AttribMsg| {
            text_input(hint, value)
                .id(id)
                .on_input(move |s| Message::Attrib(on_input(s)))
                .on_submit(Message::Attrib(AttribMsg::Ok))
                .padding(8)
                .size(13)
                .style(theme::input)
        };
        let label = |s: &'static str| text(s).size(12).color(c.muted);
        content = content
            .push(label("Modified (YYYY-MM-DD HH:MM:SS, local time; empty: leave)"))
            .push(field("leave unchanged", &d.modified, dialog_input_id(), AttribMsg::Modified))
            .push(label("Created"))
            .push(field("leave unchanged", &d.created, dialog_comment_id(), AttribMsg::Created))
            .push(
                row![
                    text("Tab moves between the controls").size(11).color(c.muted),
                    Space::with_width(Length::Fill),
                    view::dialog_button("Cancel", Message::Attrib(AttribMsg::Cancel), false, d.focus == Control::Cancel),
                    view::dialog_button("OK", Message::Attrib(AttribMsg::Ok), true, d.focus == Control::Ok),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        container(content.spacing(8)).padding(20).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }
}
