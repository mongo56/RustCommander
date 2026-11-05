//! Folder synchronisation (TC `cm_SyncDirs`): a recursive comparison of two
//! folder trees, then a plan of what to copy (or delete) in which direction.
//!
//! [`compare_trees`] walks both trees together through a listing function
//! (local folders, archives and servers alike), and gives one [`Row`] per
//! file present on either side; a folder found on one side only is one row
//! (copying it copies all of it). [`default_action`] picks what to do with
//! a row in a [`Mode`]; the only default that deletes anything is
//! "mirror" for an item that exists on the target side only. [`plan`] turns
//! the chosen actions into transfers and deletions for the jobs in `ops`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Local};

use crate::compare::CompareOptions;
use crate::entry::{Entry, EntryKind};
use crate::ops::Transfer;
use crate::rename::name_key;

/// Modification times closer than this count as equal (FAT stores times
/// with 2 s precision), as in `compare`.
pub const TIME_TOLERANCE_MS: i64 = 2000;

/// How the default direction of each row is chosen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Newer wins; items on one side only are copied to the other side;
    /// files that differ with the same date are left alone (conflicts).
    #[default]
    TwoWay,
    /// The right side becomes a copy of the left: everything goes to the
    /// right, items only on the right are deleted.
    Mirror,
    /// Only copies to the right (new and newer files), never deletes.
    UpdateRight,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::TwoWay, Mode::Mirror, Mode::UpdateRight];

    /// Name in `config.toml`.
    pub fn id(self) -> &'static str {
        match self {
            Mode::TwoWay => "two-way",
            Mode::Mirror => "mirror",
            Mode::UpdateRight => "update",
        }
    }

    pub fn from_id(id: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.id().eq_ignore_ascii_case(id.trim()))
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::TwoWay => "Two-way (newer wins)",
            Mode::Mirror => "Mirror left \u{2192} right",
            Mode::UpdateRight => "Update right",
        }
    }
}

/// One side of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: EntryKind,
    /// A file's size; for a folder, the total of the files in it (folders
    /// on one side only are measured, others are 0).
    pub size: u64,
    pub modified: Option<DateTime<Local>>,
    /// Files in it (1 for a file).
    pub files: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Equal,
    LeftOnly,
    RightOnly,
    LeftNewer,
    RightNewer,
    /// Same date but different size or content, or a file on one side and
    /// a folder on the other, or links that differ.
    Different,
}

/// A file (or a folder on one side only) of either tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Path below the roots, `\`-separated (for showing and sorting).
    pub rel: String,
    /// Where it is (or would be) on each side.
    pub left_path: PathBuf,
    pub right_path: PathBuf,
    pub left: Option<Item>,
    pub right: Option<Item>,
    pub state: State,
}

impl Row {
    /// Files on both sides that differ with no newer one.
    pub fn conflict(&self) -> bool {
        self.state == State::Different
    }

    fn both_files(&self) -> bool {
        matches!((&self.left, &self.right), (Some(l), Some(r)) if l.kind == EntryKind::File && r.kind == EntryKind::File)
    }

    /// What can be done with the row, in the order Space cycles through.
    pub fn actions(&self) -> &'static [Action] {
        use Action::*;
        match self.state {
            State::Equal => &[Skip],
            State::LeftOnly => &[ToRight, Skip, DeleteLeft],
            State::RightOnly => &[ToLeft, Skip, DeleteRight],
            State::LeftNewer => &[ToRight, Skip, ToLeft],
            State::RightNewer => &[ToLeft, Skip, ToRight],
            State::Different if self.both_files() => &[Skip, ToRight, ToLeft],
            State::Different => &[Skip],
        }
    }

    /// The action after `current` (Space).
    pub fn next_action(&self, current: Action) -> Action {
        let all = self.actions();
        let i = all.iter().position(|&a| a == current).map_or(0, |i| (i + 1) % all.len());
        all[i]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Skip,
    ToRight,
    ToLeft,
    DeleteRight,
    DeleteLeft,
}

