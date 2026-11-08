//! Links in the built-in terminal's grid: OSC 8 hyperlinks a program set on
//! cells, and web addresses and absolute Windows paths found in the text.
//! Ctrl+hover underlines one, Ctrl+click opens it (`App::open_term_link`).
//!
//! Only http(s) addresses and local paths are links (file: URLs become
//! paths); other schemes are ignored, so terminal output can't get anything
//! else opened. A path is never run: a folder opens in a panel, a file is
//! selected there (or opened in the editor at a line).
//!
//! `Scan` collects the viewport's text while `Frame::build` walks the cells
//! (a line ending in a wrapped or filled last column continues on the next
//! one) and turns it into `Link`s with the cells they cover.

use regex::Regex;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::LazyLock;

/// What a link opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// An http or https address.
    Url(String),
    /// An absolute local path, with the line after it (`file.rs:12`).
    Path { path: PathBuf, line: Option<u32> },
}

/// A link in the viewport: its target and cells, (line, column, cells) per
/// line it covers.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Link {
    pub target: LinkTarget,
    pub spans: Vec<(u16, u16, u16)>,
}

impl Link {
    pub fn covers(&self, line: u16, col: u16) -> bool {
        self.spans.iter().any(|&(l, c, n)| l == line && (c..c + n).contains(&col))
    }
}

/// Adds the cell (line, col, cells) to `spans`, joined to the last span when
/// it follows it.
fn add_span(spans: &mut Vec<(u16, u16, u16)>, line: u16, col: u16, cells: u16) {
    match spans.last_mut() {
        Some((l, c, n)) if *l == line && *c + *n == col => *n += cells,
        _ => spans.push((line, col, cells)),
    }
}

/// Text of one viewport line: per character (byte offset) its cells; and
/// whether the line goes on in the next one.
#[derive(Debug, Default)]
struct LineText {
    text: String,
    /// (byte offset, column, cells, part of an OSC 8 link).
    chars: Vec<(usize, u16, u16, bool)>,
    joins: bool,
}

/// An OSC 8 link while scanning: id, uri and its cells.
type OscLink = (String, String, Vec<(u16, u16, u16)>);

/// Collects the viewport's text and OSC 8 links (see the module comment).
#[derive(Debug, Default)]
pub(super) struct Scan {
    lines: Vec<LineText>,
    osc: Vec<OscLink>,
}

impl Scan {
    pub fn new(lines: usize) -> Scan {
        Scan { lines: (0..lines).map(|_| LineText::default()).collect(), osc: Vec::new() }
    }

    /// A cell's character (`c`, blank for spaces and hidden text) with its
    /// zero-width extras, and its OSC 8 link (id, uri) if it has one.
    pub fn cell(&mut self, line: u16, col: u16, cells: u16, c: char, extra: &[char], link: Option<(&str, &str)>) {
        let Some(l) = self.lines.get_mut(line as usize) else { return };
        l.chars.push((l.text.len(), col, cells, link.is_some()));
        l.text.push(c);
        l.text.extend(extra);
        let Some((id, uri)) = link else { return };
        let found = self.osc.iter().rposition(|(i, u, _)| i == id && u == uri);
        let index = found.unwrap_or_else(|| {
            self.osc.push((id.to_owned(), uri.to_owned(), Vec::new()));
            self.osc.len() - 1
        });
        add_span(&mut self.osc[index].2, line, col, cells);
    }

    /// Line `line` continues in the next one (its text wrapped there).
    pub fn joins(&mut self, line: u16) {
        if let Some(l) = self.lines.get_mut(line as usize) {
            l.joins = true;
        }
    }

    /// The links: OSC 8 ones first, then the ones found in the text (not
    /// over cells of an OSC 8 link).
    pub fn finish(self) -> Vec<Link> {
        let mut links: Vec<Link> = self
            .osc
            .into_iter()
            .filter_map(|(_, uri, spans)| Some(Link { target: osc8_target(&uri)?, spans }))
            .collect();
        let mut text = String::new();
        // (byte offset, line, column, cells, OSC 8)
        let mut chars: Vec<(usize, u16, u16, u16, bool)> = Vec::new();
        for (index, l) in self.lines.into_iter().enumerate() {
            let base = text.len();
            text.push_str(&l.text);
            chars.extend(l.chars.iter().map(|&(b, col, n, osc)| (base + b, index as u16, col, n, osc)));
            if l.joins {
                continue;
            }
            if text.contains(':') {
                for (range, target) in find_links(&text) {
                    let cells = chars.iter().filter(|c| range.contains(&c.0));
                    if cells.clone().any(|c| c.4) {
                        continue;
                    }
                    let mut spans = Vec::new();
                    for &(_, line, col, n, _) in cells {
                        add_span(&mut spans, line, col, n);
                    }
                    if !spans.is_empty() {
                        links.push(Link { target, spans });
                    }
                }
            }
            text.clear();
            chars.clear();
        }
        links
    }
}

