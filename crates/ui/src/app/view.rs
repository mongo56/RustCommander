//! Drawing. Everything here is a pure function of [`App`] state, apart from
//! the lazily filled icon and disk-space caches.

use super::*;
use crate::ring::Ring;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, center, checkbox, column, container, image, mouse_area, opaque, progress_bar, responsive, row,
    stack, text, text_input, tooltip, Column, Row, Space,
};
use iced::{Alignment, Element, Length, Padding, Vector};
use rc_core::format;

const ICON: f32 = 16.0;

/// Which optional columns fit a panel of a given width (narrow panels drop
/// the date first, then the extension (merged into the name), then the size).
#[derive(Clone, Copy)]
struct Columns {
    date: bool,
    ext: bool,
    size: bool,
}

impl Columns {
    /// Room left for the name column when these columns are shown.
    fn name_room(width: f32, widths: &[f32; 3], shown: [bool; 3]) -> f32 {
        let fixed = (0..3).filter(|&i| shown[i]).fold(0.0, |sum, i| sum + widths[i] + 10.0);
        width - 20.0 - ICON - 10.0 - fixed
    }

    fn for_width(width: f32, widths: &[f32; 3]) -> Self {
        let fits = |shown| Self::name_room(width, widths, shown) >= MIN_NAME;
        let size = width >= 260.0 && fits([false, true, false]);
        let ext = size && width >= 380.0 && fits([true, true, false]);
        let date = ext && width >= 480.0 && fits([true, true, true]);
        Self { date, ext, size }
    }

    fn shown(&self) -> [bool; 3] {
        [self.ext, self.size, self.date]
    }
}

impl App {
    pub fn view(&self) -> Element<'_, Message> {
        static FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        crate::startup::mark_once(&FIRST, "first view");
        let mut panels = Row::new().height(Length::Fill);
        if self.tree_visible() {
            panels = panels
                .push(container(self.tree_side_view()).width(Length::Fixed(self.tree_width())))
                .push(self.split_line(Message::Tree(hotlist_tree::TreeMsg::ResizePress)));
        }
        if !self.left_hidden() {
            panels = panels.push(container(self.side_view(0)).width(Length::Fixed(self.left_width()))).push(self.divider());
        }
        let panels = panels.push(self.side_view(1));

