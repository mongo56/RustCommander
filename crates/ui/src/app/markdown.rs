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
use iced::widget::{column, container, rich_text, row, stack, text, Column, Row, Space};
use iced::{Border, Element, Font, Length, Padding};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
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

/// Changes when the theme's colours do (the cached widgets use them).
pub(super) fn theme_key() -> u64 {
    let c = theme::c();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for color in [c.text, c.muted, c.accent, c.panel, c.header, c.border] {
        for x in [color.r, color.g, color.b, color.a] {
            x.to_bits().hash(&mut h);
        }
    }
    h.finish()
}

/// The blocks in a centred reading column.
pub(super) fn view(blocks: &[Block]) -> Element<'static, Message> {
    let c = theme::c();
    let content = Column::with_children(blocks.iter().map(|b| block(b, &c, 0, c.text))).spacing(14).width(Length::Fill).max_width(MAX_W);
    container(content).center_x(Length::Fill).padding([28, 32]).into()
}

fn rich<'a>(spans: Vec<Span<'static, Message, Font>>, size: f32, color: iced::Color) -> Element<'a, Message> {
    rich_text(spans).size(size).line_height(LineHeight::Relative(1.6)).font(BODY).color(color).width(Length::Fill).into()
}

/// Text spans of `inlines`; `bold` makes them all bold.
fn spans(inlines: &[Inline], c: &Colors, size: f32, bold: bool) -> Vec<Span<'static, Message, Font>> {
    inlines
        .iter()
        .map(|i| {
            let st = i.style;
            let mut font = if st.code { Font::MONOSPACE } else { BODY };
            if st.bold || bold {
                font.weight = Weight::Bold;
            }
            if st.italic || st.image {
                font.style = FontStyle::Italic;
            }
            let mut span = Span::new(i.text.clone()).font(font);
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
        })
        .collect()
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

/// `color` is the text colour (muted inside quotes); `depth` the list nesting.
fn block(b: &Block, c: &Colors, depth: usize, color: iced::Color) -> Element<'static, Message> {
    match b {
        Block::Paragraph(s) => rich(spans(s, c, BODY_SIZE, false), BODY_SIZE, color),
        Block::Heading(level, s) => {
            let size = heading_size(*level);
            let color = if *level == 6 { c.muted } else { color };
            let title = rich(spans(s, c, size, true), size, color);
            let top = if *level <= 2 { 10.0 } else { 6.0 };
            let inner: Element<'static, Message> =
                if *level <= 2 { column![title, hline(c)].spacing(4).into() } else { title };
            container(inner).padding(Padding { top, ..Padding::ZERO }).into()
        }
        Block::Code(code) => {
            let (bg, border) = (c.header, c.border);
            container(
                text(code.clone())
                    .font(Font::MONOSPACE)
                    .size(CODE_SIZE)
                    .line_height(LineHeight::Relative(1.5))
                    .wrapping(Wrapping::WordOrGlyph)
                    .color(c.text),
            )
            .padding(12)
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                border: Border { color: border, width: 1.0, radius: 4.0.into() },
                ..container::Style::default()
            })
            .into()
        }
        Block::Html(html) => text(html.clone())
            .font(Font::MONOSPACE)
            .size(CODE_SIZE)
            .line_height(LineHeight::Relative(1.5))
            .wrapping(Wrapping::WordOrGlyph)
            .color(c.muted)
            .into(),
        Block::Quote(inner) => {
            let content = Column::with_children(inner.iter().map(|b| block(b, c, depth, c.muted))).spacing(10);
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
                let body = Column::with_children(item.blocks.iter().map(|b| block(b, c, depth + 1, color))).spacing(6);
                row![marker, body.width(Length::Fill)].spacing(8).into()
            }))
            .spacing(4)
            .into()
        }
        Block::Table { align, head, rows } => table(align, head, rows, c),
        Block::Rule => container(hline(c)).padding([6, 0]).into(),
    }
}

/// A grid with thin lines; columns get widths by their longest cell.
fn table(align: &[Horizontal], head: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>], c: &Colors) -> Element<'static, Message> {
    let n = head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
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
        let texts = Row::with_children((0..n).map(|k| {
            let s = cells.get(k).map(|cell| spans(cell, c, BODY_SIZE, bold)).unwrap_or_default();
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
        container(stack![texts, lines].width(Length::Fill)).width(Length::Fill).style(filled(bg)).into()
    };
    let mut lines = vec![line(head, true, c.header)];
    lines.extend(rows.iter().map(|r| line(r, false, c.panel)));
    container(Column::with_children(lines).spacing(1)).padding(1).width(Length::Fill).style(filled(c.border)).into()
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
}
