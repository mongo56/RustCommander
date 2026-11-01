//! Multi-rename (TC's Ctrl+M): new names from a mask, search & replace and a
//! case change, previewed per file, then renamed as one batch.
//!
//! Mask placeholders (inside `[...]`, `[[` / `]]` are literal brackets):
//! - `[N]` name without extension, `[E]` extension, `[P]` parent folder name;
//!   `N`/`E` take a character range: `[N2]` 2nd char, `[N2-5]`, `[N3-]` to the
//!   end, `[N2,3]` 3 chars from the 2nd; negative positions count from the
//!   end (`[N-3-]` = last three).
//! - `[C]` counter; `[C10+5:3]` = start 10, step 5, 3 digits (each part
//!   optional, defaults from [`Counter`]).
//! - `[Y] [M] [D] [h] [m] [s]`, `[YMD]`, `[hms]`: modification time.

use crate::entry::Entry;
use chrono::{DateTime, Local};
use regex::{NoExpand, Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Case {
    #[default]
    Unchanged,
    Lower,
    Upper,
    /// First letter upper, the rest lower.
    First,
    /// Every word's first letter upper, the rest lower.
    Words,
}

impl Case {
    pub const ALL: [Case; 5] = [Case::Unchanged, Case::Lower, Case::Upper, Case::First, Case::Words];

    pub fn label(self) -> &'static str {
        match self {
            Case::Unchanged => "Unchanged",
            Case::Lower => "lower",
            Case::Upper => "UPPER",
            Case::First => "First upper",
            Case::Words => "Every Word",
        }
    }

    fn apply(self, s: &str) -> String {
        match self {
            Case::Unchanged => s.into(),
            Case::Lower => s.to_lowercase(),
            Case::Upper => s.to_uppercase(),
            Case::First => {
                let lower = s.to_lowercase();
                let mut chars = lower.chars();
                chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
            }
            Case::Words => {
                let mut out = String::with_capacity(s.len());
                let mut start = true;
                for c in s.chars() {
                    if start && c.is_alphanumeric() {
                        out.extend(c.to_uppercase());
                    } else {
                        out.extend(c.to_lowercase());
                    }
                    start = !c.is_alphanumeric() && c != '\'';
                }
                out
            }
        }
    }
}

/// Counter defaults for `[C]` placeholders without their own settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counter {
    pub start: i64,
    pub step: i64,
    pub digits: usize,
}

