//! Rendered Markdown for the viewer (key 5, the default for .md files):
//! `parse` turns the text into a small list of blocks once per load, `view`
//! builds the widgets in a MarkText-like reading column. No HTML rendering
//! (raw HTML shows as text), no images (alt text only), no math.

use super::viewer::ViewerMsg;
use super::Message;
use crate::theme::{self, filled, Colors};
use iced::alignment::Horizontal;
use iced::font::{Style as FontStyle, Weight};
use iced::widget::text::{LineHeight, Span, Wrapping};
use iced::widget::{column, container, mouse_area, rich_text, row, stack, text, Column, Row, Space};
use iced::{Border, Element, Font, Length, Padding};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::iter::Peekable;
use std::path::Path;

/// Larger Markdown files open in Text mode (rendering huge docs is slow).
pub(super) const MD_LIMIT: u64 = 2 << 20;
/// Width of the reading column.
const MAX_W: f32 = 800.0;
const BODY_SIZE: f32 = 15.0;
const CODE_SIZE: f32 = 13.0;
const BODY: Font = Font::with_name("Segoe UI");

/// A `.md` / `.markdown` file (any case).
pub(super) fn is_markdown(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    /// Alt text of an image.
    pub image: bool,
}

/// A run of text in one style; `link` is the target it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// A task list item: done or not.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Level 1-6.
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    /// A fenced or indented code block.
    Code(String),
    /// Raw HTML, shown as text.
    Html(String),
    Quote(Vec<Block>),
    /// `start` is the first number of a numbered list.
    List { start: Option<u64>, items: Vec<Item> },
    Table { align: Vec<Horizontal>, head: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>> },
    Rule,
}

/// Parses Markdown (CommonMark with tables, task lists, strikethrough).
pub(super) fn parse(src: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let mut md = Md { ev: Parser::new_ext(src, options).peekable(), nest: Nest::default(), links: Vec::new(), alts: Vec::new(), task: None };
    let mut out = Vec::new();
    md.blocks(&mut out);
    out
}

/// Open inline tags.
#[derive(Default)]
struct Nest {
    bold: u32,
    italic: u32,
    strike: u32,
    image: u32,
}

struct Md<'a> {
    ev: Peekable<Parser<'a>>,
    nest: Nest,
    /// Targets of the open links.
    links: Vec<String>,
    /// Text length where each open image's alt text starts.
    alts: Vec<usize>,
    /// Task marker of the list item being read.
    task: Option<bool>,
}

fn is_inline(event: &Event) -> bool {
    match event {
        Event::Start(tag) => matches!(
            tag,
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Superscript | Tag::Subscript | Tag::Link { .. } | Tag::Image { .. }
        ),
        Event::End(tag) => matches!(
            tag,
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Superscript | TagEnd::Subscript | TagEnd::Link | TagEnd::Image
        ),
        Event::Text(_)
        | Event::Code(_)
        | Event::InlineHtml(_)
        | Event::InlineMath(_)
        | Event::SoftBreak
        | Event::HardBreak
        | Event::FootnoteReference(_)
        | Event::TaskListMarker(_) => true,
        _ => false,
    }
}

