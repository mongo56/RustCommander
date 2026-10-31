use crate::entry::{Entry, EntryKind};
use crate::fs::list_dir;
use std::cmp::Ordering;
use std::collections::HashSet;
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

/// What happened when the user pressed Enter on an entry.
#[derive(Debug)]
pub enum Activated {
    /// Navigated into a directory (or up).
    Navigated,
    /// The entry is a file; the caller decides how to open it.
    File(PathBuf),
    Nothing,
}

/// One file panel: a folder listing plus cursor, selection and scroll state.
#[derive(Debug, Clone)]
pub struct Panel {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    /// Index of the first visible row.
    pub scroll: usize,
    /// Selected entries, by name (survives re-sorting and refreshes).
    selected: HashSet<String>,
    pub sort: Sort,
    pub show_hidden: bool,
}

impl Panel {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let mut panel = Self {
            path: path.into(),
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            selected: HashSet::new(),
            sort: Sort::default(),
            show_hidden: false,
        };
        panel.reload()?;
        Ok(panel)
    }

    /// Re-reads the current folder, keeping cursor and selection by name.
    pub fn reload(&mut self) -> io::Result<()> {
        let focus = self.current().map(|e| e.name.clone());
        let old_cursor = self.cursor;
        let mut entries = list_dir(&self.path, self.show_hidden)?;
        sort_entries(&mut entries, self.sort);
        self.entries = entries;
        let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.selected.retain(|n| names.contains(n.as_str()));
        // If the cursor entry vanished (deleted/moved), stay at the same row.
        self.cursor = old_cursor;
        self.focus_name(focus.as_deref());
        Ok(())
    }

    /// Changes folder. On error the panel is left untouched.
    pub fn navigate(&mut self, path: PathBuf, focus: Option<&str>) -> io::Result<()> {
        let mut entries = list_dir(&path, self.show_hidden)?;
        sort_entries(&mut entries, self.sort);
        self.path = path;
        self.entries = entries;
        self.selected.clear();
        self.scroll = 0;
        self.cursor = 0;
        self.focus_name(focus);
        Ok(())
    }

    pub fn go_parent(&mut self) -> io::Result<()> {
        let Some(parent) = self.path.parent().map(Path::to_path_buf) else {
            return Ok(());
        };
        // Put the cursor on the folder we just came out of.
        let came_from = self.path.file_name().map(|n| n.to_string_lossy().into_owned());
        self.navigate(parent, came_from.as_deref())
    }

    pub fn go_root(&mut self) -> io::Result<()> {
        match self.path.ancestors().last() {
            Some(root) if root != self.path => self.navigate(root.to_path_buf(), None),
            _ => Ok(()),
        }
    }

    pub fn activate(&mut self) -> io::Result<Activated> {
        let Some(entry) = self.current() else { return Ok(Activated::Nothing) };
        match entry.kind {
            EntryKind::Parent => self.go_parent().map(|_| Activated::Navigated),
            EntryKind::Dir => {
                let path = entry.path.clone();
                self.navigate(path, None).map(|_| Activated::Navigated)
            }
            EntryKind::File => Ok(Activated::File(entry.path.clone())),
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
        self.scroll = visible_start(self.scroll, self.cursor, rows, self.entries.len());
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
        self.cursor = 0;
        self.focus_name(focus.as_deref());
    }

    pub fn toggle_hidden(&mut self) -> io::Result<()> {
        self.show_hidden = !self.show_hidden;
        self.reload()
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
        assert!(matches!(panel.activate().unwrap(), Activated::Navigated));
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

        std::fs::remove_dir_all(&root).unwrap();
    }
}
