//! Built-in file viewer (F3, TC's Lister): text with optional word wrap,
//! hex dump, images. Fills the whole window while open; N/P step through
//! the files of the panel it was opened from.

use super::*;
use crate::theme::filled;
use iced::widget::text::{Span, Wrapping};
use iced::widget::scrollable::{self, AbsoluteOffset, Direction, RelativeOffset};
use iced::{Point, Size};
use super::markdown::{self, Block};
use iced::widget::{center, column, container, lazy, mouse_area, responsive, rich_text, row, text, Column, Space};
use iced::{Alignment, ContentFit, Element, Font, Length};
use rc_core::viewer::{self, Document, Hit, Mode, Part, Selection, COPY_LIMIT, HEX_WIDTH, WINDOW};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const LINE_H: f32 = 18.0;
const FONT_SIZE: f32 = 13.0;
/// Width of a monospace character at `FONT_SIZE`, measured once with the
/// font the system resolves `Font::MONOSPACE` to (Cascadia Mono, Consolas,
/// Courier New … differ), so columns and mouse positions match the text.
fn char_w() -> f32 {
    use iced::advanced::text::Paragraph as _;
    static W: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *W.get_or_init(|| {
        const N: usize = 100;
        let sample = "0".repeat(N);
        let paragraph = iced::advanced::graphics::text::Paragraph::with_text(iced::advanced::Text {
            content: sample.as_str(),
            bounds: Size::INFINITY,
            size: iced::Pixels(FONT_SIZE),
            line_height: iced::widget::text::LineHeight::Absolute(iced::Pixels(LINE_H)),
            font: Font::MONOSPACE,
            horizontal_alignment: iced::alignment::Horizontal::Left,
            vertical_alignment: iced::alignment::Vertical::Top,
            shaping: iced::widget::text::Shaping::Basic,
            wrapping: Wrapping::None,
        });
        let w = paragraph.min_width() / N as f32;
        if w.is_finite() && w > 1.0 { w } else { 7.8 }
    })
}
/// Left padding of the text area.
const TEXT_PAD: f32 = 12.0;
/// Columns a wrapped line is laid out to at most.
const WRAP_MAX: usize = 20_000;
/// Header, footer and their padding around the text area.
const VIEWER_CHROME_H: f32 = 64.0;
const SEARCH_H: f32 = 40.0;
/// Columns Left/Right scroll by.
const H_STEP: usize = 4;
/// Images are scaled down to fit this many pixels per side (GPU texture limit).
const MAX_IMAGE_SIDE: u32 = 8192;
/// Zoom factor per +/- press, and the zoom range.
const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 32.0;
/// Room the image scrollbars take at the view's right and bottom edges
/// (a press there doesn't start a pan).
const IMAGE_BAR: f32 = scrollbar::WIDTH + 2.0;
/// Longest prefix of a line measured when clamping horizontal scroll.
const MEASURE_MAX: usize = 100_000;

/// Which image `viewer_step_image` goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Prev,
    Next,
    First,
    Last,
}

/// The file's extension names an image format (`load_sync` tries to decode it).
fn is_image_name(path: &Path) -> bool {
    ::image::ImageFormat::from_path(path).is_ok()
}

fn image_scroll_id() -> scrollable::Id {
    scrollable::Id::new("viewer-image")
}

fn markdown_scroll_id() -> scrollable::Id {
    scrollable::Id::new("viewer-markdown")
}

/// Pixels Up/Down scroll the rendered Markdown by.
const MD_STEP: f32 = 40.0;

fn search_input_id() -> text_input::Id {
    text_input::Id::new("viewer-search")
}

/// A match found in a large file: the window holding it, the match in
/// that window, and whether the search wrapped around.
pub type Found = (Arc<Document>, Hit, bool);

/// The parsed blocks of a Markdown file, if it is rendered.
pub type Rendered = Option<Arc<[Block]>>;

/// What a file was loaded as.
#[derive(Debug, Clone)]
pub enum Loaded {
    Doc(Arc<Document>),
    Image { handle: image::Handle, width: u32, height: u32 },
}

#[derive(Debug, Clone)]
pub enum ViewerMsg {
    Loaded(u64, Result<Loaded, String>, Rendered),
    /// Search result in a large file: the window holding the match, the
    /// match, and whether the search wrapped around.
    Found(u64, Result<Option<Found>, String>, Arc<AtomicBool>),
    Wheel(f32),
    /// Viewer scrollbar: the row (whole file) or byte (large file) to show at the top.
    Seek(f32),
    /// The zoomed image was scrolled (offset in pixels).
    Panned(AbsoluteOffset),
    /// Pointer moved over (or left) the viewer body.
    Pointer(Option<Point>),
    /// Wheel anywhere while an image is shown (zooms when Ctrl is held).
    ZoomWheel(f32),
    Search(String),
    Find,
    /// Left button pressed while the viewer shows a document (starts a selection over the body).
    Press,
    /// A link in rendered Markdown was clicked (its target).
    Link(String),
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
    /// Parsed Markdown of the shown file, and whether it is shown rendered
    /// (key 5) instead of `mode`.
    md: Rendered,
    rendered: bool,
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
    /// Image scale; `None` fits the window (never enlarging).
    zoom: Option<f32>,
    /// Scroll position of a zoomed image, in pixels.
    offset: AbsoluteOffset,
    /// Mouse pointer over the image area (relative to it); zoom anchor.
    pointer: Option<Point>,
    /// Stops the running search of a large file.
    cancel: Arc<AtomicBool>,
    /// Mouse selection, by file offsets; `selecting` while the left button drags it.
    sel: Option<Selection>,
    selecting: bool,
    /// Window position of the pointer while the left button drags (pans) a
    /// zoomed image.
    pan: Option<Point>,
    /// Window position of the body's top-left corner (from pointer moves),
    /// to place the pointer while a selection drag leaves the body.
    origin: Point,
}

impl Viewer {
    fn path(&self) -> &Path {
        &self.files[self.index]
    }

    /// Where the shown file's bytes are read from: the file itself, or the
    /// temp copy of a file inside an archive (made by `load_sync`).
    fn source(&self) -> PathBuf {
        rc_core::vfs::temp_path(self.path())
    }

    fn doc_arc(&self) -> Option<Arc<Document>> {
        match &self.content {
            Content::Loaded(Loaded::Doc(doc)) => Some(doc.clone()),
            _ => None,
        }
    }

    pub(super) fn is_image(&self) -> bool {
        self.image_size().is_some()
    }

    fn image_size(&self) -> Option<(u32, u32)> {
        match &self.content {
            Content::Loaded(Loaded::Image { width, height, .. }) => Some((*width, *height)),
            _ => None,
        }
    }

    fn doc(&self) -> Option<&Document> {
        match &self.content {
            Content::Loaded(Loaded::Doc(doc)) => Some(doc),
            _ => None,
        }
    }

