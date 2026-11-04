//! Compare files by content (Alt+F2, TC's `cm_CompareFilesByContent`):
//! two selected files, or the cursor files of both panels, side by side
//! with the differences highlighted. Fills the window while open; N/P jump
//! between differences. The diff runs on a worker thread
//! (`rc_core::diff::compare`): a line diff for text, a list of differing
//! hex rows for binary or very large files.

use super::*;
use crate::theme::filled;
use iced::widget::text::{Span, Wrapping};
use iced::widget::{center, column, container, mouse_area, responsive, rich_text, row, text, Column, Space};
use iced::{Alignment, Color, Element, Font, Length};
use rc_core::diff::{self as fdiff, BinaryDiff, Reason, RowKind, TextDiff};
use rc_core::viewer::{self, HEX_WIDTH};

const LINE_H: f32 = 18.0;
const FONT_SIZE: f32 = 13.0;
/// Approximate width of a monospace character at `FONT_SIZE`.
const CHAR_W: f32 = 7.8;
/// Header and footer around the rows.
const CHROME_H: f32 = 64.0;
/// Line number column, in characters.
const GUTTER: usize = 7;
/// Rows kept above a difference jumped to.
const CONTEXT: usize = 3;
/// Columns Left/Right scroll by.
const H_STEP: usize = 4;

#[derive(Debug, Clone)]
pub enum DiffMsg {
    Done(u64, Result<Arc<fdiff::Outcome>, String>),
    Wheel(f32),
    /// Scrollbar: first row to show.
    ScrollTo(usize),
}

#[derive(Debug)]
enum Content {
    Loading,
    Failed(String),
    Loaded(Arc<fdiff::Outcome>),
}

#[derive(Debug)]
pub(super) struct FileDiff {
    paths: [PathBuf; 2],
    seq: u64,
    content: Content,
    /// First row shown, and the first column.
    top: usize,
    left: usize,
    /// Difference block last jumped to; cleared by scrolling.
    block: Option<usize>,
    /// Stops a running byte comparison.
    cancel: Arc<AtomicBool>,
}

impl FileDiff {
    fn outcome(&self) -> Option<&fdiff::Outcome> {
        match &self.content {
            Content::Loaded(o) => Some(o),
            _ => None,
        }
    }
}

impl App {
    /// The two files to compare: two selected files of the active panel,
    /// or the selected-or-cursor file of each panel (left panel first).
    fn diff_pair(&self) -> Result<[PathBuf; 2], String> {
        let files = |side: usize| -> Vec<PathBuf> {
            let panel = self.panel(side);
            panel.targets().into_iter().filter(|e| e.kind == EntryKind::File).map(|e| e.path.clone()).collect()
        };
        let mine = files(self.active);
        if self.panel(self.active).selected_count() >= 2 {
            return match <[PathBuf; 2]>::try_from(mine) {
                Ok(pair) => Ok(pair),
                Err(_) => Err("Select exactly two files to compare".into()),
            };
        }
        let other = files(1 - self.active);
        match (mine.into_iter().next(), other.into_iter().next()) {
            (Some(a), Some(b)) => Ok(if self.active == 0 { [a, b] } else { [b, a] }),
            _ => Err("Select two files, or put the cursor on a file in each panel".into()),
        }
    }

