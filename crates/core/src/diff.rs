//! Compare two files by content (TC's `cm_CompareFilesByContent`): a line
//! diff for text files, a byte comparison for binary or very large ones.
//!
//! Text files up to [`TEXT_LIMIT`] each are decoded like the viewer does
//! ([`Document`]) and diffed line by line (Myers, via `similar`, with a
//! [`DEADLINE`] after which the result is correct but may not be minimal).
//! Anything else is compared byte by byte as a stream, listing up to
//! [`MAX_BYTE_ROWS`] differing 16-byte rows.

use crate::viewer::{Document, HEX_WIDTH};
use similar::{capture_diff_slices_deadline, Algorithm, DiffOp};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Largest file (each side) diffed line by line.
pub const TEXT_LIMIT: u64 = 32 << 20;
/// Differing hex rows listed for a byte comparison.
pub const MAX_BYTE_ROWS: usize = 10_000;
/// Time the line diff may take before it settles for a non-minimal result.
pub const DEADLINE: Duration = Duration::from_secs(5);
/// Bytes read per step of a byte comparison (a multiple of `HEX_WIDTH`).
const CHUNK: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Same,
    /// Different lines on both sides.
    Changed,
    /// Only on the left.
    Removed,
    /// Only on the right.
    Added,
}

/// One row of the side-by-side view: line indices on each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub left: Option<usize>,
    pub right: Option<usize>,
}

#[derive(Debug)]
pub struct TextDiff {
    pub left: Document,
    pub right: Document,
    pub rows: Vec<Row>,
    /// First row of each run of differing rows.
    pub blocks: Vec<usize>,
    /// The deadline was hit: differences are right but may not be minimal.
    pub approximate: bool,
    /// The raw bytes are equal (the text may be equal while these differ:
    /// line endings, BOM).
    pub bytes_equal: bool,
}

/// A 16-byte row where the files differ; a side is shorter (or empty)
/// past its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteRow {
    pub offset: u64,
    pub left: Vec<u8>,
    pub right: Vec<u8>,
}

#[derive(Debug)]
pub struct BinaryDiff {
    pub sizes: (u64, u64),
    /// Differing rows, at most [`MAX_BYTE_ROWS`].
    pub rows: Vec<ByteRow>,
    /// First row of each run of adjacent rows.
    pub blocks: Vec<usize>,
    /// Offset of the first differing byte.
    pub first: Option<u64>,
    /// Bytes that differ (bytes only one file has count too).
    pub differing: u64,
    /// More rows differ than are listed.
    pub truncated: bool,
    /// Why the files weren't diffed as text.
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Binary,
    TooLarge,
}

#[derive(Debug)]
pub enum Outcome {
    Text(TextDiff),
    Binary(BinaryDiff),
}

impl Outcome {
    pub fn blocks(&self) -> &[usize] {
        match self {
            Outcome::Text(t) => &t.blocks,
            Outcome::Binary(b) => &b.blocks,
        }
    }

    pub fn rows(&self) -> usize {
        match self {
            Outcome::Text(t) => t.rows.len(),
            Outcome::Binary(b) => b.rows.len(),
        }
    }
}

/// Compares two files; `cancel` stops a byte comparison (an `Interrupted`
/// error).
pub fn compare(left: &Path, right: &Path, cancel: &AtomicBool) -> io::Result<Outcome> {
    let sizes = (std::fs::metadata(left)?.len(), std::fs::metadata(right)?.len());
    if sizes.0.max(sizes.1) > TEXT_LIMIT {
        return compare_bytes(left, right, Reason::TooLarge, cancel).map(Outcome::Binary);
    }
    let (a, b) = (Document::open(left)?, Document::open(right)?);
    if a.binary || b.binary {
        return compare_bytes(left, right, Reason::Binary, cancel).map(Outcome::Binary);
    }
    Ok(Outcome::Text(diff_text(a, b)))
}

/// Line diff of two decoded documents.
pub fn diff_text(left: Document, right: Document) -> TextDiff {
    fn lines(d: &Document) -> Vec<&str> {
        (0..d.rows(crate::viewer::Mode::Text)).map(|i| d.line(i)).collect()
    }
    let (a, b) = (lines(&left), lines(&right));
    let start = Instant::now();
    let ops = capture_diff_slices_deadline(Algorithm::Myers, &a, &b, Some(start + DEADLINE));
    let approximate = start.elapsed() >= DEADLINE;
    let rows = rows_of(&ops);
    let blocks = blocks_of(&rows, |p, r| p.kind == RowKind::Same || r.kind == RowKind::Same);
    let bytes_equal = left.bytes() == right.bytes();
    TextDiff { left, right, rows, blocks, approximate, bytes_equal }
}