    /// Stops a running search, if any.
    fn cancel_search(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Replaces the window of a large file with one whose text starts at
    /// or after `want`; the row holding file offset `keep` becomes the top
    /// row and the hit carries over.
    fn load_window(&mut self, want: u64, keep: u64) {
        let Some(old) = self.doc_arc() else { return };
        match Document::open_window(&self.source(), want, Some(old.encoding)) {
            Ok(doc) => {
                let hit = self.hit.and_then(|h| move_hit(&old, &doc, h));
                self.top = doc.row_of(self.mode, keep);
                self.hit = hit;
                self.content = Content::Loaded(Loaded::Doc(Arc::new(doc)));
            }
            Err(e) => self.note = Some((format!("Can't read: {e}"), Tone::Error)),
        }
    }
}

/// `hit` of `from` as a hit in `to`, if that window holds it.
fn move_hit(from: &Document, to: &Document, hit: Hit) -> Option<Hit> {
    match hit.mode {
        Mode::Text => {
            let (a, b) = (from.byte_of(hit.start), from.byte_of(hit.end));
            let (start, end) = to.text_span();
            (a >= start && b <= end).then(|| Hit { start: to.text_of(a), end: to.text_of(b), ..hit })
        }
        Mode::Hex => {
            let (a, b) = (from.base() + hit.start as u64, from.base() + hit.end as u64);
            (a >= to.base() && b <= to.end())
                .then(|| Hit { start: (a - to.base()) as usize, end: (b - to.base()) as usize, ..hit })
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
                let result = load_sync(&path);
                let md = match &result {
                    Ok(Loaded::Doc(doc)) if renders_markdown(&path, doc) => Some(markdown::parse(doc.text()).into()),
                    _ => None,
                };
                let _ = tx.send((result, md));
            });
            rx.await.unwrap_or_else(|_| (Err("viewer thread failed".into()), None))
        },
        move |(result, md)| Message::Viewer(ViewerMsg::Loaded(seq, result, md)),
    )
}

/// `doc` (read from `path`) is a Markdown file small enough to render.
fn renders_markdown(path: &Path, doc: &Document) -> bool {
    markdown::is_markdown(path) && !doc.binary && doc.is_whole() && doc.size <= markdown::MD_LIMIT
}