    pub(super) fn open_file_diff(&mut self) -> Task<Message> {
        let paths = match self.diff_pair() {
            Ok(p) => p,
            Err(e) => {
                self.error(e);
                return Task::none();
            }
        };
        self.leave_edits();
        self.next_listing += 1;
        let seq = self.next_listing;
        let cancel = Arc::new(AtomicBool::new(false));
        self.screen = Some(Screen::FileDiff(Box::new(FileDiff {
            paths: paths.clone(),
            seq,
            content: Content::Loading,
            top: 0,
            left: 0,
            block: None,
            cancel: cancel.clone(),
        })));
        let [a, b] = paths;
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    // Files inside archives are compared via temp copies.
                    let local = |p: &PathBuf| rc_core::vfs::extract_temp(p).map_err(|e| format!("{}: {e}", p.display()));
                    let result = local(&a).and_then(|a| Ok((a, local(&b)?))).and_then(|(a, b)| {
                        fdiff::compare(&a, &b, &cancel).map(Arc::new).map_err(|e| e.to_string())
                    });
                    let _ = tx.send(result);
                });
                rx.await.unwrap_or_else(|_| Err("compare thread failed".into()))
            },
            move |result| Message::FileDiff(DiffMsg::Done(seq, result)),
        )
    }

    pub(super) fn close_file_diff(&mut self) {
        if let Some(d) = self.screen.take_file_diff() {
            d.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn file_diff_update(&mut self, msg: DiffMsg) -> Task<Message> {
        match msg {
            DiffMsg::Done(seq, result) => {
                let Some(d) = self.screen.file_diff_mut().filter(|d| d.seq == seq) else { return Task::none() };
                d.content = match result {
                    Ok(o) => Content::Loaded(o),
                    Err(e) => Content::Failed(e),
                };
                // Start on the first difference, like TC.
                self.diff_jump(true);
            }
            DiffMsg::Wheel(lines) => self.diff_scroll((-lines * 3.0).round() as isize),
            DiffMsg::ScrollTo(row) => {
                let top = self.screen.file_diff().map_or(0, |d| d.top);
                self.diff_scroll(row as isize - top as isize);
            }
        }
        Task::none()
    }

    fn diff_rows(&self) -> usize {
        ((self.window_height - CHROME_H) / LINE_H).floor().max(1.0) as usize
    }

    /// Text columns per side.
    fn diff_cols(&self) -> usize {
        ((self.window_width / 2.0 - 24.0) / CHAR_W).floor().max(10.0) as usize - GUTTER
    }

    fn diff_scroll(&mut self, delta: isize) {
        let page = self.diff_rows();
        let Some(d) = self.screen.file_diff_mut() else { return };
        let total = d.outcome().map_or(0, |o| o.rows());
        d.top = d.top.saturating_add_signed(delta).min(total.saturating_sub(page));
        d.block = None;
    }

    /// Moves to the next (or previous) block of differing rows.
    fn diff_jump(&mut self, forward: bool) {
        let page = self.diff_rows();
        let Some(d) = self.screen.file_diff_mut() else { return };
        let Content::Loaded(o) = &d.content else { return };
        let o = o.clone();
        let blocks = o.blocks();
        let target = match (d.block, forward) {
            (Some(b), true) => (b + 1 < blocks.len()).then_some(b + 1),
            (Some(b), false) => b.checked_sub(1),
            // After scrolling: the first block from the top of the view on,
            // or the last one above it.
            (None, true) => blocks.iter().position(|&r| r >= d.top),
            (None, false) => blocks.iter().rposition(|&r| r < d.top),
        };
        if let Some(b) = target {
            let total = o.rows();
            d.block = Some(b);
            d.top = blocks[b].saturating_sub(CONTEXT).min(total.saturating_sub(page));
        }
    }

    /// Keys of `file_diff_key`, for the F1 help. Keep in sync.
    pub(super) const DIFF_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Esc, Q", "Close the comparison"),
        ("N, Space", "Next difference"),
        ("P, Shift+Space, Backspace", "Previous difference"),
        ("Up, Down, PgUp, PgDn, Home, End", "Scroll"),
        ("Left, Right", "Scroll sideways"),
    ];

    pub(super) fn file_diff_key(&mut self, named: Option<Named>, key: &keyboard::Key, mods: keyboard::Modifiers) {
        let rows = self.diff_rows() as isize;
        let ch = match key {
            keyboard::Key::Character(c) if !mods.control() && !mods.alt() => Some(c.to_lowercase()),
            _ => None,
        };
        let shift = mods.shift();
        match (named, ch.as_deref()) {
            (Some(Named::Escape), _) | (_, Some("q")) => self.close_file_diff(),
            (Some(Named::ArrowDown), _) => self.diff_scroll(1),
            (Some(Named::ArrowUp), _) => self.diff_scroll(-1),
            (Some(Named::PageDown), _) => self.diff_scroll(rows),
            (Some(Named::PageUp), _) => self.diff_scroll(-rows),
            (Some(Named::Home), _) => self.diff_scroll(isize::MIN / 2),
            (Some(Named::End), _) => self.diff_scroll(isize::MAX / 2),
            (Some(Named::ArrowRight), _) => {
                if let Some(d) = self.screen.file_diff_mut() {
                    d.left = (d.left + H_STEP).min(20_000);
                }
            }
            (Some(Named::ArrowLeft), _) => {
                if let Some(d) = self.screen.file_diff_mut() {
                    d.left = d.left.saturating_sub(H_STEP);
                }
            }
            (Some(Named::Space), _) => self.diff_jump(!shift),
            (_, Some("n")) => self.diff_jump(true),
            (Some(Named::Backspace), _) | (_, Some("p")) => self.diff_jump(false),
            _ => {}
        }
    }

    pub(super) fn file_diff_view<'a>(&'a self, d: &'a FileDiff) -> Element<'a, Message> {
        let side_info = |i: usize| -> String {
            match d.outcome() {
                Some(fdiff::Outcome::Text(t)) => {
                    let doc = if i == 0 { &t.left } else { &t.right };
                    format!("{} · {}", rc_core::format::size(doc.size), doc.encoding.label())
                }
                Some(fdiff::Outcome::Binary(b)) => {
                    rc_core::format::size(if i == 0 { b.sizes.0 } else { b.sizes.1 })
                }
                None => String::new(),
            }
        };
        let title = |i: usize| -> Element<'a, Message> {
            let path = &d.paths[i];
            let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            let folder = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            // Long folders keep their end (text in a row wraps instead of
            // clipping): roughly what fits beside the name and sizes.
            let room = ((self.window_width / 2.0 - 220.0) / 6.5 - name.chars().count() as f32).max(8.0) as usize;
            let folder = tail(&folder, room);
            container(
                row![
                    text(name).size(14).wrapping(Wrapping::None),
                    container(text(folder).size(12).color(theme::c().muted).wrapping(Wrapping::None))
                        .width(Length::Fill)
                        .clip(true),
                    text(side_info(i)).size(12).color(theme::c().muted).wrapping(Wrapping::None),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .padding([6, 12])
            .width(Length::FillPortion(1))
            .clip(true)
            .into()
        };
        let header = container(row![title(0), title(1)]).style(filled(theme::c().header));

        let body: Element<'a, Message> = match &d.content {
            Content::Loading => center(text("Comparing…").size(13).color(theme::c().muted)).into(),
            Content::Failed(e) => center(text(format!("Can't compare: {e}")).size(13).color(theme::c().error)).into(),
            Content::Loaded(o) => self.diff_body(d, o),
        };
        let body = mouse_area(container(body).width(Length::Fill).height(Length::Fill).style(filled(theme::c().panel)))
            .on_scroll(|delta| {
                Message::FileDiff(DiffMsg::Wheel(match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / LINE_H / 3.0,
                }))
            });

        let (summary, tone) = d.outcome().map_or((String::new(), Tone::Info), |o| summary(o, d.block));
        let help = "Esc close · N/Space next difference · P/Shift+Space previous · arrows/PgUp/PgDn scroll";
        let footer = row![
            text(summary).size(12).color(match tone {
                Tone::Error => theme::c().error,
                Tone::Success => theme::c().success,
                Tone::Info => theme::c().text,
            }),
            Space::with_width(Length::Fill),
            text(help).size(12).color(theme::c().muted).wrapping(Wrapping::None),
        ]
        .spacing(16);
        let footer = container(footer).padding([4, 12]).width(Length::Fill).clip(true).style(filled(theme::c().header));
        container(column![header, body, footer])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(theme::c().bg))
            .into()
    }

    fn diff_body<'a>(&'a self, d: &'a FileDiff, o: &'a fdiff::Outcome) -> Element<'a, Message> {
        let cols = self.diff_cols();
        let current = d.block.map(|b| {
            let blocks = o.blocks();
            blocks[b]..blocks.get(b + 1).copied().unwrap_or(usize::MAX)
        });
        responsive(move |size| {
            let rows = (size.height / LINE_H).ceil() as usize;
            // The last row may be cut off: the bar counts whole rows.
            let full = (size.height / LINE_H).floor().max(1.0);
            let end = (d.top + rows).min(o.rows());
            let lines = (d.top..end).map(|i| {
                // Rows of the block jumped to get a marked gutter.
                let in_block = current.as_ref().is_some_and(|r| r.contains(&i)) && differs(o, i);
                let (a, b) = match o {
                    fdiff::Outcome::Text(t) => text_cells(t, i, d.left, cols),
                    fdiff::Outcome::Binary(bin) => hex_cells(bin, i, d.left, cols),
                };
                row![cell(a, in_block), cell(b, in_block)].into()
            });
            let lines = container(Column::with_children(lines).width(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true);
            scrollbar::attach(lines, d.top as f32, full, o.rows() as f32, |at| {
                Message::FileDiff(DiffMsg::ScrollTo(at.round() as usize))
            })
        })
        .into()
    }
}

