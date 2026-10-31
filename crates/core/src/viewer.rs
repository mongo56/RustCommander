//! File viewer model (F3): a file read into memory, split into lines for
//! the text view and 16-byte rows for the hex view, with text search.
//!
//! Files larger than [`LOAD_LIMIT`] are shown up to that size.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Most bytes read from a file.
pub const LOAD_LIMIT: u64 = 64 << 20;
/// Bytes per hex view row.
pub const HEX_WIDTH: usize = 16;
/// Columns a tab advances to the next multiple of.
pub const TAB_WIDTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Hex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    /// Not valid UTF-8: each byte is shown as its Latin-1 character.
    Latin1,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Latin1 => "ANSI",
        }
    }
}

/// A search hit: byte range in the decoded text (text mode) or in the
/// raw bytes (hex mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub mode: Mode,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug)]
pub struct Document {
    /// Size of the file on disk.
    pub size: u64,
    /// Only the first `LOAD_LIMIT` bytes were read.
    pub truncated: bool,
    pub encoding: Encoding,
    /// Looks like a binary file (NUL bytes, not UTF-16): opens in hex mode.
    pub binary: bool,
    bytes: Vec<u8>,
    text: String,
    /// Start offset of each line in `text`.
    lines: Vec<usize>,
}

impl Document {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        let mut bytes = Vec::with_capacity(size.min(LOAD_LIMIT) as usize);
        file.take(LOAD_LIMIT).read_to_end(&mut bytes)?;
        let truncated = (bytes.len() as u64) < size;
        Ok(Self::from_bytes(bytes, size, truncated))
    }

    pub fn from_bytes(bytes: Vec<u8>, size: u64, truncated: bool) -> Self {
        let (text, encoding) = decode(&bytes);
        let utf16 = matches!(encoding, Encoding::Utf16Le | Encoding::Utf16Be);
        let binary = !utf16 && bytes.iter().take(8192).any(|&b| b == 0);
        let mut lines = vec![0];
        lines.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        // A final newline doesn't start another line.
        if lines.len() > 1 && *lines.last().unwrap() == text.len() {
            lines.pop();
        }
        Document { size, truncated, encoding, binary, bytes, text, lines }
    }

    pub fn default_mode(&self) -> Mode {
        if self.binary {
            Mode::Hex
        } else {
            Mode::Text
        }
    }

    /// Rows in `mode`: lines of text or hex rows.
    pub fn rows(&self, mode: Mode) -> usize {
        match mode {
            Mode::Text => self.lines.len(),
            Mode::Hex => self.bytes.len().div_ceil(HEX_WIDTH).max(1),
        }
    }

    /// Line `i` without its line break.
    pub fn line(&self, i: usize) -> &str {
        let Some(&start) = self.lines.get(i) else { return "" };
        let end = self.lines.get(i + 1).copied().unwrap_or(self.text.len());
        let line = &self.text[start..end];
        let line = line.strip_suffix('\n').unwrap_or(line);
        line.strip_suffix('\r').unwrap_or(line)
    }

    /// Byte offset of line `i` in the text.
    pub fn line_start(&self, i: usize) -> usize {
        self.lines.get(i).copied().unwrap_or(self.text.len())
    }

    /// Line containing text offset `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        self.lines.partition_point(|&s| s <= offset).saturating_sub(1)
    }

    /// Hex row `i`: `offset  hex bytes  ascii`.
    pub fn hex_row(&self, i: usize) -> String {
        let start = i * HEX_WIDTH;
        let chunk = self.bytes.get(start..(start + HEX_WIDTH).min(self.bytes.len())).unwrap_or(&[]);
        let mut s = format!("{start:08X}  ");
        for k in 0..HEX_WIDTH {
            match chunk.get(k) {
                Some(b) => s.push_str(&format!("{b:02X} ")),
                None => s.push_str("   "),
            }
            if k == HEX_WIDTH / 2 - 1 {
                s.push(' ');
            }
        }
        s.push(' ');
        s.extend(chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }));
        s
    }

    /// Next (or previous) match of `needle` in `mode`. Forward searches
    /// start at `from`; backward ones find a match starting before it.
    pub fn find(&self, mode: Mode, needle: &str, from: usize, case: bool, forward: bool) -> Option<Hit> {
        if needle.is_empty() {
            return None;
        }
        let (start, end) = match mode {
            Mode::Text => find_text(&self.text, needle, from, case, forward)?,
            Mode::Hex => find_bytes(&self.bytes, needle.as_bytes(), from, case, forward)?,
        };
        Some(Hit { mode, start, end })
    }
}