fn rows_of(ops: &[DiffOp]) -> Vec<Row> {
    let mut rows = Vec::new();
    for op in ops {
        let (old, old_len, new, new_len) = match *op {
            DiffOp::Equal { old_index, new_index, len } => {
                rows.extend((0..len).map(|k| Row {
                    kind: RowKind::Same,
                    left: Some(old_index + k),
                    right: Some(new_index + k),
                }));
                continue;
            }
            DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_len, new_index, 0),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index, 0, new_index, new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => (old_index, old_len, new_index, new_len),
        };
        // Pair lines up while both sides have some, then one-sided rows.
        for k in 0..old_len.max(new_len) {
            let left = (k < old_len).then_some(old + k);
            let right = (k < new_len).then_some(new + k);
            let kind = match (left, right) {
                (Some(_), Some(_)) => RowKind::Changed,
                (Some(_), None) => RowKind::Removed,
                _ => RowKind::Added,
            };
            rows.push(Row { kind, left, right });
        }
    }
    rows
}

/// Indices of the differing rows that start a block: `breaks(prev, row)`
/// says whether `row` doesn't continue the block `prev` is in.
fn blocks_of<T>(rows: &[T], breaks: impl Fn(&T, &T) -> bool) -> Vec<usize>
where
    T: Differs,
{
    (0..rows.len())
        .filter(|&i| rows[i].differs() && (i == 0 || breaks(&rows[i - 1], &rows[i])))
        .collect()
}

trait Differs {
    fn differs(&self) -> bool;
}

impl Differs for Row {
    fn differs(&self) -> bool {
        self.kind != RowKind::Same
    }
}

impl Differs for ByteRow {
    fn differs(&self) -> bool {
        true
    }
}