/// Footer text for a finished comparison.
fn summary(o: &fdiff::Outcome, block: Option<usize>) -> (String, Tone) {
    let count = o.blocks().len();
    let at = block.map(|b| format!("Difference {} of {count}", b + 1));
    match o {
        fdiff::Outcome::Text(t) => {
            if count == 0 {
                return if t.bytes_equal {
                    ("Files are identical".into(), Tone::Success)
                } else {
                    ("Text is identical; files differ in line endings or encoding".into(), Tone::Info)
                };
            }
            let mut s = at.unwrap_or_else(|| format!("{count} difference{}", plural(count)));
            if t.approximate {
                s.push_str(" (diff took too long: may not be minimal)");
            }
            (s, Tone::Error)
        }
        fdiff::Outcome::Binary(b) => {
            let Some(first) = b.first else { return ("Files are identical".into(), Tone::Success) };
            let mut s = match b.reason {
                Reason::Binary => "Binary files".to_string(),
                Reason::TooLarge => format!("Over {} MB: byte comparison", fdiff::TEXT_LIMIT >> 20),
            };
            s.push_str(&format!(" differ at offset {first:#X} ({} byte{})", b.differing, plural(b.differing as usize)));
            if b.sizes.0 != b.sizes.1 {
                s.push_str(", sizes differ");
            }
            if b.truncated {
                s.push_str(&format!("; first {} rows shown", fdiff::MAX_BYTE_ROWS));
            }
            if let Some(at) = at {
                s = format!("{at} · {s}");
            }
            (s, Tone::Error)
        }
    }
}