        let base: Element<'_, Message> = match &self.screen {
            Some(Screen::Help(h)) => self.help_view(h),
            Some(Screen::Settings(s)) => self.settings_view(s),
            Some(Screen::Connect(c)) => self.connect_view(c),
            Some(Screen::Find(f)) => self.find_view(f),
            Some(Screen::DiskUsage(d)) => self.disk_usage_view(d),
            Some(Screen::FileDiff(d)) => self.file_diff_view(d),
            Some(Screen::MultiRename(mr)) => self.multi_rename_view(mr),
            Some(Screen::Viewer(v)) => self.viewer_view(v),
            Some(Screen::Term(id)) => self.term_view(*id),
            None => container(column![panels, hline(), self.jobs_view(), self.bottom_bar()])
                .width(Length::Fill)
                .height(Length::Fill)
                .style(filled(theme::c().bg))
                .into(),
        };

        let base: Element<'_, Message> = if self.term_side_w() > 0.0 {
            row![base, self.term_side_view()].into()
        } else {
            base
        };

        let modal = self
            .jobs
            .iter()
            .find_map(|j| j.conflict.as_ref().map(|c| self.conflict_view(j.id, c)))
            .or_else(|| self.dialog.as_ref().map(|d| self.dialog_view(d)));

        // A divider being dragged with the software renderer.
        let base = match self.split_ghost {
            Some(x) => {
                let line = container(Space::new(Length::Fixed(2.0), Length::Fill)).style(filled(theme::c().accent));
                stack![base, row![Space::new(Length::Fixed((x - 1.0).max(0.0)), Length::Fill), line]].into()
            }
            None => base,
        };

        if let Some(card) = modal {
            return stack![base, opaque(center(card).style(theme::backdrop))].into();
        }
        if let Some(menu) = self.tree_menu_view() {
            return stack![base, menu].into();
        }
        if let Some(label) = self.drag_label() {
            return stack![base, label].into();
        }
        base
    }

    /// Width of the hotlist tree when shown: as set, kept narrow enough
    /// to leave both panels room.
    pub(super) fn tree_width(&self) -> f32 {
        let max = (self.area_width() - 2.0 * SPLIT_W - 2.0 * MIN_PANEL).max(MIN_TREE);
        self.tree.width.clamp(MIN_TREE, max).round()
    }

    /// Window x where the panels start (right of the hotlist tree and its
    /// divider when shown).
    pub(super) fn panels_x(&self) -> f32 {
        if self.tree_visible() { self.tree_width() + SPLIT_W } else { 0.0 }
    }

    /// Width of the left panel: the split share, kept within bounds so
    /// neither panel gets too narrow.
    pub(super) fn left_width(&self) -> f32 {
        if self.left_hidden() {
            return 0.0;
        }
        let room = (self.area_width() - self.panels_x() - SPLIT_W).max(1.0);
        let min = MIN_PANEL.min(room / 2.0);
        (room * self.split).clamp(min, room - min).round()
    }

    /// Moves the divider between the panels to window x.
    pub(super) fn drag_split_to(&mut self, x: f32) {
        let room = (self.area_width() - self.panels_x() - SPLIT_W).max(1.0);
        self.split = (x - self.panels_x() - SPLIT_W / 2.0) / room;
        self.split = self.left_width() / room;
    }

    /// Window x of the middle of the divider.
    pub(super) fn split_x(&self) -> f32 {
        if self.left_hidden() { self.panels_x() } else { self.panels_x() + self.left_width() + SPLIT_W / 2.0 }
    }

    pub(super) fn panel_width(&self, side: usize) -> f32 {
        let left = self.left_width();
        let split = if self.left_hidden() { 0.0 } else { SPLIT_W };
        if side == 0 { left } else { (self.area_width() - self.panels_x() - split - left).max(1.0) }
    }

    /// Draggable divider between the panels (double click: back to equal, see
    /// `Message::SplitPress`).
    fn divider(&self) -> Element<'_, Message> {
        self.split_line(Message::SplitPress)
    }

    /// A divider line that resizes what is left of it when dragged.
    fn split_line(&self, press: Message) -> Element<'_, Message> {
        let line = row![
            Space::new(Length::Fixed(2.0), Length::Fill),
            container(Space::new(Length::Fixed(1.0), Length::Fill)).style(filled(theme::c().border)),
            Space::new(Length::Fixed(2.0), Length::Fill),
        ];
        mouse_area(line)
            .on_press(press)
            .interaction(mouse::Interaction::ResizingHorizontally)
            .into()
    }

    /// Press on the edge in front of column `col`: start resizing it.
    pub(super) fn column_press(&mut self, side: usize, col: usize) {
        let shown = Columns::for_width(self.panel_width(side), &self.columns).shown();
        let visible: Vec<usize> = (0..3).filter(|&i| shown[i]).collect();
        let Some(edge) = visible.iter().position(|&i| i == col) else { return };
        self.column_drag = Some(ColumnDrag { side, visible, edge, x0: None, start: self.columns });
    }

    /// Mouse moved to window x while an edge is dragged: the edge follows
    /// the mouse (see `drag_edge`).
    pub(super) fn column_dragged(&mut self, x: f32) {
        let Some(mut d) = self.column_drag.take() else { return };
        let x0 = *d.x0.get_or_insert(x);
        let width = self.panel_width(d.side);
        let shown = Columns::for_width(width, &d.start).shown();
        let spare = (Columns::name_room(width, &d.start, shown) - MIN_NAME).max(0.0);
        self.columns = drag_edge(d.start, &d.visible, d.edge, x - x0, spare);
        self.column_drag = Some(d);
    }

    pub(super) fn dragging(&self) -> bool {
        self.drag.as_ref().is_some_and(|d| d.active)
    }

    /// Floating "Copy 3 items → target" label next to the mouse.
    fn drag_label(&self) -> Option<Element<'_, Message>> {
        let label = match self.tree_drag_text() {
            Some(label) => label,
            None => {
                let drag = self.drag.as_ref().filter(|d| d.active)?;
                let verb = if self.modifiers.shift() { "Move" } else { "Copy" };
                let what = describe(&drag.sources);
                match self.drop_target(drag) {
                    Some(target) => {
                        let name = target.file_name().map(|n| n.to_string_lossy().into_owned());
                        format!("{verb} {what} → {}", name.unwrap_or_else(|| target.display().to_string()))
                    }
                    None => format!("{verb} {what}"),
                }
            }
        };
        let tag = container(text(label).size(12)).padding([4, 8]).style(theme::drag_label);
        // Drawn at the cursor iced hands to `draw`, so it keeps up with the
        // mouse without a round trip through `update` and `view`.
        Some(crate::follow::Follow::new(tag, self.mouse, Vector::new(14.0, 16.0)).into())
    }

    fn side_view(&self, side: usize) -> Element<'_, Message> {
        let panel = self.panel(side);
        let active = self.panel_focused(side);
        let drop_here = self
            .drag
            .as_ref()
            .is_some_and(|d| d.active && self.side_under_mouse() == side && self.drop_target(d).is_some())
            || self.tree_drop_side() == Some(side);

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
        let toggle_side = usize::from(self.left_hidden());
        let toggle = (side == toggle_side && self.config.hotlist_tree).then(|| self.tree_button());
        let buttons = Row::with_children(toggle.into_iter().chain(self.drives.iter().map(|drive| {
            let current = panel.path.starts_with(drive.as_str());
            let letter = drive.trim_end_matches(['\\', '/', ':']).to_string();
            let letter = if letter.is_empty() { drive.clone() } else { letter.to_lowercase() };
            let mut content = Row::new().spacing(4).align_y(Alignment::Center);
            if let Some(icon) = self.icon(Path::new(drive), true) {
                content = content.push(image(icon).width(ICON).height(ICON));
            }
            content = content.push(text(letter).size(12));
            let button: Element<'_, Message> = mouse_area(
                button(content)
                    .on_press(Message::Drive(side, drive.clone()))
                    .padding([2, 6])
                    .style(theme::drive(current)),
            )
            .on_middle_press(Message::DriveMiddle(side, drive.clone()))
            .on_right_press(Message::DriveRight(side, drive.clone()))
            .into();
            match self.drive_labels.get(drive) {
                Some(label) => tooltip(
                    button,
                    container(text(label).size(12)).padding([3, 8]).style(theme::drag_label),
                    tooltip::Position::Bottom,
                )
                .into(),
                None => button,
            }
        })))
        .spacing(2);

        let width = self.panel_width(side);
        let space = self
            .disk_space(&panel.path)
            .map(|(free, total)| match width {
                w if w >= 420.0 => format!("{} free of {}", format::size(free), format::size(total)),
                w if w >= 300.0 => format!("{} free", format::size(free)),
                _ => String::new(),
            })
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
        let active = self.panel_focused(side);

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
                    Some(target) => text(elide_left(&format!("{}  (reading…)", rc_core::remote::display(target)), self.path_chars(side)))
                        .size(13)
                        .color(theme::c().muted)
                        .wrapping(Wrapping::None),
                    // Startup: the session's folder is still being read.
                    None if self.restoring(side) => text(elide_left(
                        &format!("{}  (reading…)", rc_core::remote::display(&panel.path)),
                        self.path_chars(side),
                    ))
                    .size(13)
                    .color(theme::c().muted)
                    .wrapping(Wrapping::None),
                    None => text(elide_left(
                        &match panel.results() {
                            Some(what) => format!("{}  \u{2014} search results: {what}", rc_core::remote::display(&panel.path)),
                            None => rc_core::remote::display(&panel.path),
                        },
                        self.path_chars(side),
                    ))
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
    /// The rows of a folder popup, scrolling when they don't fit the window
    /// (kept on the selection by [`App::popup_snap`]).
    fn popup_list<'a>(&self, items: Column<'a, Message>) -> Element<'a, Message> {
        container(scrollbar::thin(items).id(super::hotlist::popup_list_id()))
            .max_height((self.window_height - 200.0).max(120.0))
            .into()
    }

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
            .clip(true)
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
            .clip(true)
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
            .wrapping(Wrapping::None)
            .color(theme::c().muted),
        )
        .padding([4, 10])
        .width(Length::Fill)
        .clip(true)
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
            // Right-aligned by the container, not the text: iced_tiny_skia
            // 0.13 gets the redraw area of right-aligned text wrong, so the
            // label vanished when hovering recoloured it.
            let label = text(format!("{label}{arrow}")).size(12);
            let label = container(label).width(Length::Fill);
            let label = if right { label.align_x(Alignment::End) } else { label };
            button(label)
                .on_press(Message::Sort(side, key))
                .padding(0)
                .width(width)
                .style(theme::flat)
        };
        let cols = Columns::for_width(self.panel_width(side), &self.columns);
        // The 10 px gaps between the columns are the drag handles of the
        // column on their right.
        let handle = |col: usize| {
            let line = container(Space::new(Length::Fixed(1.0), Length::Fixed(12.0))).style(filled(theme::c().border));
            mouse_area(container(line).center_x(Length::Fixed(10.0)))
                .on_press(Message::ColumnPress(side, col))
                .interaction(mouse::Interaction::ResizingHorizontally)
        };
        let mut header = row![
            Space::with_width(ICON + 10.0),
            sort_button("Name", SortKey::Name, Length::Fill, false)
        ];
        if cols.ext {
            header = header.push(handle(0));
            header = header.push(sort_button("Ext", SortKey::Ext, Length::Fixed(self.columns[0]), false));
        }
        if cols.size {
            header = header.push(handle(1));
            header = header.push(sort_button("Size", SortKey::Size, Length::Fixed(self.columns[1]), true));
        }
        if cols.date {
            header = header.push(handle(2));
            header = header.push(sort_button("Modified", SortKey::Modified, Length::Fixed(self.columns[2]), false));
        }
        container(header.align_y(Alignment::Center).clip(true))
        .padding([4, 10])
        .style(filled(theme::c().panel))
        .into()
    }

    /// The panel's entries: the list, or the thumbnail grid.
    fn file_list(&self, side: usize, active: bool) -> Element<'_, Message> {
        let list = if self.panel(side).thumbnails { self.thumb_grid(side, active) } else { self.list_rows(side, active) };
        let mut area = mouse_area(list)
            .on_scroll(move |delta| {
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / ROW_H,
                };
                Message::Wheel(side, lines)
            })
            .on_right_press(Message::RightClickBackground(side))
            .on_press(Message::ClickBackground(side));
        if self.dragging() {
            area = area.on_exit(Message::HoverEnd(side));
        }
        area.into()
    }

    /// Virtualised list: only the rows that fit on screen are built.
    fn list_rows(&self, side: usize, active: bool) -> Element<'_, Message> {
        let panel = self.panel(side);
        responsive(move |size| {
            let rows = ((size.height / ROW_H).floor() as usize).max(1);
            let len = panel.entries.len();
            let start = panel.view_start(rows);
            let end = (start + rows).min(len);
            // Entry `i` goes in slot `i % rows` when the screen is full, so a
            // scroll step re-shapes only the rows that came into view (see
            // `ring`); a shorter list is just in order.
            let slots = end - start;
            let children = (0..slots).map(|k| self.entry_row(side, start + (k + slots - start % slots) % slots, active)).collect();
            let rows_view = container(Ring::new(children, ROW_H, start))
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true);
            let bar = scrollbar::attach(rows_view, start as f32, rows as f32, len as f32, move |at| {
                Message::ScrollTo(side, at.round() as usize, rows)
            });
            container(bar).width(Length::Fill).height(Length::Fill).style(filled(theme::c().panel)).into()
        })
        .into()
    }

    fn entry_row(&self, side: usize, index: usize, active: bool) -> Element<'_, Message> {
        let panel = self.panel(side);
        let entry = &panel.entries[index];
        let cols = Columns::for_width(self.panel_width(side), &self.columns);

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
        // Narrow panels ellipsize the name instead of just clipping it.
        let name_chars = (Columns::name_room(self.panel_width(side), &self.columns, cols.shown()) / 7.0).max(4.0) as usize;
        let narrow = !cols.date;
        let fit = |s: &str| if narrow { elide_end(s, name_chars) } else { s.to_string() };
        let size = match entry.kind {
            EntryKind::Parent => String::new(),
            EntryKind::Dir => match panel.dir_size(&entry.name) {
                Some(Some(bytes)) => format::size(bytes),
                Some(None) => "…".into(),
                None => "<DIR>".into(),
            },
            EntryKind::File => format::size(entry.size),
        };

        let icon: Element<'_, Message> = match self.entry_icon(entry) {
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
            Some(r) => content.push(SelectRange::new(
                text_input("", &r.text)
                    .id(rename_input_id())
                    .on_input(Message::RenameInput)
                    .on_submit(Message::RenameSubmit)
                    .size(13)
                    .padding([1, 4])
                    .width(Length::Fill)
                    .style(theme::input),
                rename_input_id(),
                r.select,
                r.selection(),
            )),
            None if cols.ext => content
                .push(cell(fit(stem), Length::Fill))
                .push(cell(ext.to_string(), Length::Fixed(self.columns[0]))),
            None => content.push(cell(fit(&entry.name), Length::Fill)),
        };
        if cols.size {
            // Aligned by a container (see `column_header`).
            let size = container(cell(size, Length::Shrink)).width(Length::Fixed(self.columns[1])).align_x(Alignment::End);
            content = content.push(size);
        }
        if cols.date {
            content = content.push(cell(format::date(&entry.modified), Length::Fixed(self.columns[2])).color(theme::c().muted));
        }
        let content = content
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

    /// The editor of a hotlist folder's display name and comment.
    fn hot_edit_view<'a>(&'a self, dir: &'a str, name: &'a str, comment: &'a str, focus: usize) -> Element<'a, Message> {
        let field = |hint: &'static str, value: &'a str, id: iced::widget::text_input::Id, on_input: fn(String) -> Message| {
            text_input(hint, value)
                .id(id)
                .on_input(on_input)
                .on_submit(Message::DialogEnter)
                .padding(8)
                .size(13)
                .style(theme::input)
        };
        let label = |s: &'static str| text(s).size(12).color(theme::c().muted);
        let content = column![
            text("Hotlist entry").size(16),
            text(dir).size(13).color(theme::c().muted),
            label("Display name (empty: the folder's own name)"),
            field("Name", name, dialog_input_id(), Message::DialogInput),
            label("Comment (shown as a tooltip)"),
            field("Comment", comment, dialog_comment_id(), Message::DialogComment),
            row![
                text("Tab switches boxes").size(11).color(theme::c().muted),
                Space::with_width(Length::Fill),
                dialog_button("Cancel", Message::DialogCancel, false, focus == 0),
                dialog_button("OK", Message::DialogSubmit, true, focus == 1),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(8);
        container(content).padding(20).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }

    fn dialog_view<'a>(&'a self, dialog: &'a Dialog) -> Element<'a, Message> {
        match dialog {
            Dialog::Hotlist(menu) => return self.hotlist_view(menu),
            Dialog::History { selected } => return self.history_view(*selected),
            Dialog::Palette(palette) => return self.palette_view(palette),
            Dialog::Compare { opts } => return compare_view(opts, self.dialog_focus.unwrap_or(1)),
            Dialog::Update(d) => return self.update_view(d),
            Dialog::Checksum(d) => return self.checksum_view(d),
            Dialog::Attrib(d) => return self.attrib_view(d),
            Dialog::HotEdit { dir, name, comment, .. } => {
                return self.hot_edit_view(dir, name, comment, self.dialog_focus.unwrap_or(1));
            }
            _ => {}
        }
        let input_field = |value: &'a str| {
            text_input("", value)
                .id(dialog_input_id())
                .on_input(Message::DialogInput)
                .on_submit(Message::DialogEnter)
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
                "Create folder in:".into(),
                Some(input_field(input)),
                "Create",
            ),
            Dialog::Pack { sources, input, .. } => (
                "Pack".into(),
                format!("Pack {} into the zip archive:", describe(sources)),
                Some(input_field(input)),
                "Pack",
            ),
            Dialog::Split { src, size, input, .. } => (
                "Split".into(),
                format!(
                    "Split {} ({}) into parts in the other panel's folder. Part size (a plain number is in MB; units B, KB, MB, GB):",
                    src.file_name().unwrap_or_default().to_string_lossy(),
                    rc_core::format::size(*size)
                ),
                Some(input_field(input)),
                "Split",
            ),
            Dialog::Unpack { archives, input, .. } => (
                "Unpack".into(),
                format!("Unpack {} to:", describe(archives)),
                Some(input_field(input)),
                "Unpack",
            ),
            Dialog::Password { archive, input, .. } => (
                "Password".into(),
                format!("{} is encrypted. Password:", archive.file_name().unwrap_or_default().to_string_lossy()),
                Some(input_field(input).secure(true)),
                "OK",
            ),
            Dialog::EditNew { input } => (
                "Edit file".into(),
                "File to edit (created if missing) in:".into(),
                Some(input_field(input)),
                "Edit",
            ),
            Dialog::Delete { paths, .. } if paths.first().is_some_and(|p| rc_core::vfs::is_virtual(p)) => (
                "Delete".into(),
                format!(
                    "Delete {} from the {}? This cannot be undone.",
                    describe(paths),
                    if paths.first().is_some_and(|p| rc_core::remote::is_remote(p)) { "server" } else { "archive" }
                ),
                None,
                "Delete",
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
            Dialog::CloseTerm { program, .. } => (
                "Close terminal".into(),
                format!("{program} is still running in this terminal. Close the terminal anyway?"),
                None,
                "Close",
            ),
            Dialog::QuitTerms => ("Quit".into(), self.quit_text(), None, "Quit"),
            Dialog::RemoveGroup { name, count, .. } => (
                "Remove group".into(),
                format!(
                    "Remove the group \"{name}\" and the {} in it from the hotlist? The folders themselves stay.",
                    if *count == 1 { "entry".to_owned() } else { format!("{count} entries") }
                ),
                None,
                "Remove",
            ),
            Dialog::Hotlist(_) | Dialog::History { .. } | Dialog::Palette(_) | Dialog::Compare { .. } | Dialog::Update(_) | Dialog::Checksum(_) | Dialog::Attrib(_) | Dialog::HotEdit { .. } => {
                unreachable!("drawn above")
            }
        };

        // The folder a new item goes into: on its own line, its start elided
        // so a long path stays on one line and its end (the folder) shows.
        let place = match dialog {
            Dialog::MakeDir { .. } | Dialog::EditNew { .. } => {
                Some(rc_core::remote::display(&self.panel(self.active).path).to_string())
            }
            _ => None,
        };

        let focus = self.dialog_focus.unwrap_or(1);
        let mut content = column![text(title).size(16), text(body).size(13).color(theme::c().muted)].spacing(12);
        if let Some(place) = place {
            let line = text(elide_path(&place, DIALOG_PATH_CHARS)).size(13).wrapping(Wrapping::None);
            content = content.push(container(line).width(Length::Fill).clip(true));
        }
        if let Some(field) = field {
            content = content.push(field);
        }
        if let Dialog::Split { .. } = dialog {
            let mut presets = row![text("Presets (Up/Down)").size(12).color(theme::c().muted), Space::with_width(Length::Fill)].spacing(6);
            for (label, _) in rc_core::split::PRESETS {
                let value = label.split(" (").next().unwrap_or_default().to_string();
                presets = presets.push(small_button(label, Message::DialogInput(value)));
            }
            content = content.push(presets.align_y(Alignment::Center));
        }
        content = content.push(
            row![
                Space::with_width(Length::Fill),
                dialog_button("Cancel", Message::DialogCancel, false, focus == 0),
                dialog_button(ok, Message::DialogSubmit, true, focus == 1),
            ]
            .spacing(8),
        );

        container(content).padding(20).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }

    /// A row of the folder popups: digit key, icon, name, dimmed detail (its
    /// start elided so the row stays on one line).
    fn popup_row(
        &self,
        i: usize,
        icon: Option<&Path>,
        name: String,
        color: iced::Color,
        detail: String,
    ) -> Row<'static, Message> {
        let key = if i < 9 { (i + 1).to_string() } else { String::new() };
        let mut content = Row::new().spacing(8).align_y(Alignment::Center);
        content = content.push(text(key).size(12).color(theme::c().muted).width(Length::Fixed(12.0)));
        match icon.and_then(|path| self.icon(path, true)) {
            Some(icon) => content = content.push(image(icon).width(ICON).height(ICON)),
            None => content = content.push(Space::with_width(ICON)),
        }
        let detail = elide_left(&detail, 66usize.saturating_sub(name.chars().count() * 6 / 5).max(16));
        content
            .push(text(name).size(13).color(color).wrapping(Wrapping::None))
            .push(text(detail).size(11).color(theme::c().muted).wrapping(Wrapping::None))
    }

    fn hotlist_view(&self, menu: &hotlist::HotlistMenu) -> Element<'_, Message> {
        use crate::hotlist::{self as hl, HotItem};
        let selected = menu.selected;
        let here = &self.panel(self.active).path;
        let entries = hl::menu(&self.hotlist, &menu.at);
        let row_button = |content: Row<'static, Message>, i: usize| {
            mouse_area(
                button(content)
                    .on_press(Message::HotlistPick(i))
                    .width(Length::Fill)
                    .padding([4, 8])
                    .style(theme::suggestion(selected == i)),
            )
            .on_middle_press(Message::HotlistNewTab(i))
        };
        let mut items = Column::new();
        let new_name = menu.naming.as_ref().filter(|_| !menu.renaming);
        for (i, item) in entries.iter().enumerate() {
            if i == selected {
                items = items.push_maybe(new_name.map(|typed| new_submenu_row(typed)));
            }
            let content = match item {
                HotItem::Folder { path, .. } => {
                    let color = if path == here { theme::c().selected } else { theme::c().text };
                    self.popup_row(i, Some(path), item.label(), color, path.display().to_string())
                }
                HotItem::Menu { name, items, .. } => {
                    let name = match &menu.naming {
                        Some(typed) if menu.renaming && i == selected => format!("{typed}_"),
                        _ => format!("{name}  \u{203a}"),
                    };
                    let count = match items.len() {
                        1 => "1 entry".to_owned(),
                        n => format!("{n} entries"),
                    };
                    self.popup_row(i, None, name, theme::c().text, count)
                }
            };
            items = items.push(match item.tip() {
                Some(tip) => Element::from(
                    tooltip(
                        row_button(content, i),
                        container(text(tip).size(12)).padding([3, 8]).max_width(420).style(theme::drag_label),
                        tooltip::Position::Bottom,
                    )
                    .gap(2),
                ),
                None => row_button(content, i).into(),
            });
        }
        let n = entries.len();
        if selected >= n {
            items = items.push_maybe(new_name.map(|typed| new_submenu_row(typed)));
        }
        let in_menu = entries.iter().any(|it| it.folder() == Some(here.as_path()));
        let toggle = if in_menu { "Remove current folder" } else { "Add current folder" };
        let toggle_row = Row::new()
            .spacing(8)
            .push(text("A").size(12).color(theme::c().muted).width(Length::Fixed(12.0)))
            .push(text(toggle).size(13));
        items = items.push(row_button(toggle_row, n));
        let empty = (n == 0).then(|| text("No folders yet.").size(13).color(theme::c().muted));
        let hint = if menu.naming.is_some() {
            "Type the name \u{b7} Enter OK \u{b7} Esc cancel"
        } else {
            "Enter go \u{b7} Shift+Enter new tab \u{b7} 1-9 jump \u{b7} N submenu \u{b7} Ctrl+arrows move \u{b7} Del remove"
        };
        let hint = text(hint).size(11).color(theme::c().muted);
        // Title: the submenu path, with a way back for the mouse.
        let mut title = String::from("Hotlist");
        let mut level = self.hotlist.as_slice();
        for &i in &menu.at {
            if let Some(HotItem::Menu { name, items, .. }) = level.get(i) {
                title.push_str(&format!(" \u{203a} {name}"));
                level = items;
            }
        }
        let mut header = Row::new().spacing(8).align_y(Alignment::Center).push(text(title).size(16).width(Length::Fill));
        if !menu.at.is_empty() {
            header = header.push(small_button("Back", Message::HotlistBack));
        }
        let content = column![header].push_maybe(empty).push(self.popup_list(items)).push(hint).spacing(10);
        container(content).padding(16).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }

    fn history_view(&self, selected: usize) -> Element<'_, Message> {
        let (list, current) = self.panel(self.active).history();
        let mut items = Column::new();
        for (i, path) in list.iter().enumerate() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let (name, color) = if i == current {
                (format!("{name}  (current)"), theme::c().selected)
            } else {
                (name, theme::c().text)
            };
            let content = self.popup_row(i, Some(path), name, color, path.display().to_string());
            items = items.push(
                button(content)
                    .on_press(Message::HistoryPick(i))
                    .width(Length::Fill)
                    .padding([4, 8])
                    .style(theme::suggestion(selected == i)),
            );
        }
        let hint = text("Most recent first \u{b7} Enter go \u{b7} 1-9 jump \u{b7} Esc close").size(11).color(theme::c().muted);
        let title = format!("Folder history ({} of {})", selected + 1, list.len());
        let content = column![text(title).size(16), self.popup_list(items), hint].spacing(10);
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
        let focus = self.conflict_focus;
        let answers = button_focus::conflict_answers(c.resumable);
        let answer = |i: usize, primary| {
            let (label, r) = answers[i];
            dialog_button(label, Message::Answer(id, r), primary, focus == Some(i))
        };
        // Overwrite..., then Resume (when offered), then Skip... and Cancel.
        let n = answers.len();
        let resume = c.resumable.then(|| {
            row![
                answer(3, false),
                answer(4, false),
                text("keep the existing part, copy the rest").size(12).color(theme::c().muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        });

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
                answer(0, true),
                answer(1, false),
                answer(2, false),
            ]
            .spacing(8),
        ]
        .push_maybe(resume)
        .push(
            row![
                answer(n - 3, false),
                answer(n - 2, false),
                Space::with_width(Length::Fill),
                answer(n - 1, false),
            ]
            .spacing(8),
        )
        .spacing(12);

        container(content).padding(20).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }

    fn bottom_bar(&self) -> Element<'_, Message> {
        const CHAR_W: f32 = 6.6; // rough width of a size-12 character
        // Hint on the right (only without the function-key bar): full, keys only, or hidden as the window narrows.
        let hint = if self.config.show_fkeys {
            String::new()
        } else {
            let full = self.keymap.bar_hint();
            let short = self.keymap.bar_hint_short();
            let need = |s: &str| s.chars().count() as f32 * CHAR_W + 20.0;
            if self.window_width >= need(&full) * 2.2 {
                full
            } else if self.window_width >= need(&short) * 2.5 {
                short
            } else {
                String::new()
            }
        };
        let hint_w = if hint.is_empty() { 0.0 } else { hint.chars().count() as f32 * CHAR_W + 16.0 };
        let room = ((self.window_width - 20.0 - hint_w) / CHAR_W).max(0.0) as usize;
        let ellipsize = |msg: &str| -> String {
            if msg.chars().count() <= room {
                msg.to_string()
            } else {
                let cut: String = msg.chars().take(room.saturating_sub(1)).collect();
                format!("{}…", cut.trim_end())
            }
        };
        let line = self.status_line();
        let status = match &line {
            Some((msg, tone)) => text(ellipsize(&msg.replace('\n', " "))).size(12).color(match tone {
                Tone::Info => theme::c().text,
                Tone::Success => theme::c().success,
                Tone::Error => theme::c().error,
            }),
            None => text(""),
        };

        if !self.config.show_fkeys {
            // Just a status line with a quiet hint on the right.
            let hint = text(hint).size(12).color(theme::c().muted).wrapping(Wrapping::None);
            let status = self.status_clickable(container(status.wrapping(Wrapping::None)).clip(true).into());
            return container(row![status, Space::with_width(Length::Fill), hint].spacing(16))
                .padding([3, 10])
                .width(Length::Fill)
                .clip(true)
                .into();
        }

        let fkeys = Row::with_children(self.keymap.fkey_bar().into_iter().map(|(key, cmd)| {
            let label = cmd.map_or("", Command::short_label);
            button(
                container(row![text(key).size(12).color(theme::c().accent), text(label).size(12)].spacing(6))
                    .center_x(Length::Fill),
            )
            .on_press_maybe(cmd.map(Message::Run))
            .padding([4, 10])
            .width(Length::Fill)
            .style(theme::fkey)
            .into()
        }))
        .spacing(6);

        column![
            self.status_clickable(container(status.wrapping(Wrapping::None)).padding([3, 10]).clip(true).into()),
            container(fkeys).padding(Padding { top: 0.0, right: 8.0, bottom: 8.0, left: 8.0 }),
        ]
        .into()
    }

    /// Roughly how many path characters fit in a panel's path bar.
    fn path_chars(&self, side: usize) -> usize {
        ((self.panel_width(side) - 40.0) / 7.0).max(10.0) as usize
    }
}

