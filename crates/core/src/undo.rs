//! Undo of finished file operations (Ctrl+Z).
//!
//! An [`UndoStep`] records what an operation did and what it left behind, so
//! it can be reversed later, but only while things are still as the
//! operation left them: [`UndoStep::verify_with`] checks that first, and
//! [`UndoStep::undo_with`] refuses (doing nothing) when anything changed.
//! An undo never destroys data:
//! - renames and same-volume moves are renamed back without replacing
//!   anything ([`rc_platform::rename_no_replace`]);
//! - a new folder is removed only while it is still empty (`remove_dir`);
//! - a new file and the copies made by a copy go to the Recycle Bin, never
//!   deleted outright, and only while unchanged (a copy also only while its
//!   original is still there with the same contents list, so even a drive
//!   without a Recycle Bin loses nothing);
//! - recycled items are restored from the Recycle Bin only where their
//!   original place is free again.
//!
//! The Recycle Bin is reached through [`Bin`], so tests can use a fake one.

use crate::ops;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
pub use trash::TrashItem;

/// How many steps the app keeps.
pub const MAX_STEPS: usize = 20;

/// Seconds of slack when matching a Recycle Bin item's deletion time with
/// the time the job recycled it.
const BIN_SLACK: i64 = 2;

/// The Recycle Bin, as far as undo needs it.
pub trait Bin {
    fn list(&self) -> Result<Vec<TrashItem>, String>;
    /// Puts `items` back where they came from (none of those places exist).
    fn restore(&self, items: Vec<TrashItem>) -> Result<(), String>;
    fn recycle(&self, path: &Path) -> Result<(), String>;
}

/// The real Recycle Bin (the `trash` crate).
pub struct RecycleBin;

impl Bin for RecycleBin {
    #[cfg(windows)]
    fn list(&self) -> Result<Vec<TrashItem>, String> {
        trash::os_limited::list().map_err(|e| format!("Can't read the Recycle Bin: {e}"))
    }

    #[cfg(not(windows))]
    fn list(&self) -> Result<Vec<TrashItem>, String> {
        Err("Restoring from the trash isn't supported here".into())
    }

    #[cfg(windows)]
    fn restore(&self, items: Vec<TrashItem>) -> Result<(), String> {
        trash::os_limited::restore_all(items).map_err(|e| match e {
            trash::Error::RestoreCollision { path, .. } => format!("{} exists again", path.display()),
            e => format!("Can't restore from the Recycle Bin: {e}"),
        })
    }

    #[cfg(not(windows))]
    fn restore(&self, _items: Vec<TrashItem>) -> Result<(), String> {
        Err("Restoring from the trash isn't supported here".into())
    }

    fn recycle(&self, path: &Path) -> Result<(), String> {
        trash::delete(path).map_err(|e| format!("Can't recycle {}: {e}", path.display()))
    }
}

/// Kind, size and date of one item (links not followed), to tell whether it
/// changed. A folder's date changes when something is added to or removed
/// from it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStamp {
    dir: bool,
    link: bool,
    size: u64,
    modified: Option<SystemTime>,
}

impl ItemStamp {
    pub fn of(path: &Path) -> io::Result<Self> {
        let m = fs::symlink_metadata(path)?;
        let dir = m.is_dir();
        Ok(Self { dir, link: m.file_type().is_symlink(), size: if dir { 0 } else { m.len() }, modified: m.modified().ok() })
    }
}

/// Everything below an item (or the file itself): counts, bytes and a
/// digest of every name, kind, size and file date. Links are not followed.
/// Folder dates don't count (a copy's folders get new ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    files: u64,
    dirs: u64,
    bytes: u64,
    digest: u64,
}

impl Snapshot {
    pub fn of(path: &Path) -> io::Result<Self> {
        let mut rows = Vec::new();
        collect(path, String::new(), &mut rows)?;
        rows.sort();
        let mut snap = Snapshot { files: 0, dirs: 0, bytes: 0, digest: 0 };
        let mut hasher = DefaultHasher::new();
        for row in &rows {
            match row.1 {
                'd' => snap.dirs += 1,
                _ => snap.files += 1,
            }
            snap.bytes += row.2;
            row.hash(&mut hasher);
        }
        snap.digest = hasher.finish();
        Ok(snap)
    }
}

