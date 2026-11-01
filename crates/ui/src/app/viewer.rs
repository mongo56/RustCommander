//! Built-in file viewer (F3, TC's Lister): text with optional word wrap,
//! hex dump, images. Fills the whole window while open; N/P step through
//! the files of the panel it was opened from.

use super::*;
use crate::theme::filled;
use iced::widget::text::{Span, Wrapping};
use iced::widget::scrollable::{self, AbsoluteOffset, Direction, RelativeOffset, Scrollbar};
use iced::{Point, Size};
use iced::widget::{center, column, container, mouse_area, responsive, rich_text, row, text, Column, Space};
use iced::{Alignment, ContentFit, Element, Font, Length};
use rc_core::viewer::{self, Document, Hit, Mode, HEX_WIDTH, WINDOW};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// Zoom factor per +/- press, and the zoom range.
const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 32.0;
/// Pixels the arrow keys pan a zoomed image by.
const PAN_STEP: f32 = 48.0;
/// Longest prefix of a line measured when clamping horizontal scroll.
const MEASURE_MAX: usize = 100_000;

fn image_scroll_id() -> scrollable::Id {
    scrollable::Id::new("viewer-image")
}

fn search_input_id() -> text_input::Id {
    text_input::Id::new("viewer-search")
}

/// A match found in a large file: the window holding it, the match in
/// that window, and whether the search wrapped around.
pub type Found = (Arc<Document>, Hit, bool);

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
    /// Image scale; `None` fits the window (never enlarging).
    zoom: Option<f32>,
    /// Scroll position of a zoomed image, in pixels.
    offset: AbsoluteOffset,
    /// Mouse pointer over the image area (relative to it); zoom anchor.
    pointer: Option<Point>,
    /// Stops the running search of a large file.
    cancel: Arc<AtomicBool>,
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
                let _ = tx.send(load_sync(&path));
            });
            rx.await.unwrap_or_else(|_| Err("viewer thread failed".into()))
        },
        move |result| Message::ViewerLoaded(seq, result),
    )
}

