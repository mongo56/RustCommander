//! Disk usage (Alt+Shift+F7): fills the window while open. Scans the folder
//! under the cursor (or the panel's folder) on a worker thread
//! (`rc_core::disk_usage::scan`) and shows the whole tree as an outline,
//! largest first, with each item's share of its folder, and below it as a
//! treemap (one tile per file, coloured by extension; Z zooms it to a folder),
//! with a list of the extensions on the right (a click highlights its tiles).
//! Ctrl+Enter shows the item in the panel; F8 / Delete deletes it through the
//! normal delete job and takes it out of the tree without a rescan (a folder
//! that is partly left scans again on its own and goes back in); a right
//! click or Shift+F10 shows the shell menu. Closing keeps the tree, so
//! reopening the same folder is instant (F5 scans again). Files are never
//! opened from here.
//!
//! A folder inside an archive, a whole archive (the cursor on one) or a
//! folder on a server is scanned through its `Vfs` (`disk_usage::scan_path`):
//! sizes are logical only (no size on disk, S says so), there is no shell
//! menu, and deletes go through the same job as the panels' (a folder whose
//! delete didn't finish, or anything whose state isn't clear, is checked
//! again on a worker, never on the UI thread).

use super::*;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, mouse_area, responsive, row, text, Column, Row, Space};
use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{self, Widget};
use iced::advanced::{renderer, Clipboard, Shell};
use iced::{event, Alignment, Background, Border, Color, Element, Event, Length, Rectangle, Size, Theme};
use rc_core::disk_usage::{self, Measure, NodeKind, Tree, ROOT};
use rc_core::treemap::{self, Rect, Tile};
use rc_core::format;
use rc_core::panel::visible_start;
use rc_platform::MenuAction;

const INDENT: f32 = 16.0;
const ARROW_W: f32 = 14.0;
const COL_SIZE: f32 = 90.0;
const COL_PCT: f32 = 44.0;
const BAR_W: f32 = 70.0;
const COL_FILES: f32 = 90.0;
/// Height of the header, list and map headers and footer (for paging estimates).
const DU_CHROME_H: f32 = 160.0;

/// Default share of the height the list area gets (the treemap gets the rest).
pub const DU_DEFAULT_SPLIT: f32 = 0.5;
/// Smallest and largest share of the list area.
const DU_SPLIT_RANGE: (f32, f32) = (0.2, 0.8);
/// Ctrl+Up / Ctrl+Down move the split by this much.
const DU_SPLIT_STEP: f32 = 0.05;

/// `ratio` limited to the allowed range (NaN gives the default).
pub fn du_clamp_split(ratio: f32) -> f32 {
    if ratio.is_nan() { DU_DEFAULT_SPLIT } else { ratio.clamp(DU_SPLIT_RANGE.0, DU_SPLIT_RANGE.1) }
}

/// `FillPortion` values (per mille) of the list area and the treemap.
fn du_portions(ratio: f32) -> (u16, u16) {
    let top = (du_clamp_split(ratio) * 1000.0).round() as u16;
    (top, 1000 - top)
}
/// Width of the extension list.
const EXT_W: f32 = 320.0;
const EXT_SWATCH: f32 = 10.0;
const EXT_SIZE: f32 = 66.0;
const EXT_PCT: f32 = 44.0;
const EXT_COUNT: f32 = 54.0;

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

    /// The rows again after the tree changed (a node removed), keeping
    /// which folders are open.
    fn rebuild(&mut self, tree: &Tree) {
        self.expanded.resize(tree.nodes.len(), false);
        self.rows = flatten(tree, &self.expanded);
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
    /// Extension ids with files, most bytes first ([`ext_order`]).
    ext_order: Vec<u16>,
    /// Bytes per extension id by the active measure.
    ext_bytes: Vec<u64>,
    /// Folder the treemap shows (the root unless zoomed).
    zoom: u32,
}

impl Scanned {
    fn new(mut tree: Arc<Tree>, took: Duration, gen: u64, measure: Measure) -> Self {
        if tree.measure != measure {
            Arc::make_mut(&mut tree).set_measure(measure);
        }
        let outline = Outline::new(&tree);
        let ext_bytes = tree.ext_bytes();
        let ext_colors = ext_colors(&tree.exts, &ext_bytes);
        let ext_order = ext_order(&tree.exts, &ext_bytes);
        Self { tree, outline, took, gen, ext_colors, ext_order, ext_bytes, zoom: ROOT }
    }

    fn refresh_exts(&mut self) {
        self.ext_bytes = self.tree.ext_bytes();
        self.ext_colors = ext_colors(&self.tree.exts, &self.ext_bytes);
        self.ext_order = ext_order(&self.tree.exts, &self.ext_bytes);
    }

    /// Switches between logical and on-disk sizes: sorts the tree again and
    /// refreshes everything derived from it; `gen` is the new tree id.
    fn set_measure(&mut self, measure: Measure, gen: u64) {
        if self.tree.measure == measure {
            return;
        }
        Arc::make_mut(&mut self.tree).set_measure(measure);
        self.outline.rebuild(&self.tree);
        self.refresh_exts();
        self.gen = gen;
    }