/// (relative name, kind, size, file date in ns) of `path` and everything below it.
fn collect(path: &Path, rel: String, rows: &mut Vec<(String, char, u64, u128)>) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    let date = |m: &fs::Metadata| {
        m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
    };
    if m.file_type().is_symlink() {
        rows.push((rel, 'l', 0, 0));
    } else if m.is_dir() {
        if !rel.is_empty() {
            rows.push((rel.clone(), 'd', 0, 0));
        }
        for child in fs::read_dir(path)? {
            let child = child?;
            let name = child.file_name().to_string_lossy().to_lowercase();
            let rel = if rel.is_empty() { name } else { format!("{rel}\\{name}") };
            collect(&child.path(), rel, rows)?;
        }
    } else {
        rows.push((rel, 'f', m.len(), date(&m)));
    }
    Ok(())
}

/// One item renamed or moved from `from` to `to` (stamped there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub from: PathBuf,
    pub to: PathBuf,
    pub stamp: ItemStamp,
}

/// One top-level item a copy made at `dst` from `src` (which it equals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub snap: Snapshot,
}

/// One item recycled from `path` between the times `from` and `to`
/// (seconds since 1970).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recycled {
    pub path: PathBuf,
    pub from: i64,
    pub to: i64,
}

/// A finished operation that can be undone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoStep {
    Rename(Moved),
    /// Same-volume moves (renames); moves done as copy+delete aren't here.
    Move(Vec<Moved>),
    /// Renames by the multi-rename tool (may swap names among themselves).
    MultiRename(Vec<Moved>),
    /// Folders made by one F7, outermost first (`a`, `a\b`).
    MakeDir(Vec<PathBuf>),
    NewFile { path: PathBuf, stamp: ItemStamp },
    /// The top-level items a copy made (not ones it merged into or overwrote).
    Copy(Vec<Copied>),
    Recycle(Vec<Recycled>),
}

/// What an undo did: a message, and the item to put the cursor on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undone {
    pub message: String,
    pub focus: Option<PathBuf>,
}

/// Seconds since 1970, as the Recycle Bin dates deletions.
pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The folders `create_dir_all(path)` would make, outermost first.
pub fn missing_dirs(path: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = path
        .ancestors()
        .take_while(|p| !p.as_os_str().is_empty() && fs::symlink_metadata(p).is_err())
        .map(Path::to_path_buf)
        .collect();
    out.reverse();
    out
}

fn name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn stamped(pairs: Vec<(PathBuf, PathBuf)>) -> Vec<Moved> {
    pairs
        .into_iter()
        .filter_map(|(from, to)| ItemStamp::of(&to).ok().map(|stamp| Moved { from, to, stamp }))
        .collect()
}

fn items<T>(v: Vec<T>) -> Option<Vec<T>> {
    (!v.is_empty()).then_some(v)
}

impl UndoStep {
    pub fn renamed(from: PathBuf, to: PathBuf) -> Option<Self> {
        stamped(vec![(from, to)]).pop().map(UndoStep::Rename)
    }

    pub fn moved(pairs: Vec<(PathBuf, PathBuf)>) -> Option<Self> {
        items(stamped(pairs)).map(UndoStep::Move)
    }

    pub fn multi_renamed(pairs: Vec<(PathBuf, PathBuf)>) -> Option<Self> {
        items(stamped(pairs)).map(UndoStep::MultiRename)
    }

    /// `dirs`: what [`missing_dirs`] said before they were made.
    pub fn made_dirs(dirs: Vec<PathBuf>) -> Option<Self> {
        items(dirs).filter(|d| d.iter().all(|p| p.is_dir())).map(UndoStep::MakeDir)
    }

    pub fn new_file(path: PathBuf) -> Option<Self> {
        let stamp = ItemStamp::of(&path).ok().filter(|s| !s.dir && !s.link)?;
        Some(UndoStep::NewFile { path, stamp })
    }