impl Action {
    pub fn glyph(self) -> &'static str {
        match self {
            Action::Skip => "\u{b7}",
            Action::ToRight => "\u{2192}",
            Action::ToLeft => "\u{2190}",
            Action::DeleteRight => "del \u{2192}",
            Action::DeleteLeft => "\u{2190} del",
        }
    }

    pub fn deletes(self) -> bool {
        matches!(self, Action::DeleteLeft | Action::DeleteRight)
    }
}

/// What to do with `row` unless the user changes it.
pub fn default_action(row: &Row, mode: Mode) -> Action {
    use Action::*;
    let file_conflict = row.state == State::Different && row.both_files();
    match (mode, row.state) {
        (_, State::Equal) => Skip,
        (Mode::TwoWay, State::LeftOnly | State::LeftNewer) => ToRight,
        (Mode::TwoWay, State::RightOnly | State::RightNewer) => ToLeft,
        (Mode::TwoWay, State::Different) => Skip,
        (Mode::Mirror, State::LeftOnly | State::LeftNewer | State::RightNewer) => ToRight,
        (Mode::Mirror, State::Different) if file_conflict => ToRight,
        (Mode::Mirror, State::Different) => Skip,
        (Mode::Mirror, State::RightOnly) => DeleteRight,
        (Mode::UpdateRight, State::LeftOnly | State::LeftNewer) => ToRight,
        (Mode::UpdateRight, _) => Skip,
    }
}

/// The result of [`compare_trees`].
#[derive(Debug, Clone, Default)]
pub struct Comparison {
    pub rows: Vec<Row>,
    /// Folders that couldn't be listed (left out of the comparison).
    pub errors: Vec<String>,
    /// Folder pairs compared.
    pub folders: u64,
}

/// What [`compare_trees`] needs from the file systems.
pub trait Source {
    /// The folder's entries, hidden ones included (flagged), `..` optional.
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    /// Whether two files have the same bytes; read errors count as
    /// different, `None` when cancelled.
    fn same_content(&self, a: &Path, b: &Path, cancel: &AtomicBool) -> Option<bool>;
    /// Whether `path` is a link or junction (not descended into).
    fn is_link(&self, path: &Path) -> bool;
    /// Where a link points (links are equal when they point the same way).
    fn link_target(&self, path: &Path) -> Option<PathBuf>;
}

/// Local folders, archives and servers through `vfs`.
pub struct AnySource;

impl Source for AnySource {
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        crate::vfs::list(dir, true)
    }

    fn same_content(&self, a: &Path, b: &Path, cancel: &AtomicBool) -> Option<bool> {
        if !crate::vfs::is_virtual(a) && !crate::vfs::is_virtual(b) {
            return crate::compare::same_content(a, b, cancel);
        }
        let (Some(ha), Some(hb)) = (hash_any(a, cancel)?, hash_any(b, cancel)?) else { return Some(false) };
        Some(ha == hb)
    }

    fn is_link(&self, path: &Path) -> bool {
        !crate::vfs::is_virtual(path) && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
    }

    fn link_target(&self, path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }
}

/// SHA-256 of a file anywhere: `Some(None)` when it can't be read, `None`
/// when cancelled.
fn hash_any(path: &Path, cancel: &AtomicBool) -> Option<Option<Vec<u8>>> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut stopped = false;
    let result = crate::vfs::open(path).and_then(|vfs| {
        vfs.read_files(&[path.to_path_buf()], &mut |_, reader| {
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                if cancel.load(Ordering::Relaxed) {
                    stopped = true;
                    return Ok(false);
                }
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    return Ok(true);
                }
                hasher.update(&buf[..n]);
            }
        })
    });
    if stopped {
        return None;
    }
    Some(result.ok().map(|()| hasher.finalize().to_vec()))
}

/// Compares the trees under `left` and `right`. Hidden entries take part
/// with `opts.show_hidden`; `opts.by_content` compares same-size files byte
/// by byte, `opts.ignore_date` makes dates not matter (no side is newer).
/// `progress` gets each folder pair as it's compared. `Err` when a root
/// can't be listed, `Ok(None)` when cancelled.
pub fn compare_trees(
    left: &Path,
    right: &Path,
    opts: &CompareOptions,
    source: &dyn Source,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> Result<Option<Comparison>, String> {
    let l = source.list(left).map_err(|e| format!("{}: {e}", left.display()))?;
    let r = source.list(right).map_err(|e| format!("{}: {e}", right.display()))?;
    let mut walk = Walk { opts, source, cancel, progress, out: Comparison::default() };
    match walk.pair(left, right, "", l, r) {
        Some(()) => Ok(Some(walk.out)),
        None => Ok(None),
    }
}

struct Walk<'a> {
    opts: &'a CompareOptions,
    source: &'a dyn Source,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(&Path),
    out: Comparison,
}

