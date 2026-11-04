//! Find files (Ctrl+F, TC's `cm_SearchFor`): walks a folder tree matching
//! names against masks or a regex, optionally keeping only files whose
//! contents contain a text (plain or regex). Runs on the caller's thread
//! (the UI uses a worker), reporting hits in batches and stopping when the
//! cancel flag is set.
//!
//! Hits are [`Entry`]s named by their path relative to the search root
//! (`sub\file.txt`), so they can be shown in a panel as they are.
//!
//! With [`Query::archives`], archives met on the way (zip, 7z, tar, gz;
//! not archives inside them) are searched like folders, through their
//! [`crate::vfs::Vfs`]; a root inside an archive is always searched so.
//!
//! Contents are searched as raw bytes, like TC: the text is looked for as
//! UTF-8, which also finds ASCII text in ANSI files and strings inside
//! binary files. Files starting with a UTF-16 byte order mark are decoded
//! first (up to [`UTF16_LIMIT`]). Large files are read in [`CHUNK`]s; a
//! regex match longer than [`REGEX_OVERLAP`] bytes that crosses a chunk
//! boundary can be missed.

use crate::entry::{Entry, EntryKind};
use crate::fs::is_hidden;
use crate::vfs;
use chrono::{DateTime, Local};
use memchr::memmem;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bytes read from a file at a time.
pub const CHUNK: usize = 1 << 20;
/// Bytes carried over between chunks for regex searches.
pub const REGEX_OVERLAP: usize = 4096;
/// UTF-16 files are decoded whole up to this size.
pub const UTF16_LIMIT: u64 = 64 << 20;
/// The search stops after this many hits.
pub const MAX_HITS: usize = 50_000;
/// Hits are reported at least this often while searching...
const BATCH_TIME: Duration = Duration::from_millis(100);
/// ...or once this many are waiting.
const BATCH_HITS: usize = 500;

/// What to look for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub root: PathBuf,
    /// Name masks (see [`NameMatcher`]) or a regex; empty = everything.
    pub name: String,
    pub name_regex: bool,
    /// Text the file must contain; empty = don't read files.
    pub text: String,
    pub text_regex: bool,
    /// Text search is case-sensitive (names never are).
    pub match_case: bool,
    /// Descend into subfolders (links and junctions are not followed).
    pub subfolders: bool,
    /// Include hidden/system entries (and look inside hidden folders).
    pub hidden: bool,
    /// Look inside archives too.
    pub archives: bool,
}

/// Hits found since the last report, and where the search is.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    pub hits: Vec<Entry>,
    /// Folder being searched.
    pub folder: PathBuf,
    /// Files and folders looked at so far.
    pub checked: u64,
}

/// How a finished search went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub hits: usize,
    /// Files and folders looked at.
    pub files: u64,
    pub folders: u64,
    /// Folders or files that couldn't be read.
    pub errors: u64,
    pub cancelled: bool,
    /// Stopped at [`MAX_HITS`].
    pub truncated: bool,
}

/// Name test. Masks: patterns separated by `;`, any may match; a pattern
/// with `*`/`?` is a wildcard over the whole name, otherwise it matches
/// anywhere in the name. Patterns after a `|` exclude (`*.rs|test*`).
/// Case-insensitive. Regex: searched anywhere in the name, case-insensitive.
#[derive(Debug)]
pub enum NameMatcher {
    Masks { include: Vec<Vec<char>>, exclude: Vec<Vec<char>> },
    Regex(regex::Regex),
}

impl NameMatcher {
    pub fn new(pattern: &str, regex: bool) -> Result<Self, String> {
        if regex {
            return regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map(NameMatcher::Regex)
                .map_err(|e| format!("Bad name regex: {e}"));
        }
        let (inc, exc) = pattern.split_once('|').unwrap_or((pattern, ""));
        let parse = |s: &str| -> Vec<Vec<char>> {
            s.split(';').map(str::trim).filter(|p| !p.is_empty()).map(|p| p.to_lowercase().chars().collect()).collect()
        };
        Ok(NameMatcher::Masks { include: parse(inc), exclude: parse(exc) })
    }