    /// `pairs`: (source, new copy) of top-level items copied completely to
    /// places that were free.
    pub fn copied(pairs: Vec<(PathBuf, PathBuf)>) -> Option<Self> {
        let copies = pairs
            .into_iter()
            .filter_map(|(src, dst)| Snapshot::of(&dst).ok().map(|snap| Copied { src, dst, snap }))
            .collect();
        items(copies).map(UndoStep::Copy)
    }

    pub fn recycled(list: Vec<Recycled>) -> Option<Self> {
        items(list).map(UndoStep::Recycle)
    }

    /// For the question "Undo: ...?".
    pub fn describe(&self) -> String {
        match self {
            UndoStep::Rename(m) => format!("rename {} back to {}", name(&m.to), name(&m.from)),
            UndoStep::Move(v) => match v.as_slice() {
                [one] => format!("move {} back to {}", name(&one.to), parent(&one.from)),
                _ => format!("move {} items back to {}", v.len(), parent(&v[0].from)),
            },
            UndoStep::MultiRename(v) => match v.as_slice() {
                [one] => format!("rename {} back to {}", name(&one.to), name(&one.from)),
                _ => format!("rename {} files back to their old names", v.len()),
            },
            UndoStep::MakeDir(dirs) => format!("remove the new folder {}", name(&dirs[0])),
            UndoStep::NewFile { path, .. } => format!("move the new file {} to the Recycle Bin", name(path)),
            UndoStep::Copy(v) => match v.as_slice() {
                [one] => format!("move the copy {} to the Recycle Bin", one.dst.display()),
                _ => format!("move the {} copies in {} to the Recycle Bin", v.len(), parent(&v[0].dst)),
            },
            UndoStep::Recycle(v) => match v.as_slice() {
                [one] => format!("restore {} from the Recycle Bin", one.path.display()),
                _ => format!("restore {} items to {} from the Recycle Bin", v.len(), parent(&v[0].path)),
            },
        }
    }

    /// Checks that everything is still as the operation left it (with the
    /// real Recycle Bin).
    pub fn verify(&self) -> Result<(), String> {
        self.verify_with(&RecycleBin)
    }

    /// Verifies, then undoes (with the real Recycle Bin).
    pub fn undo(&self) -> Result<Undone, String> {
        self.undo_with(&RecycleBin)
    }

    /// Checks that everything is still as the operation left it; the error
    /// says what changed. Nothing is changed.
    pub fn verify_with(&self, bin: &dyn Bin) -> Result<(), String> {
        match self {
            UndoStep::Rename(m) => verify_moves(std::slice::from_ref(m), false),
            UndoStep::Move(v) => verify_moves(v, false),
            UndoStep::MultiRename(v) => verify_moves(v, true),
            UndoStep::MakeDir(dirs) => verify_dirs(dirs),
            UndoStep::NewFile { path, stamp } => match ItemStamp::of(path) {
                Ok(now) if now == *stamp => Ok(()),
                Ok(_) => Err(format!("{} was changed since it was made", name(path))),
                Err(_) => Err(format!("{} is gone", path.display())),
            },
            UndoStep::Copy(v) => v.iter().try_for_each(|c| {
                match Snapshot::of(&c.dst) {
                    Ok(s) if s == c.snap => {}
                    Ok(_) => return Err(format!("The copy {} was changed since", c.dst.display())),
                    Err(_) => return Err(format!("The copy {} is gone", c.dst.display())),
                }
                match Snapshot::of(&c.src) {
                    Ok(s) if s == c.snap => Ok(()),
                    _ => Err(format!(
                        "The original {} changed or is gone since the copy: the copy is kept",
                        c.src.display()
                    )),
                }
            }),
            UndoStep::Recycle(v) => {
                for r in v {
                    if exists(&r.path) {
                        return Err(format!("{} exists again", r.path.display()));
                    }
                }
                find_in_bin(&bin.list()?, v).map(drop)
            }
        }
    }