impl Walk<'_> {
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Entries of a listing by name key, without `..` and (unless shown)
    /// hidden ones.
    fn index(&self, entries: Vec<Entry>) -> BTreeMap<String, Entry> {
        let keep = |e: &Entry| !e.is_parent() && (self.opts.show_hidden || !e.hidden);
        entries.into_iter().filter(keep).map(|e| (name_key(&e.name), e)).collect()
    }

    /// Compares the folders `ldir` and `rdir` (both exist), `rel` below the roots.
    fn pair(&mut self, ldir: &Path, rdir: &Path, rel: &str, l: Vec<Entry>, r: Vec<Entry>) -> Option<()> {
        if self.stopped() {
            return None;
        }
        (self.progress)(ldir);
        self.out.folders += 1;
        let (mut l, mut r) = (self.index(l), self.index(r));
        let mut keys: Vec<String> = l.keys().chain(r.keys()).cloned().collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            if self.stopped() {
                return None;
            }
            let (le, re) = (l.remove(&key), r.remove(&key));
            let name = le.as_ref().or(re.as_ref()).map(|e| e.name.clone()).unwrap_or_default();
            let sub = if rel.is_empty() { name.clone() } else { format!("{rel}\\{name}") };
            let left_path = le.as_ref().map_or_else(|| ldir.join(&name), |e| e.path.clone());
            let right_path = re.as_ref().map_or_else(|| rdir.join(&name), |e| e.path.clone());
            let row = |left: Option<Item>, right: Option<Item>, state: State| Row {
                rel: sub.clone(),
                left_path: left_path.clone(),
                right_path: right_path.clone(),
                left,
                right,
                state,
            };
            match (le, re) {
                (Some(a), None) => {
                    let item = self.item(&a)?;
                    self.out.rows.push(row(Some(item), None, State::LeftOnly));
                }
                (None, Some(b)) => {
                    let item = self.item(&b)?;
                    self.out.rows.push(row(None, Some(item), State::RightOnly));
                }
                (Some(a), Some(b)) if a.kind == EntryKind::Dir && b.kind == EntryKind::Dir => {
                    let (la, lb) = (self.source.is_link(&a.path), self.source.is_link(&b.path));
                    if la || lb {
                        let same = la && lb && self.source.link_target(&a.path) == self.source.link_target(&b.path);
                        let state = if same { State::Equal } else { State::Different };
                        self.out.rows.push(row(Some(plain(&a)), Some(plain(&b)), state));
                        continue;
                    }
                    let lists = (self.source.list(&a.path), self.source.list(&b.path));
                    match lists {
                        (Ok(x), Ok(y)) => self.pair(&a.path, &b.path, &sub, x, y)?,
                        (Err(e), _) => self.out.errors.push(format!("{}: {e}", a.path.display())),
                        (_, Err(e)) => self.out.errors.push(format!("{}: {e}", b.path.display())),
                    }
                }
                (Some(a), Some(b)) if a.kind != b.kind => {
                    self.out.rows.push(row(Some(plain(&a)), Some(plain(&b)), State::Different));
                }
                (Some(a), Some(b)) => {
                    let state = self.files(&a, &b)?;
                    self.out.rows.push(row(Some(plain(&a)), Some(plain(&b)), state));
                }
                (None, None) => {}
            }
        }
        Some(())
    }

    /// State of two same-name files.
    fn files(&mut self, a: &Entry, b: &Entry) -> Option<State> {
        let delta = match (a.modified, b.modified) {
            (Some(x), Some(y)) if !self.opts.ignore_date => (x - y).num_milliseconds(),
            _ => 0,
        };
        let differ = if a.size != b.size {
            true
        } else if self.opts.by_content {
            !self.source.same_content(&a.path, &b.path, self.cancel)?
        } else {
            delta.abs() > TIME_TOLERANCE_MS
        };
        Some(match differ {
            false => State::Equal,
            true if delta > TIME_TOLERANCE_MS => State::LeftNewer,
            true if delta < -TIME_TOLERANCE_MS => State::RightNewer,
            true => State::Different,
        })
    }

    /// A one-side entry; a folder is measured (links aren't followed).
    fn item(&mut self, e: &Entry) -> Option<Item> {
        let mut item = plain(e);
        if e.kind == EntryKind::Dir && !self.source.is_link(&e.path) {
            let (size, files) = self.measure(&e.path)?;
            item.size = size;
            item.files = files;
        }
        Some(item)
    }

    /// (bytes, files) of everything in the folder `dir`.
    fn measure(&mut self, dir: &Path) -> Option<(u64, u64)> {
        if self.stopped() {
            return None;
        }
        let Ok(entries) = self.source.list(dir) else { return Some((0, 0)) };
        let mut total = (0, 0);
        for e in entries.iter().filter(|e| !e.is_parent()) {
            if e.kind == EntryKind::Dir {
                if !self.source.is_link(&e.path) {
                    let (b, n) = self.measure(&e.path)?;
                    total.0 += b;
                    total.1 += n;
                }
            } else {
                total.0 += e.size;
                total.1 += 1;
            }
        }
        Some(total)
    }
}