fn load_sync(path: &Path) -> Result<Loaded, String> {
    // A file inside an archive is viewed from a temp copy.
    let local = rc_core::vfs::extract_temp(path).map_err(|e| e.to_string())?;
    let path = local.as_path();
    if is_image_name(path) {
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
    pub(super) fn viewer_update(&mut self, msg: ViewerMsg) -> Task<Message> {
        match msg {
            ViewerMsg::Loaded(seq, result, md) => return self.viewer_loaded(seq, result, md),
            ViewerMsg::Found(seq, result, cancel) => self.viewer_found(seq, result, &cancel),
            ViewerMsg::Wheel(lines) => self.viewer_wheel(lines),
            ViewerMsg::Seek(at) => self.viewer_seek(at),
            ViewerMsg::Panned(offset) => self.viewer_panned(offset),
            ViewerMsg::Pointer(at) => self.viewer_pointer(at),
            ViewerMsg::ZoomWheel(lines) => return self.viewer_zoom_wheel(lines),
            ViewerMsg::Search(text) => self.viewer_search_input(text),
            ViewerMsg::Find => return self.viewer_submit(),
            ViewerMsg::Press => self.viewer_press(),
            ViewerMsg::Link(target) => return self.viewer_link(&target),
        }
        Task::none()
    }

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
        self.screen = Some(Screen::Viewer(Box::new(Viewer {
            files,
            index,
            side,
            seq: 0,
            content: Content::Loading,
            mode: Mode::Text,
            md: None,
            rendered: false,
            wrap: false,
            top: 0,
            left: 0,
            search: None,
            query: String::new(),
            case: false,
            hit: None,
            note: None,
            zoom: None,
            offset: AbsoluteOffset { x: 0.0, y: 0.0 },
            pointer: None,
            cancel: Arc::new(AtomicBool::new(false)),
            sel: None,
            selecting: false,
            pan: None,
            origin: Point::ORIGIN,
        })));
        self.viewer_show(index)
    }

    fn viewer_show(&mut self, index: usize) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        v.cancel_search();
        v.index = index;
        v.seq = seq;
        v.content = Content::Loading;
        v.md = None;
        v.rendered = false;
        v.top = 0;
        v.left = 0;
        v.hit = None;
        v.sel = None;
        v.selecting = false;
        v.pan = None;
        v.note = None;
        v.zoom = None;
        v.offset = AbsoluteOffset { x: 0.0, y: 0.0 };
        load(v.path().to_path_buf(), seq)
    }

    fn viewer_loaded(&mut self, seq: u64, result: Result<Loaded, String>, md: Rendered) -> Task<Message> {
        let Some(v) = self.screen.viewer_mut().filter(|v| v.seq == seq) else { return Task::none() };
        match result {
            Ok(loaded) => {
                if let Loaded::Doc(doc) = &loaded {
                    v.mode = doc.default_mode();
                    if md.is_none() && !doc.binary && markdown::is_markdown(v.path()) {
                        v.note = Some(("Markdown file over 2 MB: shown as text".into(), Tone::Info));
                    }
                }
                v.content = Content::Loaded(loaded);
            }
            Err(e) => v.content = Content::Failed(e),
        }
        // Markdown files open rendered.
        v.rendered = md.is_some();
        v.md = md;
        if v.rendered {
            return scrollable::snap_to(markdown_scroll_id(), RelativeOffset::START);
        }
        Task::none()
    }

    /// Key 5: the rendered view of a Markdown file.
    fn viewer_markdown(&mut self) {
        let Some(v) = self.screen.viewer_mut() else { return };
        if v.md.is_some() {
            v.rendered = true;
            v.sel = None;
            v.selecting = false;
            v.hit = None;
        } else if v.doc().is_some_and(|d| !d.binary) && markdown::is_markdown(v.path()) {
            v.note = Some(("Markdown file over 2 MB: only the text view".into(), Tone::Info));
        }
    }

    /// Keys of the rendered Markdown view; `None` leaves the key to the
    /// text view's handling (after switching to it for find).
    fn viewer_markdown_key(
        &mut self,
        named: Option<Named>,
        ch: Option<&str>,
        mods: keyboard::Modifiers,
    ) -> Option<Task<Message>> {
        let page = (self.viewer_body_size().height - MD_STEP).max(MD_STEP);
        let v = self.screen.viewer_mut()?;
        let ctrl = mods.control();
        let typed = ch.filter(|_| !ctrl && !mods.alt());
        let by = |dy: f32| scrollable::scroll_by(markdown_scroll_id(), AbsoluteOffset { x: 0.0, y: dy });
        Some(match (named, typed) {
            _ if ctrl && (matches!(ch, Some("c" | "a")) || named == Some(Named::Insert)) => {
                v.note = Some(("Copy works in the source view: press 1, select, then Ctrl+C".into(), Tone::Info));
                Task::none()
            }
            // Find searches the source: switch to it and let the text view
            // open the find box (or find the next match).
            _ if ctrl && ch == Some("f") => {
                v.rendered = false;
                return None;
            }
            (Some(Named::F7 | Named::F3), _) | (_, Some("f" | "/")) => {
                v.rendered = false;
                v.note = Some(("Find works in the source view (5 shows the rendered view again)".into(), Tone::Info));
                return None;
            }
            (Some(Named::ArrowDown), _) => by(MD_STEP),
            (Some(Named::ArrowUp), _) => by(-MD_STEP),
            (Some(Named::PageDown), _) => by(page),
            (Some(Named::PageUp), _) => by(-page),
            (Some(Named::Home), _) => scrollable::snap_to(markdown_scroll_id(), RelativeOffset::START),
            (Some(Named::End), _) => scrollable::snap_to(markdown_scroll_id(), RelativeOffset::END),
            (Some(Named::ArrowLeft | Named::ArrowRight), _) => Task::none(),
            _ => return None,
        })
    }

    /// A clicked Markdown link: web links open in the browser, relative
    /// links to other Markdown files open in the viewer.
    fn viewer_link(&mut self, target: &str) -> Task<Message> {
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        let lower = target.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            v.note = Some(match rc_platform::open_default(Path::new(target)) {
                Ok(()) => (format!("Opened {target}"), Tone::Info),
                Err(e) => (format!("Can't open {target}: {e}"), Tone::Error),
            });
            return Task::none();
        }
        let Some(path) = link_path(v.path(), target) else {
            v.note = Some((format!("Link not followed: {target}"), Tone::Info));
            return Task::none();
        };
        if !markdown::is_markdown(&path) || !path.is_file() {
            v.note = Some((format!("Only links to other Markdown files open here: {target}"), Tone::Info));
            return Task::none();
        }
        let index = match v.files.iter().position(|p| *p == path) {
            Some(i) => i,
            None => {
                v.files.insert(v.index + 1, path);
                v.index + 1
            }
        };
        self.viewer_show(index)
    }

    pub(super) fn close_viewer(&mut self) {
        let Some(mut v) = self.screen.take_viewer() else { return };
        v.cancel_search();
        // Leave the panel cursor on the file viewed last.
        let path = v.path();
        let panel = self.panel_mut(v.side);
        // (By path: search results name entries by their relative path.)
        if let Some(name) = panel.entries.iter().find(|e| e.path == path).map(|e| e.name.clone()) {
            panel.focus(&name);
        }
        let rows = self.page_rows();
        self.panel_mut(v.side).ensure_visible(rows);
    }

    /// Text rows that fit in the viewer.
    fn viewer_rows(&self) -> usize {
        let search = if self.screen.viewer().is_some_and(|v| v.search.is_some()) { SEARCH_H } else { 0.0 };
        ((self.window_height - VIEWER_CHROME_H - search) / LINE_H).floor().max(1.0) as usize
    }

    fn viewer_cols(&self) -> usize {
        ((self.window_width - 24.0) / char_w()).floor().max(10.0) as usize
    }

    fn viewer_scroll(&mut self, delta: isize) {
        let rows = self.viewer_rows();
        let Some(v) = self.screen.viewer_mut() else { return };
        let total = v.doc().map_or(0, |d| d.rows(v.mode));
        let max = total.saturating_sub(if v.wrap { 1 } else { rows });
        v.top = v.top.saturating_add_signed(delta).min(max);
        self.viewer_slide();
    }

    /// In a large file, moves the window along when the view nears its
    /// edge, centring it on the top row.
    fn viewer_slide(&mut self) {
        let rows = self.viewer_rows();
        let Some(v) = self.screen.viewer_mut() else { return };
        let Some(doc) = v.doc_arc() else { return };
        let margin = rows * 2;
        let near_start = v.top < margin && doc.has_before();
        let near_end = v.top + rows + margin > doc.rows(v.mode) && doc.has_after();
        if near_start || near_end {
            let keep = doc.row_byte(v.mode, v.top);
            v.load_window(keep.saturating_sub(WINDOW / 2), keep);
        }
    }

    /// Home / End: the first or last rows of the file.
    fn viewer_home_end(&mut self, end: bool) {
        let Some(v) = self.screen.viewer_mut() else { return };
        let Some(doc) = v.doc_arc() else { return };
        v.left = 0;
        if end {
            if doc.has_after() {
                let want = doc.size.saturating_sub(WINDOW - 64 * 1024);
                v.load_window(want, want);
            }
            self.viewer_scroll(isize::MAX);
        } else {
            if doc.has_before() {
                v.load_window(0, 0);
            }
            v.top = 0;
        }
    }

    /// Furthest `left` that still shows the end of the longest visible line.
    fn viewer_max_left(&self) -> usize {
        let Some(v) = self.screen.viewer() else { return 0 };
        let Some(doc) = v.doc() else { return 0 };
        if v.mode != Mode::Text || v.wrap {
            return 0;
        }
        let end = (v.top + self.viewer_rows()).min(doc.rows(v.mode));
        let longest = (v.top..end)
            .map(|i| viewer::display_line(doc.line(i), 0, MEASURE_MAX).chars().count())
            .max()
            .unwrap_or(0);
        (longest + 1).saturating_sub(self.viewer_cols())
    }

    /// Scale that fits an image of `size` into the viewer, never enlarging.
    fn viewer_fit_scale(&self, (w, h): (u32, u32)) -> f32 {
        let avail_w = (self.window_width - 16.0).max(1.0);
        let avail_h = (self.window_height - VIEWER_CHROME_H - 16.0).max(1.0);
        (avail_w / w.max(1) as f32).min(avail_h / h.max(1) as f32).min(1.0)
    }

    /// Keys of `viewer_key` / `viewer_image_key`, for the F1 help. Keep in sync.
    pub(super) const VIEWER_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Esc, Q", "Close the viewer (Esc first closes the find box, then clears a selection)"),
        ("Up, Down, PgUp, PgDn", "Scroll"),
        ("Home, End", "Start / end of the file"),
        ("Left, Right", "Scroll sideways (wrap off)"),
        ("N, Space", "Next file in the panel"),
        ("P, Backspace", "Previous file in the panel"),
        ("1", "Text mode (images: actual size)"),
        ("3", "Hex mode"),
        ("5", "Markdown files: rendered view (the default for .md; 1 shows the source, where copy and find work)"),
        ("W", "Wrap long lines on / off"),
        ("F7, F, /, Ctrl+F", "Find text"),
        ("F3, Shift+F3", "Find next / previous"),
        ("C", "Case-sensitive find on / off"),
        ("Mouse drag, Shift+click", "Select text (hex mode: in the hex or the character column)"),
        ("Ctrl+C, Ctrl+Insert", "Copy the selection (hex column as hex digits; at most 16 MB)"),
        ("Ctrl+A", "Select the whole file"),
        ("+, -, Ctrl+wheel", "Zoom an image in / out"),
        ("0, *", "Fit the image to the window"),
        ("Mouse drag", "Pan a zoomed image"),
        ("Left/Up/PgUp, Right/Down/PgDn", "Images: previous / next image in the panel"),
        ("Home, End", "Images: first / last image in the panel"),
    ];

    /// Image-only keys: zoom, and stepping through the panel's images.
    /// `None` when the key is not one of them, or no image is shown (or
    /// loading).
    fn viewer_image_key(&mut self, named: Option<Named>, typed: Option<&str>) -> Option<Task<Message>> {
        let v = self.screen.viewer()?;
        let loading_image = matches!(v.content, Content::Loading) && is_image_name(v.path());
        if !v.is_image() && !loading_image {
            return None;
        }
        Some(match (named, typed) {
            (_, Some("+" | "=")) => self.viewer_set_zoom(|z| Some(z * ZOOM_STEP)),
            (_, Some("-")) => self.viewer_set_zoom(|z| Some(z / ZOOM_STEP)),
            (_, Some("0" | "*")) => self.viewer_set_zoom(|_| None),
            (_, Some("1")) => self.viewer_set_zoom(|_| Some(1.0)),
            (Some(Named::ArrowLeft | Named::ArrowUp | Named::PageUp), _) => self.viewer_step_image(Step::Prev),
            (Some(Named::ArrowRight | Named::ArrowDown | Named::PageDown), _) => self.viewer_step_image(Step::Next),
            (Some(Named::Home), _) => self.viewer_step_image(Step::First),
            (Some(Named::End), _) => self.viewer_step_image(Step::Last),
            _ => return None,
        })
    }

    /// Shows another image file of the viewer's list (files named as
    /// images; one that doesn't decode opens as text). N/P step through
    /// all files instead.
    fn viewer_step_image(&mut self, step: Step) -> Task<Message> {
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        let (i, n) = (v.index, v.files.len());
        let mut range: Box<dyn Iterator<Item = usize>> = match step {
            Step::Prev => Box::new((0..i).rev()),
            Step::Next => Box::new(i + 1..n),
            Step::First => Box::new(0..n),
            Step::Last => Box::new((0..n).rev()),
        };
        let found = range.find(|&k| is_image_name(&v.files[k]));
        match found {
            Some(index) if index != i => self.viewer_show(index),
            Some(_) => Task::none(),
            None => {
                let msg = if step == Step::Prev { "No previous image in the panel" } else { "No next image in the panel" };
                v.note = Some((msg.into(), Tone::Info));
                Task::none()
            }
        }
    }

    fn viewer_panned(&mut self, offset: AbsoluteOffset) {
        // Ctrl+wheel zooms; the image scrollable also scrolls on that wheel
        // event, which `viewer_set_zoom` overrides with its own offset.
        if self.modifiers.control() {
            return;
        }
        if let Some(v) = self.screen.viewer_mut() {
            v.offset = offset;
        }
    }

    fn viewer_pointer(&mut self, at: Option<Point>) {
        let Some(v) = self.screen.viewer_mut() else { return };
        v.pointer = at;
        if let Some(p) = at.filter(|_| v.selecting) {
            self.viewer_drag_to(p);
        }
    }

    /// The viewer wants window-wide pointer moves and left presses: a
    /// document is shown (to learn the body's position for selection drags
    /// past its edges), or a zoomed image (drag panning).
    pub(super) fn viewer_tracks_mouse(&self) -> bool {
        self.screen.viewer().is_some_and(|v| (v.doc().is_some() && !v.rendered) || (v.is_image() && v.zoom.is_some()))
    }

    /// A zoomed image larger than the view on some axis, so dragging pans it.
    fn viewer_can_pan(&self) -> bool {
        let Some(v) = self.screen.viewer() else { return false };
        let (Some((w, h)), Some(zoom)) = (v.image_size(), v.zoom) else { return false };
        let view = self.viewer_body_size();
        w as f32 * zoom > view.width || h as f32 * zoom > view.height
    }

    /// A window-wide pointer move (`self.mouse`): pans a dragged image; over
    /// the body it fixes the body's origin; outside it, it carries a
    /// selection drag on.
    pub(super) fn viewer_mouse_moved(&mut self) -> Task<Message> {
        let mouse = self.mouse;
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        if let Some(last) = v.pan {
            v.pan = Some(mouse);
            // The scrollable clamps, and reports the offset it ends up at
            // through `ViewerMsg::Panned`.
            let by = AbsoluteOffset { x: last.x - mouse.x, y: last.y - mouse.y };
            return scrollable::scroll_by(image_scroll_id(), by);
        }
        match v.pointer {
            Some(p) => v.origin = Point::new(mouse.x - p.x, mouse.y - p.y),
            None if v.selecting => {
                let p = Point::new(mouse.x - v.origin.x, mouse.y - v.origin.y);
                self.viewer_drag_to(p);
            }
            None => {}
        }
        Task::none()
    }

    /// Left button pressed (anywhere; acts over the body): starts a
    /// selection there, or with Shift extends the current one; on a zoomed
    /// image, starts panning it.
    fn viewer_press(&mut self) {
        let shift = self.modifiers.shift();
        let can_pan = self.viewer_can_pan();
        let view = self.viewer_body_size();
        let mouse = self.mouse;
        let Some(v) = self.screen.viewer_mut() else { return };
        if v.is_image() {
            // Not over the body, or on its scrollbars.
            let on_image = v.pointer.is_some_and(|p| p.x < view.width - IMAGE_BAR && p.y < view.height - IMAGE_BAR);
            if can_pan && on_image {
                v.pan = Some(mouse);
            }
            return;
        }
        // Not over the body, or on the scrollbar in its right padding.
        let Some(p) = v.pointer.filter(|p| p.x < self.window_width - TEXT_PAD) else { return };
        let keep = v.sel.filter(|_| shift);
        let Some((part, at)) = self.viewer_pos_at(p, keep.map(|s| s.part)) else { return };
        let Some(v) = self.screen.viewer_mut() else { return };
        v.sel = Some(match keep {
            Some(s) => Selection { head: at, ..s },
            None => Selection::new(part, at),
        });
        v.selecting = true;
        v.note = None;
    }

    /// Left button released anywhere: the drag ends; a plain click leaves no selection.
    pub(super) fn viewer_release(&mut self) {
        if let Some(v) = self.screen.viewer_mut() {
            v.selecting = false;
            v.pan = None;
            if v.sel.is_some_and(|s| s.is_empty()) {
                v.sel = None;
            }
        }
    }

    /// Moves the selection's head to body position `p`; above or below the
    /// body it first scrolls a row that way.
    fn viewer_drag_to(&mut self, p: Point) {
        let height = self.viewer_body_size().height;
        let Some(part) = self.screen.viewer().filter(|v| v.selecting).and_then(|v| v.sel).map(|s| s.part) else {
            return;
        };
        if p.y < 0.0 {
            self.viewer_scroll(-1);
        } else if p.y >= height {
            self.viewer_scroll(1);
        }
        let p = Point::new(p.x, p.y.clamp(0.0, (height - 1.0).max(0.0)));
        let Some((_, at)) = self.viewer_pos_at(p, Some(part)) else { return };
        if let Some(s) = self.screen.viewer_mut().and_then(|v| v.sel.as_mut()) {
            s.head = at;
        }
    }

    /// File offset at body position `p` (and the part of a hex row it is
    /// in, unless `part` fixes it): the character boundary nearest to it.
    fn viewer_pos_at(&self, p: Point, part: Option<Part>) -> Option<(Part, u64)> {
        let v = self.screen.viewer()?;
        let doc = v.doc()?;
        let x = (p.x - TEXT_PAD) / char_w();
        let r = (p.y / LINE_H).floor().max(0.0) as usize;
        match v.mode {
            Mode::Text => {
                let rows = text_rows(doc, v, self.viewer_cols(), r + 1);
                // Below the last line: the end of the text.
                let Some(row) = rows.get(r) else { return Some((Part::Text, doc.byte_of(usize::MAX))) };
                let mut col = row.col + x.round().max(0.0) as usize;
                if !row.last {
                    col = col.min(row.col + row.shown.chars().count());
                }
                let t = doc.line_start(row.line) + viewer::offset_at_column(doc.line(row.line), col);
                Some((Part::Text, doc.byte_of(t)))
            }
            Mode::Hex => {
                let row = v.top + r;
                if row >= doc.rows(Mode::Hex) {
                    return Some((part.unwrap_or(Part::Hex), doc.end()));
                }
                let off = doc.row_byte(Mode::Hex, row);
                let len = (doc.end() - off).min(HEX_WIDTH as u64) as usize;
                let part = part.unwrap_or_else(|| viewer::hex_part(off, x));
                Some((part, off + viewer::hex_boundary(part, off, len, x) as u64))
            }
        }
    }

    /// Ctrl+A: the whole file (copying takes at most `COPY_LIMIT` of it).
    fn viewer_select_all(&mut self) {
        let Some(v) = self.screen.viewer_mut() else { return };
        let Some(size) = v.doc().map(|d| d.size) else { return };
        let part = if v.mode == Mode::Text { Part::Text } else { Part::Hex };
        v.sel = Some(Selection { part, anchor: 0, head: size });
        v.note = (size > COPY_LIMIT).then(|| {
            let limit = rc_core::format::size(COPY_LIMIT);
            (format!("Selected all; Ctrl+C copies the first {limit} (copy limit)"), Tone::Info)
        });
    }

    /// Ctrl+C / Ctrl+Insert: the selection to the clipboard, at most
    /// `COPY_LIMIT` bytes of it (read from the file when outside the window).
    fn viewer_copy(&mut self) -> Task<Message> {
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        let Some(doc) = v.doc_arc() else { return Task::none() };
        let Some(sel) = v.sel.filter(|s| !s.is_empty()) else {
            v.note = Some(("Nothing selected: drag with the mouse or press Ctrl+A".into(), Tone::Info));
            return Task::none();
        };
        let (start, end) = sel.range();
        let end = end.min(doc.size);
        let cut = end.min(start + COPY_LIMIT);
        let bytes = match doc.slice(start, cut) {
            Some(b) => std::borrow::Cow::Borrowed(b),
            None => match viewer::read_range(&v.source(), start, cut) {
                Ok(b) => std::borrow::Cow::Owned(b),
                Err(e) => {
                    v.note = Some((format!("Can't read: {e}"), Tone::Error));
                    return Task::none();
                }
            },
        };
        let text = viewer::selection_text(sel.part, start, &bytes, doc.encoding);
        let size = rc_core::format::size;
        let how = match sel.part {
            Part::Text => "",
            Part::Hex => " as hex",
            Part::Ascii => " as characters",
        };
        v.note = Some((
            if cut < end {
                format!("Copied the first {} of {} selected{how} (copy limit)", size(cut - start), size(end - start))
            } else {
                format!("Copied {}{how}", size(cut - start))
            },
            Tone::Info,
        ));
        iced::clipboard::write(text)
    }

    /// Ctrl+wheel over an image: one zoom step per notch, around the pointer.
    fn viewer_zoom_wheel(&mut self, notches: f32) -> Task<Message> {
        if !self.modifiers.control() || notches == 0.0 {
            return Task::none();
        }
        let anchor = self.screen.viewer().and_then(|v| v.pointer);
        self.viewer_zoom_at(anchor, |z| Some(z * ZOOM_STEP.powf(notches)))
    }

    /// Sets the image zoom from the current scale, keeping the view centre in place.
    fn viewer_set_zoom(&mut self, zoom: impl FnOnce(f32) -> Option<f32>) -> Task<Message> {
        self.viewer_zoom_at(None, zoom)
    }

    /// Sets the image zoom from the current scale (the fit scale when
    /// fitted); `None` = fit. The image point under `anchor` (view
    /// coordinates; `None` = the view centre) stays where it is.
    /// No-op unless an image is shown.
    fn viewer_zoom_at(&mut self, anchor: Option<Point>, zoom: impl FnOnce(f32) -> Option<f32>) -> Task<Message> {
        let Some((w, h)) = self.screen.viewer().and_then(Viewer::image_size) else {
            return Task::none();
        };
        let fit = self.viewer_fit_scale((w, h));
        let view = self.viewer_body_size();
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        let old = v.zoom.unwrap_or(fit);
        // In fit mode nothing is scrolled.
        let offset = if v.zoom.is_some() { v.offset } else { AbsoluteOffset { x: 0.0, y: 0.0 } };
        v.zoom = zoom(old).map(|z| z.clamp(ZOOM_MIN, ZOOM_MAX));
        let Some(new) = v.zoom else { return Task::none() };
        let p = anchor.unwrap_or(Point::new(view.width / 2.0, view.height / 2.0));
        v.offset = AbsoluteOffset {
            x: zoom_offset(w as f32, view.width, old, new, offset.x, p.x),
            y: zoom_offset(h as f32, view.height, old, new, offset.y, p.y),
        };
        scrollable::scroll_to(image_scroll_id(), v.offset)
    }

    /// Size of the area between the header and the footer / find box.
    fn viewer_body_size(&self) -> Size {
        let search = if self.screen.viewer().is_some_and(|v| v.search.is_some()) { SEARCH_H } else { 0.0 };
        Size::new(self.window_width.max(1.0), (self.window_height - VIEWER_CHROME_H - search).max(1.0))
    }

    /// Scrollbar: `at` is a row of a whole file, else a byte of the file;
    /// infinite = the end.
    fn viewer_seek(&mut self, at: f32) {
        let Some(v) = self.screen.viewer_mut() else { return };
        let Some(doc) = v.doc_arc() else { return };
        if at.is_infinite() {
            // Like End, keeping the horizontal scroll.
            if doc.has_after() {
                let want = doc.size.saturating_sub(WINDOW - 64 * 1024);
                v.load_window(want, want);
            }
            return self.viewer_scroll(isize::MAX);
        }
        if doc.is_whole() {
            v.top = at.round() as usize;
        } else {
            let byte = (at.max(0.0) as u64).min(doc.size);
            if (doc.base()..doc.end()).contains(&byte) {
                v.top = doc.row_of(v.mode, byte);
            } else {
                v.load_window(byte.saturating_sub(WINDOW / 2), byte);
            }
        }
        // Clamps to the last page and slides the window near its edges.
        self.viewer_scroll(0);
    }

    fn viewer_wheel(&mut self, lines: f32) {
        // The rendered view scrolls itself.
        if self.screen.viewer().is_some_and(|v| v.rendered) {
            return;
        }
        self.viewer_scroll((-lines * 3.0).round() as isize);
    }

    pub(super) fn viewer_key(&mut self, key: &keyboard::Key, mods: keyboard::Modifiers) -> Task<Message> {
        let named = match key {
            keyboard::Key::Named(n) => Some(*n),
            _ => None,
        };
        let rows = self.viewer_rows() as isize;
        let ctrl = mods.control();
        let ch = match key {
            keyboard::Key::Character(c) => Some(c.to_lowercase()),
            _ => None,
        };
        if self.screen.viewer().is_some_and(|v| v.rendered && v.search.is_none()) {
            if let Some(task) = self.viewer_markdown_key(named, ch.as_deref(), mods) {
                return task;
            }
        }
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };

        if v.search.is_some() {
            match named {
                Some(Named::Escape) => v.search = None,
                Some(Named::F3) => return self.viewer_find(!mods.shift()),
                _ => {}
            }
            return Task::none();
        }

        if ctrl && (ch.as_deref() == Some("c") || named == Some(Named::Insert)) {
            return self.viewer_copy();
        }
        if ctrl && ch.as_deref() == Some("a") {
            self.viewer_select_all();
            return Task::none();
        }
        if ctrl && ch.as_deref() == Some("f") {
            v.search = Some(v.query.clone());
            return Task::batch([text_input::focus(search_input_id()), text_input::select_all(search_input_id())]);
        }
        let typed = ch.filter(|_| !ctrl && !mods.alt());
        if let Some(task) = self.viewer_image_key(named, typed.as_deref()) {
            return task;
        }
        let max_left = self.viewer_max_left();
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        match (named, typed.as_deref()) {
            // Esc drops a selection before it closes.
            (Some(Named::Escape), _) if v.sel.is_some() => v.sel = None,
            (Some(Named::Escape), _) | (_, Some("q")) => self.close_viewer(),
            (Some(Named::ArrowDown), _) => self.viewer_scroll(1),
            (Some(Named::ArrowUp), _) => self.viewer_scroll(-1),
            (Some(Named::PageDown), _) => self.viewer_scroll(rows),
            (Some(Named::PageUp), _) => self.viewer_scroll(-rows),
            (Some(Named::Home), _) => self.viewer_home_end(false),
            (Some(Named::End), _) => self.viewer_home_end(true),
            (Some(Named::ArrowRight), _) if !v.wrap => v.left = (v.left + H_STEP).min(max_left.max(v.left)),
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
            (_, Some("1")) => {
                v.rendered = false;
                self.viewer_mode(Mode::Text);
            }
            (_, Some("3")) => {
                v.rendered = false;
                self.viewer_mode(Mode::Hex);
            }
            (_, Some("5")) => self.viewer_markdown(),
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
        let Some(v) = self.screen.viewer_mut() else { return };
        let Some(doc) = v.doc() else { return };
        if v.mode == mode {
            return;
        }
        // Keep roughly the same place in the file.
        v.top = doc.row_of(mode, doc.row_byte(v.mode, v.top));
        v.mode = mode;
        // The selection covers the same bytes in the other mode.
        if let Some(s) = &mut v.sel {
            s.part = if mode == Mode::Text { Part::Text } else { Part::Hex };
        }
        v.left = 0;
        v.hit = None;
    }

    fn viewer_search_input(&mut self, text: String) {
        if let Some(v) = self.screen.viewer_mut() {
            v.search = Some(text);
        }
    }

    /// Enter in the search box: remember the query, close the box, find.
    fn viewer_submit(&mut self) -> Task<Message> {
        if let Some(v) = self.screen.viewer_mut() {
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
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        if v.query.is_empty() {
            v.search = Some(String::new());
            return text_input::focus(search_input_id());
        }
        let Some(doc) = v.doc_arc() else { return Task::none() };
        if !doc.is_whole() {
            return self.viewer_find_in_file(doc, forward);
        }
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
        self.viewer_show_hit(hit, wrapped, (rows, cols));
        Task::none()
    }

    /// Searches a large file on a worker thread, window by window.
    fn viewer_find_in_file(&mut self, doc: Arc<Document>, forward: bool) -> Task<Message> {
        let Some(v) = self.screen.viewer_mut() else { return Task::none() };
        let from = match v.hit.filter(|h| h.mode == v.mode) {
            Some(h) => match h.mode {
                Mode::Text => doc.byte_of(if forward { h.start + 1 } else { h.start }),
                Mode::Hex => doc.base() + h.start as u64 + u64::from(forward),
            },
            None => doc.row_byte(v.mode, v.top),
        };
        v.cancel_search();
        let cancel = Arc::new(AtomicBool::new(false));
        v.cancel = cancel.clone();
        let token = cancel.clone();
        v.note = Some((format!("Searching for \"{}\"…", v.query), Tone::Info));
        let (path, encoding, mode, query, case, size, seq) =
            (v.source(), doc.encoding, v.mode, v.query.clone(), v.case, doc.size, v.seq);
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let find = |from| viewer::find_in_file(&path, encoding, mode, &query, from, case, forward, &cancel);
                    let result = find(from).and_then(|found| match found {
                        Some((doc, hit)) => Ok(Some((Arc::new(doc), hit, false))),
                        None if cancel.load(Ordering::Relaxed) => Ok(None),
                        None => Ok(find(if forward { 0 } else { size })?.map(|(doc, hit)| (Arc::new(doc), hit, true))),
                    });
                    let _ = tx.send(result.map_err(|e| e.to_string()));
                });
                rx.await.unwrap_or_else(|_| Err("search thread failed".into()))
            },
            move |result| Message::Viewer(ViewerMsg::Found(seq, result, token.clone())),
        )
    }

    /// Result of a large-file search; `search` tells it from older ones.
    fn viewer_found(&mut self, seq: u64, result: Result<Option<Found>, String>, search: &Arc<AtomicBool>) {
        let dims = (self.viewer_rows(), self.viewer_cols());
        let Some(v) = self.screen.viewer_mut().filter(|v| v.seq == seq) else { return };
        if !Arc::ptr_eq(&v.cancel, search) || search.load(Ordering::Relaxed) {
            return;
        }
        match result {
            Ok(Some((doc, hit, wrapped))) => {
                v.content = Content::Loaded(Loaded::Doc(doc));
                v.mode = hit.mode;
                self.viewer_show_hit(hit, wrapped, dims);
            }
            Ok(None) => v.note = Some((format!("\"{}\" not found", v.query), Tone::Error)),
            Err(e) => v.note = Some((format!("Search failed: {e}"), Tone::Error)),
        }
    }

    /// Scrolls `hit` (in the current document) into view and marks it.
    fn viewer_show_hit(&mut self, hit: Hit, wrapped: bool, (rows, cols): (usize, usize)) {
        let Some(v) = self.screen.viewer_mut() else { return };
        let Some(doc) = v.doc_arc() else { return };
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
    }

    pub(super) fn viewer_view<'a>(&'a self, v: &'a Viewer) -> Element<'a, Message> {
        let name = v.path().file_name().map_or_else(|| v.path().display().to_string(), |n| n.to_string_lossy().into_owned());
        let mut info = vec![format!("{} / {}", v.index + 1, v.files.len())];
        match &v.content {
            Content::Loaded(Loaded::Doc(doc)) if v.rendered => {
                info.push(rc_core::format::size(doc.size));
                info.push("Markdown".into());
            }
            Content::Loaded(Loaded::Doc(doc)) => {
                info.push(rc_core::format::size(doc.size));
                info.push(match v.mode {
                    Mode::Text => format!("Text · {}", doc.encoding.label()),
                    Mode::Hex => "Hex".into(),
                });
                if v.wrap && v.mode == Mode::Text {
                    info.push("Wrap".into());
                }
                if let Some((a, b)) = v.sel.filter(|s| !s.is_empty()).map(|s| s.range()) {
                    info.push(format!("{} selected", rc_core::format::size(b.min(doc.size) - a.min(doc.size))));
                }
                if !doc.is_whole() && doc.size > 0 {
                    let at = doc.row_byte(v.mode, v.top);
                    info.push(format!("{}%", at * 100 / doc.size));
                }
            }
            Content::Loaded(Loaded::Image { width, height, .. }) => {
                info.push(format!("{width} × {height}"));
                let scale = v.zoom.unwrap_or_else(|| self.viewer_fit_scale((*width, *height)));
                let pct = format!("{:.0}%", scale * 100.0);
                info.push(if v.zoom.is_none() { format!("Fit {pct}") } else { pct });
            }
            _ => {}
        }
        let header = container(
            row![
                text(name).size(14).wrapping(Wrapping::None),
                Space::with_width(Length::Fill),
                text(info.join("   ")).size(12).color(theme::c().muted).wrapping(Wrapping::None),
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .clip(true)
        .style(filled(theme::c().header));

        let body: Element<'a, Message> = match &v.content {
            Content::Loading => center(text("Loading…").size(13).color(theme::c().muted)).into(),
            Content::Failed(e) => center(text(format!("Can't open: {e}")).size(13).color(theme::c().error)).into(),
            Content::Loaded(Loaded::Image { handle, width, height }) => match v.zoom {
                None => center(
                    image(handle.clone()).content_fit(ContentFit::ScaleDown).filter_method(image::FilterMethod::Linear),
                )
                .padding(8)
                .into(),
                Some(zoom) => zoomed_image(handle, *width as f32 * zoom, *height as f32 * zoom, zoom),
            },
            Content::Loaded(Loaded::Doc(_)) if v.rendered && v.md.is_some() => markdown_body(v),
            Content::Loaded(Loaded::Doc(doc)) => self.text_body(v, doc),
        };
        let mut body = mouse_area(container(body).width(Length::Fill).height(Length::Fill).style(filled(theme::c().panel)));
        if v.pan.is_some() {
            body = body.interaction(mouse::Interaction::Grabbing);
        } else if self.viewer_can_pan() {
            body = body.interaction(mouse::Interaction::Grab);
        }
        let body = body
            .on_move(|at| Message::Viewer(ViewerMsg::Pointer(Some(at))))
            .on_exit(Message::Viewer(ViewerMsg::Pointer(None)))
            .on_scroll(|delta| {
                Message::Viewer(ViewerMsg::Wheel(match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / LINE_H / 3.0,
                }))
            });

        let mut page = column![header, body];
        if let Some(query) = &v.search {
            page = page.push(
                container(
                    row![
                        text("Find:").size(13),
                        text_input("text to find", query)
                            .id(search_input_id())
                            .on_input(|text| Message::Viewer(ViewerMsg::Search(text)))
                            .on_submit(Message::Viewer(ViewerMsg::Find))
                            .padding(5)
                            .size(13)
                            .style(theme::input),
                        text(if v.case { "Aa (C)" } else { "aa (C)" }).size(12).color(theme::c().muted),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([4, 12])
                .height(SEARCH_H)
                .style(filled(theme::c().header)),
            );
        }
        let help = if v.image_size().is_some() {
            "Esc close · ←/→ prev/next image · N/P next/prev file · +/- or Ctrl+wheel zoom · 0 fit · 1 actual size · drag to pan"
        } else if v.rendered {
            "Esc close · N/P next/prev file · 1 source · 3 hex · F7 find (in the source) · click a link to open it · copy: in the source"
        } else if v.md.is_some() {
            "Esc close · N/P next/prev file · 1 text · 3 hex · 5 Markdown · W wrap · F7 find · F3/Shift+F3 next/prev · C match case · drag select · Ctrl+C copy"
        } else {
            "Esc close · N/P next/prev file · 1 text · 3 hex · W wrap · F7 find · F3/Shift+F3 next/prev · C match case · drag select · Ctrl+C copy"
        };
        let footer: Element<'a, Message> = match &v.note {
            Some((msg, tone)) => text(msg.as_str())
                .size(12)
                .color(if *tone == Tone::Error { theme::c().error } else { theme::c().text })
                .into(),
            None => text(help)
            .size(12)
            .color(theme::c().muted)
            .wrapping(Wrapping::None)
            .into(),
        };
        page = page.push(container(footer).padding([4, 12]).width(Length::Fill).clip(true).style(filled(theme::c().header)));
        container(page).width(Length::Fill).height(Length::Fill).style(filled(theme::c().bg)).into()
    }

    fn text_body<'a>(&'a self, v: &'a Viewer, doc: &'a Document) -> Element<'a, Message> {
        let cols = self.viewer_cols();
        responsive(move |size| {
            let rows = (size.height / LINE_H).ceil() as usize;
            let sel = v.sel.filter(|s| !s.is_empty()).map(|s| s.range());
            let lines: Vec<Element<'a, Message>> = match v.mode {
                Mode::Text => {
                    // The selection as text offsets of this window (clamped to it).
                    let sel = sel.map(|(a, b)| (doc.text_of(a), doc.text_of(b)));
                    text_rows(doc, v, cols, rows)
                        .into_iter()
                        .map(|r| {
                            let len = r.shown.chars().count();
                            // Line columns to this row's columns; a wrapped row
                            // that isn't the line's last ends at its text.
                            let clip = |(a, b): (usize, usize)| {
                                let (a, b) = (a.saturating_sub(r.col), b.saturating_sub(r.col));
                                let b = if r.last { b } else { b.min(len) };
                                (a < b).then_some((a, b))
                            };
                            let line = doc.line(r.line);
                            let hits: Vec<_> = v
                                .hit
                                .filter(|h| h.mode == Mode::Text && doc.line_of(h.start) == r.line)
                                .and_then(|h| {
                                    let start = doc.line_start(r.line);
                                    clip((viewer::column_of(line, h.start - start), viewer::column_of(line, h.end - start)))
                                })
                                .into_iter()
                                .collect();
                            let sel: Vec<_> =
                                sel.and_then(|(a, b)| doc.selected_cols(r.line, a, b)).and_then(clip).into_iter().collect();
                            row_text(r.shown, &sel, &hits)
                        })
                        .collect()
                }
                Mode::Hex => {
                    let end = (v.top + rows).min(doc.rows(v.mode));
                    (v.top..end)
                        .map(|i| {
                            let off = doc.row_byte(Mode::Hex, i);
                            let len = (doc.end() - off).min(HEX_WIDTH as u64);
                            // File offsets `a..b` as byte indexes of this row.
                            let spans = |(a, b): (u64, u64)| {
                                let (a, b) = (a.clamp(off, off + len) - off, b.clamp(off, off + len) - off);
                                viewer::hex_spans(off, a as usize, b as usize)
                            };
                            let hits = v
                                .hit
                                .filter(|h| h.mode == Mode::Hex)
                                .map(|h| spans((doc.base() + h.start as u64, doc.base() + h.end as u64)))
                                .unwrap_or_default();
                            let sel = sel.map(spans).unwrap_or_default();
                            row_text(doc.hex_row(i), &sel, &hits)
                        })
                        .collect()
                }
            };
            let lines = container(Column::with_children(lines).width(Length::Fill))
                .padding([0, 12])
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true);
            // Whole files scroll by rows; in a large file's window the bar
            // stands for bytes of the whole file (see `viewer_seek`).
            let full = (size.height / LINE_H).floor().max(1.0);
            let (offset, visible, total) = if doc.is_whole() {
                (v.top as f32, full, doc.rows(v.mode) as f32)
            } else {
                let per_row = (doc.end() - doc.base()) as f32 / doc.rows(v.mode).max(1) as f32;
                (doc.row_byte(v.mode, v.top) as f32, per_row * full, doc.size as f32)
            };
            // The bar's end is the file's end (the visible bytes are only estimated).
            let end = total - visible;
            scrollbar::attach(lines, offset, visible, total, move |at| {
                Message::Viewer(ViewerMsg::Seek(if at >= end { f32::INFINITY } else { at }))
            })
        })
        .into()
    }
}