    /// Verifies (doing nothing if anything changed), then undoes.
    pub fn undo_with(&self, bin: &dyn Bin) -> Result<Undone, String> {
        self.verify_with(bin)?;
        match self {
            UndoStep::Rename(m) => {
                rename_back(m)?;
                let message = format!("Renamed {} back to {}", name(&m.to), name(&m.from));
                Ok(Undone { message, focus: Some(m.from.clone()) })
            }
            UndoStep::Move(v) => {
                for (i, m) in v.iter().enumerate() {
                    if let Err(e) = rename_back(m) {
                        return Err(format!("Moved back {i} of {}; then {e}", v.len()));
                    }
                }
                let what = if v.len() == 1 { name(&v[0].from) } else { format!("{} items", v.len()) };
                Ok(Undone { message: format!("Moved {what} back"), focus: Some(v[0].from.clone()) })
            }
            UndoStep::MultiRename(v) => {
                let pairs: Vec<(PathBuf, PathBuf)> = v.iter().map(|m| (m.to.clone(), m.from.clone())).collect();
                let (done, failed) = crate::rename::rename_all(&pairs);
                match failed.first() {
                    None => Ok(Undone {
                        message: format!("Renamed {} back", plural(done.len(), "file")),
                        focus: Some(v[0].from.clone()),
                    }),
                    Some(f) => Err(format!(
                        "Renamed {} of {} back; {}: {}",
                        done.len(),
                        v.len(),
                        name(&f.from),
                        f.error
                    )),
                }
            }
            UndoStep::MakeDir(dirs) => {
                for d in dirs.iter().rev() {
                    fs::remove_dir(d).map_err(|e| format!("Can't remove {}: {e}", d.display()))?;
                }
                Ok(Undone { message: format!("Removed the new folder {}", name(&dirs[0])), focus: None })
            }
            UndoStep::NewFile { path, .. } => {
                bin.recycle(path)?;
                Ok(Undone { message: format!("Moved the new file {} to the Recycle Bin", name(path)), focus: None })
            }
            UndoStep::Copy(v) => {
                for (i, c) in v.iter().enumerate() {
                    if let Err(e) = bin.recycle(&c.dst) {
                        return Err(format!("Recycled {i} of {} copies; then {e}", v.len()));
                    }
                }
                let what = if v.len() == 1 { format!("the copy {}", name(&v[0].dst)) } else { plural(v.len(), "copy") };
                Ok(Undone { message: format!("Moved {what} to the Recycle Bin"), focus: None })
            }
            UndoStep::Recycle(v) => {
                let found = find_in_bin(&bin.list()?, v)?;
                bin.restore(found)?;
                let back = v.iter().filter(|r| exists(&r.path)).count();
                if back < v.len() {
                    return Err(format!("Restored {back} of {} items from the Recycle Bin", v.len()));
                }
                let what = if v.len() == 1 { name(&v[0].path) } else { format!("{} items", v.len()) };
                Ok(Undone { message: format!("Restored {what} from the Recycle Bin"), focus: Some(v[0].path.clone()) })
            }
        }
    }
}

fn parent(p: &Path) -> String {
    p.parent().map_or_else(String::new, |p| p.display().to_string())
}

fn plural(n: usize, what: &str) -> String {
    match n {
        1 => format!("1 {what}"),
        _ if what.ends_with('y') => format!("{n} {}ies", &what[..what.len() - 1]),
        _ => format!("{n} {what}s"),
    }
}

/// Each `to` is as it was left and each `from` is free (a case-only rename
/// counts as free; with `chained`, so does a name another item had).
fn verify_moves(moves: &[Moved], chained: bool) -> Result<(), String> {
    for m in moves {
        match ItemStamp::of(&m.to) {
            Ok(now) if now == m.stamp => {}
            Ok(_) => return Err(format!("{} was changed since", m.to.display())),
            Err(_) => return Err(format!("{} is gone", m.to.display())),
        }
        let case_only = ops::same_path(&m.from, &m.to);
        let taken_by_us = chained && moves.iter().any(|o| ops::same_path(&o.to, &m.from));
        if exists(&m.from) && !case_only && !taken_by_us {
            return Err(format!("{} exists again", m.from.display()));
        }
        if !m.from.parent().is_some_and(Path::is_dir) {
            return Err(format!("The folder {} is gone", parent(&m.from)));
        }
    }
    Ok(())
}