impl Md<'_> {
    /// Blocks up to the end of the enclosing container (consumed) or of the text.
    fn blocks(&mut self, out: &mut Vec<Block>) {
        while let Some(event) = self.ev.peek() {
            if is_inline(event) {
                // Text right inside a container: a tight list item.
                let spans = self.inlines();
                if !spans.is_empty() {
                    out.push(Block::Paragraph(spans));
                }
                continue;
            }
            let Some(event) = self.ev.next() else { break };
            match event {
                Event::End(_) => return,
                Event::Start(Tag::Paragraph) => {
                    let spans = self.inlines();
                    self.skip_end();
                    if !spans.is_empty() {
                        out.push(Block::Paragraph(spans));
                    }
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    let spans = self.inlines();
                    self.skip_end();
                    out.push(Block::Heading(level as u8, spans));
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    let mut inner = Vec::new();
                    self.blocks(&mut inner);
                    out.push(Block::Quote(inner));
                }
                Event::Start(Tag::CodeBlock(_)) => out.push(Block::Code(self.raw())),
                Event::Start(Tag::HtmlBlock) => out.push(Block::Html(self.raw())),
                Event::Start(Tag::MetadataBlock(_)) => {
                    self.raw();
                }
                Event::Start(Tag::List(start)) => {
                    let items = self.items();
                    out.push(Block::List { start, items });
                }
                Event::Start(Tag::Table(align)) => out.push(self.table(align)),
                // Other containers (footnote definitions, ...): their blocks.
                Event::Start(_) => self.blocks(out),
                Event::Rule => out.push(Block::Rule),
                Event::Html(t) | Event::DisplayMath(t) => out.push(Block::Html(t.trim_end().to_string())),
                _ => {}
            }
        }
    }

    /// Skips to the end of the current block (its End consumed).
    fn skip_end(&mut self) {
        for event in self.ev.by_ref() {
            if matches!(event, Event::End(_)) {
                break;
            }
        }
    }

    /// The text of a code or HTML block, up to its end.
    fn raw(&mut self) -> String {
        let mut s = String::new();
        for event in self.ev.by_ref() {
            match event {
                Event::End(_) => break,
                Event::Text(t) | Event::Html(t) => s.push_str(&t),
                _ => {}
            }
        }
        s.truncate(s.trim_end_matches(['\n', '\r']).len());
        s
    }

    fn items(&mut self) -> Vec<Item> {
        let mut items = Vec::new();
        while let Some(event) = self.ev.next() {
            match event {
                Event::Start(Tag::Item) => {
                    // An outer item's marker survives the nested list.
                    let outer = self.task.take();
                    let mut blocks = Vec::new();
                    self.blocks(&mut blocks);
                    let task = std::mem::replace(&mut self.task, outer);
                    items.push(Item { task, blocks });
                }
                _ => break,
            }
        }
        items
    }

    fn table(&mut self, align: Vec<Alignment>) -> Block {
        let (mut head, mut rows, mut row) = (Vec::new(), Vec::new(), Vec::new());
        while let Some(event) = self.ev.next() {
            match event {
                Event::Start(Tag::TableCell) => {
                    let cell = self.inlines();
                    self.skip_end();
                    row.push(cell);
                }
                Event::End(TagEnd::TableHead) => head = std::mem::take(&mut row),
                Event::End(TagEnd::TableRow) => rows.push(std::mem::take(&mut row)),
                Event::End(TagEnd::Table) => break,
                _ => {}
            }
        }
        let align = align
            .into_iter()
            .map(|a| match a {
                Alignment::Center => Horizontal::Center,
                Alignment::Right => Horizontal::Right,
                Alignment::None | Alignment::Left => Horizontal::Left,
            })
            .collect();
        Block::Table { align, head, rows }
    }

    fn style(&self) -> Style {
        let n = &self.nest;
        Style { bold: n.bold > 0, italic: n.italic > 0, strike: n.strike > 0, code: false, image: n.image > 0 }
    }

    /// Adds text in the current style, merged into the last run if it has the same style.
    fn push(&self, out: &mut Vec<Inline>, text: &str, style: Style) {
        let link = self.links.last().cloned();
        match out.last_mut() {
            Some(last) if last.style == style && last.link == link => last.text.push_str(text),
            _ if text.is_empty() => {}
            _ => out.push(Inline { text: text.to_string(), style, link }),
        }
    }

    /// Inline content up to the first event that isn't inline (not consumed).
    fn inlines(&mut self) -> Vec<Inline> {
        let mut out = Vec::new();
        while self.ev.peek().is_some_and(is_inline) {
            let Some(event) = self.ev.next() else { break };
            let style = self.style();
            match event {
                Event::Text(t) | Event::InlineHtml(t) | Event::InlineMath(t) => self.push(&mut out, &t, style),
                Event::Code(t) => self.push(&mut out, &t, Style { code: true, ..style }),
                Event::SoftBreak => self.push(&mut out, " ", style),
                Event::HardBreak => self.push(&mut out, "\n", style),
                Event::FootnoteReference(name) => self.push(&mut out, &format!("[^{name}]"), style),
                Event::TaskListMarker(done) => self.task = Some(done),
                Event::Start(Tag::Strong) => self.nest.bold += 1,
                Event::End(TagEnd::Strong) => self.nest.bold = self.nest.bold.saturating_sub(1),
                Event::Start(Tag::Emphasis) => self.nest.italic += 1,
                Event::End(TagEnd::Emphasis) => self.nest.italic = self.nest.italic.saturating_sub(1),
                Event::Start(Tag::Strikethrough) => self.nest.strike += 1,
                Event::End(TagEnd::Strikethrough) => self.nest.strike = self.nest.strike.saturating_sub(1),
                Event::Start(Tag::Link { dest_url, .. }) => self.links.push(dest_url.to_string()),
                Event::End(TagEnd::Link) => {
                    self.links.pop();
                }
                Event::Start(Tag::Image { .. }) => {
                    self.nest.image += 1;
                    self.alts.push(out.iter().map(|i: &Inline| i.text.len()).sum());
                }
                Event::End(TagEnd::Image) => {
                    let start = self.alts.pop().unwrap_or(0);
                    if out.iter().map(|i| i.text.len()).sum::<usize>() == start {
                        self.push(&mut out, "image", style);
                    }
                    self.nest.image = self.nest.image.saturating_sub(1);
                }
                _ => {}
            }
        }
        out
    }
}

/// What a selectable leaf of the rendered view is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// Code or raw HTML: copied verbatim.
    Code,
    /// A table row: one piece per cell, copied tab-separated.
    Row,
}

/// A selectable block of the rendered view, in document order: a paragraph
/// (also in a list item or quote), heading, code or HTML block, or table row.
/// Find searches it and copy writes it out.
#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    pub kind: Kind,
    /// Plain text of each separately drawn part: the cells of a table row, else one.
    pub pieces: Vec<String>,
    /// Put before the text when copied: list indentation and marker.
    pub prefix: String,
    /// The leaves of one list (with its nested lists) or table share it.
    pub group: Option<usize>,
}

/// A parsed Markdown file: its blocks and their selectable leaves.
#[derive(Debug)]
pub struct Doc {
    pub blocks: Vec<Block>,
    pub leaves: Vec<Leaf>,
}

pub(super) fn parse_doc(src: &str) -> Doc {
    let blocks = parse(src);
    let leaves = leaves(&blocks);
    Doc { blocks, leaves }
}