/// The rendered Markdown of `v`, scrolling vertically. The widgets are
/// built once per load (and theme), not on every redraw.
fn markdown_body(v: &Viewer) -> Element<'_, Message> {
    let Some(md) = &v.md else { return Space::new(Length::Fill, Length::Fill).into() };
    let blocks: &[Block] = md;
    let key = (v.seq, blocks.as_ptr() as usize, markdown::theme_key());
    scrollable::Scrollable::new(lazy(key, move |_| markdown::view(blocks)))
        .id(markdown_scroll_id())
        .direction(Direction::Vertical(scrollbar::bar()))
        .style(scrollbar::style)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The file a relative link `target` in the file `from` points at
/// (`#anchor` and `?query` dropped, `%xx` decoded, `..` resolved); `None`
/// for links with a scheme or to a place in the same file.
fn link_path(from: &Path, target: &str) -> Option<PathBuf> {
    let target = target.split(['#', '?']).next().unwrap_or("");
    // A scheme (mailto:, ftp:, ...), but not a drive letter.
    let scheme = target.find(':').is_some_and(|i| i > 1);
    if target.is_empty() || scheme {
        return None;
    }
    let target = percent_decode(target);
    let mut path = PathBuf::new();
    for part in from.parent()?.join(&target).components() {
        match part {
            std::path::Component::ParentDir => {
                path.pop();
            }
            std::path::Component::CurDir => {}
            other => path.push(other),
        }
    }
    Some(path)
}

