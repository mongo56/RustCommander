use crate::entry::{Entry, EntryKind};
use crate::fs::list_dir;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

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
    Back,
    Forward,
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
    pub sort: Sort,
    pub show_hidden: bool,
    /// Quick filter (Ctrl+S): only entries matching it are in `entries`.
    filter: String,
    /// The whole sorted listing while a filter is active.
    unfiltered: Option<Vec<Entry>>,
    /// Folders visited before / after the current one (Alt+Left/Right).
    back: Vec<Visit>,
    forward: Vec<Visit>,
}

/// A history entry: the folder and the entry the cursor was on.
#[derive(Debug, Clone)]
struct Visit {
    path: PathBuf,
    focus: Option<String>,
}

/// Most folders kept in each history direction.
const HISTORY_LIMIT: usize = 100;

impl Panel {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let mut panel = Self {
            path: path.into(),
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            wheel_anchor: None,
            selected: HashSet::new(),
            sort: Sort::default(),
            show_hidden: false,
            filter: String::new(),
            unfiltered: None,
            back: Vec::new(),
            forward: Vec::new(),
        };
        panel.reload()?;
        Ok(panel)
    }

    /// Re-reads the current folder, keeping cursor and selection by name.
    pub fn reload(&mut self) -> io::Result<()> {
        let entries = list_dir(&self.path, self.show_hidden)?;
        self.apply_listing(entries);
        Ok(())
    }

    /// Replaces the entries with a fresh listing of the current folder (e.g.
    /// read in the background), keeping cursor and selection by name. Hidden
    /// entries are dropped unless the panel shows them, so one listing made
    /// with `show_hidden = true` serves every panel on that folder.
    pub fn apply_listing(&mut self, mut entries: Vec<Entry>) {
        if !self.show_hidden {
            entries.retain(|e| !e.hidden);
        }
        let focus = self.current().map(|e| e.name.clone());
        let old_cursor = self.cursor;
        sort_entries(&mut entries, self.sort);
        self.set_listing(entries);
        let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.selected.retain(|n| names.contains(n.as_str()));
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
            match list_dir(&path, true) {
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
            Nav::Back => self.back.last().map(|v| v.path.clone()),
            Nav::Forward => self.forward.last().map(|v| v.path.clone()),
        }
    }

    /// The history target of `nav` can't be listed (deleted, drive
    /// removed): drops it so the next attempt tries the one before. Returns
    /// false if there is nothing to retry.
    pub fn forget_target(&mut self, nav: &Nav) -> bool {
        match nav {
            Nav::To { .. } => false,
            Nav::Back => self.back.pop().is_some(),
            Nav::Forward => self.forward.pop().is_some(),
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
            Nav::Back | Nav::Forward => {
                let (from, to) = if *nav == Nav::Back {
                    (&mut self.back, &mut self.forward)
                } else {
                    (&mut self.forward, &mut self.back)
                };
                let Some(target) = from.pop() else { return };
                push_limited(to, leaving);
                target
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
        self.go(Nav::Back)
    }

    pub fn go_forward(&mut self) -> io::Result<()> {
        self.go(Nav::Forward)
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
        // A new folder starts unfiltered, like in TC.
        self.filter.clear();
        self.set_listing(entries);
        self.selected.clear();
        self.scroll = 0;
        self.wheel_anchor = None;
        self.cursor = 0;
        self.focus_name(focus);
    }

    /// Up one level, with the cursor on the folder we came out of.
    pub fn parent_nav(&self) -> Option<Nav> {
        let parent = self.path.parent()?.to_path_buf();
        let focus = self.path.file_name().map(|n| n.to_string_lossy().into_owned());
        Some(Nav::To { path: parent, focus })
    }

    pub fn root_nav(&self) -> Option<Nav> {
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

    /// Adjusts `scroll` so the cursor lies within `rows` visible rows.
    pub fn ensure_visible(&mut self, rows: usize) {
        self.wheel_anchor = None;
        self.scroll = visible_start(self.scroll, self.cursor, rows, self.entries.len());
    }

    /// Scrolls the view by `delta` rows without moving the cursor.
    pub fn scroll_by(&mut self, delta: isize, rows: usize) {
        let start = self.view_start(rows);
        let max = self.entries.len().saturating_sub(rows.max(1));
        self.scroll = (start as isize + delta).clamp(0, max as isize) as usize;
        self.wheel_anchor = Some(self.cursor);
    }

    /// First row to draw when `rows` rows fit on screen.
    pub fn view_start(&self, rows: usize) -> usize {
        let len = self.entries.len();
        if self.wheel_anchor == Some(self.cursor) {
            self.scroll.min(len.saturating_sub(rows.max(1)))
        } else {
            visible_start(self.scroll, self.cursor, rows, len)
        }
    }

    /// Puts the cursor on the entry called `name`, if present.
    pub fn focus(&mut self, name: &str) {
        self.focus_name(Some(name));
    }

    /// Moves the cursor to `name`; if not found the cursor keeps its row.
    fn focus_name(&mut self, name: Option<&str>) {
        if let Some(i) = name.and_then(|n| self.entries.iter().position(|e| e.name == n)) {
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
                !e.is_parent() && e.name.to_lowercase().starts_with(&prefix)
            })
    }

    /// Short label for a tab: the folder name, or the whole path for roots.
    pub fn title(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
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

    pub fn select_all(&mut self) {
        self.selected = self
            .entries
            .iter()
            .filter(|e| !e.is_parent())
            .map(|e| e.name.clone())
            .collect();
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

    /// (selected files+dirs, total files+dirs, selected bytes, total bytes)
    pub fn summary(&self) -> (usize, usize, u64, u64) {
        let mut total = 0;
        let mut total_bytes = 0;
        let mut sel_bytes = 0;
        for e in self.entries.iter().filter(|e| !e.is_parent()) {
            total += 1;
            total_bytes += e.size;
            if self.is_selected(e) {
                sel_bytes += e.size;
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
fn wildcard(p: &[char], s: &[char]) -> bool {
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

    #[test]
    fn visible_start_keeps_cursor_on_screen() {
        assert_eq!(visible_start(0, 5, 10, 100), 0);
        assert_eq!(visible_start(0, 15, 10, 100), 6);
        assert_eq!(visible_start(20, 15, 10, 100), 15);
        assert_eq!(visible_start(95, 99, 10, 100), 90);
        assert_eq!(visible_start(3, 2, 10, 5), 0);
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
