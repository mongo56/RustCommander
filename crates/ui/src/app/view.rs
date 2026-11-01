//! Drawing. Everything here is a pure function of [`App`] state, apart from
//! the lazily filled icon and disk-space caches.

use super::*;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, center, checkbox, column, container, image, mouse_area, opaque, progress_bar, responsive, row,
    stack, text, text_input, Column, Row, Space,
};
use iced::{Alignment, Element, Length, Padding};
use rc_core::format;

const COL_EXT: f32 = 60.0;
const COL_SIZE: f32 = 80.0;
const COL_DATE: f32 = 120.0;
const ICON: f32 = 16.0;

impl App {
    pub fn view(&self) -> Element<'_, Message> {
        let panels = row![self.side_view(0), vline(), self.side_view(1)].height(Length::Fill);

        let base: Element<'_, Message> = match (&self.multi_rename, &self.viewer) {
            (Some(mr), _) => self.multi_rename_view(mr),
            (None, Some(v)) => self.viewer_view(v),
            (None, None) => container(column![panels, hline(), self.jobs_view(), self.bottom_bar()])
                .width(Length::Fill)
                .height(Length::Fill)
                .style(filled(theme::c().bg))
                .into(),
        };

        let modal = self
            .jobs
            .iter()
            .find_map(|j| j.conflict.as_ref().map(|c| self.conflict_view(j.id, c)))
            .or_else(|| self.dialog.as_ref().map(|d| self.dialog_view(d)));

        if let Some(card) = modal {
            return stack![base, opaque(center(card).style(theme::backdrop))].into();
        }
        if let Some(label) = self.drag_label() {
            return stack![base, label].into();
        }
        base
    }

    fn dragging(&self) -> bool {
        self.drag.as_ref().is_some_and(|d| d.active)
    }

    /// Floating "Copy 3 items → target" label next to the mouse.
    fn drag_label(&self) -> Option<Element<'_, Message>> {
        let drag = self.drag.as_ref().filter(|d| d.active)?;
        let verb = if self.modifiers.shift() { "Move" } else { "Copy" };
        let what = describe(&drag.sources);
        let label = match self.drop_target(drag) {
            Some(target) => {
                let name = target.file_name().map(|n| n.to_string_lossy().into_owned());
                format!("{verb} {what} → {}", name.unwrap_or_else(|| target.display().to_string()))
            }
            None => format!("{verb} {what}"),
        };
        let tag = container(text(label).size(12)).padding([4, 8]).style(theme::drag_label);
        Some(
            container(tag)
                .padding(Padding { top: self.mouse.y + 16.0, left: self.mouse.x + 14.0, ..Padding::ZERO })
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        )
    }

    fn side_view(&self, side: usize) -> Element<'_, Message> {
        let panel = self.panel(side);
        let active = side == self.active;
        let drop_here = self
            .drag
            .as_ref()
            .is_some_and(|d| d.active && self.side_under_mouse() == side && self.drop_target(d).is_some());

        // Thin strip marks the active panel (green while it's a drop target).
        let strip = if drop_here {
            theme::c().success
        } else if active {
            theme::c().accent
        } else {
            theme::c().header
        };
        let accent = container(Space::new(Length::Fill, Length::Fixed(2.0))).style(filled(strip));

        // The suggestions dropdown floats over the file list.
        let mut list = stack![column![self.column_header(side), self.file_list(side, active)]];
        if let Some(dropdown) = self.suggestions_view(side) {
            list = list.push(dropdown);
        }

        column![accent, self.tab_bar(side), self.drive_bar(side), self.path_bar(side), list, self.footer(side, panel)]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn tab_bar(&self, side: usize) -> Element<'_, Message> {
        let s = &self.sides[side];
        let closable = s.tabs.len() > 1;
        let tabs = Row::with_children(s.tabs.iter().enumerate().map(|(i, panel)| {
            let current = i == s.current;
            let mut label = row![text(panel.title()).size(12).wrapping(Wrapping::None)]
                .spacing(6)
                .align_y(Alignment::Center);
            if closable {
                label = label.push(
                    button(text("×").size(12))
                        .on_press(Message::TabClose(side, i))
                        .padding([0, 4])
                        .style(theme::flat),
                );
            }
            mouse_area(
                button(label)
                    .on_press(Message::TabSelect(side, i))
                    .padding([3, 10])
                    .style(theme::tab(current)),
            )
            .on_middle_press(Message::TabClose(side, i))
            .into()
        }))
        .spacing(2);

        let new_tab = button(text("+").size(13))
            .on_press(Message::TabNew(side))
            .padding([2, 8])
            .style(theme::flat);

        // Buttons capture their own clicks, so the mouse area only sees
        // clicks on the empty part of the bar.
        mouse_area(
            container(row![tabs, new_tab].spacing(4).align_y(Alignment::Center))
                .padding(Padding { top: 4.0, right: 8.0, bottom: 0.0, left: 8.0 })
                .width(Length::Fill)
                .clip(true)
                .style(filled(theme::c().bg)),
        )
        .on_press(Message::TabBarClick(side))
        .into()
    }

    /// One button per drive, plus free space on the current one.
    fn drive_bar(&self, side: usize) -> Element<'_, Message> {
        let panel = self.panel(side);
        let buttons = Row::with_children(self.drives.iter().map(|drive| {
            let current = panel.path.starts_with(drive.as_str());
            let letter = drive.trim_end_matches(['\\', '/', ':']).to_string();
            let letter = if letter.is_empty() { drive.clone() } else { letter };
            let mut content = Row::new().spacing(4).align_y(Alignment::Center);
            if let Some(icon) = self.icon(Path::new(drive), true) {
                content = content.push(image(icon).width(ICON).height(ICON));
            }
            content = content.push(text(letter).size(12));
            button(content)
                .on_press(Message::Drive(side, drive.clone()))
                .padding([2, 6])
                .style(theme::drive(current))
                .into()
        }))
        .spacing(2);

        let space = self
            .disk_space(&panel.path)
            .map(|(free, total)| format!("{} free of {}", format::size(free), format::size(total)))
            .unwrap_or_default();

        container(
            row![
                buttons,
                Space::with_width(Length::Fill),
                text(space).size(12).color(theme::c().muted).wrapping(Wrapping::None),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([4, 8])
        .width(Length::Fill)
        .clip(true)
        .style(filled(theme::c().header))
        .into()
    }

    /// Current path; click to edit it.
    fn path_bar(&self, side: usize) -> Element<'_, Message> {
        let panel = self.panel(side);
        let active = side == self.active;

        let content: Element<'_, Message> = match &self.path_edit {
            Some(edit) if edit.side == side => text_input("Path", &edit.text)
                .id(path_input_id())
                .on_input(Message::PathInput)
                .on_submit(Message::PathSubmit)
                .size(13)
                .padding([3, 6])
                .style(theme::input)
                .into(),
            _ => mouse_area(
                container(match self.pending_nav(side) {
                    // Waiting for a slow folder: show where we're going.
                    Some(target) => text(elide_left(&format!("{}  (reading…)", target.display()), self.path_chars()))
                        .size(13)
                        .color(theme::c().muted)
                        .wrapping(Wrapping::None),
                    None => text(elide_left(&panel.path.display().to_string(), self.path_chars()))
                        .size(13)
                        .color(if active { theme::c().text } else { theme::c().muted })
                        .wrapping(Wrapping::None),
                })
                .padding([4, 6])
                .width(Length::Fill),
            )
            .on_press(Message::EditPath(side))
            .interaction(mouse::Interaction::Text)
            .into(),
        };

        container(content)
            .padding([2, 4])
            .width(Length::Fill)
            .clip(true)
            .style(filled(theme::c().header))
            .into()
    }

    /// Folder suggestions under the path bar while it's being edited.
    fn suggestions_view(&self, side: usize) -> Option<Element<'_, Message>> {
        let edit = self.path_edit.as_ref().filter(|e| e.side == side && !e.suggestions.is_empty())?;
        let items = Column::with_children(edit.suggestions.iter().enumerate().map(|(i, path)| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut content = Row::new().spacing(8).align_y(Alignment::Center);
            if let Some(icon) = self.icon(path, true) {
                content = content.push(image(icon).width(ICON).height(ICON));
            }
            content = content.push(text(name).size(13).wrapping(Wrapping::None));
            button(content)
                .on_press(Message::PathPick(path.clone()))
                .width(Length::Fill)
                .padding([3, 8])
                .style(theme::suggestion(edit.selected == Some(i)))
                .into()
        }));
        let hint = text("Tab completes · ↑↓ choose · Enter go").size(11).color(theme::c().muted);
        let card = container(column![items, container(hint).padding([4, 8])])
            .padding(4)
            .width(Length::Fill)
            .style(theme::dialog);
        Some(container(card).padding([0, 8]).width(Length::Fill).into())
    }

    fn footer<'a>(&'a self, side: usize, panel: &'a Panel) -> Element<'a, Message> {
        if let Some(q) = self.quick_search.as_ref().filter(|q| q.side == side) {
            return container(
                row![
                    text("Find").size(12).color(theme::c().muted),
                    container(text(&q.text).size(12)).padding([1, 6]).style(theme::drag_label),
                    text("↑↓ next match · Esc close").size(11).color(theme::c().muted),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([2, 10])
            .width(Length::Fill)
            .style(filled(theme::c().header))
            .into();
        }
        let editing = self.filter_edit == Some(side) && self.active == side;
        if editing || !panel.filter().is_empty() {
            let shown = panel.entries.iter().filter(|e| !e.is_parent()).count();
            let all = panel.unfiltered_len() - panel.entries.iter().filter(|e| e.is_parent()).count();
            let hint = if editing { "type to filter · *.ext; wildcards · Esc clear" } else { "Ctrl+S edit · Esc clear" };
            return container(
                row![
                    text("Filter").size(12).color(theme::c().muted),
                    container(text(panel.filter()).size(12))
                        .padding([1, 6])
                        .style(theme::drag_label),
                    text(format!("{shown} of {all}")).size(12),
                    text(hint).size(11).color(theme::c().muted),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([2, 10])
            .width(Length::Fill)
            .style(filled(theme::c().header))
            .into();
        }
        let (sel, total, sel_bytes, total_bytes) = panel.summary();
        container(
            text(format!(
                "{sel} of {total} selected  ·  {} of {}",
                format::size(sel_bytes),
                format::size(total_bytes)
            ))
            .size(12)
            .color(theme::c().muted),
        )
        .padding([4, 10])
        .width(Length::Fill)
        .style(filled(theme::c().header))
        .into()
    }

    fn column_header(&self, side: usize) -> Element<'_, Message> {
        let panel = self.panel(side);
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
        container(
            row![
                Space::with_width(ICON),
                sort_button("Name", SortKey::Name, Length::Fill, false),
                sort_button("Ext", SortKey::Ext, Length::Fixed(COL_EXT), false),
                sort_button("Size", SortKey::Size, Length::Fixed(COL_SIZE), true),
                sort_button("Modified", SortKey::Modified, Length::Fixed(COL_DATE), false),
            ]
            .spacing(10),
        )
        .padding([4, 10])
        .style(filled(theme::c().panel))
        .into()
    }

    /// Virtualised list: only the rows that fit on screen are built.
    fn file_list(&self, side: usize, active: bool) -> Element<'_, Message> {
        let panel = self.panel(side);
        let list = responsive(move |size| {
            let rows = ((size.height / ROW_H).floor() as usize).max(1);
            let len = panel.entries.len();
            let start = panel.view_start(rows);
            let end = (start + rows).min(len);
            container(Column::with_children((start..end).map(|i| self.entry_row(side, i, active))))
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true)
                .style(filled(theme::c().panel))
                .into()
        });
        let mut area = mouse_area(list)
            .on_scroll(move |delta| {
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / ROW_H,
                };
                Message::Wheel(side, lines)
            })
            .on_right_press(Message::RightClickBackground(side));
        if self.dragging() {
            area = area.on_exit(Message::HoverEnd(side));
        }
        area.into()
    }

    fn entry_row(&self, side: usize, index: usize, active: bool) -> Element<'_, Message> {
        let panel = self.panel(side);
        let entry = &panel.entries[index];

        let color = if panel.is_selected(entry) {
            theme::c().selected
        } else if entry.is_dir() {
            theme::c().dir
        } else if entry.hidden {
            theme::c().muted
        } else {
            theme::c().text
        };
        let cell = |s: String, width: Length| text(s).size(13).color(color).wrapping(Wrapping::None).width(width);

        let (stem, ext) = entry.split_name();
        let size = match entry.kind {
            EntryKind::Parent => String::new(),
            EntryKind::Dir => match panel.dir_size(&entry.name) {
                Some(Some(bytes)) => format::size(bytes),
                Some(None) => "…".into(),
                None => "<DIR>".into(),
            },
            EntryKind::File => format::size(entry.size),
        };

        let icon: Element<'_, Message> = match self.icon(&entry.path, entry.is_dir()) {
            Some(handle) => image(handle)
                .width(ICON)
                .height(ICON)
                .opacity(if entry.hidden { 0.5_f32 } else { 1.0 })
                .into(),
            None => Space::new(ICON, ICON).into(),
        };

        // While renaming, the name and extension columns become one input.
        let renaming = self.rename.as_ref().filter(|r| r.side == side && r.src == entry.path);
        let mut content = Row::new().push(icon);
        content = match renaming {
            Some(r) => content.push(
                text_input("", &r.text)
                    .id(rename_input_id())
                    .on_input(Message::RenameInput)
                    .on_submit(Message::RenameSubmit)
                    .size(13)
                    .padding([1, 4])
                    .width(Length::Fill)
                    .style(theme::input),
            ),
            None => content
                .push(cell(stem.to_string(), Length::Fill))
                .push(cell(ext.to_string(), Length::Fixed(COL_EXT))),
        };
        let content = content
            .push(cell(size, Length::Fixed(COL_SIZE)).align_x(Alignment::End))
            .push(cell(format::date(&entry.modified), Length::Fixed(COL_DATE)).color(theme::c().muted))
            .spacing(10)
            .padding([0, 10])
            .height(ROW_H)
            .align_y(Alignment::Center);

        let drop_target = self.drag.as_ref().is_some_and(|d| {
            d.active && self.hover == Some((side, index)) && entry.is_dir() && !d.sources.contains(&entry.path)
        });
        let background = if drop_target {
            theme::c().drop
        } else if index == panel.cursor {
            if active { theme::c().cursor } else { theme::c().cursor_inactive }
        } else if index % 2 == 1 {
            theme::c().zebra
        } else {
            theme::c().panel
        };

        let mut area = mouse_area(
            container(content)
                .width(Length::Fill)
                .height(ROW_H)
                .clip(true)
                .style(filled(background)),
        )
        .on_press(Message::Click(side, index))
        .on_middle_press(Message::MiddleClick(side, index))
        .on_right_press(Message::RightPress(side, index));
        // Hover only matters for drop targets; skipping it otherwise avoids
        // a re-render every time the mouse crosses a row.
        if self.dragging() {
            area = area.on_enter(Message::Hover(side, index));
        }
        area.into()
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
                        text(current).size(11).color(theme::c().muted).wrapping(Wrapping::None),
                    ]
                    .width(Length::Fill)
                    .clip(true),
                    progress_bar(0.0..=1.0, p.fraction())
                        .width(Length::Fixed(220.0))
                        .height(Length::Fixed(6.0))
                        .style(theme::progress),
                    text(format!("{:>3.0}%", p.fraction() * 100.0)).size(12).width(Length::Fixed(40.0)),
                    text(speed).size(12).color(theme::c().muted).width(Length::Fixed(90.0)),
                    small_button(if paused { "Resume" } else { "Pause" }, Message::JobPause(job.id)),
                    small_button("Cancel", Message::JobCancel(job.id)),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .padding([2, 10])
            .height(JOB_ROW_H)
            .style(filled(theme::c().header))
            .into()
        }))
        .into()
    }

    fn dialog_view<'a>(&'a self, dialog: &'a Dialog) -> Element<'a, Message> {
        match dialog {
            Dialog::Hotlist { selected } => return self.hotlist_view(*selected),
            Dialog::Compare { opts } => return compare_view(opts),
            _ => {}
        }
        let input_field = |value: &'a str| {
            text_input("", value)
                .id(dialog_input_id())
                .on_input(Message::DialogInput)
                .on_submit(Message::DialogSubmit)
                .padding(8)
                .size(13)
                .style(theme::input)
        };

        let (title, body, field, ok): (String, String, Option<_>, &str) = match dialog {
            Dialog::Transfer { is_move, sources, input, .. } => {
                let verb = if *is_move { "Move" } else { "Copy" };
                (verb.into(), format!("{verb} {} to:", describe(sources)), Some(input_field(input)), verb)
            }
            Dialog::MakeDir { input } => (
                "New folder".into(),
                format!("Create folder in {}:", self.panel(self.active).path.display()),
                Some(input_field(input)),
                "Create",
            ),
            Dialog::EditNew { input } => (
                "Edit file".into(),
                format!("File to edit (created if missing) in {}:", self.panel(self.active).path.display()),
                Some(input_field(input)),
                "Edit",
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
            Dialog::Hotlist { .. } | Dialog::Compare { .. } => unreachable!("drawn above"),
        };

        let mut content = column![text(title).size(16), text(body).size(13).color(theme::c().muted)].spacing(12);
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

    fn hotlist_view(&self, selected: usize) -> Element<'_, Message> {
        let here = &self.panel(self.active).path;
        let row_button = |content: Row<'static, Message>, i: usize| {
            button(content)
                .on_press(Message::HotlistPick(i))
                .width(Length::Fill)
                .padding([4, 8])
                .style(theme::suggestion(selected == i))
        };
        let mut items = Column::new();
        for (i, path) in self.hotlist.iter().enumerate() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let key = if i < 9 { (i + 1).to_string() } else { String::new() };
            let mut content = Row::new().spacing(8).align_y(Alignment::Center);
            content = content.push(text(key).size(12).color(theme::c().muted).width(Length::Fixed(12.0)));
            if let Some(icon) = self.icon(path, true) {
                content = content.push(image(icon).width(ICON).height(ICON));
            }
            let color = if path == here { theme::c().selected } else { theme::c().text };
            content = content
                .push(text(name).size(13).color(color).wrapping(Wrapping::None))
                .push(text(elide_left(&path.display().to_string(), 60)).size(11).color(theme::c().muted).wrapping(Wrapping::None));
            items = items.push(row_button(content, i));
        }
        let n = self.hotlist.len();
        let toggle = if self.hotlist.contains(here) { "Remove current folder" } else { "Add current folder" };
        let toggle_row = Row::new()
            .spacing(8)
            .push(text("A").size(12).color(theme::c().muted).width(Length::Fixed(12.0)))
            .push(text(toggle).size(13));
        items = items.push(row_button(toggle_row, n));
        let empty = (n == 0).then(|| text("No folders yet.").size(13).color(theme::c().muted));
        let hint = text("Enter go \u{b7} 1-9 jump \u{b7} Del remove \u{b7} Esc close").size(11).color(theme::c().muted);
        let content = column![text("Hotlist").size(16)].push_maybe(empty).push(items).push(hint).spacing(10);
        container(content).padding(16).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }

    fn conflict_view<'a>(&'a self, id: u64, c: &'a Conflict) -> Element<'a, Message> {
        let name = c.dst.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let line = |label: &'a str, size: u64, date: &Option<_>| {
            row![
                text(label).size(13).color(theme::c().muted).width(Length::Fixed(80.0)),
                text(format::size(size)).size(13).width(Length::Fixed(90.0)),
                text(format::date(date)).size(13),
            ]
        };
        let answer = |label, r, primary| dialog_button(label, Message::Answer(id, r), primary);

        let content = column![
            text("File already exists").size(16),
            text(name).size(13).color(theme::c().selected),
            text(c.dst.display().to_string()).size(12).color(theme::c().muted),
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
            Some((msg, tone)) => text(msg.as_str()).size(12).color(match tone {
                Tone::Info => theme::c().text,
                Tone::Success => theme::c().success,
                Tone::Error => theme::c().error,
            }),
            None => text(
                "Type to find · Tab switch panel · Ctrl+T new tab · F2 rename · Drag to copy (Shift = move, \
                 out of the window = other apps) · Click path to edit · Right-click for menu",
            )
            .size(12)
            .color(theme::c().muted),
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
                container(row![text(key).size(12).color(theme::c().accent), text(label).size(12)].spacing(6))
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
            container(fkeys).padding(Padding { top: 0.0, right: 8.0, bottom: 8.0, left: 8.0 }),
        ]
        .into()
    }

    /// Roughly how many path characters fit in a panel's path bar.
    fn path_chars(&self) -> usize {
        ((self.window_width / 2.0 - 40.0) / 7.0).max(10.0) as usize
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

fn vline<'a>() -> Element<'a, Message> {
    container(Space::new(Length::Fixed(1.0), Length::Fill)).style(filled(theme::c().border)).into()
}

fn hline<'a>() -> Element<'a, Message> {
    container(Space::new(Length::Fill, Length::Fixed(1.0))).style(filled(theme::c().border)).into()
}

/// Ctrl+Shift+F2: compare options, toggled by click or their letter.
fn compare_view(opts: &CompareOptions) -> Element<'static, Message> {
    let option = |i: u8, key: &'static str, label: &'static str, hint: &'static str, on: bool| {
        let content = row![
            text(key).size(12).color(theme::c().muted).width(Length::Fixed(12.0)),
            checkbox("", on).on_toggle(move |_| Message::CompareToggle(i)).size(14),
            column![text(label).size(13), text(hint).size(11).color(theme::c().muted)].spacing(2),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        button(content).on_press(Message::CompareToggle(i)).width(Length::Fill).padding([4, 8]).style(theme::suggestion(false))
    };
    let content = column![
        text("Compare folders").size(16),
        option(0, "C", "Compare contents", "Same-size files are read; equal contents are never marked", opts.by_content),
        option(1, "D", "Ignore dates", "Differing files are marked on both sides", opts.ignore_date),
        option(2, "S", "Include subfolders", "Folders on both sides are marked if anything inside differs", opts.subfolders),
        row![
            Space::with_width(Length::Fill),
            dialog_button("Cancel", Message::DialogCancel, false),
            dialog_button("Compare", Message::DialogSubmit, true),
        ]
        .spacing(8),
    ]
    .spacing(10);
    container(content).padding(16).width(Length::Fixed(520.0)).style(theme::dialog).into()
}
