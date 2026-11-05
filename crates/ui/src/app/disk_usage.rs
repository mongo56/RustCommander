//! Disk usage (Alt+Shift+F7): fills the window while open. Scans the folder
//! under the cursor (or the panel's folder) on a worker thread
//! (`rc_core::disk_usage::scan`) and shows the whole tree as an outline,
//! largest first, with each item's share of its folder. Ctrl+Enter shows the
//! item in the panel. Closing keeps the tree, so reopening the same folder is
//! instant (F5 scans again). Files are never opened from here.

use super::*;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, mouse_area, responsive, row, text, Column, Row, Space};
use iced::{Alignment, Element, Length};
use rc_core::disk_usage::{self, NodeKind, Tree, ROOT};
use rc_core::format;
use rc_core::panel::visible_start;

const INDENT: f32 = 16.0;
const ARROW_W: f32 = 14.0;
const COL_SIZE: f32 = 90.0;
const COL_PCT: f32 = 44.0;
const BAR_W: f32 = 70.0;
const COL_FILES: f32 = 90.0;
/// Height of the header, list header and footer (for paging estimates).
const DU_CHROME_H: f32 = 140.0;

/// A visible row of the outline: node index and depth (root = 0).
pub(super) type Line = (u32, u16);

/// Which folders are expanded, and the rows that makes visible. The rows
/// change only on expand / collapse / rescan, never while drawing.
#[derive(Debug, Clone)]
pub(super) struct Outline {
    /// Indexed by node.
    expanded: Vec<bool>,
    rows: Vec<Line>,
}

/// Appends the visible rows under `node` (not `node` itself), at `depth`.
fn push_children(tree: &Tree, expanded: &[bool], node: u32, depth: u16, out: &mut Vec<Line>) {
    let mut stack: Vec<Line> = tree.node(node).children.iter().rev().map(|&c| (c, depth)).collect();
    while let Some((n, d)) = stack.pop() {
        out.push((n, d));
        if expanded[n as usize] {
            stack.extend(tree.node(n).children.iter().rev().map(|&c| (c, d.saturating_add(1))));
        }
    }
}

/// All visible rows of `tree`: the root, then what its expanded folders show.
fn flatten(tree: &Tree, expanded: &[bool]) -> Vec<Line> {
    let mut rows = vec![(ROOT, 0)];
    if expanded[ROOT as usize] {
        push_children(tree, expanded, ROOT, 1, &mut rows);
    }
    rows
}

impl Outline {
    /// The root expanded, nothing else.
    fn new(tree: &Tree) -> Self {
        let mut expanded = vec![false; tree.nodes.len()];
        expanded[ROOT as usize] = true;
        let rows = flatten(tree, &expanded);
        Self { expanded, rows }
    }

    fn is_expanded(&self, node: u32) -> bool {
        self.expanded[node as usize]
    }

    /// Expands the folder on `row`; returns the rows added (0 if it was
    /// open already or has nothing in it). Folders under it that were open
    /// before show open again.
    fn expand(&mut self, tree: &Tree, row: usize) -> usize {
        let Some(&(node, depth)) = self.rows.get(row) else { return 0 };
        if self.expanded[node as usize] || tree.node(node).children.is_empty() {
            return 0;
        }
        self.expanded[node as usize] = true;
        let mut sub = Vec::new();
        push_children(tree, &self.expanded, node, depth.saturating_add(1), &mut sub);
        let added = sub.len();
        self.rows.splice(row + 1..row + 1, sub);
        added
    }

    /// Collapses the folder on `row`; returns the rows removed.
    fn collapse(&mut self, row: usize) -> usize {
        let Some(&(node, depth)) = self.rows.get(row) else { return 0 };
        if !self.expanded[node as usize] {
            return 0;
        }
        self.expanded[node as usize] = false;
        let end = self.rows[row + 1..].iter().position(|&(_, d)| d <= depth).map_or(self.rows.len(), |p| row + 1 + p);
        self.rows.drain(row + 1..end);
        end - row - 1
    }