/// The last `max` characters of `s`, with a leading "…" when cut.
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let mut out = String::from("…");
    out.extend(s.chars().skip(n - max + 1));
    out
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn differs(o: &fdiff::Outcome, i: usize) -> bool {
    match o {
        fdiff::Outcome::Text(t) => t.rows[i].kind != RowKind::Same,
        fdiff::Outcome::Binary(_) => true,
    }
}

/// One side of a row: line number, text, character ranges to highlight,
/// and its background tint (`None` = plain; an empty side is drawn darker).
struct Cell {
    number: String,
    text: String,
    marks: Vec<(usize, usize)>,
    tint: Option<Color>,
    empty: bool,
}

fn tint(color: Color, alpha: f32) -> Color {
    Color { a: alpha, ..color }
}

fn text_cells(t: &TextDiff, i: usize, skip: usize, cols: usize) -> (Cell, Cell) {
    let r = t.rows[i];
    let colors = theme::c();
    let tint_of = |left: bool| match r.kind {
        RowKind::Same => None,
        RowKind::Changed => Some(tint(colors.accent, 0.16)),
        RowKind::Removed if left => Some(tint(colors.error, 0.18)),
        RowKind::Added if !left => Some(tint(colors.success, 0.18)),
        _ => None,
    };
    let spans = (r.kind == RowKind::Changed).then(|| {
        let (a, b) = (r.left.map_or("", |l| t.left.line(l)), r.right.map_or("", |l| t.right.line(l)));
        fdiff::changed_span(a, b)
    });
    let side = |left: bool| {
        let (doc, line) = if left { (&t.left, r.left) } else { (&t.right, r.right) };
        let Some(n) = line else {
            return Cell { number: String::new(), text: String::new(), marks: Vec::new(), tint: None, empty: true };
        };
        let s = doc.line(n);
        let marks = spans
            .map(|(a, b)| if left { a } else { b })
            .filter(|(a, b)| a < b)
            .map(|(a, b)| {
                let (a, b) = (viewer::column_of(s, a), viewer::column_of(s, b));
                vec![(a.saturating_sub(skip), b.saturating_sub(skip))]
            })
            .unwrap_or_default();
        Cell {
            number: (n + 1).to_string(),
            text: viewer::display_line(s, skip, cols + 1),
            marks,
            tint: tint_of(left),
            empty: false,
        }
    };
    (side(true), side(false))
}