fn plain(inlines: &[Inline]) -> String {
    inlines.iter().map(|i| i.text.as_str()).collect()
}

#[derive(Default)]
struct Flat {
    out: Vec<Leaf>,
    groups: usize,
    /// List marker for the next leaf (the first of a list item).
    marker: Option<String>,
}

impl Flat {
    fn group(&mut self) -> usize {
        self.groups += 1;
        self.groups
    }

    fn push(&mut self, kind: Kind, pieces: Vec<String>, depth: usize, group: Option<usize>) {
        let prefix = match self.marker.take() {
            Some(m) => "  ".repeat(depth.saturating_sub(1)) + &m,
            None => "  ".repeat(depth),
        };
        self.out.push(Leaf { kind, pieces, prefix, group });
    }

    /// In the order `Ctx::block` numbers the leaves it draws.
    fn blocks(&mut self, blocks: &[Block], depth: usize, group: Option<usize>) {
        for b in blocks {
            match b {
                Block::Paragraph(s) | Block::Heading(_, s) => self.push(Kind::Text, vec![plain(s)], depth, group),
                Block::Code(t) | Block::Html(t) => self.push(Kind::Code, vec![t.clone()], depth, group),
                Block::Quote(inner) => self.blocks(inner, depth, group),
                Block::List { start, items } => {
                    let group = Some(group.unwrap_or_else(|| self.group()));
                    for (k, item) in items.iter().enumerate() {
                        self.marker = Some(match (item.task, start) {
                            (Some(true), _) => "[x] ".into(),
                            (Some(false), _) => "[ ] ".into(),
                            (None, Some(n)) => format!("{}. ", n + k as u64),
                            (None, None) => "- ".into(),
                        });
                        self.blocks(&item.blocks, depth + 1, group);
                        self.marker = None;
                    }
                }
                Block::Table { head, rows, .. } => {
                    if table_columns(head, rows) == 0 {
                        continue;
                    }
                    let group = Some(self.group());
                    for cells in std::iter::once(head).chain(rows) {
                        self.push(Kind::Row, cells.iter().map(|c| plain(c)).collect(), 0, group);
                    }
                }
                Block::Rule => {}
            }
        }
    }
}

/// The selectable leaves of `blocks`, in document order.
pub(super) fn leaves(blocks: &[Block]) -> Vec<Leaf> {
    let mut flat = Flat::default();
    flat.blocks(blocks, 0, None);
    flat.out
}

/// A find match: in leaf `leaf`, its piece `piece` (a table cell, else 0),
/// bytes `start..end` of the piece's plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub leaf: usize,
    pub piece: usize,
    pub start: usize,
    pub end: usize,
}

/// All matches of `query` (`case`: case-sensitive), in document order; a
/// match never spans leaves or cells.
pub(super) fn find(leaves: &[Leaf], query: &str, case: bool) -> Vec<Hit> {
    let mut hits = Vec::new();
    if query.is_empty() {
        return hits;
    }
    for (leaf, l) in leaves.iter().enumerate() {
        for (piece, hay) in l.pieces.iter().enumerate() {
            let mut from = 0;
            while from < hay.len() {
                let Some((start, end)) = rc_core::viewer::find_text(hay, query, from, case, true) else { break };
                hits.push(Hit { leaf, piece, start, end });
                from = end.max(start + 1);
            }
        }
    }
    hits
}

/// `leaves` as plain text for the clipboard: headings without `#`, list
/// items with their marker and indentation, code verbatim, table cells
/// tab-separated; the lines of one list or table one per line, other blocks
/// separated by a blank line.
pub(super) fn copy_text(leaves: &[Leaf]) -> String {
    let mut out = String::new();
    for (k, l) in leaves.iter().enumerate() {
        if k > 0 {
            let same = l.group.is_some() && l.group == leaves[k - 1].group;
            out.push_str(if same { "\n" } else { "\n\n" });
        }
        match l.kind {
            Kind::Text => {
                out.push_str(&l.prefix);
                out.push_str(l.pieces.first().map_or("", String::as_str));
            }
            Kind::Code => out.push_str(l.pieces.first().map_or("", String::as_str)),
            Kind::Row => out.push_str(&l.pieces.join("\t")),
        }
    }
    out
}

/// Id of the container around the leaf holding the current find match.
const HIT_ID: &str = "viewer-md-hit";

/// Where the current match's leaf is (its top and height, in the
/// scrollable's content coordinates) and the scroll state, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Located {
    pub top: f32,
    pub height: f32,
    /// Height of the visible area.
    pub view: f32,
    /// Current scroll offset.
    pub offset: f32,
    /// Height of the whole content.
    pub content: f32,
}

