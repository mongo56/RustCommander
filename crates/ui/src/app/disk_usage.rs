//! Disk usage (Alt+Shift+F7): fills the window while open. Scans the folder
//! under the cursor (or the panel's folder) on a worker thread
//! (`rc_core::disk_usage::scan`) and shows the whole tree as an outline,
//! largest first, with each item's share of its folder, and below it as a
//! treemap (one tile per file, coloured by extension). Ctrl+Enter shows the
//! item in the panel. Closing keeps the tree, so reopening the same folder is
//! instant (F5 scans again). Files are never opened from here.

use super::*;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, mouse_area, responsive, row, text, Column, Row, Space};
use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{self, Widget};
use iced::advanced::{renderer, Clipboard, Shell};
use iced::{event, Alignment, Background, Border, Color, Element, Event, Length, Rectangle, Size, Theme};
use rc_core::disk_usage::{self, NodeKind, Tree, ROOT};
use rc_core::treemap::{self, Rect, Tile};
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

    /// Expands the folders above `node` and returns its row.
    fn reveal(&mut self, tree: &Tree, node: u32) -> Option<usize> {
        let mut chain = vec![node];
        let mut i = node;
        while i != ROOT {
            i = tree.node(i).parent;
            chain.push(i);
        }
        let mut row = 0;
        for &child in chain.iter().rev().skip(1) {
            self.expand(tree, row);
            let depth = self.rows[row].1;
            let below = self.rows[row + 1..].iter().take_while(|&&(_, d)| d > depth);
            row = row + 1 + below.into_iter().position(|&(n, _)| n == child)?;
        }
        Some(row)
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
    /// Identifies this tree (the scan's `seq`), for the treemap's tile cache.
    gen: u64,
    /// Treemap colour per extension id ([`ext_colors`]).
    ext_colors: Vec<u8>,
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
    /// The treemap tile under the mouse changed (its node).
    Hover(Option<u32>),
    /// Click on a treemap tile: select its node in the list.
    MapClick(u32),
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
    /// Node under the mouse on the treemap.
    hover: Option<u32>,
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
            hover: None,
        });
        du.hover = None;
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
        d.hover = None;
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

    /// Rows of the list, which gets half the space (the treemap the other half).
    fn du_rows(&self) -> usize {
        ((self.window_height - DU_CHROME_H) / 2.0 / ROW_H).floor().max(1.0) as usize
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
                            let ext_colors = ext_colors(&tree.exts);
                            d.scanned = Some(Scanned { tree, outline, took, gen: seq, ext_colors });
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
            DuMsg::Hover(node) => d.hover = node,
            DuMsg::MapClick(node) => {
                d.last_click = None;
                self.du_outline(|o, t, c| o.reveal(t, node).unwrap_or(c));
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
        ("Click on the map", "Select that file or folder in the list"),
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

        let map: Element<'a, Message> = match &d.scanned {
            Some(s) => Element::new(Treemap {
                tree: &s.tree,
                gen: s.gen,
                ext_colors: &s.ext_colors,
                selected: s.outline.rows.get(d.cursor).map(|r| r.0),
            }),
            None => Space::new(Length::Fill, Length::Fill).into(),
        };
        let map = container(map).width(Length::Fill).height(Length::FillPortion(1)).padding(6).style(filled(colors.panel));

        // The tile under the mouse, else the keys.
        let hovered = d.scanned.as_ref().zip(d.hover).filter(|(s, n)| (*n as usize) < s.tree.nodes.len());
        let (note, note_color) = match hovered {
            Some((s, n)) => {
                let node = s.tree.node(n);
                let what = if node.kind == NodeKind::Dir && n != ROOT && !node.children.is_empty() {
                    format!(" \u{b7} {} files", node.files)
                } else {
                    String::new()
                };
                (
                    format!("{} \u{b7} {}{what}", s.tree.path(&d.root, n).display(), format::size(node.size)),
                    colors.text,
                )
            }
            None => (
                "\u{2191}\u{2193} move \u{b7} \u{2192}/+ expand \u{b7} \u{2190}/- collapse \u{b7} Enter expand/collapse \
                 \u{b7} Ctrl+Enter go to it in the panel \u{b7} F5 scan again \u{b7} Esc close"
                    .to_owned(),
                colors.muted,
            ),
        };
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
                container(text(note).size(12).color(note_color).wrapping(Wrapping::None)).width(Length::Fill).clip(true),
                Space::with_width(Length::Fixed(8.0)),
                buttons
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(filled(colors.header));

        let list = column![list_header, list].height(Length::FillPortion(1));
        container(column![header, list, map, footer])
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

// ---- Treemap -------------------------------------------------------------

/// Folders narrower or lower than this (in pixels) are one tile.
const MAP_MIN_PX: f32 = 3.0;
/// Extensions with their own colour (the largest by bytes).
const MAP_TOP_EXTS: usize = 12;
/// `ext_colors` value of extensions without their own colour.
const MAP_GREY: u8 = u8::MAX;

/// Fill colours of the top extensions: mid tones that read on dark and light
/// themes alike (Tableau 10 plus two).
const MAP_PALETTE: [u32; MAP_TOP_EXTS] = [
    0x4e79a7, 0xf28e2b, 0xe15759, 0x76b7b2, 0x59a14f, 0xedc948, 0xb07aa1, 0xff9da7, 0x9c755f, 0xa0cbe8, 0x8cd17d,
    0xbab0ac,
];
/// Files of the other extensions.
const MAP_OTHER: u32 = 0x7d828a;
/// Folders drawn as one tile (too small to show what is in them).
const MAP_FOLDER: u32 = 0x4f545c;

/// Palette index per extension id (`MAP_GREY` for the rest): the
/// `MAP_TOP_EXTS` extensions with the most bytes, the "other" bucket excluded.
fn ext_colors(exts: &[(Box<str>, u64, u32)]) -> Vec<u8> {
    let mut order: Vec<usize> =
        (0..exts.len()).filter(|&i| i != disk_usage::EXT_OTHER as usize && exts[i].1 > 0).collect();
    order.sort_by(|&a, &b| exts[b].1.cmp(&exts[a].1).then(a.cmp(&b)));
    let mut colors = vec![MAP_GREY; exts.len()];
    for (k, &i) in order.iter().take(MAP_TOP_EXTS).enumerate() {
        colors[i] = k as u8;
    }
    colors
}

/// When the tiles must be laid out again: another size or another tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MapKey {
    w: u32,
    h: u32,
    gen: u64,
}