    /// Expands or collapses `row`; returns where the cursor (at `cursor`)
    /// goes so it stays on the same item, or on `row` if that got hidden.
    fn toggle_at(&mut self, tree: &Tree, row: usize, cursor: usize) -> usize {
        let Some(&(node, _)) = self.rows.get(row) else { return cursor };
        if self.is_expanded(node) {
            let removed = self.collapse(row);
            match cursor {
                c if c <= row => c,
                c if c <= row + removed => row,
                c => c - removed,
            }
        } else {
            let added = self.expand(tree, row);
            if cursor > row { cursor + added } else { cursor }
        }
    }

    /// The row of the folder that holds the item on `row`.
    fn parent_row(&self, row: usize) -> Option<usize> {
        let &(_, depth) = self.rows.get(row)?;
        if depth == 0 {
            return None;
        }
        self.rows[..row].iter().rposition(|&(_, d)| d < depth)
    }

    /// Right / +: expands a closed folder; on an open one, goes to its first
    /// item. Returns the new cursor.
    fn right(&mut self, tree: &Tree, cursor: usize, step_in: bool) -> usize {
        let Some(&(node, _)) = self.rows.get(cursor) else { return cursor };
        if tree.node(node).children.is_empty() {
            cursor
        } else if !self.is_expanded(node) {
            self.expand(tree, cursor);
            cursor
        } else if step_in {
            cursor + 1
        } else {
            cursor
        }
    }

    /// Left / -: collapses an open folder, else goes to the folder the item
    /// is in. Returns the new cursor.
    fn left(&mut self, cursor: usize, step_out: bool) -> usize {
        let Some(&(node, _)) = self.rows.get(cursor) else { return cursor };
        if self.is_expanded(node) {
            self.collapse(cursor);
            cursor
        } else if step_out {
            self.parent_row(cursor).unwrap_or(cursor)
        } else {
            cursor
        }
    }
}

/// A finished scan.
pub(super) struct Scanned {
    tree: Arc<Tree>,
    outline: Outline,
    took: Duration,
}

/// The scanned tree, as sent from the worker (no `Debug` dump of millions
/// of nodes).
#[derive(Clone)]
pub struct TreeBox(Arc<Tree>);

impl std::fmt::Debug for TreeBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tree({} nodes)", self.0.nodes.len())
    }
}

#[derive(Debug, Clone)]
pub enum DuEvent {
    Progress(disk_usage::Progress),
    /// `None`: cancelled.
    Done(Option<TreeBox>, Duration),
}

#[derive(Debug, Clone)]
pub enum DuMsg {
    Event(u64, DuEvent),
    Click(usize),
    /// Click on a folder's arrow.
    Toggle(usize),
    Wheel(f32),
    /// Scrollbar: first row to show, rows on screen.
    ScrollTo(usize, usize),
    GoTo,
    Rescan,
    Close,
}

pub(super) struct DiskUsage {
    /// Panel the tool was opened from (Ctrl+Enter navigates it).
    side: usize,
    root: PathBuf,
    scanned: Option<Scanned>,
    /// Cancel flag of the running scan.
    running: Option<Arc<AtomicBool>>,
    seq: u64,
    progress: Option<disk_usage::Progress>,
    cursor: usize,
    top: usize,
    last_click: Option<(usize, Instant)>,
}

