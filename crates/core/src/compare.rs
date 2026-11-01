//! Folder comparison (TC `cm_CompareDirs`): which entries of two listings
//! differ from their counterpart on the other side.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::entry::{Entry, EntryKind};
use crate::rename::name_key;

/// Modification times closer than this count as equal (FAT stores times
/// with 2 s precision, and copies to/from it round them).
const TIME_TOLERANCE_MS: i64 = 2000;

/// What counts as a difference (Ctrl+Shift+F2 options).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompareOptions {
    /// Same-name, same-size files are compared byte by byte; equal contents
    /// are never marked, whatever their dates.
    pub by_content: bool,
    /// Dates don't matter: differing files are marked on both sides.
    pub ignore_date: bool,
    /// Folders present on both sides are compared recursively and marked
    /// on both sides when anything inside differs.
    pub subfolders: bool,
    /// Hidden entries take part when descending into subfolders.
    pub show_hidden: bool,
}

impl CompareOptions {
    /// Whether comparing reads the disk (and should run on a worker).
    pub fn reads_disk(&self) -> bool {
        self.by_content || self.subfolders
    }
}

/// The names to select on each side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Differences {
    pub left: Vec<String>,
    pub right: Vec<String>,
}

impl Differences {
    pub fn is_empty(&self) -> bool {
        self.left.is_empty() && self.right.is_empty()
    }
}

/// Like TC: an entry is marked when it has no counterpart on the other side
/// (same name, case-insensitive on Windows, same kind), or — for files —
/// when it's newer than its counterpart. Files with equal times but
/// different sizes are marked on both sides. Folders that exist on both
/// sides are never marked (their contents aren't compared).
pub fn compare(left: &[Entry], right: &[Entry]) -> Differences {
    let never = AtomicBool::new(false);
    compare_with(left, right, CompareOptions::default(), &never).unwrap_or_default()
}

/// [`compare`] with options; `None` when cancelled.
pub fn compare_with(left: &[Entry], right: &[Entry], opts: CompareOptions, cancel: &AtomicBool) -> Option<Differences> {
    diff(left, right, &opts, cancel, false)
}

/// The differences between two listings; with `first_only` it stops at the
/// first one (enough to know two folders differ).
fn diff(left: &[Entry], right: &[Entry], opts: &CompareOptions, cancel: &AtomicBool, first_only: bool) -> Option<Differences> {
    let index = |entries: &[Entry]| -> HashMap<String, usize> {
        let real = entries.iter().enumerate().filter(|(_, e)| !e.is_parent());
        real.map(|(i, e)| (name_key(&e.name), i)).collect()
    };
    let (left_index, right_index) = (index(left), index(right));
    let mut out = Differences::default();

    let one_side = |entries: &[Entry], other: &[Entry], other_index: &HashMap<String, usize>, mark: &mut Vec<String>| {
        for e in entries.iter().filter(|e| !e.is_parent()) {
            let twin = other_index.get(&name_key(&e.name)).map(|&i| &other[i]);
            if !twin.is_some_and(|t| t.kind == e.kind) {
                mark.push(e.name.clone());
                if first_only {
                    return;
                }
            }
        }
    };
    one_side(left, right, &right_index, &mut out.left);
    if first_only && !out.is_empty() {
        return Some(out);
    }
    one_side(right, left, &left_index, &mut out.right);
    if first_only && !out.is_empty() {
        return Some(out);
    }

    for l in left.iter().filter(|e| !e.is_parent()) {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Some(r) = right_index.get(&name_key(&l.name)).map(|&i| &right[i]) else { continue };
        if r.kind != l.kind {
            continue;
        }
        let (mark_left, mark_right) = match l.kind {
            EntryKind::Dir if opts.subfolders => {
                let differ = dirs_differ(&l.path, &r.path, opts, cancel)?;
                (differ, differ)
            }
            EntryKind::Dir => continue,
            _ => files_differ(l, r, opts, cancel)?,
        };
        if mark_left {
            out.left.push(l.name.clone());
        }
        if mark_right {
            out.right.push(r.name.clone());
        }
        if first_only && !out.is_empty() {
            break;
        }
    }
    Some(out)
}

/// Which of two same-name files to mark: (left, right).
fn files_differ(l: &Entry, r: &Entry, opts: &CompareOptions, cancel: &AtomicBool) -> Option<(bool, bool)> {
    let delta = match (l.modified, r.modified) {
        (Some(a), Some(b)) if !opts.ignore_date => (a - b).num_milliseconds(),
        _ => 0,
    };
    let differ = if opts.by_content {
        l.size != r.size || !same_content(&l.path, &r.path, cancel)?
    } else {
        delta.abs() > TIME_TOLERANCE_MS || l.size != r.size
    };
    Some(match differ {
        false => (false, false),
        true if delta > TIME_TOLERANCE_MS => (true, false),
        true if delta < -TIME_TOLERANCE_MS => (false, true),
        true => (true, true),
    })
}