/// Finds the current match's leaf (as drawn by the last `view`) in the
/// scrollable `scroll`.
pub(super) fn locate(scroll: iced::widget::scrollable::Id) -> iced::Task<Option<Located>> {
    use iced::advanced::widget::{self, operation, Operation};
    use iced::{Rectangle, Vector};

    struct Locate {
        scroll: widget::Id,
        target: widget::Id,
        frame: Option<(Rectangle, Rectangle, Vector)>,
        found: Option<Located>,
    }

    impl Operation<Option<Located>> for Locate {
        fn scrollable(
            &mut self,
            _state: &mut dyn operation::Scrollable,
            id: Option<&widget::Id>,
            bounds: Rectangle,
            content: Rectangle,
            translation: Vector,
        ) {
            if id == Some(&self.scroll) {
                self.frame = Some((bounds, content, translation));
            }
        }

        fn container(
            &mut self,
            id: Option<&widget::Id>,
            bounds: Rectangle,
            operate_on_children: &mut dyn FnMut(&mut dyn Operation<Option<Located>>),
        ) {
            if self.found.is_some() {
                return;
            }
            if id == Some(&self.target) {
                if let Some((view, content, t)) = self.frame {
                    let top = bounds.y - content.y;
                    self.found =
                        Some(Located { top, height: bounds.height, view: view.height, offset: t.y, content: content.height });
                }
                return;
            }
            operate_on_children(self);
        }

        fn finish(&self) -> operation::Outcome<Option<Located>> {
            operation::Outcome::Some(self.found)
        }
    }

    widget::operate(Locate { scroll: scroll.into(), target: widget::Id::new(HIT_ID), frame: None, found: None })
}

/// About how far (pixels) into its leaf the match `hit` is: its line in a
/// code block, else the top.
pub(super) fn hit_depth(leaves: &[Leaf], hit: Hit) -> f32 {
    match leaves.get(hit.leaf) {
        Some(l) if l.kind == Kind::Code => {
            let lines = l.pieces.first().map_or(0, |t| t[..hit.start.min(t.len())].matches('\n').count());
            12.0 + lines as f32 * CODE_SIZE * 1.5
        }
        _ => 0.0,
    }
}

/// New scroll offset that shows the match `depth` pixels into the leaf
/// `at`, or `None` when it is visible already. A leaf that fits is shown
/// whole, a third of the way down when it has to move.
pub(super) fn scroll_target(at: Located, depth: f32) -> Option<f32> {
    let max = (at.content - at.view).max(0.0);
    let (top, bottom) = (at.offset, at.offset + at.view);
    let want = if at.height <= at.view {
        if at.top >= top && at.top + at.height <= bottom {
            return None;
        }
        at.top - (at.view - at.height) / 3.0
    } else {
        let point = at.top + depth;
        // The match's line (about 30 px) inside the visible area.
        if point >= top && point + 30.0 <= bottom {
            return None;
        }
        point - at.view / 3.0
    };
    Some(want.clamp(0.0, max))
}

/// Changes when the theme's colours do (the cached widgets use them).
pub(super) fn theme_key() -> u64 {
    let c = theme::c();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for color in [c.text, c.muted, c.accent, c.panel, c.header, c.border, c.bg, c.selected, c.cursor] {
        for x in [color.r, color.g, color.b, color.a] {
            x.to_bits().hash(&mut h);
        }
    }
    h.finish()
}

/// What the view marks besides the text: the selected leaves (an inclusive
/// range, either way round), the find matches (in document order) and the
/// current one (an index of `hits`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Marks<'a> {
    pub sel: Option<(usize, usize)>,
    pub hits: &'a [Hit],
    pub current: Option<usize>,
}

/// A find match in a piece: bytes `start..end`, and whether it is the current one.
type Mark = (usize, usize, bool);

/// State while building the view: the colours, the marks, and the number
/// of the next leaf (counted in the order of `leaves`).
struct Ctx<'a> {
    c: Colors,
    marks: Marks<'a>,
    next: Cell<usize>,
}

/// The blocks in a centred reading column.
pub(super) fn view(blocks: &[Block], marks: Marks<'_>) -> Element<'static, Message> {
    view_counted(blocks, marks).0
}

/// The view, and the number of leaves it drew.
fn view_counted(blocks: &[Block], marks: Marks<'_>) -> (Element<'static, Message>, usize) {
    let ctx = Ctx { c: theme::c(), marks, next: Cell::new(0) };
    let content = Column::with_children(blocks.iter().map(|b| ctx.block(b, 0, ctx.c.text)))
        .spacing(14)
        .width(Length::Fill)
        .max_width(MAX_W);
    (container(content).center_x(Length::Fill).padding([28, 32]).into(), ctx.next.get())
}

fn rich<'a>(spans: Vec<Span<'static, Message, Font>>, size: f32, color: iced::Color) -> Element<'a, Message> {
    rich_text(spans).size(size).line_height(LineHeight::Relative(1.6)).font(BODY).color(color).width(Length::Fill).into()
}

/// A find match's look: the current one like the text view's, the others fainter.
fn marked(span: Span<'static, Message, Font>, current: Option<bool>, c: &Colors) -> Span<'static, Message, Font> {
    match current {
        Some(true) => span.color(c.bg).background(c.selected),
        Some(false) => span.background(iced::Color { a: 0.4, ..c.selected }),
        None => span,
    }
}

/// Splits bytes `start..end` of a piece at the edges of `marks`: each part
/// with whether a mark covers it (and if it is the current match).
fn cut(start: usize, end: usize, marks: &[Mark]) -> Vec<(usize, usize, Option<bool>)> {
    let mut at = vec![start, end];
    at.extend(marks.iter().flat_map(|&(a, b, _)| [a, b]).filter(|&x| x > start && x < end));
    at.sort_unstable();
    at.dedup();
    at.windows(2)
        .map(|w| (w[0], w[1], marks.iter().find(|&&(a, b, _)| a <= w[0] && w[1] <= b).map(|m| m.2)))
        .collect()
}

/// Text spans of `inlines`; `bold` makes them all bold; `marks` are find matches.
fn spans(inlines: &[Inline], c: &Colors, size: f32, bold: bool, marks: &[Mark]) -> Vec<Span<'static, Message, Font>> {
    let mut out = Vec::new();
    let mut pos = 0;
    for i in inlines {
        let start = pos;
        pos += i.text.len();
        for (a, b, mark) in cut(start, pos, marks) {
            out.push(marked(styled(i, i.text[a - start..b - start].to_string(), c, size, bold), mark, c));
        }
    }
    out
}

