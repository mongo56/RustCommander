//! Built-in file viewer (F3, TC's Lister): text with optional word wrap,
//! hex dump, images. Fills the whole window while open; N/P step through
//! the files of the panel it was opened from.

use super::*;
use crate::theme::filled;
use iced::widget::text::{Span, Wrapping};
use iced::widget::{center, column, container, mouse_area, responsive, rich_text, row, text, Column, Space};
use iced::{Alignment, ContentFit, Element, Font, Length};
use rc_core::viewer::{self, Document, Hit, Mode, HEX_WIDTH};
use std::sync::Arc;

const LINE_H: f32 = 18.0;
const FONT_SIZE: f32 = 13.0;
/// Approximate width of a monospace character at `FONT_SIZE`.
const CHAR_W: f32 = 7.8;
/// Header, footer and their padding around the text area.
const VIEWER_CHROME_H: f32 = 64.0;
const SEARCH_H: f32 = 40.0;
/// Columns Left/Right scroll by.
const H_STEP: usize = 4;
/// Images are scaled down to fit this many pixels per side (GPU texture limit).
const MAX_IMAGE_SIDE: u32 = 8192;

fn search_input_id() -> text_input::Id {
    text_input::Id::new("viewer-search")
}

/// What a file was loaded as.
#[derive(Debug, Clone)]
pub enum Loaded {
    Doc(Arc<Document>),
    Image { handle: image::Handle, width: u32, height: u32 },
}

#[derive(Debug)]
enum Content {
    Loading,
    Failed(String),
    Loaded(Loaded),
}

#[derive(Debug)]
pub(super) struct Viewer {
    /// Files N/P step through, and the one shown.
    files: Vec<PathBuf>,
    index: usize,
    /// Panel the viewer was opened from; gets the cursor on close.
    side: usize,
    /// Bumped per load so a late result for another file is dropped.
    seq: u64,
    content: Content,
    mode: Mode,
    wrap: bool,
    /// First row shown, and the first column when not wrapping.
    top: usize,
    left: usize,
    /// Text of the open search box.
    search: Option<String>,
    query: String,
    case: bool,
    hit: Option<Hit>,
    note: Option<(String, Tone)>,
}

impl Viewer {
    fn path(&self) -> &Path {
        &self.files[self.index]
    }

    fn doc_arc(&self) -> Option<Arc<Document>> {
        match &self.content {
            Content::Loaded(Loaded::Doc(doc)) => Some(doc.clone()),
            _ => None,
        }
    }

    fn doc(&self) -> Option<&Document> {
        match &self.content {
            Content::Loaded(Loaded::Doc(doc)) => Some(doc),
            _ => None,
        }
    }
}

/// Reads `path` on a worker thread: as an image if the extension names an
/// image format and it decodes, otherwise as text/binary.
fn load(path: PathBuf, seq: u64) -> Task<Message> {
    Task::perform(
        async move {
            let (tx, rx) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(load_sync(&path));
            });
            rx.await.unwrap_or_else(|_| Err("viewer thread failed".into()))
        },
        move |result| Message::ViewerLoaded(seq, result),
    )
}

fn load_sync(path: &Path) -> Result<Loaded, String> {
    if ::image::ImageFormat::from_path(path).is_ok() {
        if let Ok(img) = ::image::open(path) {
            let img = if img.width() > MAX_IMAGE_SIDE || img.height() > MAX_IMAGE_SIDE {
                img.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, ::image::imageops::FilterType::Triangle)
            } else {
                img
            };
            let rgba = img.into_rgba8();
            let (width, height) = rgba.dimensions();
            let handle = image::Handle::from_rgba(width, height, rgba.into_raw());
            return Ok(Loaded::Image { handle, width, height });
        }
    }
    Document::open(path).map(|d| Loaded::Doc(Arc::new(d))).map_err(|e| e.to_string())
}

