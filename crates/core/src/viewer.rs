//! File viewer model (F3): a file read into memory, split into lines for
//! the text view and 16-byte rows for the hex view, with text search.
//!
//! Files up to [`LOAD_LIMIT`] are read whole. Larger ones are shown through
//! a [`WINDOW`]-sized slice ([`Document::open_window`]) that the viewer moves
//! as it scrolls; [`find_in_file`] searches them window by window.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Largest file read whole.
pub const LOAD_LIMIT: u64 = 64 << 20;
/// Bytes read per window of a larger file.
pub const WINDOW: u64 = 4 << 20;
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

    /// Bytes per code unit.
    fn unit(self) -> usize {
        match self {
            Encoding::Utf16Le | Encoding::Utf16Be => 2,
            _ => 1,
        }
    }

    fn newline(self) -> &'static [u8] {
        match self {
            Encoding::Utf16Le => &[b'\n', 0],
            Encoding::Utf16Be => &[0, b'\n'],
            _ => b"\n",
        }
    }

    fn bom(self) -> &'static [u8] {
        match self {
            Encoding::Utf8 => &[0xEF, 0xBB, 0xBF],
            Encoding::Utf16Le => &[0xFF, 0xFE],
            Encoding::Utf16Be => &[0xFE, 0xFF],
            Encoding::Latin1 => &[],
        }
    }
}

/// A search hit: byte range in the decoded text (text mode) or in the
/// raw bytes (hex mode) of the document it was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub mode: Mode,
    pub start: usize,
    pub end: usize,
}

/// A file, or a window of one: raw bytes for hex rows and their decoded
/// text (whole lines only) for text lines.
#[derive(Debug)]
pub struct Document {
    /// Size of the file on disk.
    pub size: u64,
    pub encoding: Encoding,
    /// Looks like a binary file (NUL bytes, not UTF-16): opens in hex mode.
    pub binary: bool,
    /// File offset of `bytes[0]`.
    base: u64,
    bytes: Vec<u8>,
    /// Offset in `bytes` where `text` starts / ends.
    text_start: usize,
    text_end: usize,
    text: String,
    /// Start offset of each line in `text`.
    lines: Vec<usize>,
}

