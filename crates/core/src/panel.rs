use crate::entry::{Entry, EntryKind};
use crate::vfs;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Ext,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self { key: SortKey::Name, descending: false }
    }
}

/// What pressing Enter on an entry asks for.
#[derive(Debug)]
pub enum Activated {
    /// A directory (or `..`): the caller performs the navigation, possibly
    /// listing the folder in the background first.
    Navigate(Nav),
    /// The entry is a file; the caller decides how to open it.
    File(PathBuf),
    Nothing,
}

/// A change of folder. Callers list the target (`Panel::nav_target`) and
/// then commit with `Panel::arrive`, so slow folders can be read off the UI
/// thread; `Panel::go` does both synchronously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nav {
    /// Go to `path`, putting the cursor on `focus` if present.
    To { path: PathBuf, focus: Option<String> },
    /// Back / forward this many steps (at least 1) in the history; the
    /// folders stepped over move to the other direction, as if stepped one
    /// by one.
    Back(usize),
    Forward(usize),
}

impl Nav {
    pub fn to(path: impl Into<PathBuf>) -> Self {
        Nav::To { path: path.into(), focus: None }
    }
}

/// One file panel: a folder listing plus cursor, selection and scroll state.
#[derive(Debug, Clone)]
pub struct Panel {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    /// Index of the first visible row.
    pub scroll: usize,
    /// Set by mouse-wheel scrolling: while the cursor stays on this row the
    /// view keeps `scroll` as is, even if the cursor is off screen.
    wheel_anchor: Option<usize>,
    /// Selected entries, by name (survives re-sorting and refreshes).
    selected: HashSet<String>,
    /// Entry where range selection (Shift+click / Shift+arrows) starts.
    anchor: Option<String>,
    pub sort: Sort,
    pub show_hidden: bool,
    /// Quick filter (Ctrl+S): only entries matching it are in `entries`.
    filter: String,
    /// The whole sorted listing while a filter is active.
    unfiltered: Option<Vec<Entry>>,
    /// Folders visited before / after the current one (Alt+Left/Right).
    back: Vec<Visit>,
    forward: Vec<Visit>,
    /// Folder sizes computed on request (Space), by name; `None` while
    /// counting. Kept across refreshes, cleared on navigation.
    dir_sizes: HashMap<String, Option<u64>>,
    /// Search results fed into the panel (Ctrl+F, TC's "feed to listbox"):
    /// what was searched for. `entries` then hold the hits, named by their
    /// path relative to `path` (the search root); refreshes re-check them
    /// on disk instead of listing the folder, and any navigation ends it.
    results: Option<String>,
    /// Thumbnail view (Ctrl+Shift+F5) instead of the list. The grid's
    /// layout comes from the UI ([`Panel::set_grid`]).
    pub thumbnails: bool,
    /// Thumbnail grid: (columns, fully visible rows). While `thumbnails` is
    /// on, `scroll` counts grid rows and the `rows` arguments of the scroll
    /// methods are ignored in favour of this.
    grid: (usize, usize),
}

/// A history entry: the folder and the entry the cursor was on.
#[derive(Debug, Clone)]
struct Visit {
    path: PathBuf,
    focus: Option<String>,
}

/// The results label of the branch view (all files of a folder tree).
pub const BRANCH_LABEL: &str = "branch view";

/// Most folders kept in each history direction.
const HISTORY_LIMIT: usize = 100;

/// The visit `n` steps into a history stack (its top is one step away).
fn step_target(stack: &[Visit], n: usize) -> Option<&Visit> {
    stack.len().checked_sub(n.max(1)).map(|i| &stack[i])
}

/// Drops the visit `n` steps away, so the same `n` next tries the one
/// beyond it. False if there is none.
fn forget_step(stack: &mut Vec<Visit>, n: usize) -> bool {
    match stack.len().checked_sub(n.max(1)) {
        Some(i) => {
            stack.remove(i);
            true
        }
        None => false,
    }
}