impl App {
    /// Opens the viewer on the cursor file of the active panel.
    pub(super) fn open_viewer(&mut self) -> Task<Message> {
        let side = self.active;
        let panel = self.panel(side);
        let Some(current) = panel.current().filter(|e| e.kind == EntryKind::File) else {
            return Task::none();
        };
        let files: Vec<PathBuf> =
            panel.entries.iter().filter(|e| e.kind == EntryKind::File).map(|e| e.path.clone()).collect();
        let index = files.iter().position(|p| *p == current.path).unwrap_or(0);
        self.leave_edits();
        self.viewer = Some(Viewer {
            files,
            index,
            side,
            seq: 0,
            content: Content::Loading,
            mode: Mode::Text,
            wrap: false,
            top: 0,
            left: 0,
            search: None,
            query: String::new(),
            case: false,
            hit: None,
            note: None,
        });
        self.viewer_show(index)
    }

    fn viewer_show(&mut self, index: usize) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        let Some(v) = &mut self.viewer else { return Task::none() };
        v.index = index;
        v.seq = seq;
        v.content = Content::Loading;
        v.top = 0;
        v.left = 0;
        v.hit = None;
        v.note = None;
        load(v.path().to_path_buf(), seq)
    }

    pub(super) fn viewer_loaded(&mut self, seq: u64, result: Result<Loaded, String>) -> Task<Message> {
        let Some(v) = self.viewer.as_mut().filter(|v| v.seq == seq) else { return Task::none() };
        match result {
            Ok(loaded) => {
                if let Loaded::Doc(doc) = &loaded {
                    v.mode = doc.default_mode();
                }
                v.content = Content::Loaded(loaded);
            }
            Err(e) => v.content = Content::Failed(e),
        }
        Task::none()
    }

    pub(super) fn close_viewer(&mut self) {
        let Some(v) = self.viewer.take() else { return };
        // Leave the panel cursor on the file viewed last.
        let path = v.path();
        let panel = self.panel_mut(v.side);
        if path.parent() == Some(panel.path.as_path()) {
            if let Some(name) = path.file_name() {
                panel.focus(&name.to_string_lossy());
            }
        }
        let rows = self.page_rows();
        self.panel_mut(v.side).ensure_visible(rows);
    }

    /// Text rows that fit in the viewer.
    fn viewer_rows(&self) -> usize {
        let search = if self.viewer.as_ref().is_some_and(|v| v.search.is_some()) { SEARCH_H } else { 0.0 };
        ((self.window_height - VIEWER_CHROME_H - search) / LINE_H).floor().max(1.0) as usize
    }

    fn viewer_cols(&self) -> usize {
        ((self.window_width - 24.0) / CHAR_W).floor().max(10.0) as usize
    }

    fn viewer_scroll(&mut self, delta: isize) {
        let rows = self.viewer_rows();
        let Some(v) = &mut self.viewer else { return };
        let total = v.doc().map_or(0, |d| d.rows(v.mode));
        let max = total.saturating_sub(if v.wrap { 1 } else { rows });
        v.top = v.top.saturating_add_signed(delta).min(max);
    }

    pub(super) fn viewer_wheel(&mut self, lines: f32) {
        self.viewer_scroll((-lines * 3.0).round() as isize);
    }

    pub(super) fn viewer_key(&mut self, key: &keyboard::Key, mods: keyboard::Modifiers) -> Task<Message> {
        let named = match key {
            keyboard::Key::Named(n) => Some(*n),
            _ => None,
        };
        let rows = self.viewer_rows() as isize;
        let ctrl = mods.control();
        let Some(v) = &mut self.viewer else { return Task::none() };

        if v.search.is_some() {
            match named {
                Some(Named::Escape) => v.search = None,
                Some(Named::F3) => return self.viewer_find(!mods.shift()),
                _ => {}
            }
            return Task::none();
        }

        let ch = match key {
            keyboard::Key::Character(c) => Some(c.to_lowercase()),
            _ => None,
        };
        if ctrl && ch.as_deref() == Some("f") {
            v.search = Some(v.query.clone());
            return Task::batch([text_input::focus(search_input_id()), text_input::select_all(search_input_id())]);
        }
        let typed = ch.filter(|_| !ctrl && !mods.alt());
        match (named, typed.as_deref()) {
            (Some(Named::Escape), _) | (_, Some("q")) => self.close_viewer(),
            (Some(Named::ArrowDown), _) => self.viewer_scroll(1),
            (Some(Named::ArrowUp), _) => self.viewer_scroll(-1),
            (Some(Named::PageDown), _) => self.viewer_scroll(rows),
            (Some(Named::PageUp), _) => self.viewer_scroll(-rows),
            (Some(Named::Home), _) => {
                v.top = 0;
                v.left = 0;
            }
            (Some(Named::End), _) => self.viewer_scroll(isize::MAX),
            (Some(Named::ArrowRight), _) if !v.wrap => v.left += H_STEP,
            (Some(Named::ArrowLeft), _) if !v.wrap => v.left = v.left.saturating_sub(H_STEP),
            (Some(Named::Space), _) | (_, Some("n")) => {
                if v.index + 1 < v.files.len() {
                    let next = v.index + 1;
                    return self.viewer_show(next);
                }
            }
            (Some(Named::Backspace), _) | (_, Some("p")) => {
                if v.index > 0 {
                    let prev = v.index - 1;
                    return self.viewer_show(prev);
                }
            }
            (_, Some("1")) => self.viewer_mode(Mode::Text),
            (_, Some("3")) => self.viewer_mode(Mode::Hex),
            (_, Some("w")) => {
                v.wrap = !v.wrap;
                v.left = 0;
            }
            (_, Some("c")) => {
                v.case = !v.case;
                v.note = Some((format!("Case-sensitive search {}", if v.case { "on" } else { "off" }), Tone::Info));
            }
            (Some(Named::F7), _) | (_, Some("f" | "/")) => {
                v.search = Some(v.query.clone());
                return Task::batch([text_input::focus(search_input_id()), text_input::select_all(search_input_id())]);
            }
            (Some(Named::F3), _) => return self.viewer_find(!mods.shift()),
            _ => {}
        }
        Task::none()
    }

    fn viewer_mode(&mut self, mode: Mode) {
        let Some(v) = &mut self.viewer else { return };
        let Some(doc) = v.doc() else { return };
        if v.mode == mode {
            return;
        }
        // Keep roughly the same place in the file.
        v.top = match mode {
            Mode::Hex => doc.line_start(v.top) / HEX_WIDTH,
            Mode::Text => doc.line_of(v.top * HEX_WIDTH),
        };
        v.mode = mode;
        v.left = 0;
        v.hit = None;
    }

    pub(super) fn viewer_search_input(&mut self, text: String) {
        if let Some(v) = &mut self.viewer {
            v.search = Some(text);
        }
    }

    /// Enter in the search box: remember the query, close the box, find.
    pub(super) fn viewer_submit(&mut self) -> Task<Message> {
        if let Some(v) = &mut self.viewer {
            if let Some(q) = v.search.take() {
                if q != v.query {
                    v.hit = None;
                }
                v.query = q;
            }
        }
        self.viewer_find(true)
    }

    /// Finds the next (or previous) match of the last query, wrapping
    /// around the file once.
    fn viewer_find(&mut self, forward: bool) -> Task<Message> {
        let (rows, cols) = (self.viewer_rows(), self.viewer_cols());
        let Some(v) = &mut self.viewer else { return Task::none() };
        if v.query.is_empty() {
            v.search = Some(String::new());
            return text_input::focus(search_input_id());
        }
        let Some(doc) = v.doc_arc() else { return Task::none() };
        let row_start = |row: usize| match v.mode {
            Mode::Text => doc.line_start(row),
            Mode::Hex => row * HEX_WIDTH,
        };
        let from = match v.hit.filter(|h| h.mode == v.mode) {
            Some(h) if forward => h.start + 1,
            Some(h) => h.start,
            None => row_start(v.top),
        };
        let mut wrapped = false;
        let hit = doc.find(v.mode, &v.query, from, v.case, forward).or_else(|| {
            wrapped = true;
            doc.find(v.mode, &v.query, if forward { 0 } else { usize::MAX }, v.case, forward)
        });
        let Some(hit) = hit else {
            v.note = Some((format!("\"{}\" not found", v.query), Tone::Error));
            return Task::none();
        };
        let row = match v.mode {
            Mode::Text => doc.line_of(hit.start),
            Mode::Hex => hit.start / HEX_WIDTH,
        };
        if row < v.top || row >= v.top + rows {
            v.top = row.saturating_sub(rows / 3);
        }
        if v.mode == Mode::Text && !v.wrap {
            let line = doc.line(row);
            let col = viewer::column_of(line, hit.start - doc.line_start(row));
            if col < v.left || col + v.query.chars().count() > v.left + cols {
                v.left = col.saturating_sub(8);
            }
        }
        v.hit = Some(hit);
        v.note = wrapped.then(|| ("Search wrapped around".to_string(), Tone::Info));
        Task::none()
    }

    pub(super) fn viewer_view<'a>(&'a self, v: &'a Viewer) -> Element<'a, Message> {
        let name = v.path().file_name().map_or_else(|| v.path().display().to_string(), |n| n.to_string_lossy().into_owned());
        let mut info = vec![format!("{} / {}", v.index + 1, v.files.len())];
        match &v.content {
            Content::Loaded(Loaded::Doc(doc)) => {
                info.push(rc_core::format::size(doc.size));
                info.push(match v.mode {
                    Mode::Text => format!("Text · {}", doc.encoding.label()),
                    Mode::Hex => "Hex".into(),
                });
                if v.wrap && v.mode == Mode::Text {
                    info.push("Wrap".into());
                }
                if doc.truncated {
                    info.push(format!("first {} shown", rc_core::format::size(viewer::LOAD_LIMIT)));
                }
            }
            Content::Loaded(Loaded::Image { width, height, .. }) => info.push(format!("{width} × {height}")),
            _ => {}
        }
        let header = container(
            row![
                text(name).size(14).wrapping(Wrapping::None),
                Space::with_width(Length::Fill),
                text(info.join("   ")).size(12).color(theme::MUTED).wrapping(Wrapping::None),
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .clip(true)
        .style(filled(theme::HEADER));

        let body: Element<'a, Message> = match &v.content {
            Content::Loading => center(text("Loading…").size(13).color(theme::MUTED)).into(),
            Content::Failed(e) => center(text(format!("Can't open: {e}")).size(13).color(theme::ERROR)).into(),
            Content::Loaded(Loaded::Image { handle, .. }) => center(
                image(handle.clone()).content_fit(ContentFit::ScaleDown).filter_method(image::FilterMethod::Linear),
            )
            .padding(8)
            .into(),
            Content::Loaded(Loaded::Doc(doc)) => self.text_body(v, doc),
        };
        let body = mouse_area(container(body).width(Length::Fill).height(Length::Fill).style(filled(theme::PANEL)))
            .on_scroll(|delta| {
                Message::ViewerWheel(match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / LINE_H / 3.0,
                })
            });

        let mut page = column![header, body];
        if let Some(query) = &v.search {
            page = page.push(
                container(
                    row![
                        text("Find:").size(13),
                        text_input("text to find", query)
                            .id(search_input_id())
                            .on_input(Message::ViewerSearch)
                            .on_submit(Message::ViewerFind)
                            .padding(5)
                            .size(13)
                            .style(theme::input),
                        text(if v.case { "Aa (C)" } else { "aa (C)" }).size(12).color(theme::MUTED),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([4, 12])
                .height(SEARCH_H)
                .style(filled(theme::HEADER)),
            );
        }
        let footer: Element<'a, Message> = match &v.note {
            Some((msg, tone)) => text(msg.as_str())
                .size(12)
                .color(if *tone == Tone::Error { theme::ERROR } else { theme::TEXT })
                .into(),
            None => text(
                "Esc close · N/P next/prev file · 1 text · 3 hex · W wrap · F7 find · F3/Shift+F3 next/prev · C match case",
            )
            .size(12)
            .color(theme::MUTED)
            .wrapping(Wrapping::None)
            .into(),
        };
        page = page.push(container(footer).padding([4, 12]).width(Length::Fill).clip(true).style(filled(theme::HEADER)));
        container(page).width(Length::Fill).height(Length::Fill).style(filled(theme::BG)).into()
    }

    fn text_body<'a>(&'a self, v: &'a Viewer, doc: &'a Document) -> Element<'a, Message> {
        let cols = self.viewer_cols();
        responsive(move |size| {
            let rows = (size.height / LINE_H).ceil() as usize;
            let end = (v.top + rows).min(doc.rows(v.mode));
            let lines = (v.top..end).map(|i| {
                let (shown, marks) = match v.mode {
                    Mode::Text => {
                        let line = doc.line(i);
                        let skip = if v.wrap { 0 } else { v.left };
                        // Wrapped lines are shown whole (up to a sane length).
                        let max = if v.wrap { 20_000 } else { cols + 1 };
                        let shown = viewer::display_line(line, skip, max);
                        let marks = v.hit.filter(|h| h.mode == Mode::Text && doc.line_of(h.start) == i).map(|h| {
                            let start = doc.line_start(i);
                            let a = viewer::column_of(line, h.start - start);
                            let b = viewer::column_of(line, h.end - start);
                            vec![(a.saturating_sub(skip), b.saturating_sub(skip))]
                        });
                        (shown, marks.unwrap_or_default())
                    }
                    Mode::Hex => (doc.hex_row(i), hex_marks(v.hit, i)),
                };
                let line: Element<'a, Message> = if marks.is_empty() {
                    text(if shown.is_empty() { " ".to_string() } else { shown })
                        .font(Font::MONOSPACE)
                        .size(FONT_SIZE)
                        .line_height(iced::Pixels(LINE_H))
                        .wrapping(if v.wrap { Wrapping::WordOrGlyph } else { Wrapping::None })
                        .into()
                } else {
                    rich_text(highlight(&shown, &marks))
                        .font(Font::MONOSPACE)
                        .size(FONT_SIZE)
                        .line_height(iced::Pixels(LINE_H))
                        .wrapping(if v.wrap { Wrapping::WordOrGlyph } else { Wrapping::None })
                        .into()
                };
                line
            });
            container(Column::with_children(lines).width(Length::Fill))
                .padding([0, 12])
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true)
                .into()
        })
        .into()
    }
}