impl Default for Counter {
    fn default() -> Self {
        Self { start: 1, step: 1, digits: 1 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub name_mask: String,
    pub ext_mask: String,
    pub search: String,
    pub replace: String,
    pub regex: bool,
    pub match_case: bool,
    pub case: Case,
    pub counter: Counter,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            name_mask: "[N]".into(),
            ext_mask: "[E]".into(),
            search: String::new(),
            replace: String::new(),
            regex: false,
            match_case: false,
            case: Case::Unchanged,
            counter: Counter::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Whole,
    /// Up to and including this position.
    To(i64),
    Len(usize),
    Rest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Lit(String),
    Name(i64, End),
    Ext(i64, End),
    Parent,
    Counter { start: Option<i64>, step: Option<i64>, digits: Option<usize> },
    /// strftime format of the modification time.
    Date(&'static str),
}

fn parse_mask(mask: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut lit = String::new();
    let mut rest = mask;
    while let Some(c) = rest.chars().next() {
        if rest.starts_with("[[") || rest.starts_with("]]") {
            lit.push(c);
            rest = &rest[2..];
            continue;
        }
        if c != '[' {
            lit.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let close = rest.find(']').ok_or_else(|| format!("Missing ] in {rest}"))?;
        let inner = &rest[1..close];
        if !lit.is_empty() {
            tokens.push(Token::Lit(std::mem::take(&mut lit)));
        }
        tokens.push(parse_placeholder(inner).ok_or_else(|| format!("Unknown placeholder [{inner}]"))?);
        rest = &rest[close + 1..];
    }
    if !lit.is_empty() {
        tokens.push(Token::Lit(lit));
    }
    Ok(tokens)
}

fn parse_placeholder(s: &str) -> Option<Token> {
    let date = match s {
        "Y" => Some("%Y"),
        "M" => Some("%m"),
        "D" => Some("%d"),
        "h" => Some("%H"),
        "m" => Some("%M"),
        "s" => Some("%S"),
        "YMD" => Some("%Y%m%d"),
        "hms" => Some("%H%M%S"),
        _ => None,
    };
    if let Some(f) = date {
        return Some(Token::Date(f));
    }
    let mut chars = s.chars();
    let head = chars.next()?.to_ascii_uppercase();
    let spec = chars.as_str();
    match head {
        'N' | 'E' => {
            let (from, end) = parse_range(spec)?;
            Some(if head == 'N' { Token::Name(from, end) } else { Token::Ext(from, end) })
        }
        'P' if spec.is_empty() => Some(Token::Parent),
        'C' => parse_counter(spec),
        _ => None,
    }
}

/// Reads an optionally signed integer from the front of `s`.
fn take_int(s: &str) -> Option<(i64, &str)> {
    let digits_at = usize::from(s.starts_with('-') || s.starts_with('+'));
    let len = s[digits_at..].bytes().take_while(u8::is_ascii_digit).count();
    if len == 0 {
        return None;
    }
    let n = s[..digits_at + len].parse().ok()?;
    Some((n, &s[digits_at + len..]))
}

fn parse_range(spec: &str) -> Option<(i64, End)> {
    if spec.is_empty() {
        return Some((1, End::Whole));
    }
    let (from, rest) = take_int(spec)?;
    if from == 0 {
        return None;
    }
    let end = if rest.is_empty() {
        End::To(from)
    } else if let Some(r) = rest.strip_prefix(',') {
        let (len, r) = take_int(r)?;
        if !r.is_empty() || len < 0 {
            return None;
        }
        End::Len(len as usize)
    } else {
        let r = rest.strip_prefix('-')?;
        if r.is_empty() {
            End::Rest
        } else {
            let (to, r) = take_int(r)?;
            if !r.is_empty() || to == 0 {
                return None;
            }
            End::To(to)
        }
    };
    Some((from, end))
}

fn parse_counter(spec: &str) -> Option<Token> {
    let (mut start, mut step, mut digits) = (None, None, None);
    let mut rest = spec;
    if let Some((n, r)) = take_int(rest).filter(|_| !rest.starts_with('+')) {
        start = Some(n);
        rest = r;
    }
    if let Some(r) = rest.strip_prefix('+') {
        let (n, r) = take_int(r)?;
        step = Some(n);
        rest = r;
    }
    if let Some(r) = rest.strip_prefix(':') {
        let (n, r) = take_int(r)?;
        digits = Some(n.clamp(1, 20) as usize);
        rest = r;
    }
    rest.is_empty().then_some(Token::Counter { start, step, digits })
}

/// Characters `from..=end` of `s` (1-based, negative = from the end).
fn slice(s: &str, from: i64, end: End) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let pos = |p: i64| if p < 0 { n + p + 1 } else { p };
    let a = pos(from).max(1);
    let b = match end {
        End::Whole | End::Rest => n,
        End::To(t) => pos(t),
        End::Len(len) => a + len as i64 - 1,
    }
    .min(n);
    if a > b {
        return String::new();
    }
    chars[(a - 1) as usize..b as usize].iter().collect()
}

struct Item<'a> {
    stem: &'a str,
    ext: &'a str,
    parent: String,
    modified: Option<DateTime<Local>>,
    index: i64,
}

fn render(tokens: &[Token], item: &Item, counter: Counter) -> String {
    let mut out = String::new();
    for t in tokens {
        match t {
            Token::Lit(s) => out.push_str(s),
            Token::Name(from, end) => out.push_str(&slice(item.stem, *from, *end)),
            Token::Ext(from, end) => out.push_str(&slice(item.ext, *from, *end)),
            Token::Parent => out.push_str(&item.parent),
            Token::Counter { start, step, digits } => {
                let v = start.unwrap_or(counter.start) + item.index * step.unwrap_or(counter.step);
                let width = digits.unwrap_or(counter.digits);
                if v < 0 {
                    out.push_str(&format!("-{:0width$}", -v));
                } else {
                    out.push_str(&format!("{v:0width$}"));
                }
            }
            Token::Date(f) => {
                if let Some(m) = item.modified {
                    out.push_str(&m.format(f).to_string());
                }
            }
        }
    }
    out
}

/// Masks and the search pattern, checked once and applied per file.
pub struct Renamer {
    name: Vec<Token>,
    ext: Vec<Token>,
    search: Option<(Regex, String)>,
    regex: bool,
    case: Case,
    counter: Counter,
}

impl Renamer {
    pub fn new(opts: &Options) -> Result<Self, String> {
        let name = parse_mask(&opts.name_mask).map_err(|e| format!("Name mask: {e}"))?;
        let ext = parse_mask(&opts.ext_mask).map_err(|e| format!("Extension mask: {e}"))?;
        let search = if opts.search.is_empty() {
            None
        } else {
            let pattern = if opts.regex { opts.search.clone() } else { regex::escape(&opts.search) };
            let re = RegexBuilder::new(&pattern)
                .case_insensitive(!opts.match_case)
                .build()
                .map_err(|e| format!("Search: {}", e.to_string().lines().last().unwrap_or("bad pattern")))?;
            Some((re, opts.replace.clone()))
        };
        Ok(Self { name, ext, search, regex: opts.regex, case: opts.case, counter: opts.counter })
    }

    /// The new name of `entry`, the `index`-th file of the batch.
    pub fn apply(&self, entry: &Entry, index: usize) -> String {
        let (stem, ext) = entry.split_name();
        let parent = entry
            .path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let item = Item { stem, ext, parent, modified: entry.modified, index: index as i64 };
        let stem = render(&self.name, &item, self.counter);
        let ext = render(&self.ext, &item, self.counter);
        let mut name = if ext.is_empty() { stem } else { format!("{stem}.{ext}") };
        if let Some((re, with)) = &self.search {
            name = if self.regex {
                re.replace_all(&name, with.as_str()).into_owned()
            } else {
                re.replace_all(&name, NoExpand(with)).into_owned()
            };
        }
        self.case.apply(&name)
    }
}

/// Name comparison key: file names are case-insensitive on Windows.
pub fn name_key(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.into()
    }
}

/// Why `name` can't be a file name, if it can't.
pub fn invalid_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("empty name");
    }
    if name == "." || name == ".." {
        return Some("reserved name");
    }
    let bad: &[char] = if cfg!(windows) { &['\\', '/', ':', '*', '?', '"', '<', '>', '|'] } else { &['/'] };
    if name.contains(bad) || name.chars().any(char::is_control) {
        return Some("invalid character");
    }
    if cfg!(windows) && (name.ends_with('.') || name.ends_with(' ')) {
        return Some("ends with a dot or space");
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub new_name: String,
    /// The name was typed by hand (an `edits` entry), not computed.
    pub edited: bool,
    /// Why this file can't be renamed as shown.
    pub problem: Option<String>,
}