/// Whether two folders' contents differ, recursively; unreadable folders
/// count as different.
fn dirs_differ(a: &Path, b: &Path, opts: &CompareOptions, cancel: &AtomicBool) -> Option<bool> {
    let (Ok(left), Ok(right)) = (crate::fs::list_dir(a, opts.show_hidden), crate::fs::list_dir(b, opts.show_hidden)) else {
        return Some(true);
    };
    diff(&left, &right, opts, cancel, true).map(|d| !d.is_empty())
}

/// Byte-by-byte comparison; read errors count as different, `None` when
/// cancelled.
fn same_content(a: &Path, b: &Path, cancel: &AtomicBool) -> Option<bool> {
    let compare = || -> io::Result<Option<bool>> {
        let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
        let mut ba = vec![0u8; 256 * 1024];
        let mut bb = vec![0u8; 256 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let n = read_full(&mut fa, &mut ba)?;
            let m = read_full(&mut fb, &mut bb)?;
            if n != m || ba[..n] != bb[..m] {
                return Ok(Some(false));
            }
            if n == 0 {
                return Ok(Some(true));
            }
        }
    };
    compare().unwrap_or(Some(false))
}

/// Fills `buf` unless the file ends first; returns the bytes read.
fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use std::path::PathBuf;

    fn file(name: &str, size: u64, secs: i64) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from(name),
            kind: EntryKind::File,
            size,
            modified: Local.timestamp_opt(secs, 0).single(),
            hidden: false,
        }
    }

    fn dir(name: &str) -> Entry {
        Entry { kind: EntryKind::Dir, ..file(name, 0, 0) }
    }

    #[test]
    fn marks_missing_newer_and_resized() {
        let left = vec![
            Entry::parent(PathBuf::from("..")),
            file("same.txt", 10, 1000),
            file("only-left.txt", 1, 1000),
            file("newer-left.txt", 1, 2000),
            file("newer-right.txt", 1, 1000),
            file("resized.txt", 1, 1000),
            file("fat.txt", 1, 1000),
            dir("both"),
            dir("only-left-dir"),
            file("kind", 1, 1000),
        ];
        let right = vec![
            Entry::parent(PathBuf::from("..")),
            file("SAME.txt", 10, 1000),
            file("newer-left.txt", 1, 1000),
            file("newer-right.txt", 1, 2000),
            file("resized.txt", 2, 1000),
            file("fat.txt", 1, 1001),
            dir("both"),
            dir("kind"),
            file("only-right.txt", 1, 1000),
        ];
        let d = compare(&left, &right);
        let mut l = d.left.clone();
        let mut r = d.right.clone();
        l.sort();
        r.sort();
        if cfg!(windows) {
            assert_eq!(l, ["kind", "newer-left.txt", "only-left-dir", "only-left.txt", "resized.txt"]);
            assert_eq!(r, ["kind", "newer-right.txt", "only-right.txt", "resized.txt"]);
        }
        assert!(!d.is_empty());
    }

    #[test]
    fn identical_folders() {
        let a = vec![file("a", 1, 5), dir("d")];
        assert!(compare(&a, &a.clone()).is_empty());
    }

    #[test]
    fn ignore_date_marks_both_sides() {
        let never = AtomicBool::new(false);
        let opts = CompareOptions { ignore_date: true, ..Default::default() };
        let left = vec![file("a", 1, 1000), file("b", 1, 1000)];
        let right = vec![file("a", 1, 9000), file("b", 2, 9000)];
        let d = compare_with(&left, &right, opts, &never).unwrap();
        assert_eq!(d.left, ["b"]);
        assert_eq!(d.right, ["b"]);
    }

    /// A scratch folder under the system temp dir, removed on drop.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, data: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn content_and_subfolders() {
        let root = std::env::temp_dir().join(format!("rc-compare-{}", std::process::id()));
        let _guard = Scratch(root.clone());
        let (a, b) = (root.join("a"), root.join("b"));
        write(&a.join("same.txt"), "hello");
        write(&b.join("same.txt"), "hello");
        write(&a.join("edited.txt"), "hello");
        write(&b.join("edited.txt"), "jello");
        write(&a.join("sub/deep/x.txt"), "1");
        write(&b.join("sub/deep/x.txt"), "2");
        write(&a.join("twin/y.txt"), "1");
        write(&b.join("twin/y.txt"), "1");
        let never = AtomicBool::new(false);
        let list = |p: &Path| crate::fs::list_dir(p, true).unwrap();
        let opts = CompareOptions { by_content: true, subfolders: true, show_hidden: true, ignore_date: false };
        let d = compare_with(&list(&a), &list(&b), opts, &never).unwrap();
        let (mut l, mut r) = (d.left.clone(), d.right.clone());
        l.sort();
        r.sort();
        assert_eq!(l, ["edited.txt", "sub"]);
        assert_eq!(r, ["edited.txt", "sub"]);

        let cancelled = AtomicBool::new(true);
        assert!(compare_with(&list(&a), &list(&b), opts, &cancelled).is_none());
    }
}