    pub fn matches(&self, name: &str) -> bool {
        match self {
            NameMatcher::Regex(re) => re.is_match(name),
            NameMatcher::Masks { include, exclude } => {
                let name: Vec<char> = name.to_lowercase().chars().collect();
                let hit = |p: &Vec<char>| {
                    if p.iter().any(|&c| c == '*' || c == '?') {
                        crate::panel::wildcard(p, &name)
                    } else {
                        name.windows(p.len()).any(|w| w == p.as_slice())
                    }
                };
                (include.is_empty() || include.iter().any(hit)) && !exclude.iter().any(hit)
            }
        }
    }
}

/// Content test: plain text (`memmem` when case-sensitive, else an escaped
/// case-insensitive regex, which folds Unicode case) or a regex (`(?m)`).
#[derive(Debug)]
pub enum TextMatcher {
    Plain(Box<memmem::Finder<'static>>),
    Regex(regex::bytes::Regex),
}

impl TextMatcher {
    pub fn new(text: &str, regex: bool, match_case: bool) -> Result<Self, String> {
        if !regex && match_case {
            return Ok(TextMatcher::Plain(Box::new(memmem::Finder::new(text.as_bytes()).into_owned())));
        }
        let pattern = if regex { text.to_owned() } else { regex::escape(text) };
        regex::bytes::RegexBuilder::new(&pattern)
            .case_insensitive(!match_case)
            .multi_line(true)
            .build()
            .map(TextMatcher::Regex)
            .map_err(|e| format!("Bad text regex: {e}"))
    }

    pub fn is_match(&self, hay: &[u8]) -> bool {
        match self {
            TextMatcher::Plain(f) => f.find(hay).is_some(),
            TextMatcher::Regex(re) => re.is_match(hay),
        }
    }

    /// Bytes kept from the end of a chunk so matches across the boundary
    /// are found.
    fn overlap(&self) -> usize {
        match self {
            TextMatcher::Plain(f) => f.needle().len().saturating_sub(1),
            TextMatcher::Regex(_) => REGEX_OVERLAP,
        }
    }

    /// Whether the file's contents match (false once `cancel` is set).
    pub fn file_matches(&self, path: &Path, cancel: &AtomicBool) -> io::Result<bool> {
        self.reader_matches(&mut File::open(path)?, cancel)
    }

    /// [`Self::file_matches`] for any reader (e.g. a file in an archive).
    pub fn reader_matches(&self, file: &mut dyn Read, cancel: &AtomicBool) -> io::Result<bool> {
        let overlap = self.overlap();
        let mut buf = vec![0u8; CHUNK + overlap];
        let mut keep = 0;
        let mut first = true;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let read = read_full(file, &mut buf[keep..])?;
            let len = keep + read;
            if first {
                first = false;
                if let Some(big_endian) = utf16_bom(&buf[..len]) {
                    let mut all = buf[..len].to_vec();
                    Read::take(&mut *file, UTF16_LIMIT.saturating_sub(len as u64)).read_to_end(&mut all)?;
                    return Ok(self.is_match(decode_utf16(&all[2..], big_endian).as_bytes()));
                }
            }
            if self.is_match(&buf[..len]) {
                return Ok(true);
            }
            if keep + read < buf.len() {
                return Ok(false); // end of file
            }
            let carry = overlap.min(len);
            buf.copy_within(len - carry..len, 0);
            keep = carry;
        }
    }
}