impl DiskUsage {
    fn stop(&mut self) {
        if let Some(cancel) = self.running.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn len(&self) -> usize {
        self.scanned.as_ref().map_or(0, |s| s.outline.rows.len())
    }
}

/// "12 %" share of `size` in `parent`, as a fraction.
fn share(size: u64, parent: u64) -> f32 {
    if parent == 0 { 0.0 } else { (size as f64 / parent as f64) as f32 }
}

fn header_text(tree: &Tree, took: Duration) -> String {
    let root = tree.root();
    let mut t = format!(
        "{} \u{b7} {} files \u{b7} {} folders",
        format::size(root.size),
        root.files,
        root.dirs
    );
    if tree.errors > 0 {
        t.push_str(&format!(" \u{b7} {} unreadable", tree.errors));
    }
    t.push_str(&format!(" \u{b7} scanned in {:.1} s", took.as_secs_f32()));
    t
}

impl App {
    pub(super) fn open_disk_usage(&mut self) -> Task<Message> {
        let side = self.active;
        self.leave_edits();
        let panel = self.panel(side);
        let root = match panel.current() {
            Some(e) if e.kind == EntryKind::Dir => e.path.clone(),
            _ => panel.path.clone(),
        };
        if remote::is_remote(&root) || vfs::in_archive(&root) || vfs::is_virtual(&root) {
            self.info("Disk usage works on local folders only");
            return Task::none();
        }
        let last = self.disk_usage_last.take().filter(|d| d.root == root && d.scanned.is_some());
        let reuse = last.is_some();
        let mut du = last.unwrap_or(DiskUsage {
            side,
            root,
            scanned: None,
            running: None,
            seq: 0,
            progress: None,
            cursor: 0,
            top: 0,
            last_click: None,
        });
        du.side = side;
        self.screen = Some(Screen::DiskUsage(Box::new(du)));
        if reuse { Task::none() } else { self.start_disk_usage() }
    }

    /// Closes the tool (cancelling its scan), keeping a finished tree for the
    /// next open.
    fn close_disk_usage(&mut self) {
        if let Some(mut d) = self.screen.take_disk_usage() {
            d.stop();
            self.disk_usage_last = d.scanned.is_some().then_some(d);
        }
    }