    /// Takes `node` out after it was deleted from disk (see
    /// [`Tree::remove`]); `gen` is the new tree id for the tile cache.
    fn remove(&mut self, node: u32, gen: u64) -> bool {
        if !Arc::make_mut(&mut self.tree).remove(node) {
            return false;
        }
        self.outline.rebuild(&self.tree);
        self.refresh_exts();
        self.zoom = self.tree.surviving(self.zoom);
        self.gen = gen;
        true
    }
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

/// A folder scanned again after a delete in it didn't finish.
#[derive(Clone)]
pub struct RescanBox(Arc<disk_usage::Rescan>);

impl std::fmt::Debug for RescanBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0 {
            disk_usage::Rescan::Gone => write!(f, "Gone"),
            disk_usage::Rescan::Tree(t) => write!(f, "Tree({} nodes)", t.nodes.len()),
            disk_usage::Rescan::File => write!(f, "File"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum DuEvent {
    Progress(disk_usage::Progress),
    /// `None`: cancelled.
    Done(Option<TreeBox>, Duration),
    /// The archive or server couldn't be opened, or the scanned folder
    /// couldn't be listed there.
    Failed(String),
    /// A folder scanned again (`DiskUsage::rescanning`); `None`: cancelled.
    Rescanned(Option<RescanBox>),
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
    /// Right click on a row: select it and show its shell menu.
    RightClick(usize),
    /// Right click on a treemap tile.
    MapRightClick(u32),
    ContextDone(Result<MenuAction, String>),
    /// Click on an extension (by id): select it, or clear it if it was.
    ExtClick(u16),
    ExtWheel(f32),
    ExtScrollTo(usize, usize),
    /// Toggle between size and size on disk.
    Measure,
    GoTo,
    Rescan,
    Close,
    /// Treemap PNG written (the path) or failed (the message).
    Exported(Result<PathBuf, String>),
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
    /// Extension whose tiles the map highlights.
    ext_sel: Option<u16>,
    /// First row shown of the extension list.
    ext_top: usize,
    /// The keyboard moves in the extension list (Tab), not the tree.
    on_exts: bool,
    /// Logical size or size on disk.
    measure: Measure,
    /// Delete asked for (waiting for the confirmation): scan seq, node, path.
    delete_ask: Option<(u64, u32, PathBuf)>,
    /// Delete job running: job id, scan seq, node, path.
    deleting: Option<(u64, u64, u32, PathBuf)>,
    /// A folder whose delete didn't finish, being scanned again (`running`
    /// is its cancel flag).
    rescanning: Option<Rescanning>,
    /// Message in the footer (the panels' status bar isn't shown here),
    /// until the next key.
    note: Option<(String, Tone)>,
    /// Why the last scan of an archive or server folder failed.
    failed: Option<String>,
    /// `root` is inside an archive (or is one) or on a server.
    in_vfs: bool,
}

/// See `DiskUsage::rescanning`.
struct Rescanning {
    seq: u64,
    node: u32,
    path: PathBuf,
    /// The delete's summary, for the footer when done.
    summary: Option<(String, Tone)>,
}

impl DiskUsage {
    /// The tree is inside an archive (or is one) or on a server: read
    /// through a `Vfs`, without sizes on disk or a shell menu.
    fn is_virtual(&self) -> bool {
        self.in_vfs
    }

    /// The cursor is the tree row and the chosen extension. A note stays while a scan runs ("… scanning
    /// again").
    pub(super) fn note_state(&mut self) -> NoteState<'_> {
        let cursor = (self.cursor, self.ext_sel.map_or(0, |e| usize::from(e) + 1));
        NoteState { cursor, pending: self.running.is_some(), note: &mut self.note }
    }

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

/// The folder `Z` zooms the map to for the item `node`: a folder with
/// something in it, else the folder it is in.
fn zoom_in_target(tree: &Tree, node: u32) -> u32 {
    let n = tree.node(node);
    if n.kind == NodeKind::Dir && !n.children.is_empty() { node } else { tree.parent(node).unwrap_or(ROOT) }
}

/// One level out from the map's folder `zoom`.
fn zoom_out_target(tree: &Tree, zoom: u32) -> u32 {
    tree.parent(zoom).unwrap_or(ROOT)
}

/// The extension list: ids with files, most bytes first (ties by id).
fn ext_order(exts: &[(Box<str>, u64, u32)], bytes: &[u64]) -> Vec<u16> {
    let mut order: Vec<u16> = (0..exts.len()).filter(|&i| exts[i].2 > 0).map(|i| i as u16).collect();
    order.sort_by(|&a, &b| bytes[b as usize].cmp(&bytes[a as usize]).then(a.cmp(&b)));
    order
}

/// The extension `delta` rows from `sel` in `order` (from the top when
/// nothing is selected), kept in the list.
fn ext_step(order: &[u16], sel: Option<u16>, delta: isize) -> Option<u16> {
    let last = order.len().checked_sub(1)?;
    let at = match sel.and_then(|s| order.iter().position(|&e| e == s)) {
        Some(pos) => pos.saturating_add_signed(delta).min(last),
        None if delta < 0 => last.saturating_add_signed(delta + 1),
        None => (delta.max(1) as usize - 1).min(last),
    };
    Some(order[at])
}

/// How an extension shows in the list.
fn ext_label(tree: &Tree, id: u16) -> String {
    match id {
        disk_usage::EXT_NONE => "(none)".to_owned(),
        disk_usage::EXT_OTHER => "other".to_owned(),
        _ => format!(".{}", tree.exts[id as usize].0),
    }
}

fn header_text(tree: &Tree, took: Duration) -> String {
    let root = tree.root();
    let mut t = format!(
        "{} \u{b7} {} files \u{b7} {} folders",
        format::size(tree.bytes(ROOT)),
        root.files,
        root.dirs
    );
    if tree.errors > 0 {
        t.push_str(&format!(" \u{b7} {} unreadable", tree.errors));
    }
    match tree.hard_links.len() {
        0 => {}
        1 => t.push_str(" \u{b7} 1 hard link counted once"),
        n => t.push_str(&format!(" \u{b7} {n} hard links counted once")),
    }
    if tree.measure == Measure::Alloc {
        t.push_str(" on disk");
    }
    t.push_str(&format!(" \u{b7} scanned in {:.1} s", took.as_secs_f32()));
    t
}

impl App {
    pub(super) fn open_disk_usage(&mut self) -> Task<Message> {
        let side = self.active;
        self.leave_edits();
        let panel = self.panel(side);
        // An archive under the cursor is scanned like a folder when the panel
        // could enter it (not one on a server).
        let root = match panel.current() {
            Some(e) if e.kind == EntryKind::Dir => e.path.clone(),
            Some(e) if e.kind == EntryKind::File && vfs::is_archive_name(&e.name) && !remote::is_remote(&e.path) => {
                e.path.clone()
            }
            _ => panel.path.clone(),
        };
        if vfs::is_net_listing(&root) {
            self.info("Disk usage scans a folder or a share, not the network list");
            return Task::none();
        }
        let last = self.disk_usage_last.take().filter(|d| d.root == root && d.scanned.is_some());
        let reuse = last.is_some();
        let mut du = last.unwrap_or_else(|| DiskUsage {
            side,
            in_vfs: vfs::in_archive(&root),
            root,
            scanned: None,
            running: None,
            seq: 0,
            progress: None,
            cursor: 0,
            top: 0,
            last_click: None,
            hover: None,
            ext_sel: None,
            ext_top: 0,
            on_exts: false,
            measure: Measure::Size,
            delete_ask: None,
            deleting: None,
            rescanning: None,
            note: None,
            failed: None,
        });
        du.hover = None;
        du.note = None;
        du.side = side;
        if du.is_virtual() {
            du.measure = Measure::Size;
        }
        self.screen = Some(Screen::DiskUsage(Box::new(du)));
        if reuse { Task::none() } else { self.start_disk_usage() }
    }

    /// Closes the tool (cancelling its scan), keeping a finished tree for the
    /// next open.
    fn close_disk_usage(&mut self) {
        if let Some(mut d) = self.screen.take_disk_usage() {
            d.stop();
            // A folder still being scanned again leaves the tree out of date.
            self.disk_usage_last = (d.scanned.is_some() && d.rescanning.is_none()).then_some(d);
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
        d.ext_sel = None;
        d.ext_top = 0;
        d.delete_ask = None;
        d.deleting = None;
        d.rescanning = None;
        d.failed = None;
        let root = d.root.clone();
        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let start = Instant::now();
            let scanned = disk_usage::scan_path(&root, &cancel, |p| {
                let _ = tx.unbounded_send(DuEvent::Progress(p));
            });
            let event = match scanned {
                Ok(tree) => DuEvent::Done(tree.map(|t| TreeBox(Arc::new(t))), start.elapsed()),
                Err(e) => DuEvent::Failed(e.to_string()),
            };
            let _ = tx.unbounded_send(event);
        });
        Task::run(rx, move |e| Message::DiskUsage(DuMsg::Event(seq, e)))
    }

    /// Rows of the list, which gets `du_split` of the space (the treemap the rest).
    fn du_rows(&self) -> usize {
        ((self.window_height - DU_CHROME_H) * du_clamp_split(self.du_split) / ROW_H).floor().max(1.0) as usize
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

    /// F8 / Delete (`permanent`: Shift+Delete) on the item under the cursor:
    /// asks with the panels' delete dialog, which shows over the tool.
    fn du_delete(&mut self, permanent: bool) -> Task<Message> {
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        let Some(s) = &d.scanned else { return Task::none() };
        let Some(&(node, _)) = s.outline.rows.get(d.cursor) else { return Task::none() };
        if d.deleting.is_some() {
            d.note = Some(("Wait for the delete that is running".to_owned(), Tone::Info));
            return Task::none();
        }
        if d.rescanning.is_some() {
            d.note = Some(("Wait for the folder to be scanned again".to_owned(), Tone::Info));
            return Task::none();
        }
        if node == ROOT {
            d.note = Some(("The scanned folder itself isn't deleted from here".to_owned(), Tone::Info));
            return Task::none();
        }
        let path = s.tree.path(&d.root, node);
        d.delete_ask = Some((d.seq, node, path.clone()));
        self.dialog = Some(Dialog::Delete { paths: vec![path], to_trash: !permanent });
        self.dialog_focus = None;
        Task::none()
    }

    /// The delete dialog was confirmed and started job `id` for `paths`: if
    /// that is the tool's delete, it waits for the job (see `du_job_done`).
    pub(super) fn du_delete_started(&mut self, id: u64, paths: &[PathBuf]) {
        let Some(d) = self.screen.disk_usage_mut() else { return };
        let Some((seq, node, path)) = d.delete_ask.take() else { return };
        if paths != std::slice::from_ref(&path) {
            return;
        }
        d.deleting = Some((id, seq, node, path));
        // Not the panel's selection: don't deselect anything there.
        if let Some(job) = self.job(id) {
            job.source_side = None;
        }
    }

    /// Job `id` finished: if it is the tool's delete, takes the item out of
    /// the tree when it is gone from disk. A file still there is unchanged; a
    /// folder still there (failed, cancelled, partly deleted) is scanned
    /// again on its own and put back in (`du_rescanned`), unless it has hard
    /// links counted across its boundary or the tree changed since: then the
    /// tool scans everything again. A closed tool forgets its tree. `summary`
    /// goes in the footer.
    pub(super) fn du_job_done(&mut self, id: u64, summary: Option<(String, Tone)>) -> Task<Message> {
        let rows = self.du_rows();
        self.next_listing += 1;
        let gen = self.next_listing;
        let mine = |d: &DiskUsage| d.deleting.as_ref().is_some_and(|x| x.0 == id);
        let in_screen = self.screen.disk_usage().is_some_and(mine);
        let d = match in_screen {
            true => self.screen.disk_usage_mut(),
            false => self.disk_usage_last.as_mut().filter(|d| mine(d)),
        };
        let Some(d) = d else { return Task::none() };
        let Some((_, seq, node, path)) = d.deleting.take() else { return Task::none() };
        if d.is_virtual() {
            // Whether it's gone is asked on a worker (a server is slow, an
            // archive is read again); `du_rescanned` applies the answer.
            return match (in_screen, seq == d.seq) {
                (true, true) => self.du_start_rescan(node, path, summary),
                (true, false) => self.du_scan_again(summary),
                (false, _) => {
                    self.disk_usage_last = None;
                    Task::none()
                }
            };
        }
        let gone = std::fs::symlink_metadata(&path).is_err();
        let removed = gone && seq == d.seq && d.scanned.as_mut().is_some_and(|s| s.remove(node, gen));
        if removed {
            let len = d.len();
            d.cursor = d.cursor.min(len.saturating_sub(1));
            d.top = visible_start(d.top, d.cursor, rows, len);
            d.hover = None;
            if let Some(s) = &d.scanned {
                d.ext_sel = d.ext_sel.filter(|e| s.ext_order.contains(e));
            }
            d.note = summary;
            Task::none()
        } else if in_screen && !gone && seq == d.seq && d.scanned.as_ref().is_some_and(|s| {
            s.tree.node(node).kind == NodeKind::File && s.tree.surviving(node) == node
        }) {
            // A file that wasn't deleted is as it was.
            d.note = summary;
            Task::none()
        } else if in_screen
            && seq == d.seq
            && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
            && d.scanned.as_ref().is_some_and(|s| {
                s.tree.node(node).kind == NodeKind::Dir && s.tree.surviving(node) == node && !s.tree.links_cross(node)
            })
        {
            self.du_start_rescan(node, path, summary)
        } else if in_screen {
            self.du_scan_again(summary)
        } else {
            self.disk_usage_last = None;
            Task::none()
        }
    }

    /// Scans the whole tree again after a delete, with its summary in the footer.
    fn du_scan_again(&mut self, summary: Option<(String, Tone)>) -> Task<Message> {
        let task = self.start_disk_usage();
        if let Some(d) = self.screen.disk_usage_mut() {
            let what = summary.map_or(String::new(), |(t, _)| format!("{t} \u{b7} "));
            d.note = Some((format!("{what}scanning again"), Tone::Info));
        }
        task
    }

    /// Scans folder `node` (at `path`) again on a worker, keeping the tree on
    /// screen; `du_rescanned` puts the result in. Inside an archive or on a
    /// server `node` may also be a file (kept if it's still there).
    fn du_start_rescan(&mut self, node: u32, path: PathBuf, summary: Option<(String, Tone)>) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        d.stop();
        let cancel = Arc::new(AtomicBool::new(false));
        d.running = Some(cancel.clone());
        d.progress = None;
        let what = summary.as_ref().map_or(String::new(), |(t, _)| format!("{t} \u{b7} "));
        let checking = d.scanned.as_ref().is_some_and(|s| s.tree.node(node).kind == NodeKind::File);
        let doing = if checking { "checking the file" } else { "scanning the folder again" };
        d.note = Some((format!("{what}{doing}"), Tone::Info));
        d.rescanning = Some(Rescanning { seq, node, path: path.clone(), summary });
        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let result = disk_usage::rescan(&path, &cancel, |p| {
                let _ = tx.unbounded_send(DuEvent::Progress(p));
            });
            let _ = tx.unbounded_send(DuEvent::Rescanned(result.map(|r| RescanBox(Arc::new(r)))));
        });
        Task::run(rx, move |e| Message::DiskUsage(DuMsg::Event(seq, e)))
    }