/// Reads until `buf` is full or the file ends.
fn read_full(file: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// `Some(big_endian)` if the bytes start with a UTF-16 byte order mark.
fn utf16_bom(bytes: &[u8]) -> Option<bool> {
    match bytes {
        [0xFF, 0xFE, ..] => Some(false),
        [0xFE, 0xFF, ..] => Some(true),
        _ => None,
    }
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let units = bytes.as_chunks::<2>().0.iter().map(|&c| if big_endian { u16::from_be_bytes(c) } else { u16::from_le_bytes(c) });
    char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect()
}

/// Runs `query`, calling `report` with batches of hits (at least every
/// [`BATCH_TIME`] while hits are found, and once at the end). `report`
/// returning false stops the search, like setting `cancel`. Folders are
/// searched in name order, each folder's own entries before its
/// subfolders. Bad masks/regexes are an error before anything is read.
pub fn search(query: &Query, cancel: &AtomicBool, mut report: impl FnMut(Batch) -> bool) -> Result<Summary, String> {
    let names = NameMatcher::new(&query.name, query.name_regex)?;
    let text = if query.text.is_empty() {
        None
    } else {
        Some(TextMatcher::new(&query.text, query.text_regex, query.match_case)?)
    };
    let mut summary = Summary::default();
    let mut batch = Batch::default();
    if vfs::in_archive(&query.root) {
        batch.folder = query.root.clone();
        search_archive(&query.root, query, &names, text.as_ref(), cancel, &mut batch, &mut summary);
        summary.cancelled = cancel.load(Ordering::Relaxed);
        batch.checked = summary.files;
        report(batch);
        return Ok(summary);
    }
    let mut last_report = Instant::now();
    let mut stack = vec![query.root.clone()];
    'walk: while let Some(dir) = stack.pop() {
        summary.folders += 1;
        batch.folder = dir.clone();
        let Ok(items) = fs::read_dir(&dir) else {
            summary.errors += 1;
            continue;
        };
        let mut subfolders = Vec::new();
        for item in items {
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break 'walk;
            }
            // Hits waiting, or just progress when nothing is found.
            if batch.hits.len() >= BATCH_HITS || last_report.elapsed() >= BATCH_TIME {
                last_report = Instant::now();
                let hits = std::mem::take(&mut batch.hits);
                if !report(Batch { hits, folder: dir.clone(), checked: summary.files }) {
                    summary.cancelled = true;
                    break 'walk;
                }
            }
            let Ok(item) = item else {
                summary.errors += 1;
                continue;
            };
            let name = item.file_name().to_string_lossy().into_owned();
            let path = item.path();
            // The entry itself (a link is not followed here).
            let Ok(own) = item.metadata() else {
                summary.errors += 1;
                continue;
            };
            let hidden = is_hidden(&name, &own);
            if hidden && !query.hidden {
                continue;
            }
            let is_link = item.file_type().is_ok_and(|t| t.is_symlink());
            let meta = if is_link { fs::metadata(&path).unwrap_or(own) } else { own };
            let is_dir = meta.is_dir();
            summary.files += 1;
            if is_dir && query.subfolders && !is_link {
                subfolders.push(path.clone());
            }
            if query.archives && !is_dir && vfs::is_archive_name(&name) {
                batch.folder = path.clone();
                search_archive(&path, query, &names, text.as_ref(), cancel, &mut batch, &mut summary);
                if summary.truncated {
                    break 'walk;
                }
            }
            if !names.matches(&name) {
                continue;
            }
            if let Some(text) = &text {
                if is_dir {
                    continue;
                }
                match text.file_matches(&path, cancel) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => {
                        summary.errors += 1;
                        continue;
                    }
                }
            }
            let rel = path.strip_prefix(&query.root).unwrap_or(&path);
            batch.hits.push(Entry {
                name: rel.to_string_lossy().into_owned(),
                path: path.clone(),
                kind: if is_dir { EntryKind::Dir } else { EntryKind::File },
                size: if is_dir { 0 } else { meta.len() },
                modified: meta.modified().ok().map(DateTime::<Local>::from),
                hidden,
            });
            summary.hits += 1;
            if summary.hits >= MAX_HITS {
                summary.truncated = true;
                break 'walk;
            }
        }
        subfolders.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        stack.extend(subfolders.into_iter().rev());
    }
    batch.checked = summary.files;
    report(batch);
    Ok(summary)
}