impl Panel {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let mut panel = Self::unlisted(path);
        panel.reload()?;
        Ok(panel)
    }

    /// A panel on `path` with no entries yet: nothing is read (the listing
    /// comes later through [`Panel::apply_listing`]).
    pub fn unlisted(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            wheel_anchor: None,
            selected: HashSet::new(),
            anchor: None,
            sort: Sort::default(),
            show_hidden: false,
            filter: String::new(),
            unfiltered: None,
            back: Vec::new(),
            forward: Vec::new(),
            dir_sizes: HashMap::new(),
            results: None,
            thumbnails: false,
            grid: (1, 1),
        }
    }

    /// Re-reads the current folder, keeping cursor and selection by name.
    pub fn reload(&mut self) -> io::Result<()> {
        if self.results.is_some() {
            self.refresh_results();
            return Ok(());
        }
        let entries = vfs::list(&self.path, self.show_hidden)?;
        self.apply_listing(entries);
        Ok(())
    }

    /// Replaces the entries with a fresh listing of the current folder (e.g.
    /// read in the background), keeping cursor and selection by name. Hidden
    /// entries are dropped unless the panel shows them, so one listing made
    /// with `show_hidden = true` serves every panel on that folder.
    pub fn apply_listing(&mut self, mut entries: Vec<Entry>) {
        if self.results.is_some() {
            self.refresh_results();
            return;
        }
        if !self.show_hidden {
            entries.retain(|e| !e.hidden);
        }
        let focus = self.current().map(|e| e.name.clone());
        let old_cursor = self.cursor;
        sort_entries(&mut entries, self.sort);
        self.set_listing(entries);
        let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.selected.retain(|n| names.contains(n.as_str()));
        self.dir_sizes.retain(|n, _| names.contains(n.as_str()));
        // If the cursor entry vanished (deleted/moved), stay at the same row.
        self.cursor = old_cursor;
        self.focus_name(focus.as_deref());
    }

    /// Changes folder and records it in the history. On error the panel is
    /// left untouched.
    pub fn navigate(&mut self, path: PathBuf, focus: Option<&str>) -> io::Result<()> {
        self.go(Nav::To { path, focus: focus.map(str::to_owned) })
    }

    /// Performs `nav` synchronously. History steps skip folders that no
    /// longer exist.
    pub fn go(&mut self, nav: Nav) -> io::Result<()> {
        let mut last_err = None;
        while let Some(path) = self.nav_target(&nav) {
            match vfs::list(&path, true) {
                Ok(entries) => {
                    self.arrive(&nav, entries);
                    return Ok(());
                }
                Err(e) => {
                    if !self.forget_target(&nav) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        last_err.map_or(Ok(()), Err)
    }

    /// The folder `nav` leads to; `None` if the history is empty.
    pub fn nav_target(&self, nav: &Nav) -> Option<PathBuf> {
        match nav {
            Nav::To { path, .. } => Some(path.clone()),
            Nav::Back(n) => step_target(&self.back, *n).map(|v| v.path.clone()),
            Nav::Forward(n) => step_target(&self.forward, *n).map(|v| v.path.clone()),
        }
    }

    /// The history target of `nav` can't be listed (deleted, drive
    /// removed): drops it so the next attempt tries the one before. Returns
    /// false if there is nothing to retry.
    pub fn forget_target(&mut self, nav: &Nav) -> bool {
        match nav {
            Nav::To { .. } => false,
            Nav::Back(n) => forget_step(&mut self.back, *n),
            Nav::Forward(n) => forget_step(&mut self.forward, *n),
        }
    }

    /// Commits `nav` with a listing of its target folder (made with or
    /// without hidden entries) and records it in the history.
    pub fn arrive(&mut self, nav: &Nav, entries: Vec<Entry>) {
        let leaving = self.visit();
        let target = match nav {
            Nav::To { path, focus } => {
                if *path != self.path {
                    push_limited(&mut self.back, leaving);
                    self.forward.clear();
                }
                Visit { path: path.clone(), focus: focus.clone() }
            }
            Nav::Back(n) | Nav::Forward(n) => {
                let (from, to) = if matches!(nav, Nav::Back(_)) {
                    (&mut self.back, &mut self.forward)
                } else {
                    (&mut self.forward, &mut self.back)
                };
                if step_target(from, *n).is_none() {
                    return;
                }
                push_limited(to, leaving);
                for _ in 1..*n {
                    let skipped = from.pop().expect("checked by step_target");
                    push_limited(to, skipped);
                }
                from.pop().expect("checked by step_target")
            }
        };
        self.load(target.path, target.focus.as_deref(), entries);
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Goes to the previous folder in the history, skipping folders that no
    /// longer exist.
    pub fn go_back(&mut self) -> io::Result<()> {
        self.go(Nav::Back(1))
    }

    pub fn go_forward(&mut self) -> io::Result<()> {
        self.go(Nav::Forward(1))
    }

    /// The whole history for the history list (Alt+Down), most recent first:
    /// the forward folders, the current one (at the returned index), then
    /// the back folders.
    pub fn history(&self) -> (Vec<&Path>, usize) {
        let mut list: Vec<&Path> = self.forward.iter().map(|v| v.path.as_path()).collect();
        let current = list.len();
        list.push(&self.path);
        list.extend(self.back.iter().rev().map(|v| v.path.as_path()));
        (list, current)
    }

    /// The navigation to entry `i` of `history()`; `None` for the current
    /// folder or past the end.
    pub fn history_nav(&self, i: usize) -> Option<Nav> {
        let f = self.forward.len();
        if i < f {
            Some(Nav::Forward(f - i))
        } else if i > f && i - f <= self.back.len() {
            Some(Nav::Back(i - f))
        } else {
            None
        }
    }

    fn visit(&self) -> Visit {
        Visit { path: self.path.clone(), focus: self.current().map(|e| e.name.clone()) }
    }

    /// Makes `path` the current folder, showing `entries`.
    fn load(&mut self, path: PathBuf, focus: Option<&str>, mut entries: Vec<Entry>) {
        if !self.show_hidden {
            entries.retain(|e| !e.hidden);
        }
        sort_entries(&mut entries, self.sort);
        self.path = path;
        self.results = None;
        // A new folder starts unfiltered, like in TC.
        self.filter.clear();
        self.set_listing(entries);
        self.selected.clear();
        self.anchor = None;
        self.dir_sizes.clear();
        self.scroll = 0;
        self.wheel_anchor = None;
        self.cursor = 0;
        self.focus_name(focus);
    }

    /// Shows search hits (named relative to `root`) instead of a folder
    /// listing; `label` says what was searched for. Recorded in the history
    /// like a folder change, so Alt+Left returns to where the panel was.
    /// `..` (and Backspace) lead to the search root's real listing.
    pub fn show_results(&mut self, root: PathBuf, label: String, hits: Vec<Entry>) {
        let leaving = self.visit();
        push_limited(&mut self.back, leaving);
        self.forward.clear();
        let mut entries = vec![Entry::parent(root.clone())];
        entries.extend(hits);
        self.load(root, None, entries);
        self.results = Some(label);
        // On the first hit rather than `..`.
        self.cursor = usize::from(self.entries.len() > 1);
    }

    /// What the search results shown were searched for, if the panel shows
    /// search results.
    pub fn results(&self) -> Option<&str> {
        self.results.as_deref()
    }

    /// Search results: drops hits that no longer exist and updates the
    /// size/date of the rest (one metadata read each).
    fn refresh_results(&mut self) {
        let show_hidden = self.show_hidden;
        let check = |list: &mut Vec<Entry>| {
            list.retain_mut(|e| {
                if e.is_parent() {
                    return true;
                }
                if e.hidden && !show_hidden {
                    return false;
                }
                match std::fs::metadata(&e.path) {
                    Ok(m) => {
                        e.kind = if m.is_dir() { EntryKind::Dir } else { EntryKind::File };
                        e.size = if m.is_dir() { 0 } else { m.len() };
                        e.modified = m.modified().ok().map(chrono::DateTime::<chrono::Local>::from);
                        true
                    }
                    Err(_) => false,
                }
            });
        };
        let focus = self.current().map(|e| e.name.clone());
        let old_cursor = self.cursor;
        match &mut self.unfiltered {
            Some(all) => {
                check(all);
                let all = std::mem::take(all);
                self.set_listing(all);
            }
            None => check(&mut self.entries),
        }
        let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.selected.retain(|n| names.contains(n.as_str()));
        self.dir_sizes.retain(|n, _| names.contains(n.as_str()));
        self.cursor = old_cursor;
        self.focus_name(focus.as_deref());
    }

    /// A search hit was renamed in place: shows it under its new path.
    /// Returns its new name in the panel. Does nothing outside results.
    pub fn result_renamed(&mut self, from: &Path, to: &Path) -> Option<String> {
        self.results.as_ref()?;
        let name = to.strip_prefix(&self.path).ok()?.to_string_lossy().into_owned();
        let lists = self.entries.iter_mut().chain(self.unfiltered.iter_mut().flatten());
        let mut found = false;
        for e in lists.filter(|e| e.path == from) {
            if self.selected.remove(&e.name) {
                self.selected.insert(name.clone());
            }
            e.path = to.to_path_buf();
            e.name = name.clone();
            found = true;
        }
        found.then_some(name)
    }

    /// Up one level, with the cursor on the folder we came out of. From
    /// search results: the search root itself.
    pub fn parent_nav(&self) -> Option<Nav> {
        if self.results.is_some() {
            return Some(Nav::to(self.path.clone()));
        }
        // A server's root has no parent (its path's parent is `sftp:`).
        if crate::remote::split(&self.path).is_some_and(|(_, remote)| remote == "/") {
            return None;
        }
        // A share's root: up to the server's share list, cursor on the share.
        if let Some((server, share)) = vfs::unc_share_parent(&self.path) {
            return Some(Nav::To { path: server, focus: Some(share) });
        }
        // A server's share list: up to the Network list, cursor on the
        // server (its `Path::parent` would be `\`).
        if let Some(server) = vfs::unc_server_only(&self.path) {
            return Some(Nav::To { path: vfs::network_path(), focus: Some(server.to_owned()) });
        }
        // The Network list is the top.
        if vfs::is_network(&self.path) {
            return None;
        }
        let parent = self.path.parent()?.to_path_buf();
        let focus = self.path.file_name().map(|n| n.to_string_lossy().into_owned());
        Some(Nav::To { path: parent, focus })
    }

    pub fn root_nav(&self) -> Option<Nav> {
        if let Some((key, remote)) = crate::remote::split(&self.path) {
            return (remote != "/").then(|| Nav::to(crate::remote::path_of(&key, "/")));
        }
        if vfs::is_net_listing(&self.path) {
            return None;
        }
        let root = self.path.ancestors().last()?;
        (root != self.path).then(|| Nav::to(root))
    }

    pub fn go_parent(&mut self) -> io::Result<()> {
        self.parent_nav().map_or(Ok(()), |nav| self.go(nav))
    }

    pub fn go_root(&mut self) -> io::Result<()> {
        self.root_nav().map_or(Ok(()), |nav| self.go(nav))
    }

    pub fn activate(&self) -> Activated {
        let Some(entry) = self.current() else { return Activated::Nothing };
        match entry.kind {
            EntryKind::Parent => self.parent_nav().map_or(Activated::Nothing, Activated::Navigate),
            EntryKind::Dir => Activated::Navigate(Nav::to(entry.path.clone())),
            // Archives open like folders (also ones inside archives), not
            // on servers.
            EntryKind::File if vfs::is_archive_name(&entry.name) && !crate::remote::is_remote(&entry.path) => {
                Activated::Navigate(Nav::to(entry.path.clone()))
            }
            EntryKind::File => Activated::File(entry.path.clone()),
        }
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    // ---- cursor -----------------------------------------------------------

    pub fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            return;
        }
        let max = self.entries.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
    }

    pub fn set_cursor(&mut self, index: usize) {
        self.cursor = index.min(self.entries.len().saturating_sub(1));
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }

    /// Sets the thumbnail grid's layout: columns and fully visible rows.
    pub fn set_grid(&mut self, cols: usize, rows: usize) {
        self.grid = (cols.max(1), rows.max(1));
    }

    /// The thumbnail grid's (columns, visible rows).
    pub fn grid(&self) -> (usize, usize) {
        self.grid
    }

    /// (columns, visible rows) of the current view: the grid in thumbnail
    /// view, one column of `rows` in the list.
    fn layout(&self, rows: usize) -> (usize, usize) {
        if self.thumbnails { self.grid } else { (1, rows.max(1)) }
    }

    /// Rows of the view: entries in the list, grid rows in thumbnail view.
    fn row_count(&self, cols: usize) -> usize {
        self.entries.len().div_ceil(cols)
    }

    /// Adjusts `scroll` so the cursor lies within `rows` visible rows.
    pub fn ensure_visible(&mut self, rows: usize) {
        let (cols, rows) = self.layout(rows);
        self.wheel_anchor = None;
        self.scroll = visible_start(self.scroll, self.cursor / cols, rows, self.row_count(cols));
    }

    /// Scrolls the view by `delta` rows without moving the cursor.
    pub fn scroll_by(&mut self, delta: isize, rows: usize) {
        let start = self.view_start(rows);
        let (cols, rows) = self.layout(rows);
        let max = self.row_count(cols).saturating_sub(rows);
        self.scroll = (start as isize + delta).clamp(0, max as isize) as usize;
        self.wheel_anchor = Some(self.cursor);
    }

    /// Scrolls the view to start at row `start` (scrollbar drag) without
    /// moving the cursor, like [`Panel::scroll_by`].
    pub fn scroll_to(&mut self, start: usize, rows: usize) {
        let (cols, rows) = self.layout(rows);
        self.scroll = start.min(self.row_count(cols).saturating_sub(rows));
        self.wheel_anchor = Some(self.cursor);
    }

    /// First row to draw when `rows` rows fit on screen (in thumbnail view:
    /// the first grid row, `rows` being ignored).
    pub fn view_start(&self, rows: usize) -> usize {
        let (cols, rows) = self.layout(rows);
        let len = self.row_count(cols);
        if self.wheel_anchor == Some(self.cursor) {
            self.scroll.min(len.saturating_sub(rows))
        } else {
            visible_start(self.scroll, self.cursor / cols, rows, len)
        }
    }

    /// The entry `dx` columns and `dy` rows from the cursor in the
    /// thumbnail grid (see [`grid_move`]).
    pub fn grid_target(&self, dx: isize, dy: isize) -> usize {
        grid_move(self.cursor, self.entries.len(), self.grid.0, dx, dy)
    }

    /// Puts the cursor on the entry called `name`, if present.
    pub fn focus(&mut self, name: &str) {
        self.focus_name(Some(name));
    }

    /// Moves the cursor to `name` (else to a name differing only in case,
    /// as `\\NAS` typed `\\nas`); if not found the cursor keeps its row.
    fn focus_name(&mut self, name: Option<&str>) {
        let at = |n: &str| {
            let entries = &self.entries;
            entries.iter().position(|e| e.name == n).or_else(|| entries.iter().position(|e| e.name.eq_ignore_ascii_case(n)))
        };
        if let Some(i) = name.and_then(at) {
            self.cursor = i;
        }
        self.set_cursor(self.cursor);
    }

    /// Index of the next entry whose name starts with `prefix` (case-
    /// insensitive), searching from `from` in the given direction and
    /// wrapping around. `..` never matches.
    pub fn find_prefix(&self, prefix: &str, from: usize, forward: bool) -> Option<usize> {
        let prefix = prefix.to_lowercase();
        let len = self.entries.len();
        if len == 0 || prefix.is_empty() {
            return None;
        }
        (0..len)
            .map(|step| {
                if forward {
                    (from + step) % len
                } else {
                    (from + len - step % len) % len
                }
            })
            .find(|&i| {
                let e = &self.entries[i];
                !e.is_parent() && e.file_name().to_lowercase().starts_with(&prefix)
            })
    }

    /// Short label for a tab: the folder name, or the whole path for roots.
    pub fn title(&self) -> String {
        if let Some(label) = &self.results {
            return if label == BRANCH_LABEL { "Branch view" } else { "Search results" }.into();
        }
        if let Some((key, remote)) = crate::remote::split(&self.path) {
            return if remote == "/" { key.split_once("://").map_or(key.clone(), |(_, server)| server.to_owned()) } else {
                remote.rsplit('/').next().unwrap_or_default().to_owned()
            };
        }
        vfs::folder_label(&self.path)
    }

    // ---- selection --------------------------------------------------------

    pub fn is_selected(&self, entry: &Entry) -> bool {
        self.selected.contains(&entry.name)
    }

    pub fn toggle_select_current(&mut self) {
        let Some(entry) = self.current() else { return };
        if entry.is_parent() {
            return;
        }
        let name = entry.name.clone();
        if !self.selected.remove(&name) {
            self.selected.insert(name);
        }
    }

    /// Makes the cursor entry the start of later range selections.
    pub fn mark_anchor(&mut self) {
        self.anchor = self.current().map(|e| e.name.clone());
    }

    /// Row where range selections start: the anchor, else the cursor.
    pub fn anchor_index(&self) -> usize {
        let found = self.anchor.as_ref().and_then(|a| self.entries.iter().position(|e| &e.name == a));
        found.unwrap_or(self.cursor).min(self.entries.len().saturating_sub(1))
    }

    /// Plain click press: moves the cursor to `index` and makes it the anchor.
    /// The selection is dropped, except when `index` is a selected item (it
    /// may be about to be dragged); then it returns true and the release
    /// should call `deselect_all` if no drag started.
    pub fn click_at(&mut self, index: usize) -> bool {
        let on_selected = self.entries.get(index).is_some_and(|e| !e.is_parent() && self.is_selected(e));
        if !on_selected {
            self.deselect_all();
        }
        self.set_cursor(index);
        self.mark_anchor();
        on_selected
    }

    /// Ctrl+click: toggles the entry at `index`, moves the cursor there and
    /// makes it the anchor.
    pub fn toggle_at(&mut self, index: usize) {
        self.set_cursor(index);
        self.toggle_select_current();
        self.mark_anchor();
    }

    /// Shift+click / Shift+arrows: moves the cursor to `index` and selects
    /// the rows between the anchor and it (`..` excluded). Without `keep`
    /// the range replaces the selection; with it the range is added to it.
    /// The anchor stays.
    pub fn select_range_to(&mut self, index: usize, keep: bool) {
        if self.entries.is_empty() {
            return;
        }
        let anchor = self.anchor_index();
        if self.anchor.is_none() {
            self.mark_anchor_at(anchor);
        }
        self.set_cursor(index);
        let (lo, hi) = (anchor.min(self.cursor), anchor.max(self.cursor));
        if !keep {
            self.selected.clear();
        }
        let names = self.entries[lo..=hi].iter().filter(|e| !e.is_parent()).map(|e| e.name.clone());
        self.selected.extend(names);
    }

    fn mark_anchor_at(&mut self, index: usize) {
        self.anchor = self.entries.get(index).map(|e| e.name.clone());
    }

    pub fn select_all(&mut self) {
        self.selected = self
            .entries
            .iter()
            .filter(|e| !e.is_parent())
            .map(|e| e.name.clone())
            .collect();
    }

    /// Replaces the selection with the named entries (unknown names and
    /// `..` are ignored).
    pub fn set_selection(&mut self, names: impl IntoIterator<Item = String>) {
        let known: std::collections::HashSet<&str> =
            self.entries.iter().filter(|e| !e.is_parent()).map(|e| e.name.as_str()).collect();
        let names: HashSet<String> = names.into_iter().filter(|n| known.contains(n.as_str())).collect();
        self.selected = names;
    }

    pub fn deselect_all(&mut self) {
        self.selected.clear();
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// The entries an operation (copy, delete, ...) should act on: the
    /// selection, or the cursor entry when nothing is selected.
    pub fn targets(&self) -> Vec<&Entry> {
        if self.selected.is_empty() {
            self.current().filter(|e| !e.is_parent()).into_iter().collect()
        } else {
            self.entries.iter().filter(|e| self.is_selected(e)).collect()
        }
    }

    /// The computed size of folder `name`: `None` if never asked for,
    /// `Some(None)` while counting.
    pub fn dir_size(&self, name: &str) -> Option<Option<u64>> {
        self.dir_sizes.get(name).copied()
    }

    /// Marks folder `name` as being counted; false if its size is already
    /// known or being counted.
    pub fn start_dir_size(&mut self, name: &str) -> bool {
        if self.dir_sizes.contains_key(name) {
            return false;
        }
        self.dir_sizes.insert(name.to_owned(), None);
        true
    }

    pub fn set_dir_size(&mut self, name: &str, size: u64) {
        self.dir_sizes.insert(name.to_owned(), Some(size));
    }

    /// Forgets sizes still being counted (the counts were stopped).
    pub fn drop_pending_dir_sizes(&mut self) {
        self.dir_sizes.retain(|_, size| size.is_some());
    }

    /// Size shown for `e`: file size, or a computed folder size (else 0).
    pub fn size_of(&self, e: &Entry) -> u64 {
        match e.kind {
            EntryKind::Dir => self.dir_sizes.get(&e.name).copied().flatten().unwrap_or(0),
            _ => e.size,
        }
    }

    /// (selected files+dirs, total files+dirs, selected bytes, total bytes)
    pub fn summary(&self) -> (usize, usize, u64, u64) {
        let mut total = 0;
        let mut total_bytes = 0;
        let mut sel_bytes = 0;
        for e in self.entries.iter().filter(|e| !e.is_parent()) {
            total += 1;
            let size = self.size_of(e);
            total_bytes += size;
            if self.is_selected(e) {
                sel_bytes += size;
            }
        }
        (self.selected.len(), total, sel_bytes, total_bytes)
    }

    // ---- sorting / filters ------------------------------------------------

    /// Sorting by the active key again flips the direction, like clicking a
    /// column header twice.
    pub fn sort_by(&mut self, key: SortKey) {
        if self.sort.key == key {
            self.sort.descending = !self.sort.descending;
        } else {
            self.sort = Sort { key, descending: false };
        }
        let focus = self.current().map(|e| e.name.clone());
        sort_entries(&mut self.entries, self.sort);
        if let Some(all) = &mut self.unfiltered {
            sort_entries(all, self.sort);
        }
        self.cursor = 0;
        self.focus_name(focus.as_deref());
    }

    /// The quick filter text ("" when off).
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Entries in the folder, ignoring the quick filter (hidden ones are
    /// still left out unless shown).
    pub fn unfiltered_len(&self) -> usize {
        self.unfiltered.as_ref().unwrap_or(&self.entries).len()
    }

    /// Shows only entries matching `filter` (see [`filter_matches`]); ""
    /// shows everything. The cursor stays on its entry if it remains
    /// visible, and entries that get hidden are deselected so operations
    /// never touch files the user can't see.
    pub fn set_filter(&mut self, filter: &str) {
        if filter == self.filter {
            return;
        }
        let focus = self.current().map(|e| e.name.clone());
        let all = self.unfiltered.take().unwrap_or_else(|| std::mem::take(&mut self.entries));
        self.filter = filter.to_owned();
        self.set_listing(all);
        let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.selected.retain(|n| names.contains(n.as_str()));
        self.cursor = 0;
        self.focus_name(focus.as_deref());
        self.wheel_anchor = None;
    }

    /// Stores a sorted listing, applying the quick filter.
    fn set_listing(&mut self, all: Vec<Entry>) {
        if self.filter.is_empty() {
            self.unfiltered = None;
            self.entries = all;
        } else {
            self.entries =
                all.iter().filter(|e| e.is_parent() || filter_matches(&self.filter, &e.name)).cloned().collect();
            self.unfiltered = Some(all);
        }
    }

    pub fn toggle_hidden(&mut self) -> io::Result<()> {
        self.show_hidden = !self.show_hidden;
        self.reload()
    }
}

fn push_limited(stack: &mut Vec<Visit>, visit: Visit) {
    if stack.last().is_some_and(|v| v.path == visit.path) {
        stack.pop();
    }
    stack.push(visit);
    if stack.len() > HISTORY_LIMIT {
        stack.remove(0);
    }
}

/// First visible row so that `cursor` is on screen, moving as little as
/// possible from `scroll`.
pub fn visible_start(scroll: usize, cursor: usize, rows: usize, len: usize) -> usize {
    let rows = rows.max(1);
    let mut start = scroll;
    if cursor < start {
        start = cursor;
    } else if cursor >= start + rows {
        start = cursor + 1 - rows;
    }
    // Don't leave empty space at the bottom when the list could fill it.
    start.min(len.saturating_sub(rows))
}

/// Moves `cursor` in a grid of `cols` columns holding `len` entries
/// (row-major, the last row maybe partial): `dx` steps through the entries
/// in order (wrapping across rows), `dy` moves whole rows in the same
/// column, stopping at the first / last row; a column missing from a
/// partial last row lands on the last entry.
pub fn grid_move(cursor: usize, len: usize, cols: usize, dx: isize, dy: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let cols = cols.max(1);
    let last = len - 1;
    let cursor = (cursor.min(last) as isize + dx).clamp(0, last as isize) as usize;
    if dy == 0 {
        return cursor;
    }
    let last_row = (last / cols) as isize;
    let row = (cursor / cols) as isize + dy;
    let row = row.clamp(0, last_row) as usize;
    (row * cols + cursor % cols).min(last)
}

/// `..` first, then folders, then files. Folders are always ordered by name
/// unless sorting by date (same as Total Commander).
/// Quick-filter match, case-insensitive. `filter` holds one or more
/// patterns separated by `;` (any may match). A pattern with `*` or `?` is
/// a wildcard over the whole name (`*.rs`); otherwise it matches anywhere
/// in the name (`main` matches `domain.rs`).
pub fn filter_matches(filter: &str, name: &str) -> bool {
    let name: Vec<char> = name.to_lowercase().chars().collect();
    filter.split(';').map(str::trim).filter(|p| !p.is_empty()).any(|p| {
        let p: Vec<char> = p.to_lowercase().chars().collect();
        if p.iter().any(|&c| c == '*' || c == '?') {
            wildcard(&p, &name)
        } else {
            name.windows(p.len()).any(|w| w == p.as_slice())
        }
    })
}

/// `*` = any run of characters, `?` = exactly one.
pub(crate) fn wildcard(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

fn sort_entries(entries: &mut [Entry], sort: Sort) {
    entries.sort_by(|a, b| {
        let rank = |e: &Entry| match e.kind {
            EntryKind::Parent => 0,
            EntryKind::Dir => 1,
            EntryKind::File => 2,
        };
        rank(a).cmp(&rank(b)).then_with(|| {
            let key = if a.kind == EntryKind::Dir && matches!(sort.key, SortKey::Ext | SortKey::Size) {
                SortKey::Name
            } else {
                sort.key
            };
            let ord = match key {
                SortKey::Name => cmp_name(&a.name, &b.name),
                SortKey::Ext => cmp_name(a.extension(), b.extension())
                    .then_with(|| cmp_name(&a.name, &b.name)),
                SortKey::Size => a.size.cmp(&b.size).then_with(|| cmp_name(&a.name, &b.name)),
                SortKey::Modified => a.modified.cmp(&b.modified).then_with(|| cmp_name(&a.name, &b.name)),
            };
            if sort.descending { ord.reverse() } else { ord }
        })
    });
}

fn cmp_name(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn up_from_a_share_root_is_the_server() {
        let up = |p: &str| match Panel::unlisted(p).parent_nav() {
            Some(Nav::To { path, focus }) => Some((path, focus)),
            _ => None,
        };
        assert_eq!(up(r"\\nas\data\"), Some((PathBuf::from(r"\\nas"), Some("data".into()))));
        assert_eq!(up(r"\\nas\data\sub"), Some((PathBuf::from(r"\\nas\data\"), Some("sub".into()))));
        assert_eq!(up(r"\\nas"), Some((vfs::network_path(), Some("nas".into()))));
        assert_eq!(up("net:"), None);
        assert!(Panel::unlisted(r"\\nas").root_nav().is_none());
        assert!(Panel::unlisted("net:").root_nav().is_none());
    }

    #[test]
    fn visible_start_keeps_cursor_on_screen() {
        assert_eq!(visible_start(0, 5, 10, 100), 0);
        assert_eq!(visible_start(0, 15, 10, 100), 6);
        assert_eq!(visible_start(20, 15, 10, 100), 15);
        assert_eq!(visible_start(95, 99, 10, 100), 90);
        assert_eq!(visible_start(3, 2, 10, 5), 0);
    }

    #[test]
    fn scroll_to_moves_the_view_not_the_cursor() {
        let root = std::env::temp_dir().join(format!("rc-core-scroll-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..50 {
            std::fs::write(root.join(format!("f{i:02}.txt")), b"").unwrap();
        }
        let mut p = Panel::open(&root).unwrap();
        let len = p.entries.len();
        p.set_cursor(3);
        p.scroll_to(20, 10);
        assert_eq!((p.view_start(10), p.cursor), (20, 3));
        // Past the end: the last full page.
        p.scroll_to(1000, 10);
        assert_eq!(p.view_start(10), len - 10);
        // Moving the cursor brings it back into view.
        p.move_cursor(1);
        p.ensure_visible(10);
        assert_eq!(p.view_start(10), 4);

        // Thumbnail view: scrolling counts grid rows of the grid's layout.
        p.thumbnails = true;
        p.set_grid(4, 3);
        let rows = len.div_ceil(4);
        p.set_cursor(len - 1);
        p.ensure_visible(10);
        assert_eq!(p.view_start(10), rows - 3);
        p.scroll_by(-100, 10);
        assert_eq!((p.view_start(10), p.cursor), (0, len - 1));
        p.scroll_to(1000, 10);
        assert_eq!(p.view_start(10), rows - 3);
        p.set_cursor(9); // row 2
        p.ensure_visible(10);
        assert_eq!(p.view_start(10), 2);
        assert_eq!(p.grid_target(0, 1), 13);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn grid_moves_in_two_dimensions() {
        // 10 entries in 4 columns:  0 1 2 3 / 4 5 6 7 / 8 9
        let mv = |cursor, dx, dy| grid_move(cursor, 10, 4, dx, dy);
        assert_eq!(mv(5, 1, 0), 6);
        assert_eq!(mv(3, 1, 0), 4); // wraps to the next row
        assert_eq!(mv(0, -1, 0), 0);
        assert_eq!(mv(9, 1, 0), 9);
        assert_eq!(mv(5, 0, -1), 1);
        assert_eq!(mv(1, 0, -1), 1); // first row: stays
        assert_eq!(mv(1, 0, 1), 5);
        assert_eq!(mv(5, 0, 1), 9);
        assert_eq!(mv(6, 0, 1), 9); // partial last row: the last entry
        assert_eq!(mv(9, 0, 1), 9);
        assert_eq!(mv(2, 0, 100), 9); // page down past the end
        assert_eq!(mv(7, 0, -100), 3); // page up keeps the column
        assert_eq!(grid_move(0, 0, 4, 1, 1), 0);
        assert_eq!(grid_move(3, 5, 1, 0, 1), 4); // one column = a list
    }

    #[test]
    fn click_and_range_selection() {
        let root = std::env::temp_dir().join(format!("rc-core-range-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..8 {
            std::fs::write(root.join(format!("f{i}.txt")), b"").unwrap();
        }
        let mut p = Panel::open(&root).unwrap();
        // Entries: "..", f0..f7 (rows 0..=8).
        let sel = |p: &Panel| -> Vec<String> {
            p.entries.iter().filter(|e| p.is_selected(e)).map(|e| e.name.clone()).collect()
        };
        p.set_cursor(2);
        p.mark_anchor();
        // Shift+click on row 4: rows 2..=4.
        p.select_range_to(4, false);
        assert_eq!(sel(&p), ["f1.txt", "f2.txt", "f3.txt"]);
        assert_eq!(p.cursor, 4);
        // Shift+click on row 3 shrinks it (anchor stays on row 2).
        p.select_range_to(3, false);
        assert_eq!(sel(&p), ["f1.txt", "f2.txt"]);
        // Ctrl+click toggles one and moves the anchor.
        p.toggle_at(7);
        assert_eq!(sel(&p), ["f1.txt", "f2.txt", "f6.txt"]);
        p.toggle_at(7);
        assert_eq!(sel(&p), ["f1.txt", "f2.txt"]);
        // Ctrl+Shift+click adds a range from the anchor (row 7) keeping the rest.
        p.select_range_to(5, true);
        assert_eq!(sel(&p), ["f1.txt", "f2.txt", "f4.txt", "f5.txt", "f6.txt"]);
        // A range over ".." never selects it.
        p.set_cursor(1);
        p.mark_anchor();
        p.select_range_to(0, false);
        assert_eq!(sel(&p), ["f0.txt"]);
        assert_eq!(p.cursor, 0);
        // Plain click on an unselected row drops the selection.
        assert!(!p.click_at(2));
        assert!(sel(&p).is_empty());
        assert_eq!((p.cursor, p.anchor_index()), (2, 2));
        // Plain click on a selected row keeps it until the release.
        p.select_range_to(4, false);
        assert!(p.click_at(3));
        assert_eq!(sel(&p), ["f1.txt", "f2.txt", "f3.txt"]);
        assert_eq!((p.cursor, p.anchor_index()), (3, 3));
        // Clicking ".." drops the selection.
        assert!(!p.click_at(0));
        assert!(sel(&p).is_empty());
        // Without an anchor the range starts at the cursor.
        p.navigate(root.clone(), None).unwrap();
        p.set_cursor(6);
        p.select_range_to(8, false);
        assert_eq!(sel(&p), ["f5.txt", "f6.txt", "f7.txt"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_results_in_the_panel() {
        let root = std::env::temp_dir().join(format!("rc-core-results-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub.d")).unwrap();
        std::fs::write(root.join("top.txt"), b"1").unwrap();
        std::fs::write(root.join("sub.d").join("deep.rs"), b"22").unwrap();
        std::fs::write(root.join("sub.d").join("noext"), b"").unwrap();
        let q = crate::search::Query { root: root.clone(), subfolders: true, ..Default::default() };
        let mut hits = Vec::new();
        crate::search::search(&q, &std::sync::atomic::AtomicBool::new(false), |b| {
            hits.extend(b.hits);
            true
        })
        .unwrap();

        let mut panel = Panel::open(&root).unwrap();
        panel.show_results(root.clone(), "*".into(), hits);
        assert_eq!(panel.results(), Some("*"));
        assert_eq!(panel.title(), "Search results");
        let deep = format!("sub.d{}deep.rs", std::path::MAIN_SEPARATOR);
        let noext = format!("sub.d{}noext", std::path::MAIN_SEPARATOR);
        let names: Vec<_> = panel.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["..", "sub.d", &deep, &noext, "top.txt"]);
        // Not on `..`.
        assert_eq!(panel.cursor, 1);
        let e = &panel.entries[2];
        assert_eq!(e.split_name().1, "rs");
        assert_eq!(e.file_name(), "deep.rs");
        assert_eq!(panel.entries[3].split_name().1, "");
        // Quick search looks at the file name.
        assert_eq!(panel.find_prefix("dee", 0, true), Some(2));

        // Renamed in place: same row, new name; deleted: gone on refresh.
        panel.set_selection([deep.clone()]);
        let renamed = root.join("sub.d").join("deeper.rs");
        std::fs::rename(root.join("sub.d").join("deep.rs"), &renamed).unwrap();
        let new = panel.result_renamed(&root.join("sub.d").join("deep.rs"), &renamed).unwrap();
        assert_eq!(panel.targets()[0].name, new);
        std::fs::remove_file(root.join("top.txt")).unwrap();
        panel.reload().unwrap();
        assert_eq!(panel.entries.len(), 4);
        assert!(panel.results().is_some());

        // `..` leads to the root's real listing, ending the results.
        let nav = panel.parent_nav().unwrap();
        panel.go(nav).unwrap();
        assert_eq!(panel.results(), None);
        assert_eq!(panel.path, root);
        assert_eq!(panel.entries.len(), 2);
        // And history goes back to before the results.
        panel.go_back().unwrap();
        assert_eq!(panel.path, root);
        assert_eq!(panel.results(), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn browses_into_and_out_of_a_zip() {
        let dir = crate::vfs::tests::TempDir::new("panel");
        let zip = dir.0.join("a.zip");
        crate::vfs::tests::make_zip(&zip);
        let mut panel = Panel::open(&dir.0).unwrap();
        panel.focus("a.zip");
        let Activated::Navigate(nav) = panel.activate() else { panic!("zip should open like a folder") };
        panel.go(nav).unwrap();
        assert_eq!(panel.path, zip);
        let names: Vec<&str> = panel.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["..", "sub", "a.txt", "Readme.md"]);

        panel.focus("sub");
        let Activated::Navigate(nav) = panel.activate() else { panic!() };
        panel.go(nav).unwrap();
        assert_eq!(panel.path, zip.join("sub"));
        panel.reload().unwrap();
        assert_eq!(panel.entries.len(), 2);
        panel.go_parent().unwrap();
        assert_eq!(panel.current().unwrap().name, "sub");
        // `..` at the archive root: back in the folder, cursor on the archive.
        panel.go_parent().unwrap();
        assert_eq!(panel.path, dir.0);
        assert_eq!(panel.current().unwrap().name, "a.zip");

        panel.navigate(zip.clone(), None).unwrap();
        panel.focus("a.txt");
        assert!(matches!(panel.activate(), Activated::File(p) if p == zip.join("a.txt")));
    }

    #[test]
    fn history_back_and_forward() {
        let root = std::env::temp_dir().join(format!("rc-core-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a").join("deep")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();

        let mut panel = Panel::open(&root).unwrap();
        assert!(!panel.can_go_back());
        panel.navigate(root.join("a"), None).unwrap();
        panel.navigate(root.join("a").join("deep"), None).unwrap();
        panel.go_parent().unwrap();
        assert_eq!(panel.current().unwrap().name, "deep");

        panel.go_back().unwrap();
        assert_eq!(panel.path, root.join("a").join("deep"));
        panel.go_back().unwrap();
        assert_eq!(panel.path, root.join("a"));
        // The cursor returns to where it was when we left.
        assert_eq!(panel.current().unwrap().name, "..");
        panel.go_forward().unwrap();
        assert_eq!(panel.path, root.join("a").join("deep"));

        // A new navigation drops the forward history.
        panel.navigate(root.join("b"), None).unwrap();
        assert!(!panel.can_go_forward());

        // Vanished folders are skipped.
        std::fs::remove_dir_all(root.join("a")).unwrap();
        panel.go_back().unwrap();
        assert_eq!(panel.path, root);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn history_list_jumps_several_steps() {
        let root = std::env::temp_dir().join(format!("rc-core-histlist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["a", "b", "c", "d"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let p = |d: &str| root.join(d);
        let mut panel = Panel::open(&root).unwrap();
        for d in ["a", "b", "c", "d"] {
            panel.navigate(p(d), None).unwrap();
        }
        panel.go_back().unwrap();
        // Most recent first, the current folder marked.
        let (list, current) = panel.history();
        assert_eq!(list, [p("d"), p("c"), p("b"), p("a"), root.clone()]);
        assert_eq!(current, 1);
        assert_eq!(panel.history_nav(1), None);
        assert_eq!(panel.history_nav(5), None);
        assert_eq!(panel.history_nav(0), Some(Nav::Forward(1)));

        // Three back: as if stepped one by one, so forward holds b, c, d.
        let nav = panel.history_nav(4).unwrap();
        assert_eq!(nav, Nav::Back(3));
        panel.go(nav).unwrap();
        assert_eq!(panel.path, root);
        let (list, current) = panel.history();
        assert_eq!(list, [p("d"), p("c"), p("b"), p("a"), root.clone()]);
        assert_eq!(current, 4);
        panel.go_forward().unwrap();
        assert_eq!(panel.path, p("a"));
        panel.go(Nav::Forward(2)).unwrap();
        assert_eq!(panel.path, p("c"));

        // A vanished target: the next one beyond it is used.
        std::fs::remove_dir_all(p("a")).unwrap();
        panel.go(Nav::Back(2)).unwrap();
        assert_eq!(panel.path, root);
        let (list, _) = panel.history();
        assert_eq!(list, [p("d"), p("c"), p("b"), root.clone()]);
        assert!(panel.go(Nav::Back(1)).is_ok() && panel.path == root);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lists_and_navigates_temp_dir() {
        let root = std::env::temp_dir().join(format!("rc-core-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("b_dir")).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        std::fs::write(root.join("C.rs"), b"fn main() {}").unwrap();

        let mut panel = Panel::open(&root).unwrap();
        let names: Vec<_> = panel.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["..", "b_dir", "a.txt", "C.rs"]);

        panel.set_cursor(1);
        let Activated::Navigate(nav) = panel.activate() else { panic!("not a folder") };
        panel.go(nav).unwrap();
        assert_eq!(panel.path, root.join("b_dir"));

        panel.go_parent().unwrap();
        assert_eq!(panel.current().unwrap().name, "b_dir");

        assert_eq!(panel.find_prefix("c", 0, true), Some(3));
        assert_eq!(panel.find_prefix("B_", 0, true), Some(1));
        assert_eq!(panel.find_prefix("a", 3, true), Some(2)); // wraps
        assert_eq!(panel.find_prefix("a", 1, false), Some(2)); // backwards, wraps
        assert_eq!(panel.find_prefix("zz", 0, true), None);
        assert_eq!(panel.find_prefix(".", 0, true), None); // ".." never matches

        panel.sort_by(SortKey::Size);
        panel.sort_by(SortKey::Size); // descending
        let files: Vec<_> = panel.entries.iter().skip(2).map(|e| e.name.as_str()).collect();
        assert_eq!(files, ["C.rs", "a.txt"]);

        // Quick filter keeps "..", the cursor entry and only visible selections.
        panel.sort_by(SortKey::Name);
        panel.select_all();
        panel.focus("C.rs");
        panel.set_filter("*.RS");
        let names: Vec<_> = panel.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["..", "C.rs"]);
        assert_eq!(panel.current().unwrap().name, "C.rs");
        assert_eq!(panel.selected_count(), 1);
        assert_eq!(panel.unfiltered_len(), 4);
        panel.reload().unwrap(); // refresh keeps the filter
        assert_eq!(panel.entries.len(), 2);
        panel.set_filter("");
        assert_eq!(panel.entries.len(), 4);
        panel.set_filter("dir");
        panel.set_cursor(1);
        let Activated::Navigate(nav) = panel.activate() else { panic!("not a folder") };
        panel.go(nav).unwrap();
        assert_eq!(panel.filter(), ""); // navigation clears it

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn filter_patterns() {
        assert!(filter_matches("main", "Domain.rs"));
        assert!(!filter_matches("main", "lib.rs"));
        assert!(filter_matches("*.rs", "lib.RS"));
        assert!(!filter_matches("*.rs", "lib.rs.bak"));
        assert!(filter_matches("*.txt; *.rs", "lib.rs"));
        assert!(filter_matches("a?c*", "abcdef"));
        assert!(!filter_matches("a?c*", "acdef"));
        assert!(filter_matches("*a*b*", "xxaYYbzz"));
        assert!(!filter_matches(";", "x"));
    }
}