impl Preview {
    pub fn unchanged(&self, old: &str) -> bool {
        self.new_name == old
    }
}

/// New names for `entries` (all in one folder). `others` holds the
/// [`name_key`]s of that folder's names that aren't being renamed. `edits`
/// are names typed by hand, by entry index; they replace the computed name
/// (an empty one is ignored).
pub fn preview(
    entries: &[Entry],
    opts: &Options,
    others: &HashSet<String>,
    edits: &HashMap<usize, String>,
) -> Result<Vec<Preview>, String> {
    let renamer = Renamer::new(opts)?;
    let names: Vec<(String, bool)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| match edits.get(&i).filter(|n| !n.is_empty()) {
            Some(n) => (n.clone(), true),
            None => (renamer.apply(e, i), false),
        })
        .collect();
    let mut count: HashMap<String, usize> = HashMap::new();
    for (n, _) in &names {
        *count.entry(name_key(n)).or_default() += 1;
    }
    Ok(names
        .into_iter()
        .map(|(new_name, edited)| {
            let key = name_key(&new_name);
            let problem = invalid_name(&new_name)
                .map(str::to_owned)
                .or_else(|| (count[&key] > 1).then(|| "duplicate name".into()))
                .or_else(|| others.contains(&key).then(|| "already exists".into()));
            Preview { new_name, edited, problem }
        })
        .collect())
}

/// One rename that failed.
#[derive(Debug)]
pub struct Failure {
    pub from: PathBuf,
    pub to: PathBuf,
    pub error: io::Error,
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

/// Renames without ever overwriting: `std::fs::rename` replaces existing
/// files on Windows. A case-only change of the same file is allowed.
fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    let same_file = from.parent() == to.parent()
        && from.file_name().map(|n| name_key(&n.to_string_lossy())) == to.file_name().map(|n| name_key(&n.to_string_lossy()));
    if !same_file && exists(to) {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "the target already exists"));
    }
    std::fs::rename(from, to)
}

