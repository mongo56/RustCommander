//! Duplicate finder: files with the same content under a folder. Candidates
//! are grouped by size, then by a hash of the first 64 KB, then by the full
//! SHA-256 (`checksum::hash_file`). Hard links of one file count once. The
//! "same name" mode only compares files with the same name (any case) and
//! size. Nothing is changed on disk; the caller shows the groups.

use crate::checksum;
use crate::entry::{Entry, EntryKind};
use crate::fs::is_hidden;
use crate::search::NameMatcher;
use chrono::{DateTime, Local};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bytes of a file's start hashed in the second pass.
pub const HEAD_LEN: u64 = 64 * 1024;
const REPORT_EVERY: Duration = Duration::from_millis(100);

/// What to look through.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub root: PathBuf,
    /// Name masks / regex deciding which files take part; empty = all.
    pub name: String,
    pub name_regex: bool,
    pub subfolders: bool,
    pub hidden: bool,
    /// Only files with the same name (any case) and size are duplicates.
    pub same_name: bool,
    /// Empty files are all "equal"; skipped unless this is set.
    pub include_empty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Scan,
    Head,
    Full,
}

/// Where the scan is. `done`/`total` count files in the phase (`total` is 0
/// while scanning); `bytes` is the content hashed so far.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
    pub bytes: u64,
}

/// Files with identical content (at least two, each a different file).
#[derive(Debug, Clone)]
pub struct Group {
    /// Size of each file.
    pub size: u64,
    /// Names are relative to the root. Oldest first.
    pub files: Vec<Entry>,
}

impl Group {
    /// Space the copies besides one take.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Biggest waste first.
    pub groups: Vec<Group>,
    /// Files looked at.
    pub files: u64,
    pub errors: u64,
    pub cancelled: bool,
}

impl Report {
    pub fn wasted(&self) -> u64 {
        self.groups.iter().map(Group::wasted).sum()
    }
}

struct Cand {
    entry: Entry,
    key_name: String,
}

struct Ticker<F> {
    report: F,
    last: Instant,
}

impl<F: FnMut(Progress)> Ticker<F> {
    fn tick(&mut self, p: Progress, force: bool) {
        if force || self.last.elapsed() >= REPORT_EVERY {
            self.last = Instant::now();
            (self.report)(p);
        }
    }
}

/// Hash of the first [`HEAD_LEN`] bytes.
fn head_hash(path: &std::path::Path) -> std::io::Result<String> {
    let mut buf = Vec::new();
    fs::File::open(path)?.take(HEAD_LEN).read_to_end(&mut buf)?;
    Ok(Sha256::digest(&buf).iter().map(|b| format!("{b:02x}")).collect())
}