impl Document {
    /// Reads `path` whole, or its first window when it is too large.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        if size > LOAD_LIMIT {
            return Self::open_window(path, 0, None);
        }
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(LOAD_LIMIT).read_to_end(&mut bytes)?;
        let size = size.max(bytes.len() as u64);
        Ok(Self::build(bytes, size, 0, 0, None))
    }

    /// Reads a window of `path` whose text starts at the first line start
    /// at or after file offset `want`. `encoding` is detected when `None`.
    pub fn open_window(path: &Path, want: u64, encoding: Option<Encoding>) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        let want = want.min(size);
        // Start a byte early to tell whether `want` begins a line; keep
        // hex rows aligned.
        let base = want.saturating_sub(1) / HEX_WIDTH as u64 * HEX_WIDTH as u64;
        file.seek(SeekFrom::Start(base))?;
        let mut bytes = Vec::with_capacity(WINDOW as usize);
        file.take(WINDOW).read_to_end(&mut bytes)?;
        Ok(Self::build(bytes, size, base, want, encoding))
    }

    /// The whole of `bytes` as a file (tests, small buffers).
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let size = bytes.len() as u64;
        Self::build(bytes, size, 0, 0, None)
    }

    fn build(bytes: Vec<u8>, size: u64, base: u64, want: u64, encoding: Option<Encoding>) -> Self {
        let encoding = encoding.unwrap_or_else(|| detect(&bytes));
        let unit = encoding.unit();
        let nl = encoding.newline();
        let line_starts_at = |p: usize| p >= unit && bytes.get(p - unit..p) == Some(nl);
        let bom = if base == 0 && bytes.starts_with(encoding.bom()) { encoding.bom().len() } else { 0 };
        let text_start = if base == 0 && want as usize <= bom {
            bom
        } else {
            // Units sit at even offsets in UTF-16 (base is even).
            let w = ((want - base) as usize).next_multiple_of(unit).min(bytes.len());
            (w..bytes.len()).step_by(unit).find(|&p| line_starts_at(p)).unwrap_or(w)
        };
        let whole_units = text_start + (bytes.len() - text_start) / unit * unit;
        let text_end = if base + bytes.len() as u64 >= size {
            bytes.len()
        } else {
            // Leave the line cut by the window end for the next window.
            let mut p = whole_units;
            while p > text_start && !line_starts_at(p) {
                p -= unit;
            }
            if p > text_start { p } else { whole_units }
        };
        let text = decode_as(&bytes[text_start..text_end], encoding);
        let utf16 = unit == 2;
        let binary = !utf16 && bytes.iter().take(8192).any(|&b| b == 0);
        let mut lines = vec![0];
        lines.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        // A final newline doesn't start another line.
        if lines.len() > 1 && *lines.last().unwrap() == text.len() {
            lines.pop();
        }
        Document { size, encoding, binary, base, bytes, text_start, text_end, text, lines }
    }

    pub fn default_mode(&self) -> Mode {
        if self.binary {
            Mode::Hex
        } else {
            Mode::Text
        }
    }

    /// The whole file is loaded (no other windows).
    pub fn is_whole(&self) -> bool {
        !self.has_before() && !self.has_after()
    }

    /// The file has bytes before this window.
    pub fn has_before(&self) -> bool {
        self.base > 0
    }

    /// The file has bytes after this window.
    pub fn has_after(&self) -> bool {
        self.base + (self.bytes.len() as u64) < self.size
    }

    /// File offset of the first byte of this window.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// File offsets where this window's text starts and ends.
    pub fn text_span(&self) -> (u64, u64) {
        (self.base + self.text_start as u64, self.base + self.text_end as u64)
    }

    /// File offset just past this window.
    pub fn end(&self) -> u64 {
        self.base + self.bytes.len() as u64
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

    /// File offset of text offset `t` (inside a character: of the next one).
    pub fn byte_of(&self, t: usize) -> u64 {
        let mut t = t.min(self.text.len());
        while !self.text.is_char_boundary(t) {
            t += 1;
        }
        let prefix = &self.text[..t];
        let n = match self.encoding {
            Encoding::Utf8 => t,
            Encoding::Latin1 => prefix.chars().count(),
            Encoding::Utf16Le | Encoding::Utf16Be => prefix.encode_utf16().count() * 2,
        };
        self.base + (self.text_start + n) as u64
    }

    /// Text offset of file offset `byte` (clamped to this window's text).
    pub fn text_of(&self, byte: u64) -> usize {
        let rel = byte.saturating_sub(self.base + self.text_start as u64) as usize;
        match self.encoding {
            Encoding::Utf8 => {
                let mut t = rel.min(self.text.len());
                while !self.text.is_char_boundary(t) {
                    t -= 1;
                }
                t
            }
            Encoding::Latin1 => self.text.char_indices().nth(rel).map_or(self.text.len(), |(i, _)| i),
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let mut units = 0;
                for (i, c) in self.text.char_indices() {
                    if units * 2 >= rel {
                        return i;
                    }
                    units += c.len_utf16();
                }
                self.text.len()
            }
        }
    }

    /// File offset where row `row` of `mode` starts.
    pub fn row_byte(&self, mode: Mode, row: usize) -> u64 {
        match mode {
            Mode::Text => self.byte_of(self.line_start(row)),
            Mode::Hex => self.base + (row * HEX_WIDTH) as u64,
        }
    }

    /// Row of `mode` holding file offset `byte`.
    pub fn row_of(&self, mode: Mode, byte: u64) -> usize {
        let row = match mode {
            Mode::Text => self.line_of(self.text_of(byte)),
            Mode::Hex => (byte.saturating_sub(self.base) / HEX_WIDTH as u64) as usize,
        };
        row.min(self.rows(mode) - 1)
    }

    /// Hex row `i`: `offset  hex bytes  ascii`.
    pub fn hex_row(&self, i: usize) -> String {
        let start = i * HEX_WIDTH;
        let chunk = self.bytes.get(start..(start + HEX_WIDTH).min(self.bytes.len())).unwrap_or(&[]);
        hex_line(self.base + start as u64, chunk)
    }

    /// The raw bytes of this document (the whole file or this window).
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Bytes at file offsets `start..end`, if this window holds them all.
    pub fn slice(&self, start: u64, end: u64) -> Option<&[u8]> {
        if start < self.base || end > self.end() || start > end {
            return None;
        }
        Some(&self.bytes[(start - self.base) as usize..(end - self.base) as usize])
    }

    /// Display columns of line `i` covered by text offsets `start..end`;
    /// one column past the text when the line break is covered too.
    pub fn selected_cols(&self, i: usize, start: usize, end: usize) -> Option<(usize, usize)> {
        let (ls, line) = (self.line_start(i), self.line(i));
        let le = ls + line.len();
        let next = self.line_start(i + 1).max(le);
        if start >= end || end <= ls || start >= next {
            return None;
        }
        let a = column_of(line, start.saturating_sub(ls));
        let b = column_of(line, end - ls) + usize::from(end > le);
        Some((a, b))
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

/// Searches `path` window by window for the next (or previous) match from
/// file offset `from`. Returns the window holding the match and the match
/// in it; `None` when there is none or `cancel` was set.
#[allow(clippy::too_many_arguments)]
pub fn find_in_file(
    path: &Path,
    encoding: Encoding,
    mode: Mode,
    needle: &str,
    from: u64,
    case: bool,
    forward: bool,
    cancel: &AtomicBool,
) -> io::Result<Option<(Document, Hit)>> {
    if needle.is_empty() {
        return Ok(None);
    }
    // Room kept at a backward window's end for matches crossing `pos`.
    let overlap = (needle.len() as u64 * 4).max(1024);
    let mut pos = from;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let want = if forward { pos } else { pos.saturating_sub(WINDOW - overlap.min(WINDOW / 2)) };
        let doc = Document::open_window(path, want, Some(encoding))?;
        let rel = match mode {
            Mode::Text => doc.text_of(pos),
            Mode::Hex => pos.saturating_sub(doc.base) as usize,
        };
        if let Some(hit) = doc.find(mode, needle, rel, case, forward) {
            return Ok(Some((doc, hit)));
        }
        let next = if forward {
            if !doc.has_after() {
                return Ok(None);
            }
            match mode {
                Mode::Text => doc.text_span().1,
                Mode::Hex => doc.end().saturating_sub(needle.len() as u64 - 1),
            }
            .max(pos + 1)
        } else {
            let start = match mode {
                Mode::Text => doc.text_span().0,
                Mode::Hex => doc.base,
            };
            if start == 0 || pos == 0 {
                return Ok(None);
            }
            start.min(pos - 1)
        };
        pos = next;
    }
}