/// The name of a hotlist submenu being created, where it will be inserted.
fn new_submenu_row(typed: &str) -> Element<'static, Message> {
    let field = Row::new()
        .spacing(8)
        .push(text("N").size(12).color(theme::c().muted).width(Length::Fixed(12.0)))
        .push(text(format!("New submenu: {typed}_")).size(13).color(theme::c().selected));
    container(field).padding([4, 8]).into()
}

/// Keeps the end of a long path: `…\sandbox\B`.
pub(super) fn elide_end(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

pub(super) fn elide_left(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count - max_chars + 1).collect();
    format!("…{tail}")
}

/// About how many characters of a path fit on one line of a 520 px dialog.
const DIALOG_PATH_CHARS: usize = 64;

/// `path` cut to `max_chars` from the start, at a separator when one is
/// left: `…\deep\folder`. A last component longer than that is elided too.
fn elide_path(path: &str, max_chars: usize) -> String {
    let short = elide_left(path, max_chars);
    if short.len() == path.len() {
        return short;
    }
    let tail = &short["…".len()..];
    match tail.find(['\\', '/']) {
        Some(i) if i + 1 < tail.len() => format!("…{}", &tail[i..]),
        _ => short,
    }
}

pub(super) fn dialog_button(label: &str, message: Message, primary: bool, focused: bool) -> Element<'_, Message> {
    let b = button(text(label).size(13)).on_press(message).padding([6, 14]);
    if focused {
        b.style(theme::focused(primary)).into()
    } else {
        b.style(if primary { theme::primary } else { theme::fkey }).into()
    }
}