/// Character ranges of hex row `row` covered by `hit`: the hex digits and
/// the ASCII column.
fn hex_marks(hit: Option<Hit>, row: usize) -> Vec<(usize, usize)> {
    let Some(hit) = hit.filter(|h| h.mode == Mode::Hex) else { return Vec::new() };
    let row_start = row * HEX_WIDTH;
    let (a, b) = (hit.start.max(row_start), hit.end.min(row_start + HEX_WIDTH));
    if a >= b {
        return Vec::new();
    }
    let hex_col = |k: usize| 10 + 3 * k + usize::from(k >= HEX_WIDTH / 2);
    let (ka, kb) = (a - row_start, b - row_start);
    let ascii = 10 + 3 * HEX_WIDTH + 2;
    vec![(hex_col(ka), hex_col(kb - 1) + 2), (ascii + ka, ascii + kb)]
}

/// Splits `s` into spans, the character ranges in `marks` highlighted.
fn highlight<'a>(s: &str, marks: &[(usize, usize)]) -> Vec<Span<'a, Message, Font>> {
    let chars: Vec<char> = s.chars().collect();
    let mut spans = Vec::new();
    let mut pos = 0;
    for &(a, b) in marks {
        let (a, b) = (a.min(chars.len()).max(pos), b.min(chars.len()));
        if a >= b {
            continue;
        }
        if a > pos {
            spans.push(Span::new(chars[pos..a].iter().collect::<String>()));
        }
        spans.push(
            Span::new(chars[a..b].iter().collect::<String>())
                .color(theme::BG)
                .background(theme::SELECTED),
        );
        pos = b;
    }
    if pos < chars.len() || spans.is_empty() {
        spans.push(Span::new(chars[pos..].iter().collect::<String>()));
    }
    spans
}