/// `%xx` escapes decoded (invalid ones kept as they are).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(a), Some(b)) => {
                out.push((a * 16 + b) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An image drawn at `w` x `h` pixels, centred when smaller than the view
/// and scrollable both ways when larger.
fn zoomed_image<'a>(handle: &'a image::Handle, w: f32, h: f32, zoom: f32) -> Element<'a, Message> {
    let filter = if zoom >= 2.0 { image::FilterMethod::Nearest } else { image::FilterMethod::Linear };
    responsive(move |size| {
        let picture = image(handle.clone()).width(w).height(h).content_fit(ContentFit::Fill).filter_method(filter);
        scrollable::Scrollable::new(container(picture).center_x(w.max(size.width)).center_y(h.max(size.height)))
            .id(image_scroll_id())
            .direction(Direction::Both { vertical: scrollbar::bar(), horizontal: scrollbar::bar() })
            .style(scrollbar::style)
            .on_scroll(|viewport| Message::Viewer(ViewerMsg::Panned(viewport.absolute_offset())))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    })
    .into()
}

/// A screen row of the text view: display text of line `line` from
/// display column `col`; `last` = it ends that line (not wrapped on).
struct TextRow {
    line: usize,
    col: usize,
    shown: String,
    last: bool,
}

/// The first `max` screen rows of the text view from row `v.top`: one per
/// line scrolled by `v.left`, or each line word-wrapped at `cols` columns.
/// Drawing and mouse hit-testing both use it, so they agree.
fn text_rows(doc: &Document, v: &Viewer, cols: usize, max: usize) -> Vec<TextRow> {
    let mut out = Vec::new();
    let mut i = v.top;
    while out.len() < max && i < doc.rows(Mode::Text) {
        let line = doc.line(i);
        if v.wrap {
            let full: Vec<char> = viewer::display_line(line, 0, WRAP_MAX).chars().collect();
            let points = viewer::wrap_points(&full.iter().collect::<String>(), cols);
            for (k, &p) in points.iter().enumerate() {
                let end = points.get(k + 1).copied().unwrap_or(full.len());
                let last = k + 1 == points.len();
                out.push(TextRow { line: i, col: p, shown: full[p..end].iter().collect(), last });
                if out.len() >= max {
                    break;
                }
            }
        } else {
            out.push(TextRow { line: i, col: v.left, shown: viewer::display_line(line, v.left, cols), last: true });
        }
        i += 1;
    }
    out
}