    /// The folder `du_start_rescan` scanned again: replaces its subtree (or
    /// removes it when it is gone), keeping open folders open and the cursor
    /// and the map on the same paths. Scans everything again if that fails.
    fn du_rescanned(&mut self, result: Option<RescanBox>) -> Task<Message> {
        let rows = self.du_rows();
        self.next_listing += 1;
        let gen = self.next_listing;
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        let Some(Rescanning { node, summary, .. }) = d.rescanning.take() else { return Task::none() };
        d.running = None;
        d.progress = None;
        let (Some(result), Some(s)) = (result, d.scanned.as_mut()) else { return self.start_disk_usage() };
        let result = Arc::try_unwrap(result.0).unwrap_or_else(|r| (*r).clone());
        let root = d.root.clone();
        let cursor = s.outline.rows.get(d.cursor).map(|&(n, _)| s.tree.path(&root, n));
        let zoom = s.tree.path(&root, s.zoom);
        // Folders open in the old subtree.
        let mut open = Vec::new();
        let mut stack = vec![node];
        while let Some(i) = stack.pop() {
            if s.outline.is_expanded(i) {
                open.push(s.tree.path(&root, i));
                stack.extend(s.tree.node(i).children.iter().copied());
            }
        }
        if Arc::make_mut(&mut s.tree).splice(node, result).is_none() {
            return self.start_disk_usage();
        }
        s.outline.expanded.resize(s.tree.nodes.len(), false);
        for p in &open {
            let n = s.tree.locate(&root, p);
            if s.tree.path(&root, n) == *p {
                s.outline.expanded[n as usize] = true;
            }
        }
        s.outline.rebuild(&s.tree);
        s.refresh_exts();
        s.zoom = s.tree.locate(&root, &zoom);
        s.gen = gen;
        let tree = s.tree.clone();
        let at = cursor.map_or(ROOT, |p| tree.locate(&root, &p));
        let row = s.outline.reveal(&tree, at).unwrap_or(0);
        let len = s.outline.rows.len();
        d.cursor = row.min(len.saturating_sub(1));
        d.top = visible_start(d.top, d.cursor, rows, len);
        d.hover = None;
        d.ext_sel = d.ext_sel.filter(|e| s.ext_order.contains(e));
        d.note = summary;
        Task::none()
    }

    /// The shell context menu of the item under the cursor (at the mouse).
    fn du_context_menu(&mut self) -> Task<Message> {
        let extended = self.modifiers.shift();
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        let Some(s) = &d.scanned else { return Task::none() };
        let Some(&(node, _)) = s.outline.rows.get(d.cursor) else { return Task::none() };
        if d.is_virtual() {
            d.note = Some(("No shell menu inside archives and on servers".to_owned(), Tone::Info));
            return Task::none();
        }
        let path = s.tree.path(&d.root, node);
        let Some(folder) = path.parent().map(Path::to_path_buf) else {
            d.note = Some(("No context menu for a drive here".to_owned(), Tone::Info));
            return Task::none();
        };
        let items = vec![path];
        window::get_oldest()
            .and_then(move |id| {
                let (folder, items) = (folder.clone(), items.clone());
                window::run_with_handle(id, move |handle| {
                    rc_platform::show_context_menu(native_handle(&handle), &folder, &items, extended, None)
                })
            })
            .map(|result| Message::DiskUsage(DuMsg::ContextDone(result)))
    }