/// Byte ranges of `a` and `b` left after trimming their common prefix and
/// suffix (whole characters): the part of a changed line to highlight.
pub fn changed_span(a: &str, b: &str) -> ((usize, usize), (usize, usize)) {
    let prefix = a
        .char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map_or(a.len().min(b.len()), |((i, _), _)| i);
    // A prefix that ends inside a char of `b` can't happen: equal chars
    // have equal lengths.
    let (ra, rb) = (&a[prefix..], &b[prefix..]);
    let suffix: usize = ra
        .chars()
        .rev()
        .zip(rb.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum();
    ((prefix, a.len() - suffix), (prefix, b.len() - suffix))
}

/// Reads until `buf` is full or the file ends.
fn read_full(file: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
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

/// Streams both files, listing the 16-byte rows that differ.
pub fn compare_bytes(left: &Path, right: &Path, reason: Reason, cancel: &AtomicBool) -> io::Result<BinaryDiff> {
    let (mut fa, mut fb) = (File::open(left)?, File::open(right)?);
    let sizes = (fa.metadata()?.len(), fb.metadata()?.len());
    compare_readers(&mut fa, &mut fb, sizes, reason, cancel)
}

fn compare_readers(
    fa: &mut impl Read,
    fb: &mut impl Read,
    sizes: (u64, u64),
    reason: Reason,
    cancel: &AtomicBool,
) -> io::Result<BinaryDiff> {
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut d = BinaryDiff {
        sizes,
        rows: Vec::new(),
        blocks: Vec::new(),
        first: None,
        differing: 0,
        truncated: false,
        reason,
    };
    let mut offset = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let (na, nb) = (read_full(fa, &mut ba)?, read_full(fb, &mut bb)?);
        if na == 0 && nb == 0 {
            break;
        }
        let n = na.max(nb);
        if na == nb && ba[..na] == bb[..nb] {
            offset += n as u64;
            continue;
        }
        for start in (0..n).step_by(HEX_WIDTH) {
            let la = ba.get(start..(start + HEX_WIDTH).min(na)).unwrap_or(&[]);
            let lb = bb.get(start..(start + HEX_WIDTH).min(nb)).unwrap_or(&[]);
            if la == lb {
                continue;
            }
            let diffs = (0..HEX_WIDTH).filter(|&k| la.get(k) != lb.get(k));
            let row = offset + start as u64;
            let mut count = 0;
            for k in diffs {
                d.first.get_or_insert(row + k as u64);
                count += 1;
            }
            d.differing += count;
            if d.rows.len() < MAX_BYTE_ROWS {
                d.rows.push(ByteRow { offset: row, left: la.to_vec(), right: lb.to_vec() });
            } else {
                d.truncated = true;
            }
        }
        offset += n as u64;
        // Only one file left and the list is full: the rest just counts.
        if d.truncated && (na < CHUNK || nb < CHUNK) {
            d.differing += sizes.0.max(sizes.1).saturating_sub(offset);
            break;
        }
    }
    d.blocks = blocks_of(&d.rows, |p, r| p.offset + HEX_WIDTH as u64 != r.offset);
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use RowKind::*;

    fn text(a: &str, b: &str) -> TextDiff {
        diff_text(Document::from_bytes(a.as_bytes().to_vec()), Document::from_bytes(b.as_bytes().to_vec()))
    }

    fn kinds(d: &TextDiff) -> Vec<RowKind> {
        d.rows.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn identical_text_has_no_blocks() {
        let d = text("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(kinds(&d), [Same, Same, Same]);
        assert!(d.blocks.is_empty());
        assert!(d.bytes_equal);
        // Line endings differ: same text, different bytes.
        let d = text("a\r\nb\r\n", "a\nb\n");
        assert!(d.blocks.is_empty());
        assert!(!d.bytes_equal);
    }

    #[test]
    fn rows_pair_changes_and_mark_one_sided_lines() {
        let d = text("a\nb\nc\nd\ne\n", "a\nB\nc\ne\nf\ng\n");
        assert_eq!(kinds(&d), [Same, Changed, Same, Removed, Same, Added, Added]);
        assert_eq!(d.blocks, [1, 3, 5]);
        assert_eq!(d.rows[1], Row { kind: Changed, left: Some(1), right: Some(1) });
        assert_eq!(d.rows[3], Row { kind: Removed, left: Some(3), right: None });
        assert_eq!(d.rows[6], Row { kind: Added, left: None, right: Some(5) });
    }

    #[test]
    fn uneven_replacement_pairs_then_adds() {
        let d = text("x\n1\ny\n", "x\n2\n3\ny\n");
        assert_eq!(kinds(&d), [Same, Changed, Added, Same]);
        assert_eq!(d.blocks, [1]);
    }

    #[test]
    fn changed_span_trims_common_ends() {
        assert_eq!(changed_span("let a = 1;", "let a = 22;"), ((8, 9), (8, 10)));
        assert_eq!(changed_span("abc", "abc"), ((3, 3), (3, 3)));
        assert_eq!(changed_span("ab", "aXb"), ((1, 1), (1, 2)));
        assert_eq!(changed_span("čaj", "čej"), ((2, 3), (2, 3)));
        // Overlapping prefix/suffix: "aa" vs "aaa".
        assert_eq!(changed_span("aa", "aaa"), ((2, 2), (2, 3)));
    }

    fn bytes(a: &[u8], b: &[u8]) -> BinaryDiff {
        let sizes = (a.len() as u64, b.len() as u64);
        compare_readers(&mut &a[..], &mut &b[..], sizes, Reason::Binary, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn byte_compare_lists_differing_rows() {
        let a: Vec<u8> = (0..64).collect();
        assert!(bytes(&a, &a).rows.is_empty());
        assert_eq!(bytes(&a, &a).first, None);

        let mut b = a.clone();
        b[20] = 0xFF;
        b[21] = 0xFF;
        b[40] = 0xFF;
        let d = bytes(&a, &b);
        assert_eq!(d.first, Some(20));
        assert_eq!(d.differing, 3);
        assert_eq!(d.rows.iter().map(|r| r.offset).collect::<Vec<_>>(), [16, 32]);
        assert_eq!(d.blocks, [0]);
        assert_eq!(d.rows[0].left, &a[16..32]);
    }

    #[test]
    fn byte_compare_counts_extra_tail() {
        let a: Vec<u8> = (0..20).collect();
        let d = bytes(&a, &a[..18]);
        assert_eq!(d.first, Some(18));
        assert_eq!(d.differing, 2);
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].right.len(), 2);
        let d = bytes(&[], &[1, 2, 3]);
        assert_eq!((d.first, d.differing, d.rows.len()), (Some(0), 3, 1));
    }

    #[test]
    fn byte_compare_caps_rows() {
        let a = vec![0u8; (MAX_BYTE_ROWS + 50) * HEX_WIDTH];
        let b = vec![1u8; a.len()];
        let d = bytes(&a, &b);
        assert_eq!(d.rows.len(), MAX_BYTE_ROWS);
        assert!(d.truncated);
        assert_eq!(d.differing, a.len() as u64);
        assert_eq!(d.blocks, [0]);
    }

    #[test]
    fn compare_picks_text_or_bytes() {
        let dir = std::env::temp_dir().join(format!("rc-diff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a.txt"), dir.join("b.txt"), dir.join("c.bin"));
        std::fs::write(&a, "one\ntwo\n").unwrap();
        std::fs::write(&b, "one\nthree\n").unwrap();
        std::fs::write(&c, b"one\0two\n").unwrap();
        let no = AtomicBool::new(false);
        assert!(matches!(compare(&a, &b, &no).unwrap(), Outcome::Text(t) if t.blocks == [1]));
        match compare(&a, &c, &no).unwrap() {
            Outcome::Binary(d) => assert_eq!((d.reason, d.first), (Reason::Binary, Some(3))),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