/// Web addresses (also file: URLs), quoted paths (may hold spaces) and plain
/// paths, each optionally followed by `:line` or `:line:column`.
static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?x)
        (?P<url>\b(?i:https?|file)://[^\s<>"'`]+)
        | "(?P<dq>[A-Za-z]:[\\/][^"<>|*?:\x00-\x1f]*)"(?::(?P<dql>\d+)(?::\d+)?)?
        | '(?P<sq>[A-Za-z]:[\\/][^'<>|*?:\x00-\x1f]*)'(?::(?P<sql>\d+)(?::\d+)?)?
        | (?P<path>\b[A-Za-z]:[\\/][^\s"'<>|*?:]*)(?::(?P<pl>\d+)(?::\d+)?)?
        "#,
    )
    .expect("link pattern")
});

/// The links in `text`: byte ranges and targets.
pub(super) fn find_links(text: &str) -> Vec<(Range<usize>, LinkTarget)> {
    let mut out = Vec::new();
    for caps in LINK.captures_iter(text) {
        let whole = caps.get(0).expect("match");
        let line = ["dql", "sql", "pl"].iter().find_map(|g| caps.name(g)).and_then(|m| m.as_str().parse().ok());
        let line = line.filter(|&n: &u32| n > 0);
        if let Some(url) = caps.name("url") {
            let s = trim_end(url.as_str());
            let target = if s.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("file:")) {
                file_url_path(s).map(|path| LinkTarget::Path { path, line: None })
            } else {
                rc_platform::is_web_url(s).then(|| LinkTarget::Url(s.to_owned()))
            };
            if let Some(target) = target {
                out.push((url.start()..url.start() + s.len(), target));
            }
        } else if let Some(path) = caps.name("dq").or_else(|| caps.name("sq")) {
            out.push((whole.range(), LinkTarget::Path { path: PathBuf::from(path.as_str()), line }));
        } else if let Some(path) = caps.name("path") {
            // With a line, the path ended at its colon; else trim punctuation.
            let (s, end) = if line.is_some() {
                (path.as_str(), whole.end())
            } else {
                let s = trim_end(path.as_str());
                (s, path.start() + s.len())
            };
            if s.len() > 2 {
                out.push((path.start()..end, LinkTarget::Path { path: PathBuf::from(s), line }));
            }
        }
    }
    out
}