fn hex_cells(b: &BinaryDiff, i: usize, skip: usize, cols: usize) -> (Cell, Cell) {
    let r = &b.rows[i];
    let pre = if r.offset >> 32 == 0 { 10 } else { 12 };
    let hex_col = |k: usize| pre + 3 * k + usize::from(k >= HEX_WIDTH / 2);
    let ascii = pre + 3 * HEX_WIDTH + 2;
    let side = |bytes: &[u8], other: &[u8]| {
        let diff: Vec<usize> = (0..bytes.len()).filter(|&k| other.get(k) != bytes.get(k)).collect();
        let mut marks: Vec<(usize, usize)> = diff.iter().map(|&k| (hex_col(k), hex_col(k) + 2)).collect();
        marks.extend(diff.iter().map(|&k| (ascii + k, ascii + k + 1)));
        let marks = marks
            .into_iter()
            .filter(|&(_, e)| e > skip)
            .map(|(a, e)| (a.saturating_sub(skip), e - skip))
            .collect();
        let line = viewer::hex_line(r.offset, bytes);
        Cell {
            number: String::new(),
            text: if bytes.is_empty() { String::new() } else { line.chars().skip(skip).take(cols + 1).collect() },
            marks,
            tint: (!bytes.is_empty()).then(|| tint(theme::c().accent, 0.10)),
            empty: bytes.is_empty(),
        }
    };
    (side(&r.left, &r.right), side(&r.right, &r.left))
}

fn cell<'a>(c: Cell, in_block: bool) -> Element<'a, Message> {
    let colors = theme::c();
    let number = text(format!("{:>w$} ", c.number, w = GUTTER - 1))
        .font(Font::MONOSPACE)
        .size(FONT_SIZE)
        .line_height(iced::Pixels(LINE_H))
        .color(if in_block { colors.accent } else { colors.muted })
        .wrapping(Wrapping::None);
    let body: Element<'a, Message> = if c.marks.is_empty() {
        text(c.text).font(Font::MONOSPACE).size(FONT_SIZE).line_height(iced::Pixels(LINE_H)).wrapping(Wrapping::None).into()
    } else {
        rich_text(spans(&c.text, &c.marks))
            .font(Font::MONOSPACE)
            .size(FONT_SIZE)
            .line_height(iced::Pixels(LINE_H))
            .wrapping(Wrapping::None)
            .into()
    };
    let bg = match (c.tint, c.empty) {
        (_, true) => colors.bg,
        (Some(t), _) => t,
        (None, _) => Color::TRANSPARENT,
    };
    let marker = container(Space::new(3.0, LINE_H)).style(filled(if in_block { colors.accent } else { Color::TRANSPARENT }));
    container(row![marker, number, body].align_y(Alignment::Center))
        .padding([0, 6])
        .width(Length::FillPortion(1))
        .height(LINE_H)
        .clip(true)
        .style(filled(bg))
        .into()
}

/// Splits `s` into spans, the character ranges in `marks` (ascending)
/// highlighted.
fn spans<'a>(s: &str, marks: &[(usize, usize)]) -> Vec<Span<'a, Message, Font>> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut pos = 0;
    for &(a, b) in marks {
        let (a, b) = (a.min(chars.len()).max(pos), b.min(chars.len()));
        if a >= b {
            continue;
        }
        if a > pos {
            out.push(Span::new(chars[pos..a].iter().collect::<String>()));
        }
        out.push(
            Span::new(chars[a..b].iter().collect::<String>())
                .color(theme::c().bg)
                .background(theme::c().selected),
        );
        pos = b;
    }
    if pos < chars.len() || out.is_empty() {
        out.push(Span::new(chars[pos..].iter().collect::<String>()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("C:/abc", 10), "C:/abc");
        assert_eq!(tail("C:/folder/sub", 6), "…r/sub");
        assert_eq!(tail("čćž", 2), "…ž");
    }
}