fn plain(e: &Entry) -> Item {
    let file = e.kind != EntryKind::Dir;
    Item { kind: e.kind, size: if file { e.size } else { 0 }, modified: e.modified, files: u64::from(file) }
}

/// What a set of chosen actions does, for the summary and confirmation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// (files, bytes) copied to the right / to the left.
    pub to_right: (u64, u64),
    pub to_left: (u64, u64),
    /// (rows, files) deleted on the right / on the left.
    pub delete_right: (u64, u64),
    pub delete_left: (u64, u64),
    /// Copies that replace a file that is newer than the one copied.
    pub overwrite_newer: u64,
}

impl Summary {
    pub fn is_empty(&self) -> bool {
        *self == Summary::default()
    }

    pub fn deletions(&self) -> u64 {
        self.delete_right.0 + self.delete_left.0
    }
}

pub fn summarize(rows: &[Row], actions: &[Action]) -> Summary {
    let mut s = Summary::default();
    for (row, &action) in rows.iter().zip(actions) {
        let side = |i: &Option<Item>| i.as_ref().map_or((0, 0), |i| (i.files, i.size));
        match action {
            Action::Skip => {}
            Action::ToRight => {
                let (n, b) = side(&row.left);
                s.to_right.0 += n;
                s.to_right.1 += b;
                s.overwrite_newer += u64::from(row.state == State::RightNewer);
            }
            Action::ToLeft => {
                let (n, b) = side(&row.right);
                s.to_left.0 += n;
                s.to_left.1 += b;
                s.overwrite_newer += u64::from(row.state == State::LeftNewer);
            }
            Action::DeleteRight => {
                s.delete_right.0 += 1;
                s.delete_right.1 += side(&row.right).0;
            }
            Action::DeleteLeft => {
                s.delete_left.0 += 1;
                s.delete_left.1 += side(&row.left).0;
            }
        }
    }
    s
}

/// The transfers and deletions of the chosen actions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub to_right: Vec<Transfer>,
    pub to_left: Vec<Transfer>,
    pub delete_right: Vec<PathBuf>,
    pub delete_left: Vec<PathBuf>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.to_right.is_empty() && self.to_left.is_empty() && self.delete_right.is_empty() && self.delete_left.is_empty()
    }
}