fn small_button(label: &str, message: Message) -> Element<'_, Message> {
    button(text(label).size(12)).on_press(message).padding([2, 10]).style(theme::fkey).into()
}

fn hline<'a>() -> Element<'a, Message> {
    container(Space::new(Length::Fill, Length::Fixed(1.0))).style(filled(theme::c().border)).into()
}

/// Ctrl+Shift+F2: compare options, toggled by click or their letter.
fn compare_view(opts: &CompareOptions, focus: usize) -> Element<'static, Message> {
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
            dialog_button("Cancel", Message::DialogCancel, false, focus == 0),
            dialog_button("Compare", Message::DialogSubmit, true, focus == 1),
        ]
        .spacing(8),
    ]
    .spacing(10);
    container(content).padding(16).width(Length::Fixed(520.0)).style(theme::dialog).into()
}

/// Widths of the fixed columns after the edge in front of `visible[edge]`
/// moved `dx` from where it was pressed (positive: right), from the widths
/// `start` it had then. Moving right shrinks the columns right of the edge,
/// nearest first, each down to its minimum, and the column left of it grows
/// (the name column for the first edge, or once the fixed one is at
/// `MAX_COLUMN`). Moving left grows `visible[edge]` and shrinks the columns
/// left of the edge, nearest first, and then the name column, by at most
/// `name_spare`. So the name column can be widened past the point where
/// the extension column is at its minimum, and narrowed again.
fn drag_edge(start: [f32; 3], visible: &[usize], edge: usize, dx: f32, name_spare: f32) -> [f32; 3] {
    let mut w = start;
    let spare = |w: &[f32; 3], c: usize| (w[c] - MIN_COLUMNS[c]).max(0.0);
    let dx = dx.round();
    if dx > 0.0 {
        let mut left = dx;
        for &c in &visible[edge..] {
            let take = left.min(spare(&w, c));
            w[c] -= take;
            left -= take;
        }
        if let Some(&p) = edge.checked_sub(1).and_then(|i| visible.get(i)) {
            w[p] = (w[p] + dx - left).min(MAX_COLUMN.max(start[p]));
        }
    } else if dx < 0.0 {
        let col = visible[edge];
        let room: f32 = visible[..edge].iter().map(|&c| spare(&w, c)).sum::<f32>() + name_spare;
        let mut left = (-dx).min(room).min((MAX_COLUMN - w[col]).max(0.0));
        w[col] += left;
        for &c in visible[..edge].iter().rev() {
            let take = left.min(spare(&w, c));
            w[c] -= take;
            left -= take;
        }
    }
    w
}