/// `text` (a part of `i`) as a span in `i`'s style.
fn styled(i: &Inline, text: String, c: &Colors, size: f32, bold: bool) -> Span<'static, Message, Font> {
    let st = i.style;
    let mut font = if st.code { Font::MONOSPACE } else { BODY };
    if st.bold || bold {
        font.weight = Weight::Bold;
    }
    if st.italic || st.image {
        font.style = FontStyle::Italic;
    }
    let mut span = Span::new(text).font(font);
    if st.code {
        span = span.size(size * 0.9).background(c.header).padding(Padding::from([0, 3]));
    }
    if st.strike {
        span = span.strikethrough(true);
    }
    if st.image {
        span = span.color(c.muted);
    }
    if let Some(url) = &i.link {
        span = span.color(c.accent).underline(true).link(Message::Viewer(ViewerMsg::Link(url.clone())));
    }
    span
}

/// Monospace text (a code or HTML block), with find matches marked.
fn mono(s: &str, color: iced::Color, c: &Colors, marks: &[Mark]) -> Element<'static, Message> {
    if marks.is_empty() {
        return text(s.to_string())
            .font(Font::MONOSPACE)
            .size(CODE_SIZE)
            .line_height(LineHeight::Relative(1.5))
            .wrapping(Wrapping::WordOrGlyph)
            .color(color)
            .into();
    }
    let spans: Vec<Span<'static, Message, Font>> =
        cut(0, s.len(), marks).into_iter().map(|(a, b, mark)| marked(Span::new(s[a..b].to_string()), mark, c)).collect();
    rich_text(spans)
        .font(Font::MONOSPACE)
        .size(CODE_SIZE)
        .line_height(LineHeight::Relative(1.5))
        .wrapping(Wrapping::WordOrGlyph)
        .color(color)
        .into()
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 28.0,
        2 => 22.0,
        3 => 18.0,
        4 => 16.0,
        5 => 15.0,
        _ => 14.0,
    }
}

/// A thin horizontal line.
fn hline<'a>(c: &Colors) -> Element<'a, Message> {
    container(Space::with_height(1)).width(Length::Fill).height(1).style(filled(c.border)).into()
}

/// The number of columns of a table.
fn table_columns(head: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>]) -> usize {
    head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0))
}