/// Picks the encoding from a BOM or UTF-8 validity.
fn decode(bytes: &[u8]) -> (String, Encoding) {
    let utf16 = |rest: &[u8], le: bool| {
        let units = rest.as_chunks::<2>().0.iter().map(|&c| if le { u16::from_le_bytes(c) } else { u16::from_be_bytes(c) });
        char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return (utf16(rest, true), Encoding::Utf16Le);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return (utf16(rest, false), Encoding::Utf16Be);
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(body) {
        Ok(s) => (s.to_owned(), Encoding::Utf8),
        // A cut multi-byte character at the load limit is still UTF-8.
        Err(e) if e.error_len().is_none() => (String::from_utf8_lossy(body).into_owned(), Encoding::Utf8),
        Err(_) => (body.iter().map(|&b| b as char).collect(), Encoding::Latin1),
    }
}

/// `line` ready for display from column `skip`: tabs expanded, control
/// characters shown as `·`, at most `max` columns.
pub fn display_line(line: &str, skip: usize, max: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    let put = |c: char, col: &mut usize, out: &mut String| {
        if *col >= skip && *col < skip + max {
            out.push(c);
        }
        *col += 1;
    };
    for c in line.chars() {
        if col >= skip + max {
            break;
        }
        match c {
            '\t' => {
                let next = (col / TAB_WIDTH + 1) * TAB_WIDTH;
                while col < next {
                    put(' ', &mut col, &mut out);
                }
            }
            c if c.is_control() => put('·', &mut col, &mut out),
            c => put(c, &mut col, &mut out),
        }
    }
    out
}

/// Columns before byte offset `offset` of `line` once tabs are expanded.
pub fn column_of(line: &str, offset: usize) -> usize {
    line[..offset.min(line.len())]
        .chars()
        .fold(0, |col, c| if c == '\t' { (col / TAB_WIDTH + 1) * TAB_WIDTH } else { col + 1 })
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// End of `needle` if it matches `hay` at `pos`.
fn match_at(hay: &str, pos: usize, needle: &str, case: bool) -> Option<usize> {
    let mut hay_chars = hay[pos..].char_indices();
    for n in needle.chars() {
        let (_, h) = hay_chars.next()?;
        if !(h == n || !case && fold(h) == fold(n)) {
            return None;
        }
    }
    Some(hay_chars.next().map_or(hay.len(), |(i, _)| pos + i))
}

fn find_text(hay: &str, needle: &str, from: usize, case: bool, forward: bool) -> Option<(usize, usize)> {
    let mut from = from.min(hay.len());
    while !hay.is_char_boundary(from) {
        from -= 1;
    }
    if case && forward {
        let start = from + hay[from..].find(needle)?;
        return Some((start, start + needle.len()));
    }
    let hit = |pos: usize| match_at(hay, pos, needle, case).map(|end| (pos, end));
    if forward {
        hay[from..].char_indices().find_map(|(i, _)| hit(from + i))
    } else {
        hay[..from].char_indices().rev().find_map(|(i, _)| hit(i))
    }
}

fn find_bytes(hay: &[u8], needle: &[u8], from: usize, case: bool, forward: bool) -> Option<(usize, usize)> {
    let eq = |pos: usize| {
        hay.get(pos..pos + needle.len())
            .is_some_and(|w| if case { w == needle } else { w.eq_ignore_ascii_case(needle) })
    };
    let from = from.min(hay.len());
    let pos = if forward { (from..hay.len()).find(|&p| eq(p))? } else { (0..from).rev().find(|&p| eq(p))? };
    Some((pos, pos + needle.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(bytes: &[u8]) -> Document {
        Document::from_bytes(bytes.to_vec(), bytes.len() as u64, false)
    }

    #[test]
    fn splits_lines_with_any_line_break() {
        let d = doc(b"one\r\ntwo\nthree\n");
        assert_eq!(d.rows(Mode::Text), 3);
        assert_eq!((d.line(0), d.line(1), d.line(2)), ("one", "two", "three"));
        assert_eq!(d.line(3), "");
        assert_eq!(d.line_of(d.line_start(1) + 1), 1);
        assert_eq!(doc(b"").rows(Mode::Text), 1);
        assert_eq!(doc(b"a\n\n").rows(Mode::Text), 2);
    }

    #[test]
    fn detects_encodings() {
        assert_eq!(doc("čšž".as_bytes()).encoding, Encoding::Utf8);
        let d = doc(&[0xFF, 0xFE, b'h', 0, b'i', 0]);
        assert_eq!((d.encoding, d.line(0), d.binary), (Encoding::Utf16Le, "hi", false));
        let d = doc(&[b'a', 0xE8, b'b']);
        assert_eq!((d.encoding, d.line(0)), (Encoding::Latin1, "aèb"));
        assert_eq!(doc(&[0xEF, 0xBB, 0xBF, b'x']).line(0), "x");
        assert!(doc(&[1, 0, 2]).binary);
        assert_eq!(doc(&[1, 0, 2]).default_mode(), Mode::Hex);
    }

    #[test]
    fn formats_hex_rows() {
        let d = doc(b"ABCDEFGHIJKLMNOPQ\x01");
        assert_eq!(d.rows(Mode::Hex), 2);
        assert!(d.hex_row(0).starts_with("00000000  41 42 43 44 45 46 47 48  49 "));
        assert!(d.hex_row(0).ends_with(" ABCDEFGHIJKLMNOP"));
        assert!(d.hex_row(1).starts_with("00000010  51 01    "));
        assert!(d.hex_row(1).ends_with(" Q."));
    }

    #[test]
    fn expands_tabs_and_controls() {
        assert_eq!(display_line("a\tb", 0, 80), "a   b");
        assert_eq!(display_line("a\u{0}b", 0, 80), "a·b");
        assert_eq!(display_line("abcdef", 2, 3), "cde");
        assert_eq!(column_of("\tx", 1), 4);
    }

    #[test]
    fn finds_text_both_ways() {
        let d = doc("Hello world\nčao Svet hello".as_bytes());
        let f = |from, case, forward| d.find(Mode::Text, "hello", from, case, forward).map(|h| h.start);
        assert_eq!(f(0, false, true), Some(0));
        assert_eq!(f(1, false, true), Some(22));
        assert_eq!(f(0, true, true), Some(22));
        assert_eq!(f(22, false, false), Some(0));
        assert_eq!(f(0, false, false), None);
        let hit = d.find(Mode::Text, "SVET", 0, false, true).unwrap();
        assert_eq!(&d.text[hit.start..hit.end], "Svet");
        assert_eq!(d.find(Mode::Text, "ČAO", 0, false, true).map(|h| h.start), Some(12));
        assert_eq!(d.find(Mode::Hex, "WORLD", 0, false, true).map(|h| (h.start, h.end)), Some((6, 11)));
        assert_eq!(d.find(Mode::Hex, "WORLD", 0, true, true), None);
    }
}
