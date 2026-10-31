use crate::keymap;
use crate::theme::{self, filled};
use iced::widget::text::Wrapping;
use iced::widget::{
    button, column, container, mouse_area, pick_list, responsive, row, text, Column, Row, Space,
};
use iced::{keyboard, mouse, window, Alignment, Element, Length, Size, Subscription, Task, Theme};
use rc_core::panel::visible_start;
use rc_core::{format, Activated, Command, EntryKind, Panel, SortKey};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const ROW_H: f32 = 22.0;
/// Approximate height of everything that isn't file rows (headers, footers,
/// function-key bar). Only used to size PageUp/PageDown jumps.
const CHROME_H: f32 = 150.0;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

const COL_EXT: f32 = 60.0;
const COL_SIZE: f32 = 80.0;
const COL_DATE: f32 = 120.0;

pub struct App {
    panels: [Panel; 2],
    active: usize,
    drives: Vec<String>,
    /// (message, is_error)
    status: Option<(String, bool)>,
    window_height: f32,
    last_click: Option<(usize, usize, Instant)>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Key(keyboard::Key, keyboard::Modifiers),
    Run(Command),
    Click(usize, usize),
    Wheel(usize, f32),
    Drive(usize, String),
    Sort(usize, SortKey),
    Resized(Size),
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
            window_height: 800.0,
            last_click: None,
        };
        (app, Task::none())
    }

    pub fn theme(&self) -> Theme {
        theme::theme()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            keyboard::on_key_press(|key, mods| Some(Message::Key(key, mods))),
            window::resize_events().map(|(_id, size)| Message::Resized(size)),
        ])
    }

    // ---- update -----------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Key(key, mods) => {
                if let Some(cmd) = keymap::command_for(&key, mods) {
                    return self.run(cmd);
                }
            }
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
            Message::Resized(size) => self.window_height = size.height,
        }
        Task::none()
    }

    fn run(&mut self, cmd: Command) -> Task<Message> {
        use Command::*;
        self.status = None;
        let rows = self.page_rows();
        let page = rows.saturating_sub(1).max(1) as isize;
        let other = 1 - self.active;

        // Commands that touch more than the active panel.
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
            Copy | Move | MakeDir | Delete => {
                let n = panel.targets().len();
                info = Some(format!("{} ({n} item(s)): not implemented yet", cmd.id()));
                Ok(())
            }
            Quit | SwitchPanel | SyncOtherPanel => unreachable!(),
        };
        panel.ensure_visible(rows);
        self.report(result);
        if let Some(msg) = info {
            self.status = Some((msg, false));
        }
        Task::none()
    }

    fn report(&mut self, result: io::Result<()>) {
        if let Err(e) = result {
            self.status = Some((e.to_string(), true));
        }
    }

    fn page_rows(&self) -> usize {
        ((self.window_height - CHROME_H) / ROW_H).floor().max(1.0) as usize
    }

    // ---- view -------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        let panels = row![self.panel_view(0), vline(), self.panel_view(1)].height(Length::Fill);

        container(column![panels, hline(), self.bottom_bar()])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(theme::BG))
            .into()
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
                text(panel.path.display().to_string())
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

    fn bottom_bar(&self) -> Element<'_, Message> {
        let status = match &self.status {
            Some((msg, is_error)) => text(msg.as_str())
                .size(12)
                .color(if *is_error { theme::ERROR } else { theme::MUTED }),
            None => text("Tab switch panel · Enter open · Backspace up · Ins/Space select · Ctrl+H hidden · Alt+O sync")
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
            container(status).padding([3, 10]),
            container(fkeys).padding(iced::Padding { top: 0.0, right: 8.0, bottom: 8.0, left: 8.0 }),
        ]
        .into()
    }
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