impl Ctx<'_> {
    /// Numbers the next leaf.
    fn take(&self) -> usize {
        let i = self.next.get();
        self.next.set(i + 1);
        i
    }

    fn selected(&self, leaf: usize) -> bool {
        self.marks.sel.is_some_and(|(a, b)| (a.min(b)..=a.max(b)).contains(&leaf))
    }

    /// The find matches in piece `piece` of leaf `leaf`.
    fn marks(&self, leaf: usize, piece: usize) -> Vec<Mark> {
        let hits = self.marks.hits;
        let from = hits.partition_point(|h| (h.leaf, h.piece) < (leaf, piece));
        hits[from..]
            .iter()
            .enumerate()
            .take_while(|(_, h)| h.leaf == leaf && h.piece == piece)
            .map(|(k, h)| (h.start, h.end, self.marks.current == Some(from + k)))
            .collect()
    }

    /// Leaf `leaf` drawn as `content`: tinted when selected (if `tint`; else
    /// `content` shows that itself), telling the viewer when the pointer
    /// enters it (selection drags), and with the id `locate` looks for when
    /// it holds the current match.
    fn leaf(&self, leaf: usize, content: Element<'static, Message>, tint: bool) -> Element<'static, Message> {
        let mut wrap = container(content).width(Length::Fill);
        if tint && self.selected(leaf) {
            wrap = wrap.style(filled(self.c.cursor));
        }
        if self.marks.current.and_then(|k| self.marks.hits.get(k)).is_some_and(|h| h.leaf == leaf) {
            wrap = wrap.id(container::Id::new(HIT_ID));
        }
        mouse_area(wrap)
            .on_enter(Message::Viewer(ViewerMsg::MdEnter(leaf)))
            .on_exit(Message::Viewer(ViewerMsg::MdExit(leaf)))
            .into()
    }

    /// `color` is the text colour (muted inside quotes); `depth` the list
    /// nesting. Numbers the leaves in the order of `leaves`.
    fn block(&self, b: &Block, depth: usize, color: iced::Color) -> Element<'static, Message> {
        let c = &self.c;
        match b {
            Block::Paragraph(s) => {
                let i = self.take();
                self.leaf(i, rich(spans(s, c, BODY_SIZE, false, &self.marks(i, 0)), BODY_SIZE, color), true)
            }
            Block::Heading(level, s) => {
                let i = self.take();
                let size = heading_size(*level);
                let color = if *level == 6 { c.muted } else { color };
                let title = rich(spans(s, c, size, true, &self.marks(i, 0)), size, color);
                let top = if *level <= 2 { 10.0 } else { 6.0 };
                let inner: Element<'static, Message> =
                    if *level <= 2 { column![title, hline(c)].spacing(4).into() } else { title };
                container(self.leaf(i, inner, true)).padding(Padding { top, ..Padding::ZERO }).into()
            }
            Block::Code(code) => {
                let i = self.take();
                let bg = if self.selected(i) { c.cursor } else { c.header };
                let border = c.border;
                let content = container(mono(code, c.text, c, &self.marks(i, 0)))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(bg.into()),
                        border: Border { color: border, width: 1.0, radius: 4.0.into() },
                        ..container::Style::default()
                    });
                self.leaf(i, content.into(), false)
            }
            Block::Html(html) => {
                let i = self.take();
                self.leaf(i, mono(html, c.muted, c, &self.marks(i, 0)), true)
            }
            Block::Quote(inner) => {
                let content = Column::with_children(inner.iter().map(|b| self.block(b, depth, c.muted))).spacing(10);
                // The bar is a layer, so it gets the quote's height.
                stack![
                    container(content).padding(Padding { left: 18.0, ..Padding::ZERO }).width(Length::Fill),
                    container(Space::with_width(4)).width(4).height(Length::Fill).style(filled(c.border)),
                ]
                .width(Length::Fill)
                .into()
            }
            Block::List { start, items } => {
                let digits = start.map_or(0, |s| (s + items.len() as u64).to_string().len());
                let marker_w = if digits > 0 { 12.0 + 8.0 * digits as f32 } else { 20.0 };
                let bullet = ["•", "◦", "▪"][depth % 3];
                Column::with_children(items.iter().enumerate().map(|(k, item)| {
                    let marker = match (item.task, start) {
                        (Some(true), _) => "☑".to_string(),
                        (Some(false), _) => "☐".to_string(),
                        (None, Some(n)) => format!("{}.", n + k as u64),
                        (None, None) => bullet.to_string(),
                    };
                    let marker = text(marker)
                        .size(BODY_SIZE)
                        .line_height(LineHeight::Relative(1.6))
                        .font(BODY)
                        .color(if item.task.is_some() { c.accent } else { color })
                        .width(marker_w)
                        .align_x(Horizontal::Right);
                    let body =
                        Column::with_children(item.blocks.iter().map(|b| self.block(b, depth + 1, color))).spacing(6);
                    row![marker, body.width(Length::Fill)].spacing(8).into()
                }))
                .spacing(4)
                .into()
            }
            Block::Table { align, head, rows } => self.table(align, head, rows),
            Block::Rule => container(hline(c)).padding([6, 0]).into(),
        }
    }

    /// A grid with thin lines; columns get widths by their longest cell.
    /// Each row is a leaf.
    fn table(&self, align: &[Horizontal], head: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>]) -> Element<'static, Message> {
        let c = &self.c;
        let n = table_columns(head, rows);
        if n == 0 {
            return Space::with_height(0).into();
        }
        let widths: Vec<u16> = (0..n)
            .map(|k| {
                let cells = head.get(k).into_iter().chain(rows.iter().filter_map(|r| r.get(k)));
                let longest = cells.map(|cell| cell.iter().map(|i| i.text.chars().count()).sum::<usize>()).max().unwrap_or(0);
                longest.clamp(3, 40) as u16
            })
            .collect();
        let line = |cells: &[Vec<Inline>], bold: bool, bg: iced::Color| -> Element<'static, Message> {
            let leaf = self.take();
            let bg = if self.selected(leaf) { c.cursor } else { bg };
            let texts = Row::with_children((0..n).map(|k| {
                let s = cells.get(k).map(|cell| spans(cell, c, BODY_SIZE, bold, &self.marks(leaf, k))).unwrap_or_default();
                container(rich_text(s).size(BODY_SIZE).line_height(LineHeight::Relative(1.5)).font(BODY).color(c.text))
                    .padding([5, 10])
                    .width(Length::FillPortion(widths[k]))
                    .align_x(align.get(k).copied().unwrap_or(Horizontal::Left))
                    .into()
            }))
            .width(Length::Fill);
            // Column lines: a layer of the row's height over the cells.
            let lines = Row::with_children((0..n).map(|k| {
                let w = Length::FillPortion(widths[k]);
                if k + 1 < n {
                    row![
                        Space::with_width(Length::Fill),
                        container(Space::with_width(1)).width(1).height(Length::Fill).style(filled(c.border))
                    ]
                    .width(w)
                    .height(Length::Fill)
                    .into()
                } else {
                    Space::with_width(w).into()
                }
            }))
            .width(Length::Fill)
            .height(Length::Fill);
            let content = container(stack![texts, lines].width(Length::Fill)).width(Length::Fill).style(filled(bg));
            self.leaf(leaf, content.into(), false)
        };
        let mut lines = vec![line(head, true, c.header)];
        lines.extend(rows.iter().map(|r| line(r, false, c.panel)));
        container(Column::with_children(lines).spacing(1)).padding(1).width(Length::Fill).style(filled(c.border)).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Inline {
        Inline { text: text.into(), style: Style::default(), link: None }
    }

    #[test]
    fn markdown_extensions() {
        assert!(is_markdown(Path::new("README.md")));
        assert!(is_markdown(Path::new(r"C:\docs\Notes.MD")));
        assert!(is_markdown(Path::new("a.Markdown")));
        assert!(!is_markdown(Path::new("a.mdx")));
        assert!(!is_markdown(Path::new("md")));
        assert!(!is_markdown(Path::new("a.txt")));
    }

    #[test]
    fn headings_and_inline_styles() {
        let blocks = parse("# Title\n\n###### Six\n\nSome **bold**, *it*, ~~gone~~ and `code` [here](https://x.org).\n");
        assert_eq!(blocks[0], Block::Heading(1, vec![plain("Title")]));
        assert_eq!(blocks[1], Block::Heading(6, vec![plain("Six")]));
        let Block::Paragraph(p) = &blocks[2] else { panic!("{:?}", blocks[2]) };
        assert_eq!(p[0], plain("Some "));
        assert_eq!(p[1], Inline { text: "bold".into(), style: Style { bold: true, ..Style::default() }, link: None });
        assert!(p[3].style.italic);
        assert!(p[5].style.strike);
        assert_eq!(p[7], Inline { text: "code".into(), style: Style { code: true, ..Style::default() }, link: None });
        assert_eq!(p[9], Inline { text: "here".into(), style: Style::default(), link: Some("https://x.org".into()) });
        assert_eq!(p[10], plain("."));
    }

    #[test]
    fn nested_list() {
        let blocks = parse("- one\n  - inner\n- two\n\n3. three\n4. four\n");
        let Block::List { start: None, items } = &blocks[0] else { panic!("{:?}", blocks[0]) };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].blocks[0], Block::Paragraph(vec![plain("one")]));
        let Block::List { start: None, items: inner } = &items[0].blocks[1] else { panic!("{:?}", items[0]) };
        assert_eq!(inner[0].blocks, vec![Block::Paragraph(vec![plain("inner")])]);
        assert_eq!(items[1].blocks, vec![Block::Paragraph(vec![plain("two")])]);
        let Block::List { start: Some(3), items } = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn code_block() {
        let blocks = parse("```rust\nfn main() {}\n  let x = 1;\n```\n\n    indented\n");
        assert_eq!(blocks[0], Block::Code("fn main() {}\n  let x = 1;".into()));
        assert_eq!(blocks[1], Block::Code("indented".into()));
    }

    #[test]
    fn table() {
        let blocks = parse("| A | **B** |\n|:--|--:|\n| 1 | 2 |\n| 3 |\n");
        let Block::Table { align, head, rows } = &blocks[0] else { panic!("{:?}", blocks[0]) };
        assert_eq!(align, &vec![Horizontal::Left, Horizontal::Right]);
        assert_eq!(head[0], vec![plain("A")]);
        assert!(head[1][0].style.bold);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], vec![plain("2")]);
    }

    #[test]
    fn task_list() {
        let blocks = parse("- [x] done\n- [ ] todo\n  - [x] sub\n- plain\n");
        let Block::List { items, .. } = &blocks[0] else { panic!("{:?}", blocks[0]) };
        assert_eq!(items[0].task, Some(true));
        assert_eq!(items[0].blocks, vec![Block::Paragraph(vec![plain("done")])]);
        assert_eq!(items[1].task, Some(false));
        let Block::List { items: sub, .. } = &items[1].blocks[1] else { panic!("{:?}", items[1]) };
        assert_eq!(sub[0].task, Some(true));
        assert_eq!(items[2].task, None);
    }

    #[test]
    fn raw_html_kept_as_text() {
        let blocks = parse("<div align=\"center\">\n<b>hi</b>\n</div>\n\nA <span>word</span>.\n");
        assert_eq!(blocks[0], Block::Html("<div align=\"center\">\n<b>hi</b>\n</div>".into()));
        assert_eq!(blocks[1], Block::Paragraph(vec![plain("A <span>word</span>.")]));
    }

    #[test]
    fn quote_rule_and_image() {
        let blocks = parse("> quoted\n> > deeper\n\n---\n\n![alt text](pic.png) ![](x.png)\n");
        let Block::Quote(inner) = &blocks[0] else { panic!("{:?}", blocks[0]) };
        assert_eq!(inner[0], Block::Paragraph(vec![plain("quoted")]));
        assert!(matches!(inner[1], Block::Quote(_)));
        assert_eq!(blocks[1], Block::Rule);
        let Block::Paragraph(p) = &blocks[2] else { panic!("{:?}", blocks[2]) };
        let image = Style { italic: false, image: true, ..Style::default() };
        assert_eq!(p[0], Inline { text: "alt text".into(), style: image, link: None });
        assert_eq!(p[2], Inline { text: "image".into(), style: image, link: None });
    }

    const MIXED: &str = "# Title **bold**\n\nSome *text* and `code`.\n\n- one\n  - inner\n- [x] done\n\n3. three\n\n\
                         > quoted\n\n---\n\n```\nfn a() {}\nfn b() {}\n```\n\n| A | B |\n|---|---|\n| 1 | x2 |\n";

    #[test]
    fn leaves_hold_plain_text_per_block() {
        let l = leaves(&parse(MIXED));
        let texts: Vec<(Kind, &str, Vec<&str>)> =
            l.iter().map(|l| (l.kind, l.prefix.as_str(), l.pieces.iter().map(String::as_str).collect())).collect();
        assert_eq!(
            texts,
            vec![
                (Kind::Text, "", vec!["Title bold"]),
                (Kind::Text, "", vec!["Some text and code."]),
                (Kind::Text, "- ", vec!["one"]),
                (Kind::Text, "  - ", vec!["inner"]),
                (Kind::Text, "[x] ", vec!["done"]),
                (Kind::Text, "3. ", vec!["three"]),
                (Kind::Text, "", vec!["quoted"]),
                (Kind::Code, "", vec!["fn a() {}\nfn b() {}"]),
                (Kind::Row, "", vec!["A", "B"]),
                (Kind::Row, "", vec!["1", "x2"]),
            ]
        );
        // One list with its nested list, another list, the table.
        assert_eq!(l[2].group, l[3].group);
        assert_eq!(l[3].group, l[4].group);
        assert_ne!(l[4].group, l[5].group);
        assert_eq!(l[8].group, l[9].group);
        assert_eq!(l[0].group, None);
    }

    #[test]
    fn view_numbers_leaves_like_leaves() {
        for src in [MIXED, include_str!("../../../../docs/markdown-sample.md")] {
            let blocks = parse(src);
            assert_eq!(view_counted(&blocks, Marks::default()).1, leaves(&blocks).len());
        }
    }

    #[test]
    fn find_marks_every_match() {
        let l = leaves(&parse("Aa aa AA\n\n```\nlet aa = 1;\n```\n\n| aa | b |\n|---|---|\n| x | aaa |\n"));
        let hit = |leaf, piece, start, end| Hit { leaf, piece, start, end };
        assert_eq!(
            find(&l, "aa", false),
            vec![
                hit(0, 0, 0, 2),
                hit(0, 0, 3, 5),
                hit(0, 0, 6, 8),
                hit(1, 0, 4, 6),
                hit(2, 0, 0, 2),
                // No overlapping matches.
                hit(3, 1, 0, 2),
            ]
        );
        assert_eq!(find(&l, "aa", true).len(), 4);
        assert_eq!(find(&l, "AA", true), vec![hit(0, 0, 6, 8)]);
        // Matches don't span blocks or cells.
        assert!(find(&l, "aa\nlet", false).is_empty());
        assert!(find(&l, "x\taaa", false).is_empty());
        assert!(find(&l, "", false).is_empty());
        // Byte offsets around non-ASCII text.
        let l = leaves(&parse("Šček ŠČEK\n"));
        assert_eq!(find(&l, "ček", false), vec![hit(0, 0, 2, 6), hit(0, 0, 9, 13)]);
    }

    #[test]
    fn copy_of_a_mixed_selection() {
        let l = leaves(&parse(MIXED));
        let text = copy_text(&l);
        let want = "Title bold\n\nSome text and code.\n\n- one\n  - inner\n[x] done\n\n3. three\n\nquoted\n\n\
                    fn a() {}\nfn b() {}\n\nA\tB\n1\tx2";
        assert_eq!(text, want);
        assert_eq!(copy_text(&l[3..6]), "  - inner\n[x] done\n\n3. three");
    }

    #[test]
    fn marks_split_spans() {
        assert_eq!(cut(0, 10, &[]), vec![(0, 10, None)]);
        assert_eq!(cut(0, 10, &[(2, 4, false), (6, 12, true)]), vec![
            (0, 2, None),
            (2, 4, Some(false)),
            (4, 6, None),
            (6, 10, Some(true))
        ]);
        // Over two inlines: each gets its part of the match.
        let c = theme::c();
        let inlines = [plain("ab"), Inline { text: "cd".into(), style: Style { bold: true, ..Style::default() }, link: None }];
        let s = spans(&inlines, &c, BODY_SIZE, false, &[(1, 3, true)]);
        let texts: Vec<&str> = s.iter().map(|s| s.text.as_ref()).collect();
        assert_eq!(texts, ["a", "b", "c", "d"]);
        assert!(s[1].highlight.is_some());
        assert!(s[3].highlight.is_none());
    }

    #[test]
    fn scrolls_only_when_the_match_is_hidden() {
        let at = |top, height, offset| Located { top, height, view: 300.0, offset, content: 2000.0 };
        assert_eq!(scroll_target(at(100.0, 50.0, 0.0), 0.0), None);
        // Below the view: the block a third of the way down.
        assert_eq!(scroll_target(at(900.0, 30.0, 0.0), 0.0), Some(810.0));
        // Above it, and clamped at the top and the end.
        assert_eq!(scroll_target(at(20.0, 30.0, 500.0), 0.0), Some(0.0));
        assert_eq!(scroll_target(at(1990.0, 10.0, 0.0), 0.0), Some(1700.0));
        // A block taller than the view: its matched line must show.
        assert_eq!(scroll_target(at(0.0, 1000.0, 0.0), 100.0), None);
        assert_eq!(scroll_target(at(0.0, 1000.0, 0.0), 600.0), Some(500.0));
        let l = leaves(&parse("```\na\nb\nc\n```\n"));
        assert_eq!(hit_depth(&l, Hit { leaf: 0, piece: 0, start: 4, end: 5 }), 12.0 + 2.0 * CODE_SIZE * 1.5);
    }
}