/// `s` without punctuation that ends a sentence around it: trailing
/// `.,;:!?'"`, and closing brackets that don't close one in `s`.
fn trim_end(mut s: &str) -> &str {
    loop {
        let Some(last) = s.chars().last() else { return s };
        let open = match last {
            ')' => Some('('),
            ']' => Some('['),
            '}' => Some('{'),
            _ => None,
        };
        let cut = match open {
            Some(open) => s.matches(open).count() < s.matches(last).count(),
            None => matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"'),
        };
        if !cut {
            return s;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

/// What an OSC 8 link's URI opens: http(s) as is, a file: URL as its path,
/// nothing for other schemes.
pub(super) fn osc8_target(uri: &str) -> Option<LinkTarget> {
    if uri.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("file:")) {
        return file_url_path(uri).map(|path| LinkTarget::Path { path, line: None });
    }
    rc_platform::is_web_url(uri).then(|| LinkTarget::Url(uri.to_owned()))
}

/// The local path of a file: URL (`file:///C:/a%20b`, `file://localhost/C:/x`,
/// `file:/C:/x`): an absolute drive path, else `None` (other hosts too, so a
/// link can't make us reach out to a server).
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.get(5..)?;
    let rest = match rest.strip_prefix("//") {
        Some(r) => {
            let slash = r.find('/')?;
            let host = &r[..slash];
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            &r[slash..]
        }
        None => rest,
    };
    let rest = rest.split(['?', '#']).next()?.strip_prefix('/')?;
    let decoded = percent_decode(rest)?;
    let b = decoded.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\');
    if !drive || decoded.chars().any(char::is_control) {
        return None;
    }
    Some(PathBuf::from(decoded.replace('/', "\\")))
}

/// `%XX` escapes decoded; `None` when they don't form UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(text: &str) -> Vec<(&str, LinkTarget)> {
        find_links(text).into_iter().map(|(r, t)| (&text[r], t)).collect()
    }

    fn url(s: &str) -> LinkTarget {
        LinkTarget::Url(s.to_owned())
    }

    fn path(s: &str, line: Option<u32>) -> LinkTarget {
        LinkTarget::Path { path: PathBuf::from(s), line }
    }

    #[test]
    fn urls_end_before_punctuation() {
        assert_eq!(links("see https://x.y/a."), [("https://x.y/a", url("https://x.y/a"))]);
        assert_eq!(links("(https://x.y/a), next"), [("https://x.y/a", url("https://x.y/a"))]);
        assert_eq!(links("go to http://x.y, then"), [("http://x.y", url("http://x.y"))]);
        let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(links(&format!("{wiki}.")), [(wiki, url(wiki))]);
        assert_eq!(links("HTTPS://X.Y/q?a=1&b=2#f"), [("HTTPS://X.Y/q?a=1&b=2#f", url("HTTPS://X.Y/q?a=1&b=2#f"))]);
        assert_eq!(links("<https://x.y/>"), [("https://x.y/", url("https://x.y/"))]);
        // Other schemes, and no host.
        assert!(links("ftp://x.y javascript:alert(1) ms-settings:display https:// http:///x").is_empty());
    }

    #[test]
    fn paths_with_lines_and_spaces_only_quoted() {
        assert_eq!(links(r"C:\src\main.rs:12:5: error"), [(r"C:\src\main.rs:12:5", path(r"C:\src\main.rs", Some(12)))]);
        assert_eq!(links(r"in D:\a\b.txt."), [(r"D:\a\b.txt", path(r"D:\a\b.txt", None))]);
        assert_eq!(links(r"(see C:\x\y)"), [(r"C:\x\y", path(r"C:\x\y", None))]);
        assert_eq!(links(r"C:\Program Files\x"), [(r"C:\Program", path(r"C:\Program", None))]);
        let quoted = r#"copy "C:\Program Files\x y.txt":3 here"#;
        assert_eq!(links(quoted), [(r#""C:\Program Files\x y.txt":3"#, path(r"C:\Program Files\x y.txt", Some(3)))]);
        assert_eq!(links(r"'E:\my dir\'"), [(r"'E:\my dir\'", path(r"E:\my dir\", None))]);
        assert_eq!(links("C:/Users/x"), [("C:/Users/x", path("C:/Users/x", None))]);
        assert_eq!(links(r"PS C:\Users\me> dir"), [(r"C:\Users\me", path(r"C:\Users\me", None))]);
        // A drive alone, a letter inside a word, a relative path, a line 0.
        assert_eq!(links(r"C:\"), [(r"C:\", path(r"C:\", None))]);
        assert!(links(r"xC:\a src\main.rs:3").is_empty());
        assert_eq!(links(r"C:\a.rs:0"), [(r"C:\a.rs", path(r"C:\a.rs", None))]);
    }

    #[test]
    fn file_urls_become_paths() {
        assert_eq!(links("file:///C:/a%20b/c.txt"), [("file:///C:/a%20b/c.txt", path(r"C:\a b\c.txt", None))]);
        assert_eq!(file_url_path("file://localhost/D:/x"), Some(PathBuf::from(r"D:\x")));
        assert_eq!(file_url_path("file:/C:/x#frag"), Some(PathBuf::from(r"C:\x")));
        assert_eq!(file_url_path("file://server/share/x"), None);
        assert_eq!(file_url_path("file:///etc/passwd"), None);
        assert_eq!(file_url_path("file:///C:/a%0Ab"), None);
        assert_eq!(file_url_path("file:///C:/a%zz"), None);
        // The URL takes the path inside it, not a second link.
        assert_eq!(links("file:///C:/x").len(), 1);
    }

    #[test]
    fn osc8_targets() {
        assert_eq!(osc8_target("https://x.y"), Some(url("https://x.y")));
        assert_eq!(osc8_target("file:///C:/x/y.txt"), Some(path(r"C:\x\y.txt", None)));
        assert_eq!(osc8_target("vscode://file/C:/x"), None);
        assert_eq!(osc8_target("cmd.exe"), None);
        assert_eq!(osc8_target(r"C:\Windows\System32\calc.exe"), None);
        assert_eq!(osc8_target("https://x.y/a b"), None);
    }

    #[test]
    fn scan_maps_text_to_cells() {
        // "> https://x.y/ab" wrapped after 10 columns, a wide char before it.
        let mut scan = Scan::new(3);
        let mut col = 0;
        for c in "界 https:/".chars() {
            let w = if c == '界' { 2 } else { 1 };
            scan.cell(0, col, w, c, &[], None);
            col += w;
        }
        scan.joins(0);
        for (i, c) in "/x.y/ab ".chars().enumerate() {
            scan.cell(1, i as u16, 1, c, &[], None);
        }
        for (i, c) in "C:\\a".chars().enumerate() {
            scan.cell(2, i as u16, 1, c, &[], Some(("1", "https://osc.example")));
        }
        let links = scan.finish();
        assert_eq!(links.len(), 2);
        // The OSC 8 one; the path in its cells isn't a second link.
        assert_eq!(links[0], Link { target: url("https://osc.example"), spans: vec![(2, 0, 4)] });
        assert_eq!(links[1], Link { target: url("https://x.y/ab"), spans: vec![(0, 3, 7), (1, 0, 7)] });
        assert!(links[1].covers(1, 6) && links[1].covers(0, 3) && !links[1].covers(0, 2) && !links[1].covers(1, 7));
    }
}