    /// Sets the folder the map shows: `pick(tree, cursor node, zoom)`.
    fn du_zoom(&mut self, pick: impl FnOnce(&Tree, u32, u32) -> u32) {
        let Some(d) = self.screen.disk_usage_mut() else { return };
        let Some(s) = &mut d.scanned else { return };
        let Some(&(node, _)) = s.outline.rows.get(d.cursor) else { return };
        s.zoom = pick(&s.tree, node, s.zoom);
        d.hover = None;
    }

    /// Renders the map as shown (zoomed folder, measure, colours, extension highlight) into a PNG in the
    /// other panel's folder (or the scanned one when that isn't a local folder), on a worker thread. A
    /// tree in an archive or on a server needs the other panel on a local folder.
    fn du_export(&mut self) -> Task<Message> {
        let other = self.screen.disk_usage_mut().map_or(0, |d| 1 - d.side.min(1));
        let other_path = self.panel(other).path.clone();
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        let Some(s) = &d.scanned else { return Task::none() };
        let (tree, zoom, ext_colors, ext_sel) = (s.tree.clone(), s.zoom, s.ext_colors.clone(), d.ext_sel);
        let root = d.root.clone();
        let dir = match () {
            _ if !vfs::in_archive(&other_path) && other_path.is_dir() => other_path,
            _ if !d.is_virtual() => root.clone(),
            _ => {
                let msg = "To save the map, open a local folder in the other panel (it goes there)";
                d.note = Some((msg.to_owned(), Tone::Info));
                return Task::none();
            }
        };
        d.note = Some(("Saving the map\u{2026}".to_owned(), Tone::Info));
        let bg = color_rgba(theme::c().panel);
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let zoom = if (zoom as usize) < tree.nodes.len() { zoom } else { ROOT };
                    let name = if zoom == ROOT {
                        root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    } else {
                        tree.node(zoom).name.to_string()
                    };
                    let mut tiles = Vec::new();
                    let rect = Rect::new(0.0, 0.0, EXPORT_W as f32, EXPORT_H as f32);
                    treemap::layout(&tree, zoom, rect, MAP_MIN_PX, &mut tiles);
                    let paints = map_paints(&tree, &tiles, &ext_colors, ext_sel, bg);
                    let pixels = rasterize(&paints, EXPORT_W, EXPORT_H, bg);
                    let _ = tx.send(export_png(&dir, &name, &pixels));
                });
                rx.await.unwrap_or_else(|_| Err("the export stopped".to_owned()))
            },
            |r| Message::DiskUsage(DuMsg::Exported(r)),
        )
    }

    fn du_toggle_measure(&mut self) {
        self.next_listing += 1;
        let gen = self.next_listing;
        let Some(d) = self.screen.disk_usage_mut() else { return };
        if d.is_virtual() && d.measure == Measure::Size {
            let msg = "Size on disk isn't known inside archives and on servers: the sizes shown are the files' own";
            d.note = Some((msg.to_owned(), Tone::Info));
            return;
        }
        d.measure = if d.measure == Measure::Size { Measure::Alloc } else { Measure::Size };
        let measure = d.measure;
        if let Some(s) = &mut d.scanned {
            s.set_measure(measure, gen);
            d.hover = None;
        }
    }

    /// Moves the extension selection (Tab focus on the extension list).
    fn du_ext_move(&mut self, delta: isize) {
        let rows = self.du_rows();
        let Some(d) = self.screen.disk_usage_mut() else { return };
        let Some(s) = &d.scanned else { return };
        d.ext_sel = ext_step(&s.ext_order, d.ext_sel, delta);
        if let Some(pos) = d.ext_sel.and_then(|e| s.ext_order.iter().position(|&x| x == e)) {
            d.ext_top = visible_start(d.ext_top, pos, rows, s.ext_order.len());
        }
    }

    pub(super) fn disk_usage_update(&mut self, msg: DuMsg) -> Task<Message> {
        let rows = self.du_rows();
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        match msg {
            DuMsg::Event(seq, e) => {
                let rescan = d.rescanning.as_ref().is_some_and(|r| r.seq == seq);
                if seq != d.seq && !rescan {
                    return Task::none();
                }
                match e {
                    DuEvent::Rescanned(result) if rescan => return self.du_rescanned(result),
                    DuEvent::Rescanned(_) => {}
                    DuEvent::Done(..) | DuEvent::Failed(_) if rescan => {}
                    DuEvent::Failed(e) => {
                        d.running = None;
                        d.progress = None;
                        d.failed = Some(e);
                    }
                    DuEvent::Progress(p) => d.progress = Some(p),
                    DuEvent::Done(tree, took) => {
                        d.running = None;
                        d.progress = None;
                        if let Some(TreeBox(tree)) = tree {
                            d.scanned = Some(Scanned::new(tree, took, seq, d.measure));
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
            DuMsg::RightClick(i) => {
                d.last_click = None;
                d.cursor = i.min(d.len().saturating_sub(1));
                return self.du_context_menu();
            }
            DuMsg::MapRightClick(node) => {
                d.last_click = None;
                self.du_outline(|o, t, c| o.reveal(t, node).unwrap_or(c));
                return self.du_context_menu();
            }
            DuMsg::ContextDone(result) => match result {
                Ok(MenuAction::Nothing) => {}
                Ok(MenuAction::Delete) => return self.du_delete(false),
                Ok(MenuAction::Rename) => {
                    d.note = Some(("Rename it in the panel (Ctrl+Enter goes there)".to_owned(), Tone::Info));
                }
                Ok(MenuAction::Invoked(_) | MenuAction::RunInTerminal | MenuAction::RunExternal) => {
                    d.note = Some(("The sizes here may be out of date: F5 scans again".to_owned(), Tone::Info));
                }
                Err(e) => d.note = Some((format!("Context menu failed: {e}"), Tone::Error)),
            },
            DuMsg::ExtClick(id) => {
                d.ext_sel = if d.ext_sel == Some(id) { None } else { Some(id) };
            }
            DuMsg::ExtWheel(lines) => {
                let len = d.scanned.as_ref().map_or(0, |s| s.ext_order.len());
                let delta = (-lines * 3.0).round() as isize;
                d.ext_top = d.ext_top.saturating_add_signed(delta).min(len.saturating_sub(rows));
            }
            DuMsg::ExtScrollTo(start, rows) => {
                let len = d.scanned.as_ref().map_or(0, |s| s.ext_order.len());
                d.ext_top = start.min(len.saturating_sub(rows));
            }
            DuMsg::GoTo => return self.du_go_to(),
            DuMsg::Measure => self.du_toggle_measure(),
            DuMsg::Rescan => return self.start_disk_usage(),
            DuMsg::Exported(result) => {
                d.note = Some(match result {
                    Ok(path) => (format!("Saved {}", path.display()), Tone::Info),
                    Err(e) => (format!("Couldn't save the map: {e}"), Tone::Error),
                });
            }
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
        (
            "F8, Delete",
            "Delete the item (recycle bin; asks first); the tree updates without a rescan (a folder partly left: \
             only it scans again)",
        ),
        ("Shift+Delete", "Delete the item permanently (asks first)"),
        (
            "Shift+F10, Menu key, right click",
            "Shell context menu of the item (list row or map tile; not in archives or on servers)",
        ),
        ("Z", "Map: show only the folder under the cursor (a file: its folder)"),
        (
            "S",
            "Switch between size and size on disk (compressed, sparse and online-only files, cluster rounding; \
             local folders only)",
        ),
        ("E", "Save the map as a PNG (1920x1080) in the other panel's folder"),
        ("Backspace", "Map: one folder level out"),
        ("Shift+Z", "Map: the whole tree again"),
        ("Click on an extension", "Highlight its files on the map (click again: clear)"),
        ("Tab", "Keys move in the extension list / the tree"),
        ("Ctrl+Up, Ctrl+Down", "Give the list less / more height (the map gets the rest; remembered)"),
        ("F5", "Scan again"),
        ("Esc", "Clear the extension highlight; else stop the scan / close"),
    ];

    pub(super) fn disk_usage_key(
        &mut self,
        named: Option<Named>,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
    ) -> Task<Message> {
        let page = self.du_rows().saturating_sub(1).max(1) as isize;
        let Some(d) = self.screen.disk_usage_mut() else { return Task::none() };
        d.note = None;
        if mods.control() && !mods.alt() && !mods.shift() && matches!(named, Some(Named::ArrowUp | Named::ArrowDown)) {
            let step = if named == Some(Named::ArrowDown) { DU_SPLIT_STEP } else { -DU_SPLIT_STEP };
            self.du_split = du_clamp_split(((self.du_split + step) * 100.0).round() / 100.0);
            return Task::none();
        }
        if named == Some(Named::Escape) && d.ext_sel.is_some() {
            d.ext_sel = None;
            return Task::none();
        }
        if d.on_exts {
            let delta = match named {
                Some(Named::ArrowDown) => Some(1),
                Some(Named::ArrowUp) => Some(-1),
                Some(Named::PageDown) => Some(page),
                Some(Named::PageUp) => Some(-page),
                Some(Named::Home) => Some(isize::MIN / 2),
                Some(Named::End) => Some(isize::MAX / 2),
                _ => None,
            };
            if let Some(delta) = delta {
                self.du_ext_move(delta);
                return Task::none();
            }
        }
        match self.keymap.command(key, mods) {
            Some(Command::Delete) => return self.du_delete(false),
            Some(Command::DeletePermanent) => return self.du_delete(true),
            _ => {}
        }
        match named {
            Some(Named::Escape) => return self.disk_usage_update(DuMsg::Close),
            Some(Named::F5) => return self.disk_usage_update(DuMsg::Rescan),
            Some(Named::Tab) => {
                if let Some(d) = self.screen.disk_usage_mut() {
                    d.on_exts = !d.on_exts && d.scanned.is_some();
                }
            }
            Some(Named::F10) if mods.shift() => return self.du_context_menu(),
            Some(Named::ContextMenu) => return self.du_context_menu(),
            Some(Named::Backspace) => self.du_zoom(|t, _, z| zoom_out_target(t, z)),
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
                    "s" | "S" => self.du_toggle_measure(),
                    "e" | "E" => return self.du_export(),
                    "z" | "Z" if mods.shift() => self.du_zoom(|_, _, _| ROOT),
                    "z" | "Z" => self.du_zoom(|t, n, _| zoom_in_target(t, n)),
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
            _ if d.rescanning.is_some() => {
                let path = d.rescanning.as_ref().map_or(String::new(), |r| remote::display(&r.path));
                let t = match &d.progress {
                    Some(p) => format!("Rescanning {path}\u{2026} {} files \u{b7} {}", p.files, format::size(p.bytes)),
                    None => format!("Rescanning {path}\u{2026}"),
                };
                (t, colors.text)
            }
            (_, Some(p)) if d.running.is_some() => {
                let (files, bytes, at) = (p.files, format::size(p.bytes), remote::display(&p.current));
                (format!("Scanning\u{2026} {files} files \u{b7} {bytes} \u{b7} {at}"), colors.text)
            }
            _ if d.running.is_some() => ("Scanning\u{2026}".to_owned(), colors.text),
            (Some(s), _) => (header_text(&s.tree, s.took), colors.text),
            (None, _) if d.failed.is_some() => {
                (format!("Couldn't scan: {}", d.failed.as_deref().unwrap_or_default()), colors.error)
            }
            (None, _) => ("Not scanned (F5 scans)".to_owned(), colors.muted),
        };
        let header = container(
            column![
                row![
                    text("Disk usage").size(16),
                    container(text(remote::display(&d.root)).size(13).color(colors.muted).wrapping(Wrapping::None))
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

        let size_label = if d.measure == Measure::Alloc { "On disk" } else { "Size" };
        let head_cell = |s: &'a str, w: Length, right: bool| {
            let c = container(text(s).size(12).color(colors.muted)).width(w);
            if right { c.align_x(Alignment::End) } else { c }
        };
        let list_header = container(
            row![
                head_cell("Name", Length::Fill, false),
                head_cell(size_label, Length::Fixed(COL_SIZE), true),
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
                    let lines = (start..end).map(|i| self.du_line(s, i, i == d.cursor, !d.on_exts));
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

        let exts: Element<'a, Message> = match &d.scanned {
            Some(s) => {
                let ext_header = container(
                    row![
                        Space::with_width(Length::Fixed(EXT_SWATCH)),
                        head_cell("Type", Length::Fill, false),
                        head_cell(size_label, Length::Fixed(EXT_SIZE), true),
                        head_cell("%", Length::Fixed(EXT_PCT), true),
                        head_cell("Files", Length::Fixed(EXT_COUNT), true),
                    ]
                    .spacing(8),
                )
                .padding([3, 10])
                .style(filled(colors.header));
                let body = responsive(move |size| {
                    let rows = (size.height / ROW_H).floor().max(1.0) as usize;
                    let len = s.ext_order.len();
                    let start = d.ext_top.min(len.saturating_sub(rows));
                    let end = (start + rows).min(len);
                    let lines = (start..end).map(|i| self.du_ext_line(s, d, i));
                    let lines = container(Column::with_children(lines).width(Length::Fill))
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .clip(true);
                    scrollbar::attach(lines, start as f32, rows as f32, len as f32, move |at| {
                        Message::DiskUsage(DuMsg::ExtScrollTo(at.round() as usize, rows))
                    })
                });
                let body = container(body).width(Length::Fill).height(Length::Fill).style(filled(colors.panel));
                let body = mouse_area(body)
                    .on_scroll(|delta| {
                        Message::DiskUsage(DuMsg::ExtWheel(match delta {
                            mouse::ScrollDelta::Lines { y, .. } => y,
                            mouse::ScrollDelta::Pixels { y, .. } => y / ROW_H / 3.0,
                        }))
                    });
                column![ext_header, body].width(Length::Fixed(EXT_W)).into()
            }
            None => container(Space::new(Length::Fixed(EXT_W), Length::Fill)).style(filled(colors.panel)).into(),
        };

        let map: Element<'a, Message> = match &d.scanned {
            Some(s) => Element::new(Treemap {
                tree: &s.tree,
                gen: s.gen,
                zoom: s.zoom,
                ext_colors: &s.ext_colors,
                selected: s.outline.rows.get(d.cursor).map(|r| r.0),
                ext_sel: d.ext_sel,
            }),
            None => Space::new(Length::Fill, Length::Fill).into(),
        };
        let map_title = match &d.scanned {
            Some(s) if s.zoom != ROOT => format!(
                "Map: {} \u{b7} Backspace one level out \u{b7} Shift+Z whole tree",
                remote::display(&s.tree.path(&d.root, s.zoom))
            ),
            Some(_) => "Map: whole tree \u{b7} Z on a folder shows only that folder".to_owned(),
            None => "Map".to_owned(),
        };
        let map_header = container(text(map_title).size(12).color(colors.muted).wrapping(Wrapping::None))
            .width(Length::Fill)
            .clip(true)
            .padding([3, 14])
            .style(filled(colors.header));
        let map = container(map).width(Length::Fill).height(Length::Fill).padding(6).style(filled(colors.panel));
        let map = column![map_header, map].height(Length::FillPortion(du_portions(self.du_split).1));

        // The tile under the mouse, else a running delete, a message, the keys.
        let hovered = d.scanned.as_ref().zip(d.hover).filter(|(s, n)| (*n as usize) < s.tree.nodes.len());
        let tone_color = |tone: &Tone| match tone {
            Tone::Info => colors.text,
            Tone::Success => colors.success,
            Tone::Error => colors.error,
        };
        let (note, note_color) = match hovered {
            Some((s, n)) => {
                let node = s.tree.node(n);
                let path = s.tree.path(&d.root, n);
                let text = if let Some(link) = s.tree.hard_link(n) {
                    format!(
                        "{} \u{b7} {} \u{b7} hard link, counted once at {}",
                        remote::display(&path),
                        format::size(s.tree.link_bytes(link)),
                        remote::display(&s.tree.path(&d.root, link.counted))
                    )
                } else if node.kind == NodeKind::Dir && n != ROOT && !node.children.is_empty() {
                    let (shown, bytes) = (remote::display(&path), format::size(s.tree.bytes(n)));
                    format!("{shown} \u{b7} {bytes} \u{b7} {} files", node.files)
                } else {
                    format!("{} \u{b7} {}", remote::display(&path), format::size(s.tree.bytes(n)))
                };
                (text, colors.text)
            }
            None if d.deleting.is_some() => {
                let path = d.deleting.as_ref().map_or(String::new(), |x| remote::display(&x.3));
                (format!("Deleting {path}\u{2026}"), colors.text)
            }
            None if d.note.is_some() => {
                let (t, tone) = d.note.as_ref().expect("checked");
                (t.clone(), tone_color(tone))
            }
            None => (String::new(), colors.muted),
        };
        const HINTS: &[theme::Hint] = &[
            (&["\u{2191}\u{2193}"], "move"),
            (&["\u{2192}", "+"], "expand"),
            (&["\u{2190}", "-"], "collapse"),
            (&["Ctrl+Enter"], "go to it"),
            (&["F8"], "delete"),
            (&["Shift+F10"], "menu"),
            (&["Z"], "zoom map"),
            (&["S"], "size on disk"),
            (&["E"], "map to PNG"),
            (&["Tab"], "extensions"),
            (&["Ctrl+\u{2191}\u{2193}"], "resize"),
            (&["F5"], "scan again"),
            (&["Esc"], "close"),
        ];
        // Archives and servers: no shell menu, no size on disk.
        let hints: Vec<theme::Hint> = match d.is_virtual() {
            true => HINTS.iter().copied().filter(|h| !matches!(h.0, ["Shift+F10"] | ["S"])).collect(),
            false => HINTS.to_vec(),
        };
        let left: Element<'a, Message> = if note.is_empty() {
            theme::key_hints_row(&hints, self.window_width - 340.0)
        } else {
            text(note).size(12).color(note_color).wrapping(Wrapping::None).into()
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
        if d.scanned.is_some() && !d.is_virtual() {
            let label = if d.measure == Measure::Alloc { "Show size" } else { "Show size on disk" };
            buttons = buttons.push(action(label, DuMsg::Measure, false));
        }
        buttons = buttons.push(action("Close", DuMsg::Close, false));
        if d.running.is_none() {
            buttons = buttons.push(action("Scan again", DuMsg::Rescan, true));
        }
        let footer = container(
            row![
                container(left).width(Length::Fill).clip(true),
                Space::with_width(Length::Fixed(8.0)),
                buttons
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(filled(colors.header));

        let list = column![list_header, list].width(Length::Fill);
        let top = row![list, exts].spacing(1).height(Length::FillPortion(du_portions(self.du_split).0));
        container(column![header, top, map, footer])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.bg))
            .into()
    }

    /// Row `i` of the extension list.
    fn du_ext_line<'a>(&'a self, s: &'a Scanned, d: &'a DiskUsage, i: usize) -> Element<'a, Message> {
        let colors = theme::c();
        let id = s.ext_order[i];
        let count = s.tree.exts[id as usize].2;
        let bytes = s.ext_bytes[id as usize];
        let cell = |t: String| text(t).size(13).wrapping(Wrapping::None);
        let swatch = container(Space::new(Length::Fixed(EXT_SWATCH), Length::Fixed(EXT_SWATCH)))
            .style(filled(hex_color(MAP_FILLS[color_slot(&s.ext_colors, id)])));
        let pct = share(bytes, s.tree.bytes(ROOT)) * 100.0;
        let right = |t: String, w: f32, color: Color| {
            container(cell(t).color(color)).width(Length::Fixed(w)).align_x(Alignment::End)
        };
        let content = row![
            swatch,
            container(cell(ext_label(&s.tree, id)).color(colors.text)).width(Length::Fill).clip(true),
            right(format::size(bytes), EXT_SIZE, colors.text),
            right(format!("{pct:.1} %"), EXT_PCT, colors.muted),
            right(count.to_string(), EXT_COUNT, colors.muted),
        ]
        .spacing(8)
        .padding([0, 10])
        .height(ROW_H)
        .align_y(Alignment::Center);
        let bg = if d.ext_sel == Some(id) {
            if d.on_exts { colors.cursor } else { colors.cursor_inactive }
        } else if i % 2 == 1 {
            colors.zebra
        } else {
            colors.panel
        };
        mouse_area(container(content).width(Length::Fill).height(ROW_H).clip(true).style(filled(bg)))
            .on_press(Message::DiskUsage(DuMsg::ExtClick(id)))
            .into()
    }

    /// Row `i` of the outline; `focused`: the keys move in the tree.
    fn du_line<'a>(&'a self, s: &'a Scanned, i: usize, cursor: bool, focused: bool) -> Element<'a, Message> {
        let colors = theme::c();
        let tree = &*s.tree;
        let (node, depth) = s.outline.rows[i];
        let n = tree.node(node);
        let is_dir = n.kind == NodeKind::Dir;
        let color = if is_dir { colors.dir } else { colors.text };
        let cell = |t: String| text(t).size(13).color(color).wrapping(Wrapping::None);

        let arrow: Element<'a, Message> = if is_dir && !n.children.is_empty() {
            // Arrows the UI font can draw (the small triangles came out as boxes).
            let glyph = if s.outline.is_expanded(node) { "\u{2193}" } else { "\u{2192}" };
            mouse_area(container(text(glyph).size(12).color(colors.muted)).width(Length::Fixed(ARROW_W)))
                .on_press(Message::DiskUsage(DuMsg::Toggle(i)))
                .into()
        } else {
            Space::with_width(Length::Fixed(ARROW_W)).into()
        };
        let own = tree.bytes(node);
        let parent = tree.parent(node).map_or(own, |p| tree.bytes(p));
        let frac = share(own, parent);
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
        // A further hard link: its own size, dimmed (it counts 0 bytes).
        let size = match tree.hard_link(node) {
            Some(link) => cell(format::size(tree.link_bytes(link))).color(colors.muted),
            None => cell(format::size(own)),
        };

        let content = row![
            Space::with_width(Length::Fixed(INDENT * depth as f32)),
            arrow,
            name,
            container(size).width(Length::Fixed(COL_SIZE)).align_x(Alignment::End),
            row![pct, bar].spacing(6).align_y(Alignment::Center),
            container(cell(files).color(colors.muted)).width(Length::Fixed(COL_FILES)).align_x(Alignment::End),
        ]
        .spacing(10)
        .padding([0, 14])
        .height(ROW_H)
        .align_y(Alignment::Center);
        let bg = if cursor {
            if focused { colors.cursor } else { colors.cursor_inactive }
        } else if i % 2 == 1 {
            colors.zebra
        } else {
            colors.panel
        };
        mouse_area(container(content).width(Length::Fill).height(ROW_H).clip(true).style(filled(bg)))
            .on_press(Message::DiskUsage(DuMsg::Click(i)))
            .on_right_press(Message::DiskUsage(DuMsg::RightClick(i)))
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
/// All fills by [`color_slot`]: the palette, other files, folders.
const MAP_FILLS: [u32; MAP_TOP_EXTS + 2] = {
    let mut all = [MAP_OTHER; MAP_TOP_EXTS + 2];
    let mut k = 0;
    while k < MAP_TOP_EXTS {
        all[k] = MAP_PALETTE[k];
        k += 1;
    }
    all[MAP_TOP_EXTS + 1] = MAP_FOLDER;
    all
};

/// Index into [`MAP_FILLS`] of a node with extension id `ext`.
fn color_slot(ext_colors: &[u8], ext: u16) -> usize {
    if ext == disk_usage::EXT_DIR {
        return MAP_TOP_EXTS + 1;
    }
    match ext_colors.get(ext as usize) {
        Some(&k) if k != MAP_GREY => k as usize,
        _ => MAP_TOP_EXTS,
    }
}

fn hex_color(hex: u32) -> Color {
    Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Palette index per extension id (`MAP_GREY` for the rest): the
/// `MAP_TOP_EXTS` extensions with the most bytes, the "other" bucket excluded.
fn ext_colors(exts: &[(Box<str>, u64, u32)], bytes: &[u64]) -> Vec<u8> {
    let mut order: Vec<usize> =
        (0..exts.len()).filter(|&i| i != disk_usage::EXT_OTHER as usize && bytes[i] > 0).collect();
    order.sort_by(|&a, &b| bytes[b].cmp(&bytes[a]).then(a.cmp(&b)));
    let mut colors = vec![MAP_GREY; exts.len()];
    for (k, &i) in order.iter().take(MAP_TOP_EXTS).enumerate() {
        colors[i] = k as u8;
    }
    colors
}

const EXPORT_W: u32 = 1920;
const EXPORT_H: u32 = 1080;

/// One rectangle to paint, in pixels, with opaque colours.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Paint {
    rect: Rect,
    fill: [u8; 4],
    /// `None`: no border (tiles under 3 px).
    border: Option<[u8; 4]>,
}

fn color_rgba(c: Color) -> [u8; 4] {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [b(c.r), b(c.g), b(c.b), 255]
}

/// `fg` over opaque `bg` with `alpha` (0..=1).
fn blend(fg: [u8; 4], bg: [u8; 4], alpha: f32) -> [u8; 4] {
    let m = |f: u8, b: u8| (f as f32 * alpha + b as f32 * (1.0 - alpha)).round() as u8;
    [m(fg[0], bg[0]), m(fg[1], bg[1]), m(fg[2], bg[2]), 255]
}

/// The map's tiles with the colours the on-screen widget gives them (dimmed when another extension is
/// highlighted).
fn map_paints(tree: &Tree, tiles: &[Tile], ext_colors: &[u8], ext_sel: Option<u16>, bg: [u8; 4]) -> Vec<Paint> {
    let fills = MAP_FILLS.map(|hex| {
        let c = color_rgba(hex_color(hex));
        (c, [(c[0] as f32 * 0.6) as u8, (c[1] as f32 * 0.6) as u8, (c[2] as f32 * 0.6) as u8, 255])
    });
    tiles
        .iter()
        .map(|t| {
            let ext = tree.node(t.node).ext_id;
            let (mut fill, mut border) = fills[color_slot(ext_colors, ext)];
            if ext_sel.is_some_and(|e| e != ext) {
                fill = blend(fill, bg, 0.15);
                border = blend(border, bg, 0.15);
            }
            let bordered = t.rect.w >= 3.0 && t.rect.h >= 3.0;
            Paint { rect: t.rect, fill, border: bordered.then_some(border) }
        })
        .collect()
}

/// RGBA pixels (`w` x `h`): `bg`, then each rectangle (edges rounded to whole pixels, so neighbours don't
/// overlap) filled, with a 1 px border inside its edge.
fn rasterize(paints: &[Paint], w: u32, h: u32, bg: [u8; 4]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(w as usize * h as usize * 4);
    for _ in 0..w as usize * h as usize {
        buf.extend_from_slice(&bg);
    }
    let edge = |v: f32, max: u32| (v.round().max(0.0) as u32).min(max);
    for p in paints {
        let (x0, x1) = (edge(p.rect.x, w), edge(p.rect.x + p.rect.w, w));
        let (y0, y1) = (edge(p.rect.y, h), edge(p.rect.y + p.rect.h, h));
        for y in y0..y1 {
            for x in x0..x1 {
                let on_edge = x == x0 || x == x1 - 1 || y == y0 || y == y1 - 1;
                let c = match p.border {
                    Some(b) if on_edge => b,
                    _ => p.fill,
                };
                let i = (y as usize * w as usize + x as usize) * 4;
                buf[i..i + 4].copy_from_slice(&c);
            }
        }
    }
    buf
}

fn encode_png(pixels: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(pixels).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Writes `treemap-<name>-<yyyymmdd-hhmmss>.png` into `dir` (never over an existing file).
fn export_png(dir: &Path, name: &str, pixels: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;
    let data = encode_png(pixels, EXPORT_W, EXPORT_H)?;
    let name: String = name.chars().map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c }).collect();
    let name = if name.trim().is_empty() { "map" } else { name.trim() };
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    for n in 0..100 {
        let suffix = if n == 0 { String::new() } else { format!("-{n}") };
        let path = dir.join(format!("treemap-{name}-{stamp}{suffix}.png"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                return f.write_all(&data).and_then(|_| f.flush()).map(|_| path.clone()).map_err(|e| {
                    let _ = std::fs::remove_file(&path);
                    e.to_string()
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("no free file name".to_owned())
}

/// When the tiles must be laid out again: another size or another tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MapKey {
    w: u32,
    h: u32,
    gen: u64,
    /// The folder shown.
    zoom: u32,
}

impl MapKey {
    fn new(size: Size, gen: u64, zoom: u32) -> Self {
        Self { w: size.width.round().max(0.0) as u32, h: size.height.round().max(0.0) as u32, gen, zoom }
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

/// The treemap of a scanned tree (or of the folder `zoom`): one quad per
/// tile, coloured by extension, with the selected item outlined and, when an
/// extension is selected, the other tiles dimmed. Tiles are laid out only
/// when the size, the tree or the zoom changes (kept in the widget state).
struct Treemap<'a> {
    tree: &'a Tree,
    gen: u64,
    zoom: u32,
    ext_colors: &'a [u8],
    selected: Option<u32>,
    ext_sel: Option<u16>,
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
        let zoom = if (self.zoom as usize) < self.tree.nodes.len() { self.zoom } else { ROOT };
        let key = MapKey::new(node.size(), self.gen, zoom);
        if state.key != Some(key) {
            state.key = Some(key);
            state.tiles.clear();
            state.sel = None;
            state.hover = None;
            let rect = Rect::new(0.0, 0.0, key.w as f32, key.h as f32);
            treemap::layout(self.tree, zoom, rect, MAP_MIN_PX, &mut state.tiles);
        }
        // Items outside the folder shown aren't outlined.
        let selected = self.selected.filter(|&n| is_within(self.tree, n, zoom));
        if state.sel.map(|s| s.0) != selected {
            state.sel = selected.map(|n| (n, node_rect(self.tree, &state.tiles, n)));
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
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                let Some(p) = at else { return event::Status::Ignored };
                if let Some(i) = hit(&state.tiles, p.x, p.y, state.hover) {
                    shell.publish(Message::DiskUsage(DuMsg::MapRightClick(state.tiles[i].node)));
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
        // (fill, border) by `color_slot`.
        let fills = MAP_FILLS.map(|hex| {
            let c = hex_color(hex);
            (c, Color::from_rgb(c.r * 0.6, c.g * 0.6, c.b * 0.6))
        });
        let quad = |r: Rect| Rectangle { x: b.x + r.x, y: b.y + r.y, width: r.w, height: r.h };
        renderer.fill_quad(renderer::Quad { bounds: b, ..renderer::Quad::default() }, Background::Color(colors.panel));
        for t in &state.tiles {
            let ext = self.tree.node(t.node).ext_id;
            let (mut fill, mut border) = fills[color_slot(self.ext_colors, ext)];
            if self.ext_sel.is_some_and(|e| e != ext) {
                fill.a = 0.15;
                border.a = 0.15;
            }
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

    #[test]
    fn rasterize_fills_borders_and_size() {
        let (bg, fill, border) = ([1, 2, 3, 255], [200, 100, 50, 255], [120, 60, 30, 255]);
        let paints = [
            Paint { rect: Rect::new(0.0, 0.0, 10.0, 8.0), fill, border: Some(border) },
            Paint { rect: Rect::new(10.0, 0.0, 2.0, 2.0), fill, border: None },
        ];
        let buf = rasterize(&paints, 16, 10, bg);
        assert_eq!(buf.len(), 16 * 10 * 4);
        let px = |x: usize, y: usize| buf[(y * 16 + x) * 4..(y * 16 + x) * 4 + 4].to_vec();
        assert_eq!(px(5, 4), fill);
        assert_eq!(px(0, 4), border);
        assert_eq!(px(9, 7), border);
        assert_eq!(px(5, 0), border);
        assert_eq!(px(1, 1), fill);
        assert_eq!(px(10, 0), fill, "no border on a tiny tile");
        assert_eq!(px(12, 0), bg);
        assert_eq!(px(5, 9), bg);
    }

    #[test]
    fn png_round_trips_size() {
        let pixels = rasterize(&[], 7, 5, [9, 8, 7, 255]);
        let data = encode_png(&pixels, 7, 5).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(data));
        let mut reader = decoder.read_info().unwrap();
        let mut out = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut out).unwrap();
        assert_eq!((info.width, info.height), (7, 5));
        assert_eq!(&out[..info.buffer_size()], &pixels[..]);
    }

    #[test]
    fn export_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("rc-treemap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let px = vec![0u8; (EXPORT_W * EXPORT_H * 4) as usize];
        let a = export_png(&dir, "a:b", &px).unwrap();
        let b = export_png(&dir, "a:b", &px).unwrap();
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("treemap-a_b-"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_is_clamped_and_portioned() {
        assert_eq!(du_clamp_split(0.5), 0.5);
        assert_eq!(du_clamp_split(0.0), 0.2);
        assert_eq!(du_clamp_split(1.0), 0.8);
        assert_eq!(du_clamp_split(f32::NAN), DU_DEFAULT_SPLIT);
        assert_eq!(du_portions(0.5), (500, 500));
        assert_eq!(du_portions(0.3), (300, 700));
        assert_eq!(du_portions(-4.0), (200, 800));
        assert_eq!(du_portions(9.0), (800, 200));
    }

    /// root(0) -> a(1) [a1(3), a2(4) -> x(6)], b(2) [b1(5)]; files are leaves.
    fn tree() -> Tree {
        let node = |name: &str, size: u64, dir: bool, parent: u32, children: Vec<u32>| Node {
            name: name.into(),
            size,
            alloc: size,
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
            ext_alloc: vec![0, 0],
            ..Tree::default()
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
        let c = ext_colors(&exts, &exts.iter().map(|e| e.1).collect::<Vec<_>>());
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
    fn tiles_are_laid_out_again_on_a_new_size_tree_or_zoom_only() {
        let k = MapKey::new(Size::new(800.0, 300.0), 7, 0);
        assert_eq!(k, MapKey::new(Size::new(800.2, 299.8), 7, 0));
        assert_ne!(k, MapKey::new(Size::new(801.0, 300.0), 7, 0));
        assert_ne!(k, MapKey::new(Size::new(800.0, 300.0), 8, 0));
        assert_ne!(k, MapKey::new(Size::new(800.0, 300.0), 7, 4));
    }

    #[test]
    fn zoom_goes_into_folders_and_out_one_level() {
        let t = tree();
        assert_eq!(zoom_in_target(&t, 1), 1, "a folder");
        assert_eq!(zoom_in_target(&t, 3), 1, "a file: its folder");
        assert_eq!(zoom_in_target(&t, ROOT), ROOT);
        assert_eq!(zoom_out_target(&t, 4), 1);
        assert_eq!(zoom_out_target(&t, 1), ROOT);
        assert_eq!(zoom_out_target(&t, ROOT), ROOT);
    }

    #[test]
    fn removing_an_item_updates_the_outline_and_the_zoom() {
        let mut s = Scanned::new(Arc::new(tree()), Duration::ZERO, 1, Measure::Size);
        s.outline.expand(&s.tree, 1);
        assert_eq!(nodes(&s.outline), [0, 1, 3, 4, 2]);
        s.zoom = 4;
        assert!(s.remove(3, 2));
        assert_eq!((nodes(&s.outline), s.zoom, s.gen), (vec![0, 2, 1, 4], 4, 2), "a (10) went below b (40)");
        assert_eq!(s.tree.root().size, 50);
        // The zoomed folder goes: the map shows the nearest folder left.
        assert!(s.remove(4, 3));
        assert_eq!((nodes(&s.outline), s.zoom), (vec![0, 2, 1], 1));
        assert!(s.remove(1, 4));
        assert_eq!((nodes(&s.outline), s.zoom), (vec![0, 2], ROOT));
        assert!(!s.remove(ROOT, 5), "never the root");
        assert_eq!(s.gen, 4);
    }

    #[test]
    fn extension_list_is_by_bytes_without_empty_ones() {
        let exts: Vec<(Box<str>, u64, u32)> = vec![
            ("".into(), 5, 1),
            ("*".into(), 0, 0),
            ("txt".into(), 30, 2),
            ("rs".into(), 30, 1),
            ("md".into(), 0, 0),
            ("png".into(), 50, 1),
        ];
        let order = ext_order(&exts, &exts.iter().map(|e| e.1).collect::<Vec<_>>());
        assert_eq!(order, [5, 2, 3, 0]);
        assert_eq!(ext_step(&order, None, 1), Some(5));
        assert_eq!(ext_step(&order, None, -1), Some(0));
        assert_eq!(ext_step(&order, None, isize::MIN / 2), Some(5));
        assert_eq!(ext_step(&order, None, isize::MAX / 2), Some(0));
        assert_eq!(ext_step(&order, Some(2), 1), Some(3));
        assert_eq!(ext_step(&order, Some(2), -5), Some(5));
        assert_eq!(ext_step(&order, Some(3), 9), Some(0));
        assert_eq!(ext_step(&[], None, 1), None);
        let t = Tree { exts, ..Tree::default() };
        let labels = (ext_label(&t, 0), ext_label(&t, 1), ext_label(&t, 5));
        assert_eq!(labels, ("(none)".into(), "other".into(), ".png".into()));
        // Percent of the total.
        assert_eq!(share(50, 200) * 100.0, 25.0);
    }

    #[test]
    fn list_swatches_use_the_map_colours() {
        let colors = vec![MAP_GREY, MAP_GREY, 0, 3];
        assert_eq!(MAP_FILLS[color_slot(&colors, 2)], MAP_PALETTE[0]);
        assert_eq!(MAP_FILLS[color_slot(&colors, 3)], MAP_PALETTE[3]);
        assert_eq!(MAP_FILLS[color_slot(&colors, 1)], MAP_OTHER);
        assert_eq!(MAP_FILLS[color_slot(&colors, EXT_DIR)], MAP_FOLDER);
    }

    #[test]
    fn header_mentions_hard_links_counted_once() {
        use rc_core::disk_usage::HardLink;
        let mut t = tree();
        assert!(!header_text(&t, Duration::ZERO).contains("hard link"));
        t.hard_links.insert(6, HardLink { size: 10, alloc: 10, counted: 3 });
        assert!(header_text(&t, Duration::ZERO).contains(" \u{b7} 1 hard link counted once"));
        t.hard_links.insert(5, HardLink { size: 40, alloc: 40, counted: 3 });
        assert!(header_text(&t, Duration::ZERO).contains(" \u{b7} 2 hard links counted once"));
    }

    #[test]
    fn shares_of_the_parent() {
        assert_eq!(share(0, 0), 0.0);
        assert_eq!(share(25, 100), 0.25);
    }
}