fn rename_back(m: &Moved) -> Result<(), String> {
    rc_platform::rename_no_replace(&m.to, &m.from).map_err(|e| format!("can't move {} back: {e}", m.to.display()))
}

/// Each folder is a real folder holding only the next one (the last one empty).
fn verify_dirs(dirs: &[PathBuf]) -> Result<(), String> {
    for (i, d) in dirs.iter().enumerate() {
        match fs::symlink_metadata(d) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            _ => return Err(format!("{} is gone", d.display())),
        }
        let inside: Vec<PathBuf> = fs::read_dir(d)
            .map_err(|e| format!("Can't read {}: {e}", d.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        let ok = match dirs.get(i + 1) {
            Some(next) => inside.len() == 1 && ops::same_path(&inside[0], next),
            None => inside.is_empty(),
        };
        if !ok {
            return Err(format!("The folder {} isn't empty any more", name(&dirs[0])));
        }
    }
    Ok(())
}

/// A path as the Recycle Bin and we may spell it: case and separators
/// don't count, nor a `\\?\` prefix or a trailing separator.
fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix("\\\\?\\").unwrap_or(&s);
    let s = s.trim_end_matches('\\');
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_owned()
    }
}

/// The Recycle Bin items that are exactly the recycled `wanted` ones: same
/// original path, deleted while the job ran. Refuses unless each has
/// exactly one match.
pub fn find_in_bin(bin: &[TrashItem], wanted: &[Recycled]) -> Result<Vec<TrashItem>, String> {
    let mut found = Vec::with_capacity(wanted.len());
    for r in wanted {
        let key = path_key(&r.path);
        let mut hits = bin.iter().filter(|t| {
            path_key(&t.original_path()) == key && t.time_deleted >= r.from - BIN_SLACK && t.time_deleted <= r.to + BIN_SLACK
        });
        match (hits.next(), hits.next()) {
            (Some(one), None) => found.push(one.clone()),
            (None, _) => return Err(format!("{} isn't in the Recycle Bin any more", r.path.display())),
            (Some(_), Some(_)) => {
                return Err(format!("The Recycle Bin holds several items like {}: restore it by hand", r.path.display()))
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::OsString;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("rc-undo-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A fake Recycle Bin: a folder in the temp dir, items listed by hand.
    struct FakeBin {
        dir: PathBuf,
        items: RefCell<Vec<TrashItem>>,
    }

    impl FakeBin {
        fn new(dir: PathBuf) -> Self {
            fs::create_dir_all(&dir).unwrap();
            FakeBin { dir, items: RefCell::new(Vec::new()) }
        }
        fn held(&self, i: usize) -> PathBuf {
            self.dir.join(&self.items.borrow()[i].id)
        }
    }

    impl Bin for FakeBin {
        fn list(&self) -> Result<Vec<TrashItem>, String> {
            Ok(self.items.borrow().clone())
        }
        fn restore(&self, items: Vec<TrashItem>) -> Result<(), String> {
            for t in &items {
                assert!(!exists(&t.original_path()), "restore onto an existing path");
                fs::rename(self.dir.join(&t.id), t.original_path()).map_err(|e| e.to_string())?;
                self.items.borrow_mut().retain(|o| o.id != t.id);
            }
            Ok(())
        }
        fn recycle(&self, path: &Path) -> Result<(), String> {
            let mut items = self.items.borrow_mut();
            let id = OsString::from(format!("item{}", items.len()));
            fs::rename(path, self.dir.join(&id)).map_err(|e| e.to_string())?;
            items.push(TrashItem {
                id,
                name: path.file_name().unwrap().to_os_string(),
                original_parent: path.parent().unwrap().to_path_buf(),
                time_deleted: unix_now(),
            });
            Ok(())
        }
    }

    fn recycle_like_a_job(bin: &FakeBin, paths: &[PathBuf]) -> UndoStep {
        let items = paths
            .iter()
            .map(|p| {
                let from = unix_now();
                bin.recycle(p).unwrap();
                Recycled { path: p.clone(), from, to: unix_now() }
            })
            .collect();
        UndoStep::recycled(items).unwrap()
    }

    fn touch_later(p: &Path) {
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(60)).unwrap();
    }

    #[test]
    fn rename_round_trip_and_refusals() {
        let tmp = TempDir::new("rename");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let (a, b) = (tmp.0.join("a.txt"), tmp.0.join("b.txt"));
        fs::write(&a, b"one").unwrap();
        fs::rename(&a, &b).unwrap();
        let step = UndoStep::renamed(a.clone(), b.clone()).unwrap();
        assert_eq!(step.describe(), "rename b.txt back to a.txt");

        // The old name is taken again: refused, nothing touched.
        fs::write(&a, b"other").unwrap();
        assert!(step.undo_with(&bin).unwrap_err().contains("exists again"));
        assert_eq!(fs::read(&a).unwrap(), b"other");
        assert_eq!(fs::read(&b).unwrap(), b"one");
        fs::remove_file(&a).unwrap();

        // Changed since: refused.
        fs::write(&b, b"longer now").unwrap();
        assert!(step.verify_with(&bin).unwrap_err().contains("changed"));
        let step = UndoStep::renamed(a.clone(), b.clone()).unwrap();
        touch_later(&b);
        assert!(step.verify_with(&bin).unwrap_err().contains("changed"));

        let step = UndoStep::renamed(a.clone(), b.clone()).unwrap();
        let done = step.undo_with(&bin).unwrap();
        assert_eq!(done.focus.as_deref(), Some(a.as_path()));
        assert_eq!(fs::read(&a).unwrap(), b"longer now");
        assert!(!exists(&b));
        // Gone now: refused.
        assert!(step.verify_with(&bin).unwrap_err().contains("is gone"));
    }

    #[test]
    fn case_only_rename_is_undone() {
        let tmp = TempDir::new("case");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let (a, b) = (tmp.0.join("name.txt"), tmp.0.join("NAME.txt"));
        fs::write(&a, b"x").unwrap();
        fs::rename(&a, &b).unwrap();
        let step = UndoStep::renamed(a.clone(), b.clone()).unwrap();
        step.undo_with(&bin).unwrap();
        let names: Vec<_> = fs::read_dir(&tmp.0).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(names.contains(&OsString::from("name.txt")), "{names:?}");
    }

    #[test]
    fn folder_rename_round_trip() {
        let tmp = TempDir::new("dirrename");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let (a, b) = (tmp.0.join("d1"), tmp.0.join("d2"));
        fs::create_dir_all(a.join("in")).unwrap();
        fs::write(a.join("in/f"), b"f").unwrap();
        fs::rename(&a, &b).unwrap();
        let step = UndoStep::renamed(a.clone(), b.clone()).unwrap();
        step.undo_with(&bin).unwrap();
        assert_eq!(fs::read(a.join("in/f")).unwrap(), b"f");
    }

    #[test]
    fn move_is_all_or_nothing() {
        let tmp = TempDir::new("move");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let (src, dst) = (tmp.0.join("src"), tmp.0.join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let mut pairs = Vec::new();
        for n in ["a", "b", "c"] {
            fs::write(dst.join(n), n).unwrap();
            pairs.push((src.join(n), dst.join(n)));
        }
        let step = UndoStep::moved(pairs).unwrap();
        // One original name taken again: nothing moves back.
        fs::write(src.join("c"), b"new c").unwrap();
        let err = step.undo_with(&bin).unwrap_err();
        assert!(err.contains("exists again") && err.contains('c'), "{err}");
        assert!(!exists(&src.join("a")) && exists(&dst.join("a")));
        fs::remove_file(src.join("c")).unwrap();

        let done = step.undo_with(&bin).unwrap();
        assert_eq!(done.message, "Moved 3 items back");
        for n in ["a", "b", "c"] {
            assert_eq!(fs::read_to_string(src.join(n)).unwrap(), n);
            assert!(!exists(&dst.join(n)));
        }
    }

    #[test]
    fn move_refuses_when_the_source_folder_is_gone() {
        let tmp = TempDir::new("movegone");
        let bin = FakeBin::new(tmp.0.join("bin"));
        fs::write(tmp.0.join("f"), b"f").unwrap();
        let step = UndoStep::moved(vec![(tmp.0.join("gone").join("f"), tmp.0.join("f"))]).unwrap();
        assert!(step.undo_with(&bin).unwrap_err().contains("is gone"));
        assert!(exists(&tmp.0.join("f")));
    }

    #[test]
    fn multi_rename_with_swap_round_trip() {
        let tmp = TempDir::new("multi");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let (a, b) = (tmp.0.join("a"), tmp.0.join("b"));
        fs::write(&a, b"A").unwrap();
        fs::write(&b, b"B").unwrap();
        let pairs = vec![(a.clone(), b.clone()), (b.clone(), a.clone())];
        let (done, failed) = crate::rename::rename_all(&pairs);
        assert!(failed.is_empty());
        assert_eq!(fs::read(&a).unwrap(), b"B");
        let step = UndoStep::multi_renamed(done).unwrap();
        step.undo_with(&bin).unwrap();
        assert_eq!(fs::read(&a).unwrap(), b"A");
        assert_eq!(fs::read(&b).unwrap(), b"B");
    }

    #[test]
    fn make_dir_removed_only_while_empty() {
        let tmp = TempDir::new("mkdir");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let deep = tmp.0.join("x").join("y");
        let dirs = missing_dirs(&deep);
        assert_eq!(dirs, vec![tmp.0.join("x"), deep.clone()]);
        fs::create_dir_all(&deep).unwrap();
        let step = UndoStep::made_dirs(dirs).unwrap();
        assert_eq!(step.describe(), "remove the new folder x");

        fs::write(deep.join("keep"), b"k").unwrap();
        assert!(step.undo_with(&bin).unwrap_err().contains("isn't empty"));
        assert!(deep.join("keep").exists());
        fs::remove_file(deep.join("keep")).unwrap();
        fs::write(tmp.0.join("x").join("side"), b"s").unwrap();
        assert!(step.undo_with(&bin).unwrap_err().contains("isn't empty"));
        fs::remove_file(tmp.0.join("x").join("side")).unwrap();

        step.undo_with(&bin).unwrap();
        assert!(!exists(&tmp.0.join("x")));
        assert!(missing_dirs(&tmp.0).is_empty());
    }

    #[test]
    fn new_file_recycled_only_while_unchanged() {
        let tmp = TempDir::new("newfile");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let f = tmp.0.join("new.txt");
        fs::File::create_new(&f).unwrap();
        let step = UndoStep::new_file(f.clone()).unwrap();
        fs::write(&f, b"typed").unwrap();
        assert!(step.undo_with(&bin).unwrap_err().contains("changed"));
        assert!(f.exists());

        fs::remove_file(&f).unwrap();
        fs::File::create_new(&f).unwrap();
        let step = UndoStep::new_file(f.clone()).unwrap();
        step.undo_with(&bin).unwrap();
        assert!(!exists(&f));
        assert!(bin.held(0).exists(), "recycled, not deleted");
        assert!(UndoStep::new_file(tmp.0.clone()).is_none(), "folders aren't new files");
    }

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"aaa").unwrap();
        fs::write(root.join("sub").join("b.txt"), b"bb").unwrap();
    }

    fn copy_tree(src: &Path, dst: &Path) {
        let job = ops::Job::Copy(vec![ops::Transfer { src: src.to_path_buf(), dst: dst.to_path_buf() }]);
        struct No;
        impl ops::Reporter for No {
            fn progress(&mut self, _: &ops::Progress) {}
            fn conflict(&mut self, _: ops::Conflict) -> ops::Resolution {
                ops::Resolution::Skip
            }
        }
        let o = ops::run(&job, &ops::Control::default(), &mut No);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
    }

    #[test]
    fn copy_recycled_only_while_copy_and_original_match() {
        let tmp = TempDir::new("copy");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let (src, dst) = (tmp.0.join("src"), tmp.0.join("dst"));
        tree(&src);
        copy_tree(&src, &dst);
        let step = UndoStep::copied(vec![(src.clone(), dst.clone())]).unwrap();
        step.verify_with(&bin).unwrap();

        // A file deep in the copy changed (same size, other date).
        touch_later(&dst.join("sub").join("b.txt"));
        assert!(step.undo_with(&bin).unwrap_err().contains("was changed"));
        assert!(dst.exists());
        let step = UndoStep::copied(vec![(src.clone(), dst.clone())]).unwrap();
        // Different now from the original: kept.
        assert!(step.verify_with(&bin).unwrap_err().contains("original"));

        fs::remove_dir_all(&dst).unwrap();
        copy_tree(&src, &dst);
        let step = UndoStep::copied(vec![(src.clone(), dst.clone())]).unwrap();
        fs::write(dst.join("sub").join("added"), b"").unwrap();
        assert!(step.verify_with(&bin).unwrap_err().contains("was changed"));
        fs::remove_file(dst.join("sub").join("added")).unwrap();

        step.undo_with(&bin).unwrap();
        assert!(!exists(&dst));
        assert!(bin.held(0).join("sub").join("b.txt").exists(), "recycled, not deleted");
        assert!(src.join("sub").join("b.txt").exists());
    }

    #[test]
    fn recycle_restores_the_exact_items() {
        let tmp = TempDir::new("recycle");
        let bin = FakeBin::new(tmp.0.join("bin"));
        let (a, d) = (tmp.0.join("a.txt"), tmp.0.join("dir"));
        fs::write(&a, b"a").unwrap();
        tree(&d);
        let step = recycle_like_a_job(&bin, &[a.clone(), d.clone()]);
        assert!(!exists(&a) && !exists(&d));

        // The place is taken again: refused, nothing restored.
        fs::write(&a, b"new").unwrap();
        assert!(step.undo_with(&bin).unwrap_err().contains("exists again"));
        assert_eq!(bin.items.borrow().len(), 2);
        fs::remove_file(&a).unwrap();

        let done = step.undo_with(&bin).unwrap();
        assert_eq!(done.focus.as_deref(), Some(a.as_path()));
        assert_eq!(fs::read(&a).unwrap(), b"a");
        assert!(d.join("sub").join("b.txt").exists());
        assert!(bin.items.borrow().is_empty());
        // Restored already: the places are taken, and the bin holds nothing.
        assert!(step.verify_with(&bin).unwrap_err().contains("exists again"));
        fs::remove_file(&a).unwrap();
        fs::remove_dir_all(&d).unwrap();
        assert!(step.verify_with(&bin).unwrap_err().contains("isn't in the Recycle Bin"));
    }

    fn item(path: &str, time: i64) -> TrashItem {
        let p = Path::new(path);
        TrashItem {
            id: OsString::from(format!("{path}@{time}")),
            name: p.file_name().unwrap().to_os_string(),
            original_parent: p.parent().unwrap().to_path_buf(),
            time_deleted: time,
        }
    }

    #[test]
    fn bin_matching_needs_path_and_time() {
        let want = |p: &str| Recycled { path: PathBuf::from(p), from: 1000, to: 1003 };
        let bin = vec![
            item(r"C:\data\a.txt", 900),  // an older deletion of the same name
            item(r"C:\data\a.txt", 1002), // ours
            item(r"C:\other\a.txt", 1002),
            item(r"C:\data\b.txt", 1010), // too late
        ];
        let found = find_in_bin(&bin, &[want(r"C:\data\a.txt")]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].time_deleted, 1002);
        if cfg!(windows) {
            // Case and a trailing separator don't count.
            assert!(find_in_bin(&bin, &[want(r"c:\DATA\A.TXT")]).is_ok());
        }
        assert!(find_in_bin(&bin, &[want(r"C:\data\b.txt")]).unwrap_err().contains("isn't in"));
        // Both must be found, or nothing is.
        assert!(find_in_bin(&bin, &[want(r"C:\data\a.txt"), want(r"C:\data\b.txt")]).is_err());

        // Two candidates in the window: ambiguous, refused.
        let twins = vec![item(r"C:\data\a.txt", 1001), item(r"C:\data\a.txt", 1002)];
        assert!(find_in_bin(&twins, &[want(r"C:\data\a.txt")]).unwrap_err().contains("several"));
    }
}