/// A monospace viewer row: plain text, or spans when parts are selected or
/// find hits.
fn row_text<'a>(shown: String, sel: &[(usize, usize)], hits: &[(usize, usize)]) -> Element<'a, Message> {
    if sel.is_empty() && hits.is_empty() {
        text(if shown.is_empty() { " ".to_string() } else { shown })
            .font(Font::MONOSPACE)
            .size(FONT_SIZE)
            .line_height(iced::Pixels(LINE_H))
            .wrapping(Wrapping::None)
            .into()
    } else {
        rich_text(highlight(&shown, sel, hits))
            .font(Font::MONOSPACE)
            .size(FONT_SIZE)
            .line_height(iced::Pixels(LINE_H))
            .wrapping(Wrapping::None)
            .into()
    }
}

/// Splits `s` into spans: character ranges in `sel` drawn as selected, in
/// `hits` as find hits (on top). A selection reaching past the text (a
/// line break) shows as one selected blank.
fn highlight<'a>(s: &str, sel: &[(usize, usize)], hits: &[(usize, usize)]) -> Vec<Span<'a, Message, Font>> {
    let mut chars: Vec<char> = s.chars().collect();
    if sel.iter().any(|&(_, b)| b > chars.len()) {
        chars.push(' ');
    }
    let mut class = vec![0u8; chars.len()];
    for (marks, c) in [(sel, 1), (hits, 2)] {
        for &(a, b) in marks {
            class[a.min(chars.len())..b.min(chars.len())].fill(c);
        }
    }
    let colors = theme::c();
    let mut spans = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
        let end = (pos..chars.len()).find(|&k| class[k] != class[pos]).unwrap_or(chars.len());
        let span = Span::new(chars[pos..end].iter().collect::<String>());
        spans.push(match class[pos] {
            1 => span.background(colors.cursor),
            2 => span.color(colors.bg).background(colors.selected),
            _ => span,
        });
        pos = end;
    }
    if spans.is_empty() {
        spans.push(Span::new(" "));
    }
    spans
}

