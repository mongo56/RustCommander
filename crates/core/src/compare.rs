//! Folder comparison (TC `cm_CompareDirs`): which entries of two listings
//! differ from their counterpart on the other side.

use std::collections::HashMap;

use crate::entry::{Entry, EntryKind};
use crate::rename::name_key;

/// Modification times closer than this count as equal (FAT stores times
/// with 2 s precision, and copies to/from it round them).
const TIME_TOLERANCE_MS: i64 = 2000;

/// The names to select on each side.
#[derive(Debug, Default, PartialEq, Eq)]
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
            }
        }
    };
    one_side(left, right, &right_index, &mut out.left);
    one_side(right, left, &left_index, &mut out.right);

    for l in left.iter().filter(|e| e.kind == EntryKind::File) {
        let Some(r) = right_index.get(&name_key(&l.name)).map(|&i| &right[i]) else { continue };
        if r.kind != EntryKind::File {
            continue;
        }
        let delta = match (l.modified, r.modified) {
            (Some(a), Some(b)) => (a - b).num_milliseconds(),
            _ => 0,
        };
        if delta > TIME_TOLERANCE_MS {
            out.left.push(l.name.clone());
        } else if delta < -TIME_TOLERANCE_MS {
            out.right.push(r.name.clone());
        } else if l.size != r.size {
            out.left.push(l.name.clone());
            out.right.push(r.name.clone());
        }
    }
    out
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
}