/// Renames every `(from, to)` pair, never overwriting. Pairs whose target is
/// another pair's source (swaps, shifts like a→b→c) go through temporary
/// names first. Returns the renames done (for undo) and the failures.
pub fn rename_all(pairs: &[(PathBuf, PathBuf)]) -> (Vec<(PathBuf, PathBuf)>, Vec<Failure>) {
    let sources: HashSet<(Option<&Path>, String)> =
        pairs.iter().map(|(f, _)| (f.parent(), name_key(&f.file_name().unwrap_or_default().to_string_lossy()))).collect();
    let chained = |to: &Path| sources.contains(&(to.parent(), name_key(&to.file_name().unwrap_or_default().to_string_lossy())));

    let mut done = Vec::new();
    let mut failed = Vec::new();
    let mut staged = Vec::new();
    let stamp = std::process::id();
    for (i, (from, to)) in pairs.iter().enumerate() {
        if from == to {
            continue;
        }
        if !chained(to) {
            match rename_new(from, to) {
                Ok(()) => done.push((from.clone(), to.clone())),
                Err(error) => failed.push(Failure { from: from.clone(), to: to.clone(), error }),
            }
            continue;
        }
        let tmp = from.with_file_name(format!(".rc-rename-{stamp}-{i}.tmp"));
        match rename_new(from, &tmp) {
            Ok(()) => staged.push((from, tmp, to)),
            Err(error) => failed.push(Failure { from: from.clone(), to: to.clone(), error }),
        }
    }
    for (from, tmp, to) in staged {
        match rename_new(&tmp, to) {
            Ok(()) => done.push((from.clone(), to.clone())),
            Err(error) => {
                // Put it back; if even that fails the file keeps its
                // temporary name, which the error shows.
                let error = match std::fs::rename(&tmp, from) {
                    Ok(()) => error,
                    Err(e) => io::Error::new(e.kind(), format!("{error}; left as {}", tmp.display())),
                };
                failed.push(Failure { from: from.clone(), to: to.clone(), error });
            }
        }
    }
    (done, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn file(name: &str) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from("/photos").join(name),
            kind: EntryKind::File,
            size: 0,
            modified: None,
            hidden: false,
        }
    }

    fn names(files: &[&str], opts: &Options) -> Vec<String> {
        let entries: Vec<Entry> = files.iter().map(|n| file(n)).collect();
        preview(&entries, opts, &HashSet::new(), &HashMap::new()).unwrap().into_iter().map(|p| p.new_name).collect()
    }

    fn mask(name: &str, ext: &str) -> Options {
        Options { name_mask: name.into(), ext_mask: ext.into(), ..Options::default() }
    }

    #[test]
    fn default_masks_keep_names() {
        assert_eq!(names(&["a.txt", "b", ".rc"], &Options::default()), ["a.txt", "b", ".rc"]);
    }

    #[test]
    fn ranges() {
        let f = ["abcdef.txt"];
        assert_eq!(names(&f, &mask("[N2]", "")), ["b"]);
        assert_eq!(names(&f, &mask("[N2-4]", "")), ["bcd"]);
        assert_eq!(names(&f, &mask("[N3-]", "")), ["cdef"]);
        assert_eq!(names(&f, &mask("[N2,3]", "")), ["bcd"]);
        assert_eq!(names(&f, &mask("[N-2-]", "")), ["ef"]);
        assert_eq!(names(&f, &mask("[N1--2]", "")), ["abcde"]);
        assert_eq!(names(&f, &mask("[N9]x", "[E1]")), ["x.t"]);
        assert_eq!(names(&f, &mask("[[[N1]]]", "[E]")), ["[a].txt"]);
    }

    #[test]
    fn counter_and_parent() {
        assert_eq!(names(&["a.jpg", "b.jpg"], &mask("[P]_[C]", "[E]")), ["photos_1.jpg", "photos_2.jpg"]);
        assert_eq!(names(&["a", "b", "c"], &mask("[C10+5:3]", "")), ["010", "015", "020"]);
        let mut opts = mask("[N][C]", "");
        opts.counter = Counter { start: 0, step: 2, digits: 2 };
        assert_eq!(names(&["a", "b"], &opts), ["a00", "b02"]);
        assert_eq!(names(&["a"], &mask("[C:2]", "")), ["01"]);
    }

    #[test]
    fn bad_masks() {
        for m in ["[N", "[X]", "[N0]", "[N1-x]", "[C+]", "[P1]"] {
            assert!(Renamer::new(&mask(m, "")).is_err(), "{m}");
        }
    }

    #[test]
    fn search_replace_and_case() {
        let mut opts = Options { search: "IMG".into(), replace: "photo".into(), ..Options::default() };
        assert_eq!(names(&["img_01.JPG"], &opts), ["photo_01.JPG"]);
        opts.match_case = true;
        assert_eq!(names(&["img_01.JPG"], &opts), ["img_01.JPG"]);
        let opts = Options { search: r"(\d+)_(\d+)".into(), replace: "$2-$1".into(), regex: true, ..Options::default() };
        assert_eq!(names(&["x12_34.png"], &opts), ["x34-12.png"]);
        let opts = Options { search: ".".into(), replace: "$0".into(), ..Options::default() };
        assert_eq!(names(&["a.b"], &opts), ["a$0b"]);
        let case = |c| Options { case: c, ..Options::default() };
        assert_eq!(names(&["hELLO wORLD.Txt"], &case(Case::Lower)), ["hello world.txt"]);
        assert_eq!(names(&["hELLO wORLD.Txt"], &case(Case::First)), ["Hello world.txt"]);
        assert_eq!(names(&["hELLO wORLD-it's.Txt"], &case(Case::Words)), ["Hello World-It's.Txt"]);
    }

    #[test]
    fn problems() {
        let entries = [file("a.txt"), file("b.txt"), file("c.txt")];
        let others: HashSet<String> = [name_key("x.txt")].into();
        let opts = mask("x", "[E]");
        let p = preview(&entries[..1], &opts, &others, &HashMap::new()).unwrap();
        assert_eq!(p[0].problem.as_deref(), Some("already exists"));
        let p = preview(&entries[1..], &opts, &others, &HashMap::new()).unwrap();
        assert!(p.iter().all(|p| p.problem.is_some()));
        let p = preview(&entries[..1], &mask("a?b", ""), &others, &HashMap::new()).unwrap();
        assert!(p[0].problem.is_some());
    }

    #[test]
    fn edits_override_computed_names() {
        let entries = [file("a.txt"), file("b.txt"), file("c.txt")];
        let opts = mask("[N]_x", "[E]");
        let edits: HashMap<usize, String> = [(1, "hand.md".to_string()), (2, String::new())].into();
        let p = preview(&entries, &opts, &HashSet::new(), &edits).unwrap();
        let got: Vec<(&str, bool)> = p.iter().map(|p| (p.new_name.as_str(), p.edited)).collect();
        assert_eq!(got, [("a_x.txt", false), ("hand.md", true), ("c_x.txt", false)]);
        assert!(p.iter().all(|p| p.problem.is_none()));
        // An edit clashing with a computed name is a duplicate.
        let edits: HashMap<usize, String> = [(1, "A_X.txt".to_string())].into();
        let p = preview(&entries, &opts, &HashSet::new(), &edits).unwrap();
        if cfg!(windows) {
            assert_eq!(p[0].problem.as_deref(), Some("duplicate name"));
            assert_eq!(p[1].problem.as_deref(), Some("duplicate name"));
        }
        let edits: HashMap<usize, String> = [(0, "bad?".to_string())].into();
        assert!(preview(&entries, &opts, &HashSet::new(), &edits).unwrap()[0].problem.is_some());
    }

    #[test]
    fn rename_all_handles_swaps_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("rc-rename-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (n, body) in [("a", "A"), ("b", "B"), ("c", "C"), ("keep", "K")] {
            std::fs::write(dir.join(n), body).unwrap();
        }
        let p = |n: &str| dir.join(n);
        // Swap a and b, shift c onto "keep" (which isn't in the batch).
        let (done, failed) = rename_all(&[(p("a"), p("b")), (p("b"), p("a")), (p("c"), p("keep"))]);
        assert_eq!(done.len(), 2);
        assert_eq!(failed.len(), 1);
        assert_eq!(std::fs::read_to_string(p("a")).unwrap(), "B");
        assert_eq!(std::fs::read_to_string(p("b")).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(p("c")).unwrap(), "C");
        assert_eq!(std::fs::read_to_string(p("keep")).unwrap(), "K");
        // Undo = the done pairs reversed.
        let undo: Vec<_> = done.iter().map(|(f, t)| (t.clone(), f.clone())).collect();
        let (_, failed) = rename_all(&undo);
        assert!(failed.is_empty());
        assert_eq!(std::fs::read_to_string(p("a")).unwrap(), "A");
        // Case-only change.
        let (_, failed) = rename_all(&[(p("a"), p("A"))]);
        assert!(failed.is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).count(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