    fn start_disk_usage(&mut self) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        d.stop();
        let cancel = Arc::new(AtomicBool::new(false));
        d.running = Some(cancel.clone());
        d.seq = seq;
        d.scanned = None;
        d.progress = None;
        d.cursor = 0;
        d.top = 0;
        d.last_click = None;
        let root = d.root.clone();
        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let start = Instant::now();
            let tree = disk_usage::scan(&root, &cancel, |p| {
                let _ = tx.unbounded_send(DuEvent::Progress(p));
            });
            let _ = tx.unbounded_send(DuEvent::Done(tree.map(|t| TreeBox(Arc::new(t))), start.elapsed()));
        });
        Task::run(rx, move |e| Message::DiskUsage(DuMsg::Event(seq, e)))
    }

    fn du_rows(&self) -> usize {
        ((self.window_height - DU_CHROME_H) / ROW_H).floor().max(1.0) as usize
    }

    fn du_move(&mut self, delta: isize) {
        let rows = self.du_rows();
        let Some(d) = self.screen.disk_usage_mut() else { return };
        let len = d.len();
        d.cursor = d.cursor.saturating_add_signed(delta).min(len.saturating_sub(1));
        d.top = visible_start(d.top, d.cursor, rows, len);
    }

    /// Runs `f` on the outline with the cursor, and keeps the cursor on screen.
    fn du_outline(&mut self, f: impl FnOnce(&mut Outline, &Tree, usize) -> usize) {
        let rows = self.du_rows();
        let Some(d) = self.screen.disk_usage_mut() else { return };
        let Some(s) = &mut d.scanned else { return };
        let tree = s.tree.clone();
        d.cursor = f(&mut s.outline, &tree, d.cursor).min(s.outline.rows.len().saturating_sub(1));
        d.top = visible_start(d.top, d.cursor, rows, s.outline.rows.len());
    }

    /// Shows the item under the cursor in the tool's panel: its folder with
    /// the cursor on it (the root: the root itself).
    fn du_go_to(&mut self) -> Task<Message> {
        let Some(d) = self.screen.disk_usage() else { return Task::none() };
        let Some(s) = &d.scanned else { return Task::none() };
        let Some(&(node, _)) = s.outline.rows.get(d.cursor) else { return Task::none() };
        let path = s.tree.path(&d.root, node);
        let nav = match path.parent() {
            Some(folder) if node != ROOT => Nav::To {
                path: folder.to_path_buf(),
                focus: Some(s.tree.node(node).name.to_string()),
            },
            _ => Nav::to(path),
        };
        let side = d.side;
        self.close_disk_usage();
        self.focus_panel(side);
        self.start_nav(side, nav)
    }

    pub(super) fn disk_usage_update(&mut self, msg: DuMsg) -> Task<Message> {
        let rows = self.du_rows();
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        match msg {
            DuMsg::Event(seq, e) => {
                if seq != d.seq {
                    return Task::none();
                }
                match e {
                    DuEvent::Progress(p) => d.progress = Some(p),
                    DuEvent::Done(tree, took) => {
                        d.running = None;
                        d.progress = None;
                        if let Some(TreeBox(tree)) = tree {
                            let outline = Outline::new(&tree);
                            d.scanned = Some(Scanned { tree, outline, took });
                        }
                    }
                }
            }
            DuMsg::Click(i) => {
                let now = Instant::now();
                let double = matches!(d.last_click, Some((j, t)) if j == i && now - t < DOUBLE_CLICK);
                d.cursor = i.min(d.len().saturating_sub(1));
                d.last_click = Some((i, now));
                if double {
                    d.last_click = None;
                    self.du_outline(|o, t, c| o.toggle_at(t, c, c));
                }
            }
            DuMsg::Toggle(i) => {
                d.last_click = None;
                self.du_outline(|o, t, c| o.toggle_at(t, i, c));
            }
            DuMsg::Wheel(lines) => {
                // The view scrolls; the cursor stays on screen.
                let len = d.len();
                let delta = (-lines * 3.0).round() as isize;
                let top = d.top.saturating_add_signed(delta).min(len.saturating_sub(rows));
                d.top = top;
                d.cursor = d.cursor.clamp(top, (top + rows).saturating_sub(1).min(len.saturating_sub(1)));
            }
            DuMsg::ScrollTo(start, rows) => {
                let len = d.len();
                let start = start.min(len.saturating_sub(rows));
                d.cursor = d.cursor.clamp(start, (start + rows).saturating_sub(1).min(len.saturating_sub(1)));
                d.top = start;
            }
            DuMsg::GoTo => return self.du_go_to(),
            DuMsg::Rescan => return self.start_disk_usage(),
            DuMsg::Close => self.close_disk_usage(),
        }
        Task::none()
    }

    /// Keys of `disk_usage_key`, for the F1 help. Keep in sync.
    pub(super) const DU_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Up, Down, PgUp, PgDn, Home, End", "Move the cursor"),
        ("Right, +", "Expand the folder; on an open one, go to its first item"),
        ("Left, -", "Collapse the folder, or go to the folder the item is in"),
        ("Enter, double-click", "Expand / collapse the folder"),
        ("Ctrl+Enter", "Go to the item in the panel (its folder, cursor on it)"),
        ("F5", "Scan again"),
        ("Esc", "Stop the scan / close"),
    ];

    pub(super) fn disk_usage_key(
        &mut self,
        named: Option<Named>,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
    ) -> Task<Message> {
        let page = self.du_rows().saturating_sub(1).max(1) as isize;
        match named {
            Some(Named::Escape) => return self.disk_usage_update(DuMsg::Close),
            Some(Named::F5) => return self.disk_usage_update(DuMsg::Rescan),
            Some(Named::Enter) if mods.control() => return self.du_go_to(),
            Some(Named::Enter) => self.du_outline(|o, t, c| o.toggle_at(t, c, c)),
            Some(Named::ArrowDown) => self.du_move(1),
            Some(Named::ArrowUp) => self.du_move(-1),
            Some(Named::PageDown) => self.du_move(page),
            Some(Named::PageUp) => self.du_move(-page),
            Some(Named::Home) => self.du_move(isize::MIN / 2),
            Some(Named::End) => self.du_move(isize::MAX / 2),
            Some(Named::ArrowRight) => self.du_outline(|o, t, c| o.right(t, c, true)),
            Some(Named::ArrowLeft) => self.du_outline(|o, _, c| o.left(c, true)),
            _ => match key {
                keyboard::Key::Character(c) if !mods.control() && !mods.alt() => match c.as_str() {
                    "+" => self.du_outline(|o, t, c| o.right(t, c, false)),
                    "-" => self.du_outline(|o, _, c| o.left(c, false)),
                    _ => {}
                },
                _ => {}
            },
        }
        Task::none()
    }

    pub(super) fn disk_usage_view<'a>(&'a self, d: &'a DiskUsage) -> Element<'a, Message> {
        let colors = theme::c();
        let (stats, stats_color) = match (&d.scanned, &d.progress) {
            (_, Some(p)) if d.running.is_some() => (
                format!("Scanning\u{2026} {} files \u{b7} {} \u{b7} {}", p.files, format::size(p.bytes), p.current.display()),
                colors.text,
            ),
            _ if d.running.is_some() => ("Scanning\u{2026}".to_owned(), colors.text),
            (Some(s), _) => (header_text(&s.tree, s.took), colors.text),
            (None, _) => ("Not scanned (F5 scans)".to_owned(), colors.muted),
        };
        let header = container(
            column![
                row![
                    text("Disk usage").size(16),
                    container(text(d.root.display().to_string()).size(13).color(colors.muted).wrapping(Wrapping::None))
                        .width(Length::Fill)
                        .clip(true),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
                container(text(stats).size(13).color(stats_color).wrapping(Wrapping::None)).width(Length::Fill).clip(true),
            ]
            .spacing(8),
        )
        .padding(14)
        .style(filled(colors.header));

        let head_cell = |s: &'a str, w: Length, right: bool| {
            let c = container(text(s).size(12).color(colors.muted)).width(w);
            if right { c.align_x(Alignment::End) } else { c }
        };
        let list_header = container(
            row![
                head_cell("Name", Length::Fill, false),
                head_cell("Size", Length::Fixed(COL_SIZE), true),
                head_cell("% of folder", Length::Fixed(COL_PCT + 6.0 + BAR_W), false),
                head_cell("Files", Length::Fixed(COL_FILES), true),
            ]
            .spacing(10),
        )
        .padding([3, 14])
        .style(filled(colors.header));

        let list: Element<'a, Message> = match &d.scanned {
            Some(s) => {
                let list = responsive(move |size| {
                    let rows = (size.height / ROW_H).floor().max(1.0) as usize;
                    let len = s.outline.rows.len();
                    let start = visible_start(d.top, d.cursor, rows, len);
                    let end = (start + rows).min(len);
                    let lines = (start..end).map(|i| self.du_line(s, i, i == d.cursor));
                    let lines = container(Column::with_children(lines).width(Length::Fill))
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .clip(true);
                    scrollbar::attach(lines, start as f32, rows as f32, len as f32, move |at| {
                        Message::DiskUsage(DuMsg::ScrollTo(at.round() as usize, rows))
                    })
                });
                mouse_area(container(list).width(Length::Fill).height(Length::Fill).style(filled(colors.panel)))
                    .on_scroll(|delta| {
                        Message::DiskUsage(DuMsg::Wheel(match delta {
                            mouse::ScrollDelta::Lines { y, .. } => y,
                            mouse::ScrollDelta::Pixels { y, .. } => y / ROW_H / 3.0,
                        }))
                    })
                    .into()
            }
            None => container(Space::new(Length::Fill, Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .style(filled(colors.panel))
                .into(),
        };

        let note = "\u{2191}\u{2193} move \u{b7} \u{2192}/+ expand \u{b7} \u{2190}/- collapse \u{b7} Enter expand/collapse \u{b7} \
                    Ctrl+Enter go to it in the panel \u{b7} F5 scan again \u{b7} Esc close";
        let action = |label: &'a str, msg: DuMsg, primary: bool| {
            button(text(label).size(13))
                .on_press(Message::DiskUsage(msg))
                .padding([6, 14])
                .style(if primary { theme::primary } else { theme::fkey })
        };
        let mut buttons = Row::new().spacing(8);
        if d.scanned.is_some() {
            buttons = buttons.push(action("Go to", DuMsg::GoTo, false));
        }
        buttons = buttons.push(action("Close", DuMsg::Close, false));
        if d.running.is_none() {
            buttons = buttons.push(action("Scan again", DuMsg::Rescan, true));
        }
        let footer = container(
            row![
                container(text(note).size(12).color(colors.muted).wrapping(Wrapping::None)).width(Length::Fill).clip(true),
                Space::with_width(Length::Fixed(8.0)),
                buttons
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(filled(colors.header));

        container(column![header, list_header, list, footer])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.bg))
            .into()
    }

    /// Row `i` of the outline.
    fn du_line<'a>(&'a self, s: &'a Scanned, i: usize, cursor: bool) -> Element<'a, Message> {
        let colors = theme::c();
        let tree = &*s.tree;
        let (node, depth) = s.outline.rows[i];
        let n = tree.node(node);
        let is_dir = n.kind == NodeKind::Dir;
        let color = if is_dir { colors.dir } else { colors.text };
        let cell = |t: String| text(t).size(13).color(color).wrapping(Wrapping::None);

        let arrow: Element<'a, Message> = if is_dir && !n.children.is_empty() {
            let glyph = if s.outline.is_expanded(node) { "\u{25be}" } else { "\u{25b8}" };
            mouse_area(container(text(glyph).size(12).color(colors.muted)).width(Length::Fixed(ARROW_W)))
                .on_press(Message::DiskUsage(DuMsg::Toggle(i)))
                .into()
        } else {
            Space::with_width(Length::Fixed(ARROW_W)).into()
        };
        let parent = tree.parent(node).map_or(n.size, |p| tree.node(p).size);
        let frac = share(n.size, parent);
        let bar = container(
            container(Space::new(Length::Fixed((BAR_W * frac).round()), Length::Fixed(4.0))).style(filled(colors.accent)),
        )
        .width(Length::Fixed(BAR_W))
        .height(Length::Fixed(4.0))
        .style(filled(colors.border));
        let pct = container(cell(format!("{:.1} %", frac * 100.0)).color(colors.muted))
            .width(Length::Fixed(COL_PCT))
            .align_x(Alignment::End);
        let files = if is_dir { n.files.to_string() } else { String::new() };
        let name = container(cell(n.name.to_string())).width(Length::Fill).clip(true);

        let content = row![
            Space::with_width(Length::Fixed(INDENT * depth as f32)),
            arrow,
            name,
            container(cell(format::size(n.size))).width(Length::Fixed(COL_SIZE)).align_x(Alignment::End),
            row![pct, bar].spacing(6).align_y(Alignment::Center),
            container(cell(files).color(colors.muted)).width(Length::Fixed(COL_FILES)).align_x(Alignment::End),
        ]
        .spacing(10)
        .padding([0, 14])
        .height(ROW_H)
        .align_y(Alignment::Center);
        let bg = if cursor {
            colors.cursor
        } else if i % 2 == 1 {
            colors.zebra
        } else {
            colors.panel
        };
        mouse_area(container(content).width(Length::Fill).height(ROW_H).clip(true).style(filled(bg)))
            .on_press(Message::DiskUsage(DuMsg::Click(i)))
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::disk_usage::{Node, EXT_DIR, EXT_NONE};

    /// root(0) -> a(1) [a1(3), a2(4) -> x(6)], b(2) [b1(5)]; files are leaves.
    fn tree() -> Tree {
        let node = |name: &str, size: u64, dir: bool, parent: u32, children: Vec<u32>| Node {
            name: name.into(),
            size,
            files: 0,
            dirs: 0,
            kind: if dir { NodeKind::Dir } else { NodeKind::File },
            ext_id: if dir { EXT_DIR } else { EXT_NONE },
            parent,
            children,
        };
        Tree {
            nodes: vec![
                node("C:\\root", 100, true, 0, vec![1, 2]),
                node("a", 60, true, 0, vec![3, 4]),
                node("b", 40, true, 0, vec![5]),
                node("a1", 50, false, 1, vec![]),
                node("a2", 10, true, 1, vec![6]),
                node("b1", 40, false, 2, vec![]),
                node("x", 10, false, 4, vec![]),
            ],
            exts: vec![("".into(), 0, 0), ("*".into(), 0, 0)],
            errors: 0,
        }
    }

    fn nodes(o: &Outline) -> Vec<u32> {
        o.rows.iter().map(|&(n, _)| n).collect()
    }

    #[test]
    fn flattening_shows_expanded_folders_only() {
        let t = tree();
        let mut expanded = vec![false; t.nodes.len()];
        assert_eq!(flatten(&t, &expanded), vec![(0, 0)]);
        expanded[0] = true;
        assert_eq!(flatten(&t, &expanded), vec![(0, 0), (1, 1), (2, 1)]);
        expanded[1] = true;
        expanded[4] = true;
        assert_eq!(flatten(&t, &expanded), vec![(0, 0), (1, 1), (3, 2), (4, 2), (6, 3), (2, 1)]);
        // A collapsed folder hides what is open under it.
        expanded[1] = false;
        assert_eq!(flatten(&t, &expanded), vec![(0, 0), (1, 1), (2, 1)]);
    }

    #[test]
    fn expand_and_collapse_splice_rows() {
        let t = tree();
        let mut o = Outline::new(&t);
        assert_eq!(nodes(&o), [0, 1, 2]);
        assert_eq!(o.expand(&t, 1), 2);
        assert_eq!(nodes(&o), [0, 1, 3, 4, 2]);
        assert_eq!(o.expand(&t, 1), 0, "already open");
        assert_eq!(o.expand(&t, 2), 0, "a file");
        assert_eq!(o.expand(&t, 3), 1);
        assert_eq!(nodes(&o), [0, 1, 3, 4, 6, 2]);
        assert_eq!(o.collapse(1), 3);
        assert_eq!(nodes(&o), [0, 1, 2]);
        // Reopening shows a2 still open.
        o.expand(&t, 1);
        assert_eq!(nodes(&o), [0, 1, 3, 4, 6, 2]);
        assert_eq!(o.rows, flatten(&t, &o.expanded));
        assert_eq!(o.collapse(0), 5);
        assert_eq!(nodes(&o), [0]);
    }

    #[test]
    fn toggling_keeps_the_cursor_on_its_item() {
        let t = tree();
        let mut o = Outline::new(&t);
        // Cursor on b (row 2); opening a above it moves it down.
        assert_eq!(o.toggle_at(&t, 1, 2), 4);
        // Cursor on a1 (row 2) inside a: closing a puts it on a.
        assert_eq!(o.toggle_at(&t, 1, 2), 1);
        o.toggle_at(&t, 1, 0);
        assert_eq!(o.toggle_at(&t, 1, 4), 2, "b, after a closes");
        assert_eq!(o.toggle_at(&t, 1, 0), 0);
    }

    #[test]
    fn left_and_right_walk_the_tree() {
        let t = tree();
        let mut o = Outline::new(&t);
        // Right on a: opens it, then steps in.
        assert_eq!(o.right(&t, 1, true), 1);
        assert_eq!(nodes(&o), [0, 1, 3, 4, 2]);
        assert_eq!(o.right(&t, 1, true), 2);
        assert_eq!(o.right(&t, 1, false), 1, "+ doesn't step in");
        // Right on a file does nothing.
        assert_eq!(o.right(&t, 2, true), 2);
        // Left on a1: to a; Left on a: closes it; Left on a: to the root.
        assert_eq!(o.parent_row(2), Some(1));
        assert_eq!(o.left(2, true), 1);
        assert_eq!(o.left(1, true), 1);
        assert_eq!(nodes(&o), [0, 1, 2]);
        assert_eq!(o.left(1, true), 0);
        assert_eq!(o.left(1, false), 1, "- doesn't step out");
        assert_eq!(o.parent_row(0), None);
        assert_eq!(o.left(0, true), 0);
        assert_eq!(nodes(&o), [0]);
        assert_eq!(o.left(0, true), 0);
    }

    #[test]
    fn shares_of_the_parent() {
        assert_eq!(share(0, 0), 0.0);
        assert_eq!(share(25, 100), 0.25);
    }
}