/// Picks the encoding from a BOM or UTF-8 validity.
fn detect(bytes: &[u8]) -> Encoding {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Encoding::Utf16Le;
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Encoding::Utf16Be;
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(body) {
        Ok(_) => Encoding::Utf8,
        // A multi-byte character cut at the end is still UTF-8.
        Err(e) if e.error_len().is_none() => Encoding::Utf8,
        Err(_) => Encoding::Latin1,
    }
}

fn decode_as(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Encoding::Latin1 => bytes.iter().map(|&b| b as char).collect(),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let le = encoding == Encoding::Utf16Le;
            let units =
                bytes.as_chunks::<2>().0.iter().map(|&c| if le { u16::from_le_bytes(c) } else { u16::from_be_bytes(c) });
            char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
        }
    }
}

/// Hex dump row of `chunk` (at most [`HEX_WIDTH`] bytes) at file offset
/// `offset`: `offset  hex bytes  ascii`.
pub fn hex_line(offset: u64, chunk: &[u8]) -> String {
    let mut s = if offset >> 32 == 0 { format!("{offset:08X}  ") } else { format!("{offset:010X}  ") };
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

/// Byte offset in `line` of the character boundary nearest display column
/// `col` (tabs expanded; inverse of [`column_of`]). Past the end: `line.len()`.
pub fn offset_at_column(line: &str, col: usize) -> usize {
    let mut c = 0;
    for (i, ch) in line.char_indices() {
        let next = if ch == '\t' { (c / TAB_WIDTH + 1) * TAB_WIDTH } else { c + 1 };
        if col < next {
            return if col - c <= next - col { i } else { i + ch.len_utf8() };
        }
        c = next;
    }
    line.len()
}

/// Character indices where the rows of display line `shown` start when
/// word-wrapped at `cols` columns: after the last space that fits, else
/// mid-word. The first is always 0.
pub fn wrap_points(shown: &str, cols: usize) -> Vec<usize> {
    let chars: Vec<char> = shown.chars().collect();
    let cols = cols.max(1);
    let mut points = vec![0];
    let mut p = 0;
    while chars.len() - p > cols {
        let end = p + cols;
        let next = chars[p + 1..end].iter().rposition(|&c| c == ' ').map_or(end, |q| p + q + 2);
        points.push(next);
        p = next;
    }
    points
}

/// Where a selection was made: decoded text (text mode), or the hex digits
/// or the character column of the hex view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Text,
    Hex,
    Ascii,
}