/// Finds duplicates. A bad name mask/regex is an error; a set `cancel`
/// returns early with `cancelled` and no groups.
pub fn find(opts: &Options, cancel: &AtomicBool, report: impl FnMut(Progress)) -> Result<Report, String> {
    let names = NameMatcher::new(&opts.name, opts.name_regex)?;
    let mut out = Report::default();
    let mut tick = Ticker { report, last: Instant::now() };
    let cancelled = |out: &mut Report| {
        let c = cancel.load(Ordering::Relaxed);
        out.cancelled = c;
        c
    };

    // 1. Candidates.
    let mut cands: Vec<Cand> = Vec::new();
    let mut stack = vec![opts.root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(items) = fs::read_dir(&dir) else {
            out.errors += 1;
            continue;
        };
        for item in items {
            if cancelled(&mut out) {
                out.groups.clear();
                return Ok(out);
            }
            let Ok(item) = item else {
                out.errors += 1;
                continue;
            };
            let Ok(meta) = item.metadata() else {
                out.errors += 1;
                continue;
            };
            let name = item.file_name().to_string_lossy().into_owned();
            if is_hidden(&name, &meta) && !opts.hidden {
                continue;
            }
            let Ok(ft) = item.file_type() else { continue };
            // Links are neither followed nor compared.
            if ft.is_symlink() || (ft.is_dir() && is_reparse(&meta)) {
                continue;
            }
            let path = item.path();
            if ft.is_dir() {
                if opts.subfolders {
                    stack.push(path);
                }
                continue;
            }
            out.files += 1;
            tick.tick(Progress { phase: Phase::Scan, done: out.files, total: 0, bytes: 0 }, false);
            if !ft.is_file() || !names.matches(&name) || (meta.len() == 0 && !opts.include_empty) {
                continue;
            }
            let rel = path.strip_prefix(&opts.root).unwrap_or(&path).to_string_lossy().into_owned();
            cands.push(Cand {
                key_name: if opts.same_name { name.to_lowercase() } else { String::new() },
                entry: Entry {
                    name: rel,
                    path,
                    kind: EntryKind::File,
                    size: meta.len(),
                    modified: meta.modified().ok().map(DateTime::<Local>::from),
                    hidden: false,
                },
            });
        }
    }

    // 2. Same size (and name): drop the unique ones.
    let mut by_size: HashMap<(u64, String), Vec<Cand>> = HashMap::new();
    for c in cands {
        by_size.entry((c.entry.size, c.key_name.clone())).or_default().push(c);
    }
    let mut groups: Vec<Vec<Cand>> = by_size.into_values().filter(|g| g.len() > 1).collect();

    // Hard links of one file are one candidate.
    for g in &mut groups {
        let mut seen = std::collections::HashSet::new();
        g.retain(|c| match rc_platform::file_id(&c.entry.path) {
            Some(id) => seen.insert(id),
            None => true,
        });
    }
    groups.retain(|g| g.len() > 1);

    // 3. First 64 KB.
    let total: u64 = groups.iter().map(|g| g.len() as u64).sum();
    let mut done = 0u64;
    let mut heads: Vec<Vec<Cand>> = Vec::new();
    for g in groups {
        let mut split: HashMap<String, Vec<Cand>> = HashMap::new();
        for c in g {
            if cancelled(&mut out) {
                out.groups.clear();
                return Ok(out);
            }
            done += 1;
            tick.tick(Progress { phase: Phase::Head, done, total, bytes: 0 }, false);
            match head_hash(&c.entry.path) {
                Ok(h) => split.entry(h).or_default().push(c),
                Err(_) => out.errors += 1,
            }
        }
        heads.extend(split.into_values().filter(|g| g.len() > 1));
    }

    // 4. Full hash (a file that fits in the head is already compared).
    let total: u64 = heads.iter().filter(|g| g[0].entry.size > HEAD_LEN).map(|g| g.len() as u64).sum();
    let (mut done, mut bytes) = (0u64, 0u64);
    for g in heads {
        if g[0].entry.size <= HEAD_LEN {
            out.groups.push(group_of(g));
            continue;
        }
        let mut split: HashMap<String, Vec<Cand>> = HashMap::new();
        for c in g {
            done += 1;
            let before = bytes;
            tick.tick(Progress { phase: Phase::Full, done, total, bytes }, true);
            let hashed = checksum::hash_file(&c.entry.path, cancel, |n| {
                tick.tick(Progress { phase: Phase::Full, done, total, bytes: before + n }, false);
            });
            match hashed {
                Ok(Some(h)) => {
                    bytes += c.entry.size;
                    split.entry(h).or_default().push(c);
                }
                Ok(None) => {
                    out.cancelled = true;
                    out.groups.clear();
                    return Ok(out);
                }
                Err(_) => out.errors += 1,
            }
        }
        out.groups.extend(split.into_values().filter(|g| g.len() > 1).map(group_of));
    }
    out.groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then_with(|| a.files[0].name.cmp(&b.files[0].name)));
    Ok(out)
}

fn group_of(cands: Vec<Cand>) -> Group {
    let mut files: Vec<Entry> = cands.into_iter().map(|c| c.entry).collect();
    files.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.name.cmp(&b.name)));
    Group { size: files[0].size, files }
}