#[cfg(test)]
mod drag_edge_tests {
    use super::*;

    const ALL: [usize; 3] = [0, 1, 2];

    #[test]
    fn long_paths_are_cut_at_a_separator() {
        assert_eq!(elide_path(r"C:\short", 20), r"C:\short");
        assert_eq!(elide_path(r"C:\Users\someone\Documents\deep\folder", 16), r"…\deep\folder");
        // Nothing to cut at: plain elision.
        assert_eq!(elide_path(r"C:\averyveryverylongname", 10), "…ylongname");
        assert!(elide_path(r"C:\a\bcdefghijkl", 8).chars().count() <= 8);
    }

    #[test]
    fn widening_the_name_column_takes_from_every_column_on_the_right() {
        let start = [60.0, 80.0, 120.0];
        // Ext gives 30, size 30, date 50.
        assert_eq!(drag_edge(start, &ALL, 0, 110.0, 0.0), [30.0, 50.0, 70.0]);
        // Further than all can give: they stay at their minimum.
        assert_eq!(drag_edge(start, &ALL, 0, 500.0, 0.0), MIN_COLUMNS);
    }

    #[test]
    fn narrowing_the_name_column_is_limited_by_its_spare_room() {
        let start = [60.0, 80.0, 120.0];
        assert_eq!(drag_edge(start, &ALL, 0, -40.0, 100.0), [100.0, 80.0, 120.0]);
        assert_eq!(drag_edge(start, &ALL, 0, -40.0, 25.0), [85.0, 80.0, 120.0]);
    }

    #[test]
    fn a_middle_edge_pushes_on_and_then_takes_from_the_name_column() {
        let start = [60.0, 80.0, 120.0];
        // Right: size gives 30, then date 20; ext grows by both.
        assert_eq!(drag_edge(start, &ALL, 1, 50.0, 0.0), [110.0, 50.0, 100.0]);
        // Left: ext gives 30, then the name column 10.
        assert_eq!(drag_edge(start, &ALL, 1, -40.0, 50.0), [30.0, 120.0, 120.0]);
    }

    #[test]
    fn only_visible_columns_change() {
        let start = [60.0, 80.0, 120.0];
        assert_eq!(drag_edge(start, &[0, 1], 0, 100.0, 0.0), [30.0, 50.0, 120.0]);
    }
}