/// Searches the archive (or the folder in one) `dir` through its `Vfs`:
/// names, then the contents of the files whose names match. Hits go to
/// `batch`.
fn search_archive(
    dir: &Path,
    query: &Query,
    names: &NameMatcher,
    text: Option<&TextMatcher>,
    cancel: &AtomicBool,
    batch: &mut Batch,
    summary: &mut Summary,
) {
    let Ok((items, vfs)) = vfs::open(dir).and_then(|v| Ok((v.walk(dir)?, v))) else {
        summary.errors += 1;
        return;
    };
    summary.folders += 1;
    let mut found = Vec::new();
    for item in items {
        if item.path == dir || (!query.subfolders && item.path.parent() != Some(dir)) {
            continue;
        }
        if item.hidden && !query.hidden {
            continue;
        }
        summary.files += 1;
        if names.matches(&item.name) && !(text.is_some() && item.is_dir()) {
            found.push(item);
        }
    }
    if let Some(text) = text {
        let paths: Vec<PathBuf> = found.iter().map(|e| e.path.clone()).collect();
        let mut matched = vec![false; found.len()];
        let mut errors = 0;
        let read = vfs.read_files(&paths, &mut |i, reader| {
            match text.reader_matches(reader, cancel) {
                Ok(m) => matched[i] = m,
                Err(_) => errors += 1,
            }
            Ok(!cancel.load(Ordering::Relaxed))
        });
        summary.errors += errors + u64::from(read.is_err());
        found = found.into_iter().zip(matched).filter_map(|(e, m)| m.then_some(e)).collect();
    }
    for mut hit in found {
        if summary.hits >= MAX_HITS {
            summary.truncated = true;
            return;
        }
        hit.name = hit.path.strip_prefix(&query.root).unwrap_or(&hit.path).to_string_lossy().into_owned();
        batch.hits.push(hit);
        summary.hits += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(pattern: &str, regex: bool, name: &str) -> bool {
        NameMatcher::new(pattern, regex).unwrap().matches(name)
    }

    #[test]
    fn name_masks() {
        assert!(names("", false, "anything"));
        assert!(names("*.rs", false, "Main.RS"));
        assert!(!names("*.rs", false, "main.rsx"));
        assert!(names("*.txt; *.rs", false, "a.rs"));
        assert!(names("main", false, "domain.rs"));
        assert!(names("*.rs|test*", false, "lib.rs"));
        assert!(!names("*.rs|test*", false, "tests.rs"));
        assert!(!names("|*.bak", false, "x.bak"));
        assert!(names("|*.bak", false, "x.txt"));
        assert!(names(r"^ma.n\.rs$", true, "MAIN.rs"));
        assert!(!names(r"^ma.n\.rs$", true, "xmain.rs"));
        assert!(NameMatcher::new("(", true).is_err());
    }

    fn text(t: &str, regex: bool, case: bool, hay: &[u8]) -> bool {
        TextMatcher::new(t, regex, case).unwrap().is_match(hay)
    }

    #[test]
    fn text_matchers() {
        assert!(text("Hello", false, true, b"say Hello\0there"));
        assert!(!text("hello", false, true, b"say Hello"));
        assert!(text("hello", false, false, b"say HELLO"));
        assert!(text("ÄBC", false, false, "xäbc".as_bytes()));
        assert!(text("a.c", false, false, b"xa.c"));
        assert!(!text("a.c", false, false, b"xabc"));
        assert!(text(r"^fn \w+", true, true, b"// x\nfn main() {}"));
        assert!(!text(r"^FN", true, true, b"fn"));
        assert!(text(r"^FN", true, false, b"fn"));
        assert!(TextMatcher::new("[", true, false).is_err());
    }

    fn temp(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rc-search-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn finds_text_across_chunk_boundaries_and_in_utf16() {
        let root = temp("chunks");
        let never = AtomicBool::new(false);
        // "needle" straddles the first chunk boundary.
        let mut big = vec![b'x'; CHUNK - 3];
        big.extend_from_slice(b"needle");
        big.extend(vec![b'y'; 100]);
        fs::write(root.join("big.bin"), &big).unwrap();
        for (t, regex, case) in [("needle", false, true), ("NEEDLE", false, false), ("ne+dle", true, true)] {
            let m = TextMatcher::new(t, regex, case).unwrap();
            assert!(m.file_matches(&root.join("big.bin"), &never).unwrap(), "{t}");
        }
        let m = TextMatcher::new("missing", false, true).unwrap();
        assert!(!m.file_matches(&root.join("big.bin"), &never).unwrap());

        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("Grüße aus Wien".encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(root.join("u16.txt"), &utf16).unwrap();
        let m = TextMatcher::new("grüße", false, false).unwrap();
        assert!(m.file_matches(&root.join("u16.txt"), &never).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn walks_tree_with_relative_names() {
        let root = temp("walk");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("one.txt"), "alpha").unwrap();
        fs::write(root.join("a/two.txt"), "beta gamma").unwrap();
        fs::write(root.join("a/b/three.txt"), "GAMMA").unwrap();
        fs::write(root.join("a/b/four.rs"), "gamma").unwrap();
        let run = |q: Query| {
            let mut hits = Vec::new();
            let never = AtomicBool::new(false);
            let summary = search(&q, &never, |b| {
                hits.extend(b.hits.into_iter().map(|e| e.name.replace('\\', "/")));
                true
            })
            .unwrap();
            hits.sort();
            (summary, hits)
        };
        let base = Query { root: root.clone(), subfolders: true, ..Query::default() };

        let (s, hits) = run(Query { name: "*.txt".into(), ..base.clone() });
        assert_eq!(hits, ["a/b/three.txt", "a/two.txt", "one.txt"]);
        assert_eq!(s.hits, 3);
        assert!(!s.cancelled && !s.truncated);

        let (_, hits) = run(Query { name: "*.txt".into(), subfolders: false, ..base.clone() });
        assert_eq!(hits, ["one.txt"]);

        // Folders match names too, but never text.
        let (_, hits) = run(Query { name: "b".into(), ..base.clone() });
        assert_eq!(hits, ["a/b"]);

        let (_, hits) = run(Query { text: "gamma".into(), ..base.clone() });
        assert_eq!(hits, ["a/b/four.rs", "a/b/three.txt", "a/two.txt"]);
        let (_, hits) = run(Query { text: "gamma".into(), match_case: true, ..base.clone() });
        assert_eq!(hits, ["a/b/four.rs", "a/two.txt"]);
        let (_, hits) = run(Query { name: r"\.txt$".into(), name_regex: true, text: "^G".into(), text_regex: true, match_case: true, ..base.clone() });
        assert_eq!(hits, ["a/b/three.txt"]);

        assert!(search(&Query { text: "(".into(), text_regex: true, ..base.clone() }, &AtomicBool::new(false), |_| true).is_err());

        let s = search(&base, &AtomicBool::new(true), |_| true).unwrap();
        assert!(s.cancelled);
        assert_eq!(s.hits, 0);

        // Inside archives: only with the option, or from a root in one.
        crate::vfs::tests::make_zip(&root.join("c").join("z.zip"));
        let (_, hits) = run(Query { name: "*.txt".into(), archives: true, ..base.clone() });
        assert_eq!(hits, ["a/b/three.txt", "a/two.txt", "c/z.zip/a.txt", "c/z.zip/sub/deep/x.txt", "one.txt"]);
        let (_, hits) = run(Query { text: "deep FILE".into(), archives: true, ..base.clone() });
        assert_eq!(hits, ["c/z.zip/sub/deep/x.txt"]);
        let (s, hits) = run(Query { text: "deep file".into(), ..base.clone() });
        assert_eq!((s.hits, hits.len()), (0, 0));
        let zip_root = Query { root: root.join("c").join("z.zip").join("sub"), subfolders: true, ..Query::default() };
        let (_, hits) = run(Query { name: "x*".into(), ..zip_root.clone() });
        assert_eq!(hits, ["deep/x.txt"]);
        let (_, hits) = run(Query { subfolders: false, ..zip_root });
        assert_eq!(hits, ["deep"]);
        fs::remove_dir_all(&root).unwrap();
    }
}