#[cfg(windows)]
fn is_reparse(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rc-dups-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("a")).unwrap();
            fs::create_dir_all(dir.join("b")).unwrap();
            Tree(dir)
        }

        fn put(&self, rel: &str, bytes: &[u8]) {
            fs::write(self.0.join(rel), bytes).unwrap();
        }

        fn run(&self, same_name: bool) -> Report {
            let opts = Options { root: self.0.clone(), subfolders: true, same_name, ..Options::default() };
            find(&opts, &AtomicBool::new(false), |_| {}).unwrap()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(r: &Report) -> Vec<Vec<String>> {
        r.groups.iter().map(|g| g.files.iter().map(|e| e.name.replace('\\', "/")).collect()).collect()
    }

    #[test]
    fn identical_files_in_two_folders_are_one_group() {
        let t = Tree::new("same");
        t.put("a/x.txt", b"hello");
        t.put("b/y.txt", b"hello");
        t.put("b/z.txt", b"world");
        let r = t.run(false);
        assert_eq!(r.groups.len(), 1);
        let mut n = names(&r).remove(0);
        n.sort();
        assert_eq!(n, ["a/x.txt", "b/y.txt"]);
        assert_eq!(r.wasted(), 5);
    }

    #[test]
    fn same_size_different_content_is_not_a_duplicate() {
        let t = Tree::new("diff");
        t.put("a/x", b"aaaa");
        t.put("b/x", b"aaab");
        assert!(t.run(false).groups.is_empty());
    }

    #[test]
    fn same_head_different_tail_is_not_a_duplicate() {
        let t = Tree::new("tail");
        let mut one = vec![7u8; HEAD_LEN as usize + 1000];
        let mut two = one.clone();
        t.put("a/x", &one);
        t.put("b/x", &two);
        assert_eq!(t.run(false).groups.len(), 1);
        *two.last_mut().unwrap() = 8;
        t.put("b/x", &two);
        assert!(t.run(false).groups.is_empty());
        one[0] = 1;
        t.put("a/x", &one);
        assert!(t.run(false).groups.is_empty());
    }

    #[test]
    fn empty_files_are_skipped() {
        let t = Tree::new("empty");
        t.put("a/x", b"");
        t.put("b/y", b"");
        assert!(t.run(false).groups.is_empty());
        let opts = Options { root: t.0.clone(), subfolders: true, include_empty: true, ..Options::default() };
        let r = find(&opts, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.groups.len(), 1);
    }

    #[test]
    fn same_name_mode_needs_equal_names() {
        let t = Tree::new("name");
        t.put("a/Doc.txt", b"data");
        t.put("b/doc.TXT", b"data");
        t.put("b/other.txt", b"data");
        assert_eq!(t.run(false).groups[0].files.len(), 3);
        let r = t.run(true);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].files.len(), 2);
    }

    #[test]
    fn hard_links_count_once() {
        let t = Tree::new("link");
        t.put("a/x", b"data");
        fs::hard_link(t.0.join("a/x"), t.0.join("b/x2")).unwrap();
        assert!(t.run(false).groups.is_empty());
        t.put("b/copy", b"data");
        let r = t.run(false);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].files.len(), 2);
    }

    #[test]
    fn name_filter_limits_the_files() {
        let t = Tree::new("mask");
        t.put("a/x.txt", b"data");
        t.put("b/x.txt", b"data");
        t.put("a/y.bin", b"data");
        let opts = Options { root: t.0.clone(), name: "*.bin".into(), subfolders: true, ..Options::default() };
        assert!(find(&opts, &AtomicBool::new(false), |_| {}).unwrap().groups.is_empty());
    }

    #[test]
    fn cancel_stops() {
        let t = Tree::new("cancel");
        t.put("a/x", b"data");
        t.put("b/x", b"data");
        let opts = Options { root: t.0.clone(), subfolders: true, ..Options::default() };
        let r = find(&opts, &AtomicBool::new(true), |_| {}).unwrap();
        assert!(r.cancelled && r.groups.is_empty());
    }
}