fn load_sync(path: &Path) -> Result<Loaded, String> {
    // A file inside an archive is viewed from a temp copy.
    let local = rc_core::vfs::extract_temp(path).map_err(|e| e.to_string())?;
    let path = local.as_path();
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
            zoom: None,
            offset: AbsoluteOffset { x: 0.0, y: 0.0 },
            pointer: None,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        self.viewer_show(index)
    }

    fn viewer_show(&mut self, index: usize) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        let Some(v) = &mut self.viewer else { return Task::none() };
        v.cancel_search();
        v.index = index;
        v.seq = seq;
        v.content = Content::Loading;
        v.top = 0;
        v.left = 0;
        v.hit = None;
        v.note = None;
        v.zoom = None;
        v.offset = AbsoluteOffset { x: 0.0, y: 0.0 };
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
        let Some(mut v) = self.viewer.take() else { return };
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
        self.viewer_slide();
    }

    /// In a large file, moves the window along when the view nears its
    /// edge, centring it on the top row.
    fn viewer_slide(&mut self) {
        let rows = self.viewer_rows();
        let Some(v) = &mut self.viewer else { return };
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
        let Some(v) = &mut self.viewer else { return };
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
        let Some(v) = &self.viewer else { return 0 };
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
        ("Esc, Q", "Close the viewer (Esc first closes the find box)"),
        ("Up, Down, PgUp, PgDn", "Scroll"),
        ("Home, End", "Start / end of the file"),
        ("Left, Right", "Scroll sideways (wrap off)"),
        ("N, Space", "Next file in the panel"),
        ("P, Backspace", "Previous file in the panel"),
        ("1", "Text mode (images: actual size)"),
        ("3", "Hex mode"),
        ("W", "Wrap long lines on / off"),
        ("F7, F, /, Ctrl+F", "Find text"),
        ("F3, Shift+F3", "Find next / previous"),
        ("C", "Case-sensitive find on / off"),
        ("+, -, Ctrl+wheel", "Zoom an image in / out"),
        ("0, *", "Fit the image to the window"),
        ("Arrows, PgUp, PgDn, Home, End", "Pan a zoomed image"),
    ];

    /// Image-only keys: zoom and pan. `None` when the key is not one of them.
    fn viewer_image_key(&mut self, named: Option<Named>, typed: Option<&str>) -> Option<Task<Message>> {
        self.viewer.as_ref()?.image_size()?;
        let page = (self.window_height - VIEWER_CHROME_H - 40.0).max(PAN_STEP);
        let pan = |x: f32, y: f32| scrollable::scroll_by(image_scroll_id(), AbsoluteOffset { x, y });
        let zoomed = self.viewer.as_ref()?.zoom.is_some();
        Some(match (named, typed) {
            (_, Some("+" | "=")) => self.viewer_set_zoom(|z| Some(z * ZOOM_STEP)),
            (_, Some("-")) => self.viewer_set_zoom(|z| Some(z / ZOOM_STEP)),
            (_, Some("0" | "*")) => self.viewer_set_zoom(|_| None),
            (_, Some("1")) => self.viewer_set_zoom(|_| Some(1.0)),
            (Some(Named::ArrowLeft), _) if zoomed => pan(-PAN_STEP, 0.0),
            (Some(Named::ArrowRight), _) if zoomed => pan(PAN_STEP, 0.0),
            (Some(Named::ArrowUp), _) if zoomed => pan(0.0, -PAN_STEP),
            (Some(Named::ArrowDown), _) if zoomed => pan(0.0, PAN_STEP),
            (Some(Named::PageUp), _) if zoomed => pan(0.0, -page),
            (Some(Named::PageDown), _) if zoomed => pan(0.0, page),
            (Some(Named::Home), _) if zoomed => scrollable::snap_to(image_scroll_id(), RelativeOffset::START),
            (Some(Named::End), _) if zoomed => scrollable::snap_to(image_scroll_id(), RelativeOffset::END),
            _ => return None,
        })
    }

    pub(super) fn viewer_panned(&mut self, offset: AbsoluteOffset) {
        // Ctrl+wheel zooms; the image scrollable also scrolls on that wheel
        // event, which `viewer_set_zoom` overrides with its own offset.
        if self.modifiers.control() {
            return;
        }
        if let Some(v) = &mut self.viewer {
            v.offset = offset;
        }
    }

    pub(super) fn viewer_pointer(&mut self, at: Option<Point>) {
        if let Some(v) = &mut self.viewer {
            v.pointer = at;
        }
    }

    /// Ctrl+wheel over an image: one zoom step per notch, around the pointer.
    pub(super) fn viewer_zoom_wheel(&mut self, notches: f32) -> Task<Message> {
        if !self.modifiers.control() || notches == 0.0 {
            return Task::none();
        }
        let anchor = self.viewer.as_ref().and_then(|v| v.pointer);
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
        let Some((w, h)) = self.viewer.as_ref().and_then(Viewer::image_size) else {
            return Task::none();
        };
        let fit = self.viewer_fit_scale((w, h));
        let view = self.viewer_body_size();
        let Some(v) = self.viewer.as_mut() else { return Task::none() };
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
        let search = if self.viewer.as_ref().is_some_and(|v| v.search.is_some()) { SEARCH_H } else { 0.0 };
        Size::new(self.window_width.max(1.0), (self.window_height - VIEWER_CHROME_H - search).max(1.0))
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
        if let Some(task) = self.viewer_image_key(named, typed.as_deref()) {
            return task;
        }
        let max_left = self.viewer_max_left();
        let Some(v) = &mut self.viewer else { return Task::none() };
        match (named, typed.as_deref()) {
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
        v.top = doc.row_of(mode, doc.row_byte(v.mode, v.top));
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
        let Some(v) = &mut self.viewer else { return Task::none() };
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
            move |result| Message::ViewerFound(seq, result, token.clone()),
        )
    }

    /// Result of a large-file search; `search` tells it from older ones.
    pub(super) fn viewer_found(&mut self, seq: u64, result: Result<Option<Found>, String>, search: &Arc<AtomicBool>) {
        let dims = (self.viewer_rows(), self.viewer_cols());
        let Some(v) = self.viewer.as_mut().filter(|v| v.seq == seq) else { return };
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
        let Some(v) = &mut self.viewer else { return };
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
            Content::Loaded(Loaded::Doc(doc)) => {
                info.push(rc_core::format::size(doc.size));
                info.push(match v.mode {
                    Mode::Text => format!("Text · {}", doc.encoding.label()),
                    Mode::Hex => "Hex".into(),
                });
                if v.wrap && v.mode == Mode::Text {
                    info.push("Wrap".into());
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
            Content::Loaded(Loaded::Doc(doc)) => self.text_body(v, doc),
        };
        let body = mouse_area(container(body).width(Length::Fill).height(Length::Fill).style(filled(theme::c().panel)))
            .on_move(|at| Message::ViewerPointer(Some(at)))
            .on_exit(Message::ViewerPointer(None))
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
            "Esc close · N/P next/prev file · +/- or Ctrl+wheel zoom · 0 fit · 1 actual size · arrows/PgUp/PgDn pan"
        } else {
            "Esc close · N/P next/prev file · 1 text · 3 hex · W wrap · F7 find · F3/Shift+F3 next/prev · C match case"
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

/// An image drawn at `w` x `h` pixels, centred when smaller than the view
/// and scrollable both ways when larger.
fn zoomed_image<'a>(handle: &'a image::Handle, w: f32, h: f32, zoom: f32) -> Element<'a, Message> {
    let filter = if zoom >= 2.0 { image::FilterMethod::Nearest } else { image::FilterMethod::Linear };
    responsive(move |size| {
        let picture = image(handle.clone()).width(w).height(h).content_fit(ContentFit::Fill).filter_method(filter);
        scrollable::Scrollable::new(container(picture).center_x(w.max(size.width)).center_y(h.max(size.height)))
            .id(image_scroll_id())
            .direction(Direction::Both { vertical: Scrollbar::default(), horizontal: Scrollbar::default() })
            .on_scroll(|viewport| Message::ViewerPanned(viewport.absolute_offset()))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    })
    .into()
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
                .color(theme::c().bg)
                .background(theme::c().selected),
        );
        pos = b;
    }
    if pos < chars.len() || spans.is_empty() {
        spans.push(Span::new(chars[pos..].iter().collect::<String>()));
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
    use super::zoom_offset;

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