impl MapKey {
    fn new(size: Size, gen: u64) -> Self {
        Self { w: size.width.round().max(0.0) as u32, h: size.height.round().max(0.0) as u32, gen }
    }
}

fn contains(r: &Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

/// Index of the tile at (`x`, `y`) (tiles don't overlap). `hint` (the tile
/// hovered before) is checked first.
fn hit(tiles: &[Tile], x: f32, y: f32, hint: Option<usize>) -> Option<usize> {
    if let Some(i) = hint.filter(|&i| tiles.get(i).is_some_and(|t| contains(&t.rect, x, y))) {
        return Some(i);
    }
    tiles.iter().position(|t| contains(&t.rect, x, y))
}

/// Whether `node` is `of` or under it.
fn is_within(tree: &Tree, mut node: u32, of: u32) -> bool {
    loop {
        if node == of {
            return true;
        }
        if node == ROOT {
            return false;
        }
        node = tree.node(node).parent;
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(x, y, (a.x + a.w).max(b.x + b.w) - x, (a.y + a.h).max(b.y + b.h) - y)
}

/// Where `node` is on the map: the bounding rect of its tiles (a folder's are
/// one run, as layout emits them depth first), or, if it has none (inside a
/// folder drawn as one tile), the tile of the nearest folder above it.
fn node_rect(tree: &Tree, tiles: &[Tile], node: u32) -> Option<Rect> {
    let mut found: Option<Rect> = None;
    for t in tiles {
        if is_within(tree, t.node, node) {
            found = Some(found.map_or(t.rect, |r| union(r, t.rect)));
        } else if found.is_some() {
            break;
        }
    }
    if found.is_some() {
        return found;
    }
    let mut up = node;
    while up != ROOT {
        up = tree.node(up).parent;
        if let Some(t) = tiles.iter().find(|t| t.node == up) {
            return Some(t.rect);
        }
    }
    None
}

#[derive(Default)]
struct MapState {
    key: Option<MapKey>,
    /// Laid out at (0, 0); drawn offset by the widget's position.
    tiles: Vec<Tile>,
    /// Selected node and where it is, for `key`.
    sel: Option<(u32, Option<Rect>)>,
    /// Tile under the mouse.
    hover: Option<usize>,
}

/// The treemap of a scanned tree: one quad per tile, coloured by extension,
/// with the selected item outlined. Tiles are laid out only when the size or
/// the tree changes (kept in the widget state).
struct Treemap<'a> {
    tree: &'a Tree,
    gen: u64,
    ext_colors: &'a [u8],
    selected: Option<u32>,
}

impl<Renderer: renderer::Renderer> Widget<Message, Theme, Renderer> for Treemap<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<MapState>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(MapState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&self, wtree: &mut widget::Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let node = layout::atomic(limits, Length::Fill, Length::Fill);
        let state = wtree.state.downcast_mut::<MapState>();
        let key = MapKey::new(node.size(), self.gen);
        if state.key != Some(key) {
            state.key = Some(key);
            state.tiles.clear();
            state.sel = None;
            state.hover = None;
            let rect = Rect::new(0.0, 0.0, key.w as f32, key.h as f32);
            treemap::layout(self.tree, ROOT, rect, MAP_MIN_PX, &mut state.tiles);
        }
        if state.sel.map(|s| s.0) != self.selected {
            state.sel = self.selected.map(|n| (n, node_rect(self.tree, &state.tiles, n)));
        }
        node
    }

    fn on_event(
        &mut self,
        wtree: &mut widget::Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        let state = wtree.state.downcast_mut::<MapState>();
        let at = cursor.position_in(layout.bounds());
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) => {
                let now = at.and_then(|p| hit(&state.tiles, p.x, p.y, state.hover));
                if now != state.hover {
                    state.hover = now;
                    let node = now.map(|i| state.tiles[i].node);
                    shell.publish(Message::DiskUsage(DuMsg::Hover(node)));
                }
                event::Status::Ignored
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(p) = at else { return event::Status::Ignored };
                if let Some(i) = hit(&state.tiles, p.x, p.y, state.hover) {
                    shell.publish(Message::DiskUsage(DuMsg::MapClick(state.tiles[i].node)));
                }
                event::Status::Captured
            }
            _ => event::Status::Ignored,
        }
    }

    fn mouse_interaction(
        &self,
        _wtree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) { mouse::Interaction::Pointer } else { mouse::Interaction::None }
    }

    fn draw(
        &self,
        wtree: &widget::Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let state = wtree.state.downcast_ref::<MapState>();
        let b = layout.bounds();
        let colors = theme::c();
        // (fill, border): the palette, then other files, then folders.
        let shade = |hex: u32| {
            let c = Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
            (c, Color::from_rgb(c.r * 0.6, c.g * 0.6, c.b * 0.6))
        };
        let mut fills = [(Color::BLACK, Color::BLACK); MAP_TOP_EXTS + 2];
        for (k, &hex) in MAP_PALETTE.iter().chain(&[MAP_OTHER, MAP_FOLDER]).enumerate() {
            fills[k] = shade(hex);
        }
        let quad = |r: Rect| Rectangle { x: b.x + r.x, y: b.y + r.y, width: r.w, height: r.h };
        renderer.fill_quad(renderer::Quad { bounds: b, ..renderer::Quad::default() }, Background::Color(colors.panel));
        for t in &state.tiles {
            let ext = self.tree.node(t.node).ext_id;
            let k = if ext == disk_usage::EXT_DIR {
                MAP_TOP_EXTS + 1
            } else {
                match self.ext_colors.get(ext as usize) {
                    Some(&k) if k != MAP_GREY => k as usize,
                    _ => MAP_TOP_EXTS,
                }
            };
            let (fill, border) = fills[k];
            // Tiles under 3 px would be all border.
            let border = if t.rect.w >= 3.0 && t.rect.h >= 3.0 {
                Border { color: border, width: 1.0, radius: 0.0.into() }
            } else {
                Border::default()
            };
            renderer.fill_quad(renderer::Quad { bounds: quad(t.rect), border, ..renderer::Quad::default() }, fill);
        }
        if let Some((_, Some(r))) = state.sel {
            // At least 6x6 so tiny items show, and inside the map.
            let (w, h) = (r.w.max(6.0).min(b.width), r.h.max(6.0).min(b.height));
            let x = (r.x + r.w / 2.0 - w / 2.0).clamp(0.0, (b.width - w).max(0.0));
            let y = (r.y + r.h / 2.0 - h / 2.0).clamp(0.0, (b.height - h).max(0.0));
            let outer = quad(Rect::new(x, y, w, h));
            let frame = |bounds: Rectangle, color: Color, width: f32| renderer::Quad {
                bounds,
                border: Border { color, width, radius: 0.0.into() },
                ..renderer::Quad::default()
            };
            renderer.fill_quad(frame(outer, colors.text, 2.0), Color::TRANSPARENT);
            let inner = Rectangle {
                x: outer.x + 2.0,
                y: outer.y + 2.0,
                width: (outer.width - 4.0).max(0.0),
                height: (outer.height - 4.0).max(0.0),
            };
            renderer.fill_quad(frame(inner, colors.bg, 1.0), Color::TRANSPARENT);
        }
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
    fn reveal_expands_the_folders_above() {
        let t = tree();
        let mut o = Outline::new(&t);
        // x is in a2 in a.
        assert_eq!(o.reveal(&t, 6), Some(4));
        assert_eq!(nodes(&o), [0, 1, 3, 4, 6, 2]);
        assert_eq!(o.reveal(&t, 5), Some(6));
        assert_eq!(nodes(&o), [0, 1, 3, 4, 6, 2, 5]);
        assert_eq!(o.reveal(&t, 0), Some(0));
        // A collapsed root opens again.
        o.collapse(0);
        assert_eq!(o.reveal(&t, 2), Some(5));
    }

    #[test]
    fn top_extensions_get_colours_by_bytes() {
        let mut exts: Vec<(Box<str>, u64, u32)> = vec![("".into(), 5, 1), ("*".into(), 1_000, 9)];
        for i in 0..14u64 {
            exts.push((format!("e{i}").into(), 100 + i, 1));
        }
        exts.push(("empty".into(), 0, 3));
        let c = ext_colors(&exts);
        assert_eq!(c.len(), exts.len());
        assert_eq!(c[1], MAP_GREY, "the other bucket");
        assert_eq!(c[0], MAP_GREY, "no extension: smallest");
        assert_eq!(c[exts.len() - 1], MAP_GREY, "no bytes");
        // e13 (largest) first, ..., e2 twelfth; e1 and e0 grey.
        assert_eq!(c[2 + 13], 0);
        assert_eq!(c[2 + 2], (MAP_TOP_EXTS - 1) as u8);
        assert_eq!((c[2], c[3]), (MAP_GREY, MAP_GREY));
    }

    fn tile(node: u32, x: f32, y: f32, w: f32, h: f32) -> Tile {
        Tile { node, rect: Rect::new(x, y, w, h), depth: 1 }
    }

    /// The map of `tree()` as layout emits it: a's tiles, then b's.
    fn tiles() -> Vec<Tile> {
        vec![tile(3, 0.0, 0.0, 50.0, 100.0), tile(6, 50.0, 0.0, 10.0, 100.0), tile(5, 60.0, 0.0, 40.0, 100.0)]
    }

    #[test]
    fn hit_finds_the_tile_under_a_point() {
        let tiles = tiles();
        assert_eq!(hit(&tiles, 10.0, 10.0, None), Some(0));
        assert_eq!(hit(&tiles, 50.0, 99.9, None), Some(1));
        assert_eq!(hit(&tiles, 99.0, 0.0, Some(0)), Some(2), "a stale hint");
        assert_eq!(hit(&tiles, 55.0, 5.0, Some(1)), Some(1));
        assert_eq!(hit(&tiles, 100.0, 5.0, None), None);
        assert_eq!(hit(&tiles, 5.0, -1.0, None), None);
    }

    #[test]
    fn a_folder_is_the_union_of_its_tiles() {
        let t = tree();
        let tiles = tiles();
        assert_eq!(node_rect(&t, &tiles, 3), Some(Rect::new(0.0, 0.0, 50.0, 100.0)));
        assert_eq!(node_rect(&t, &tiles, 1), Some(Rect::new(0.0, 0.0, 60.0, 100.0)));
        assert_eq!(node_rect(&t, &tiles, 4), Some(Rect::new(50.0, 0.0, 10.0, 100.0)));
        assert_eq!(node_rect(&t, &tiles, 0), Some(Rect::new(0.0, 0.0, 100.0, 100.0)));
        // a2 drawn as one tile: x has none of its own, so it shows as a2.
        let small = vec![tile(3, 0.0, 0.0, 50.0, 100.0), tile(4, 50.0, 0.0, 2.0, 100.0)];
        assert_eq!(node_rect(&t, &small, 6), Some(Rect::new(50.0, 0.0, 2.0, 100.0)));
        assert_eq!(node_rect(&t, &small, 5), None, "b not on the map");
    }

    #[test]
    fn tiles_are_laid_out_again_on_a_new_size_or_tree_only() {
        let k = MapKey::new(Size::new(800.0, 300.0), 7);
        assert_eq!(k, MapKey::new(Size::new(800.2, 299.8), 7));
        assert_ne!(k, MapKey::new(Size::new(801.0, 300.0), 7));
        assert_ne!(k, MapKey::new(Size::new(800.0, 300.0), 8));
    }

    #[test]
    fn shares_of_the_parent() {
        assert_eq!(share(0, 0), 0.0);
        assert_eq!(share(25, 100), 0.25);
    }
}