/// A selection by file offsets (so it survives scrolling and window moves):
/// `anchor` where it started, `head` where the mouse is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub part: Part,
    pub anchor: u64,
    pub head: u64,
}

impl Selection {
    pub fn new(part: Part, at: u64) -> Self {
        Selection { part, anchor: at, head: at }
    }

    /// Selected file offsets `start..end`.
    pub fn range(&self) -> (u64, u64) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// Most bytes of a selection put on the clipboard.
pub const COPY_LIMIT: u64 = 16 << 20;

/// Clipboard text for `bytes` (at file offset `start`) selected in `part`:
/// text decoded as the file's `encoding` (a character cut at the end
/// dropped, the BOM skipped); hex digits as `41 42 …`, or the character
/// column as shown (`.` for unprintable bytes), a line per 16-byte row.
pub fn selection_text(part: Part, start: u64, bytes: &[u8], encoding: Encoding) -> String {
    match part {
        Part::Text => {
            let bytes = match (encoding, std::str::from_utf8(bytes)) {
                (Encoding::Utf8, Err(e)) if e.error_len().is_none() => &bytes[..e.valid_up_to()],
                _ => bytes,
            };
            let text = decode_as(bytes, encoding);
            match text.strip_prefix('\u{FEFF}') {
                Some(rest) if start == 0 => rest.to_string(),
                _ => text,
            }
        }
        Part::Hex | Part::Ascii => {
            let mut s = String::with_capacity(bytes.len() * if part == Part::Hex { 3 } else { 1 });
            for (i, &b) in bytes.iter().enumerate() {
                let row_start = (start + i as u64).is_multiple_of(HEX_WIDTH as u64);
                if i > 0 && row_start {
                    s.push('\n');
                }
                if part == Part::Hex {
                    if i > 0 && !row_start {
                        s.push(' ');
                    }
                    s.push_str(&format!("{b:02X}"));
                } else {
                    s.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
                }
            }
            s
        }
    }
}

/// Reads file offsets `start..end` of `path`.
pub fn read_range(path: &Path, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(end.saturating_sub(start) as usize);
    file.take(end.saturating_sub(start)).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Column where the hex digits of a hex row at file offset `offset` start
/// (after the offset, which has 8 digits below 4 GB, else 10).
pub fn hex_prefix(offset: u64) -> usize {
    if offset >> 32 == 0 { 10 } else { 12 }
}

/// Column of the first hex digit of byte `k` of a row.
fn hex_byte_col(prefix: usize, k: usize) -> usize {
    prefix + 3 * k + usize::from(k >= HEX_WIDTH / 2)
}

/// Column where the character column of a hex row starts.
fn hex_ascii_col(prefix: usize) -> usize {
    prefix + 3 * HEX_WIDTH + 2
}

/// The part of a hex row at `offset` under (fractional) column `x`.
pub fn hex_part(offset: u64, x: f32) -> Part {
    if x < hex_ascii_col(hex_prefix(offset)) as f32 - 1.0 { Part::Hex } else { Part::Ascii }
}

/// Byte boundary (`0..=len`) of a hex row at `offset` holding `len` bytes
/// nearest (fractional) column `x` in `part` of the row.
pub fn hex_boundary(part: Part, offset: u64, len: usize, x: f32) -> usize {
    let prefix = hex_prefix(offset);
    if part == Part::Ascii {
        return (x - hex_ascii_col(prefix) as f32).round().clamp(0.0, len as f32) as usize;
    }
    // Past a byte once beyond the middle of its two digits.
    (0..len).take_while(|&k| (hex_byte_col(prefix, k) + 1) as f32 <= x).count()
}

/// Column ranges of a hex row at `offset` covering its bytes `a..b`: the
/// hex digits and the character column. Empty when `a >= b`.
pub fn hex_spans(offset: u64, a: usize, b: usize) -> Vec<(usize, usize)> {
    if a >= b {
        return Vec::new();
    }
    let prefix = hex_prefix(offset);
    let ascii = hex_ascii_col(prefix);
    vec![(hex_byte_col(prefix, a), hex_byte_col(prefix, b - 1) + 2), (ascii + a, ascii + b)]
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
    if !case && needle.is_ascii() {
        // ASCII bytes never occur inside a multi-byte character.
        return find_bytes(hay.as_bytes(), needle.as_bytes(), from, false, forward);
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

/// First match starting at or after `from` (forward) or the last one starting
/// before it (backward); `case` false folds ASCII letters only.
fn find_bytes(hay: &[u8], needle: &[u8], from: usize, case: bool, forward: bool) -> Option<(usize, usize)> {
    let n = needle.len();
    let from = from.min(hay.len());
    let &first = needle.first()?;
    let pos = if case {
        if forward {
            from + memchr::memmem::find(&hay[from..], needle)?
        } else {
            // Matches starting before `from` may run past it.
            memchr::memmem::rfind(&hay[..(from + n - 1).min(hay.len())], needle)?
        }
    } else {
        // Candidates by the first byte in either case, then a folded compare.
        let (lo, up) = (first.to_ascii_lowercase(), first.to_ascii_uppercase());
        let eq = |p: usize| hay.get(p..p + n).is_some_and(|w| w.eq_ignore_ascii_case(needle));
        if forward {
            memchr::memchr2_iter(lo, up, &hay[from..]).map(|i| from + i).find(|&p| eq(p))?
        } else {
            memchr::memrchr2_iter(lo, up, &hay[..from]).find(|&p| eq(p))?
        }
    };
    Some((pos, pos + n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(bytes: &[u8]) -> Document {
        Document::from_bytes(bytes.to_vec())
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

    #[test]
    fn byte_search_edges() {
        let hay = b"abXab-1ab";
        let f = |n: &[u8], from, case, fwd| find_bytes(hay, n, from, case, fwd).map(|h| h.0);
        assert_eq!(f(b"ab", 1, true, true), Some(3));
        assert_eq!(f(b"AB", 1, false, true), Some(3));
        assert_eq!(f(b"AB", 1, true, true), None);
        // Backward: last match starting before `from`, even if it runs past it.
        assert_eq!(f(b"ab", 4, true, false), Some(3));
        assert_eq!(f(b"Ab", 4, false, false), Some(3));
        assert_eq!(f(b"ab", 3, true, false), Some(0));
        assert_eq!(f(b"ab", 0, true, false), None);
        assert_eq!(f(b"-1", 9, false, false), Some(5));
        assert_eq!(f(b"ab", 99, true, false), Some(7));
        assert_eq!(f(b"abc", 0, false, true), None);
    }

    #[test]
    fn maps_bytes_and_text_offsets() {
        let d = doc(&[0xFF, 0xFE, b'a', 0, 0x0D, 0x01, b'\n', 0, b'b', 0]); // "ač\nb"
        assert_eq!(d.row_byte(Mode::Text, 1), 8);
        assert_eq!(d.row_of(Mode::Text, 8), 1);
        assert_eq!(d.text_of(6), d.line_start(0) + "ač".len());
        let d = doc(&[b'x', 0xE8, b'\n', b'y']);
        assert_eq!(d.encoding, Encoding::Latin1);
        assert_eq!(d.byte_of(d.line_start(1)), 3);
        assert_eq!(d.row_of(Mode::Hex, 3), 0);
    }

    #[test]
    fn windows_and_searches_large_files() {
        let path = std::env::temp_dir().join(format!("rc-viewer-big-{}.txt", std::process::id()));
        let lines = (WINDOW as usize / 13) * 5 / 2;
        let body: String = (0..lines).map(|i| format!("line {i:07}\n")).collect();
        std::fs::write(&path, &body).unwrap();

        let first = Document::open(&path).unwrap();
        assert!(first.size <= LOAD_LIMIT && first.is_whole() || first.has_after());
        let w = Document::open_window(&path, 0, None).unwrap();
        assert!(w.has_after() && !w.has_before());
        let last = w.line(w.rows(Mode::Text) - 1).to_string();
        let (_, end) = w.text_span();
        let next = Document::open_window(&path, end, Some(w.encoding)).unwrap();
        assert!(next.has_before());
        let n: usize = last[5..].parse().unwrap();
        assert_eq!(next.line(0), format!("line {:07}", n + 1));
        assert_eq!(next.row_byte(Mode::Text, 0), end);
        // A window opened mid-line starts at the next line.
        let mid = Document::open_window(&path, 13 * 100 + 4, Some(w.encoding)).unwrap();
        assert_eq!(mid.line(0), "line 0000101");
        assert_eq!(mid.hex_row(0)[..8], format!("{:08X}", 13 * 100 / 16 * 16));

        let cancel = AtomicBool::new(false);
        let target = format!("line {:07}", lines - 3);
        let (d, hit) = find_in_file(&path, w.encoding, Mode::Text, &target, 0, true, true, &cancel).unwrap().unwrap();
        assert_eq!(d.byte_of(hit.start), 13 * (lines as u64 - 3));
        let (d, hit) =
            find_in_file(&path, w.encoding, Mode::Hex, "LINE 0000002", d.size, false, false, &cancel).unwrap().unwrap();
        assert_eq!(d.base() + hit.start as u64, 26);
        assert!(find_in_file(&path, w.encoding, Mode::Text, "nope", 0, false, true, &cancel).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn maps_columns_back_to_offsets() {
        assert_eq!(offset_at_column("abc", 0), 0);
        assert_eq!(offset_at_column("abc", 2), 2);
        assert_eq!(offset_at_column("abc", 9), 3);
        // A tab spans columns 0..4: the nearer edge.
        assert_eq!(offset_at_column("\tx", 1), 0);
        assert_eq!(offset_at_column("\tx", 3), 1);
        assert_eq!(offset_at_column("\tx", 4), 1);
        assert_eq!(offset_at_column("čx", 1), 2);
        let line = "a\tčb";
        for col in 0..8 {
            assert!(column_of(line, offset_at_column(line, col)).abs_diff(col.min(6)) <= 2);
        }
    }

    #[test]
    fn wraps_at_spaces_else_mid_word() {
        assert_eq!(wrap_points("short", 10), vec![0]);
        assert_eq!(wrap_points("hello world again", 10), vec![0, 6, 12]);
        assert_eq!(wrap_points("abcdefghijkl", 5), vec![0, 5, 10]);
        assert_eq!(wrap_points("", 5), vec![0]);
    }

    #[test]
    fn selection_range_and_columns() {
        let s = Selection { part: Part::Text, anchor: 9, head: 3 };
        assert_eq!((s.range(), s.is_empty()), ((3, 9), false));
        assert!(Selection::new(Part::Hex, 4).is_empty());
        let d = doc(b"ab\tc\r\nxyz\n");
        // "b\tc" plus the line break.
        assert_eq!(d.selected_cols(0, 1, 6), Some((1, 6)));
        assert_eq!(d.selected_cols(0, 0, 2), Some((0, 2)));
        // Only the line break.
        assert_eq!(d.selected_cols(0, 4, 6), Some((5, 6)));
        assert_eq!(d.selected_cols(1, 0, 6), None);
        assert_eq!(d.selected_cols(1, 0, 8), Some((0, 2)));
        assert_eq!(d.slice(1, 3), Some(&b"b\t"[..]));
        assert_eq!(d.slice(5, 99), None);
    }

    #[test]
    fn selection_text_per_part() {
        let bytes = b"Hi\x00\nthere";
        assert_eq!(selection_text(Part::Text, 4, b"there", Encoding::Utf8), "there");
        assert_eq!(selection_text(Part::Hex, 0, &bytes[..3], Encoding::Utf8), "48 69 00");
        // Rows break at 16-byte file offsets.
        assert_eq!(selection_text(Part::Hex, 14, &[1, 2, 3], Encoding::Utf8), "01 02\n03");
        assert_eq!(selection_text(Part::Ascii, 0, &bytes[..4], Encoding::Utf8), "Hi..");
        assert_eq!(selection_text(Part::Ascii, 15, b"ab", Encoding::Latin1), "a\nb");
        // A character cut by the copy limit is dropped; the BOM is skipped at the start.
        assert_eq!(selection_text(Part::Text, 0, &"ač".as_bytes()[..2], Encoding::Utf8), "a");
        assert_eq!(selection_text(Part::Text, 0, &[0xEF, 0xBB, 0xBF, b'x'], Encoding::Utf8), "x");
        assert_eq!(selection_text(Part::Text, 2, &[b'h', 0, b'i', 0], Encoding::Utf16Le), "hi");
        assert_eq!(selection_text(Part::Text, 0, &[0xE8], Encoding::Latin1), "è");
    }

    #[test]
    fn hex_columns_map_both_ways() {
        // "00000000  41 42 …": byte 0 digits at 10..12, byte 8 after the gap at 35.
        assert_eq!(hex_spans(0, 0, 1), vec![(10, 12), (60, 61)]);
        assert_eq!(hex_spans(0, 7, 9), vec![(31, 37), (67, 69)]);
        assert!(hex_spans(0, 3, 3).is_empty());
        assert_eq!(hex_spans(1 << 32, 0, 1)[0], (12, 14));
        assert_eq!(hex_part(0, 20.0), Part::Hex);
        assert_eq!(hex_part(0, 60.0), Part::Ascii);
        assert_eq!(hex_boundary(Part::Hex, 0, 16, 10.0), 0);
        assert_eq!(hex_boundary(Part::Hex, 0, 16, 11.5), 1);
        assert_eq!(hex_boundary(Part::Hex, 0, 16, 36.0), 9);
        assert_eq!(hex_boundary(Part::Hex, 0, 4, 50.0), 4);
        assert_eq!(hex_boundary(Part::Ascii, 0, 16, 62.4), 2);
        assert_eq!(hex_boundary(Part::Ascii, 0, 3, 99.0), 3);
        let line = hex_line(0, b"ABCDEFGHIJKLMNOP");
        let col_of = |s: &str| line.find(s).unwrap();
        assert_eq!(hex_spans(0, 8, 9)[0].0, col_of("49"));
        assert_eq!(hex_spans(0, 0, 1)[1].0, col_of("ABCD"));
    }
}