/// New scroll offset along one axis when an image `size` pixels long, shown
/// in a view `view` long (centred when smaller), goes from scale `old` to
/// `new`, keeping the image point under view position `p` in place.
fn zoom_offset(size: f32, view: f32, old: f32, new: f32, offset: f32, p: f32) -> f32 {
    let lead = |scale: f32| ((view - size * scale) / 2.0).max(0.0);
    let content = |scale: f32| (size * scale).max(view);
    let u = ((offset + p - lead(old)) / old).clamp(0.0, size);
    (u * new + lead(new) - p).clamp(0.0, content(new) - view)
}

#[cfg(test)]
mod tests {
    use super::{link_path, percent_decode, zoom_offset};
    use std::path::{Path, PathBuf};

    #[test]
    fn markdown_link_paths() {
        let from = Path::new(r"C:\docs\guide\README.md");
        assert_eq!(link_path(from, "other.md"), Some(PathBuf::from(r"C:\docs\guide\other.md")));
        assert_eq!(link_path(from, "../up/My%20File.md#part"), Some(PathBuf::from(r"C:\docs\up\My File.md")));
        assert_eq!(link_path(from, "./a/b.md?x=1"), Some(PathBuf::from(r"C:\docs\guide\a\b.md")));
        assert_eq!(link_path(from, "#section"), None);
        assert_eq!(link_path(from, "mailto:me@x.org"), None);
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn zoom_keeps_point_under_pointer() {
        // 1000 px image, 500 px view, 1x -> 2x with the pointer at 100 px and
        // offset 200: image point 300 must stay at view position 100.
        let o = zoom_offset(1000.0, 500.0, 1.0, 2.0, 200.0, 100.0);
        assert_eq!(o, 500.0);
    }

    #[test]
    fn zoom_from_centred_small_image() {
        // 100 px image in a 500 px view sits at 200..300; pointer at its
        // centre (250) zoomed 10x: image centre (50 -> 500) under 250.
        let o = zoom_offset(100.0, 500.0, 1.0, 10.0, 0.0, 250.0);
        assert_eq!(o, 250.0);
    }

    #[test]
    fn zoom_clamps_to_content() {
        assert_eq!(zoom_offset(1000.0, 500.0, 1.0, 2.0, 0.0, 0.0), 0.0);
        assert_eq!(zoom_offset(1000.0, 500.0, 1.0, 2.0, 500.0, 500.0), 1500.0);
        // Zooming out below the view size: nothing to scroll.
        assert_eq!(zoom_offset(1000.0, 500.0, 1.0, 0.25, 500.0, 250.0), 0.0);
    }
}