/// Actions that a row doesn't allow are ignored (skipped).
pub fn plan(rows: &[Row], actions: &[Action]) -> Plan {
    let mut p = Plan::default();
    for (row, &action) in rows.iter().zip(actions) {
        if !row.actions().contains(&action) {
            continue;
        }
        match action {
            Action::Skip => {}
            Action::ToRight => p.to_right.push(Transfer { src: row.left_path.clone(), dst: row.right_path.clone() }),
            Action::ToLeft => p.to_left.push(Transfer { src: row.right_path.clone(), dst: row.left_path.clone() }),
            Action::DeleteRight => p.delete_right.push(row.right_path.clone()),
            Action::DeleteLeft => p.delete_left.push(row.left_path.clone()),
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(tag: &str) -> (Scratch, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("rc-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        (Scratch(root), a, b)
    }

    /// Writes `data` with the modification time `secs` after a fixed date.
    fn write(path: &Path, data: &str, secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    fn run(a: &Path, b: &Path, opts: CompareOptions) -> Comparison {
        let never = AtomicBool::new(false);
        let opts = CompareOptions { show_hidden: true, ..opts };
        compare_trees(a, b, &opts, &AnySource, &never, &mut |_| {}).unwrap().unwrap()
    }

    fn state_of<'a>(c: &'a Comparison, rel: &str) -> &'a Row {
        c.rows.iter().find(|r| r.rel.eq_ignore_ascii_case(rel)).unwrap_or_else(|| panic!("no row {rel}: {:?}", c.rows))
    }

    fn actions(c: &Comparison, mode: Mode) -> Vec<Action> {
        c.rows.iter().map(|r| default_action(r, mode)).collect()
    }

    /// A tree with one of each kind of difference.
    fn sample(a: &Path, b: &Path) {
        write(&a.join("same.txt"), "same", 100);
        write(&b.join("same.txt"), "same", 100);
        write(&a.join("fat.txt"), "fat", 100);
        write(&b.join("fat.txt"), "fat", 101); // within the tolerance
        write(&a.join("left-newer.txt"), "new", 500);
        write(&b.join("left-newer.txt"), "old", 100);
        write(&a.join("sub/right-newer.txt"), "old", 100);
        write(&b.join("sub/right-newer.txt"), "new!", 500);
        write(&a.join("clash.txt"), "aaa", 100);
        write(&b.join("clash.txt"), "bbbb", 100);
        write(&a.join("only-left.txt"), "l", 100);
        write(&b.join("only-right.txt"), "r", 100);
        write(&a.join("left-dir/x.txt"), "12345", 100);
        write(&a.join("left-dir/deep/y.txt"), "123", 100);
        write(&b.join("sub/right-dir/z.txt"), "z", 100);
    }

    #[test]
    fn compares_trees_recursively() {
        let (_g, a, b) = scratch("tree");
        sample(&a, &b);
        let c = run(&a, &b, CompareOptions::default());
        assert!(c.errors.is_empty());
        assert_eq!(state_of(&c, "same.txt").state, State::Equal);
        assert_eq!(state_of(&c, "fat.txt").state, State::Equal);
        assert_eq!(state_of(&c, "left-newer.txt").state, State::LeftNewer);
        assert_eq!(state_of(&c, "sub\\right-newer.txt").state, State::RightNewer);
        assert_eq!(state_of(&c, "clash.txt").state, State::Different);
        assert_eq!(state_of(&c, "only-left.txt").state, State::LeftOnly);
        assert_eq!(state_of(&c, "only-right.txt").state, State::RightOnly);
        assert_eq!(state_of(&c, "sub\\right-dir").state, State::RightOnly);
        // A folder on one side only is one row, measured.
        let left_dir = state_of(&c, "left-dir");
        assert_eq!(left_dir.state, State::LeftOnly);
        assert_eq!(left_dir.left.as_ref().map(|i| (i.files, i.size)), Some((2, 8)));
        assert!(!c.rows.iter().any(|r| r.rel.starts_with("left-dir\\")));
        // Folders on both sides are no rows of their own.
        assert!(!c.rows.iter().any(|r| r.rel == "sub"));
        assert_eq!(c.folders, 2);
        assert_eq!(left_dir.right_path, b.join("left-dir"));
    }

    #[test]
    fn two_way_newer_wins_and_skips_equal_and_conflicts() {
        let (_g, a, b) = scratch("two-way");
        sample(&a, &b);
        let c = run(&a, &b, CompareOptions::default());
        let get = |rel: &str| default_action(state_of(&c, rel), Mode::TwoWay);
        assert_eq!(get("same.txt"), Action::Skip);
        assert_eq!(get("fat.txt"), Action::Skip);
        assert_eq!(get("left-newer.txt"), Action::ToRight);
        assert_eq!(get("sub\\right-newer.txt"), Action::ToLeft);
        assert_eq!(get("clash.txt"), Action::Skip);
        assert!(state_of(&c, "clash.txt").conflict());
        assert_eq!(get("only-left.txt"), Action::ToRight);
        assert_eq!(get("only-right.txt"), Action::ToLeft);
        assert!(!actions(&c, Mode::TwoWay).iter().any(|a| a.deletes()));

        let p = plan(&c.rows, &actions(&c, Mode::TwoWay));
        assert!(p.delete_left.is_empty() && p.delete_right.is_empty());
        assert!(p.to_right.contains(&Transfer { src: a.join("left-dir"), dst: b.join("left-dir") }));
        assert!(p.to_left.contains(&Transfer { src: b.join("sub").join("right-newer.txt"), dst: a.join("sub").join("right-newer.txt") }));
        assert_eq!(p.to_right.len(), 3); // left-newer, only-left, left-dir
        assert_eq!(p.to_left.len(), 3); // right-newer, only-right, right-dir
    }

    #[test]
    fn mirror_deletes_right_only_items() {
        let (_g, a, b) = scratch("mirror");
        sample(&a, &b);
        let c = run(&a, &b, CompareOptions::default());
        let acts = actions(&c, Mode::Mirror);
        let p = plan(&c.rows, &acts);
        assert!(p.to_left.is_empty() && p.delete_left.is_empty());
        let mut deleted = p.delete_right.clone();
        deleted.sort();
        assert_eq!(deleted, [b.join("only-right.txt"), b.join("sub").join("right-dir")]);
        // Everything that differs goes to the right, the newer right file too.
        assert_eq!(default_action(state_of(&c, "sub\\right-newer.txt"), Mode::Mirror), Action::ToRight);
        assert_eq!(default_action(state_of(&c, "clash.txt"), Mode::Mirror), Action::ToRight);
        assert_eq!(default_action(state_of(&c, "same.txt"), Mode::Mirror), Action::Skip);
        let s = summarize(&c.rows, &acts);
        assert_eq!(s.delete_right, (2, 2));
        assert_eq!(s.overwrite_newer, 1);
        assert_eq!(s.deletions(), 2);
    }

    #[test]
    fn update_never_deletes_or_copies_left() {
        let (_g, a, b) = scratch("update");
        sample(&a, &b);
        let c = run(&a, &b, CompareOptions::default());
        let p = plan(&c.rows, &actions(&c, Mode::UpdateRight));
        assert!(p.to_left.is_empty() && p.delete_left.is_empty() && p.delete_right.is_empty());
        assert_eq!(p.to_right.len(), 3);
        assert_eq!(default_action(state_of(&c, "sub\\right-newer.txt"), Mode::UpdateRight), Action::Skip);
    }

    #[test]
    fn only_mirror_defaults_to_a_delete() {
        let item = Some(Item { kind: EntryKind::File, size: 1, modified: None, files: 1 });
        let states = [State::Equal, State::LeftOnly, State::RightOnly, State::LeftNewer, State::RightNewer, State::Different];
        for mode in Mode::ALL {
            for state in states {
                let (left, right) = match state {
                    State::LeftOnly => (item.clone(), None),
                    State::RightOnly => (None, item.clone()),
                    _ => (item.clone(), item.clone()),
                };
                let row = Row { rel: "x".into(), left_path: "a".into(), right_path: "b".into(), left, right, state };
                let action = default_action(&row, mode);
                assert!(row.actions().contains(&action), "{mode:?} {state:?}");
                let delete_ok = mode == Mode::Mirror && state == State::RightOnly;
                assert_eq!(action.deletes(), delete_ok, "{mode:?} {state:?}");
                assert_ne!(action, Action::DeleteLeft);
            }
        }
    }

    #[test]
    fn content_and_ignore_date_options() {
        let (_g, a, b) = scratch("content");
        write(&a.join("same.txt"), "hello", 100);
        write(&b.join("same.txt"), "hello", 900);
        write(&a.join("edited.txt"), "hello", 100);
        write(&b.join("edited.txt"), "jello", 100);
        let c = run(&a, &b, CompareOptions { by_content: true, ..CompareOptions::default() });
        // Same bytes: equal whatever the dates.
        assert_eq!(state_of(&c, "same.txt").state, State::Equal);
        assert_eq!(state_of(&c, "edited.txt").state, State::Different);
        let c = run(&a, &b, CompareOptions::default());
        assert_eq!(state_of(&c, "same.txt").state, State::RightNewer);
        assert_eq!(state_of(&c, "edited.txt").state, State::Equal); // same size and date
        let c = run(&a, &b, CompareOptions { ignore_date: true, ..CompareOptions::default() });
        assert_eq!(state_of(&c, "same.txt").state, State::Equal);
    }

    #[test]
    fn date_tolerance() {
        let (_g, a, b) = scratch("tolerance");
        write(&a.join("two.txt"), "x", 100);
        write(&b.join("two.txt"), "x", 102);
        write(&a.join("three.txt"), "x", 100);
        write(&b.join("three.txt"), "x", 103);
        let c = run(&a, &b, CompareOptions::default());
        assert_eq!(state_of(&c, "two.txt").state, State::Equal);
        assert_eq!(state_of(&c, "three.txt").state, State::RightNewer);
    }

    #[test]
    fn kind_clash_is_a_conflict_that_can_only_be_skipped() {
        let (_g, a, b) = scratch("kind");
        write(&a.join("thing"), "file", 100);
        write(&b.join("thing").join("inner.txt"), "x", 100);
        let c = run(&a, &b, CompareOptions::default());
        let row = state_of(&c, "thing");
        assert_eq!(row.state, State::Different);
        assert_eq!(row.actions(), [Action::Skip]);
        for mode in Mode::ALL {
            assert_eq!(default_action(row, mode), Action::Skip);
        }
        // An action the row doesn't allow isn't planned.
        assert!(plan(std::slice::from_ref(row), &[Action::ToRight]).is_empty());
    }

    #[test]
    fn space_cycles_through_allowed_actions() {
        let row = Row {
            rel: "x".into(),
            left_path: "a".into(),
            right_path: "b".into(),
            left: None,
            right: Some(Item { kind: EntryKind::File, size: 1, modified: None, files: 1 }),
            state: State::RightOnly,
        };
        assert_eq!(row.next_action(Action::ToLeft), Action::Skip);
        assert_eq!(row.next_action(Action::Skip), Action::DeleteRight);
        assert_eq!(row.next_action(Action::DeleteRight), Action::ToLeft);
    }

    #[test]
    fn hidden_entries_follow_the_option_and_cancel_stops() {
        /// Local folders where dot names are hidden (any OS).
        struct Dots;
        impl Source for Dots {
            fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
                let mut entries = AnySource.list(dir)?;
                entries.iter_mut().for_each(|e| e.hidden |= e.name.starts_with('.'));
                Ok(entries)
            }
            fn same_content(&self, a: &Path, b: &Path, cancel: &AtomicBool) -> Option<bool> {
                AnySource.same_content(a, b, cancel)
            }
            fn is_link(&self, _: &Path) -> bool {
                false
            }
            fn link_target(&self, _: &Path) -> Option<PathBuf> {
                None
            }
        }
        let (_g, a, b) = scratch("hidden");
        write(&a.join(".hidden"), "x", 100);
        write(&a.join("shown"), "x", 100);
        let never = AtomicBool::new(false);
        let opts = CompareOptions::default();
        let mut seen = Vec::new();
        let c = compare_trees(&a, &b, &opts, &Dots, &never, &mut |p| seen.push(p.to_path_buf())).unwrap().unwrap();
        assert_eq!(c.rows.len(), 1);
        assert_eq!(seen, std::slice::from_ref(&a));
        let shown = CompareOptions { show_hidden: true, ..opts };
        let c = compare_trees(&a, &b, &shown, &Dots, &never, &mut |_| {}).unwrap().unwrap();
        assert_eq!(c.rows.len(), 2);
        let cancelled = AtomicBool::new(true);
        assert!(compare_trees(&a, &b, &opts, &AnySource, &cancelled, &mut |_| {}).unwrap().is_none());
        assert!(compare_trees(&a.join("missing"), &b, &opts, &AnySource, &never, &mut |_| {}).is_err());
    }
}
