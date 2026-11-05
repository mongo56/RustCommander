//! Quick view (`cm_QuickView`): the other panel's area shows a live preview
//! of the active panel's cursor entry. Text/binary files as the F3 viewer's
//! text or hex (only the file's head is read), images fitted to the area,
//! Markdown rendered, folders as a summary with their size counted in the
//! background, archives as their top-level listing. Read-only: the only
//! input is the mouse wheel (and a click, which leaves quick view for that
//! panel). Not saved in the session.

use super::markdown;
use super::viewer::{self, Loaded, LINE_H};
use super::*;
use crate::theme::filled;
use iced::widget::scrollable::{self, Direction};
use iced::widget::text::Wrapping;
use iced::widget::{center, column, container, lazy, mouse_area, responsive, row, text, Column, Space};
use iced::{Alignment, ContentFit, Element, Length};
use rc_core::viewer::{Document, Mode};

/// Cursor rest before the entry under it is loaded.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Bytes of a text/binary file read for its preview.
const HEAD: u64 = 512 << 10;
/// Images are scaled down to this many pixels per side for the preview.
const IMAGE_SIDE: u32 = 2048;
/// Rows of an archive listing shown at most.
const ARCHIVE_ROWS: usize = 1000;

/// What an entry is previewed as (chosen from its kind and name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Blank,
    /// In an archive or on a server: not previewed.
    Virtual,
    Folder,
    Archive,
    Image,
    Markdown,
    /// Text, or hex when the file turns out binary.
    Text,
}

/// The preview kind of an entry; `virtual_` = its path is in an archive or
/// on a server (`vfs::is_virtual`).
pub(super) fn kind_of(kind: EntryKind, path: &Path, virtual_: bool) -> Kind {
    match kind {
        EntryKind::Parent => Kind::Blank,
        _ if virtual_ => Kind::Virtual,
        EntryKind::Dir => Kind::Folder,
        EntryKind::File => {
            let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
            if vfs::is_archive_name(&name) {
                Kind::Archive
            } else if viewer::is_image_name(path) {
                Kind::Image
            } else if markdown::is_markdown(path) {
                Kind::Markdown
            } else {
                Kind::Text
            }
        }
    }
}

/// What the preview shows.
#[derive(Debug, Clone)]
pub enum Preview {
    Blank,
    Loading,
    Note(String),
    Failed(String),
    Doc(Arc<Document>),
    Image { handle: image::Handle, width: u32, height: u32 },
    Markdown(Arc<Document>, Arc<markdown::Doc>),
    /// Subfolders and files directly in it; total size once counted.
    Folder { dirs: usize, files: usize, size: Option<u64> },
    /// Top-level entries (folders first), and the files and bytes of the whole archive.
    Archive { entries: Arc<Vec<Entry>>, files: usize, total: u64 },
}

#[derive(Debug, Clone)]
pub enum QvMsg {
    /// The debounce of a generation elapsed.
    Settle(u64),
    Loaded(u64, Preview),
    /// The size of the previewed folder of a generation.
    Size(u64, u64),
    Wheel(f32),
    /// Click on the preview: back to that panel.
    Click,
}

/// Debounce and load gate: one generation per cursor entry; a load starts
/// only once its debounce elapsed, and only one runs at a time (the newest
/// entry is loaded when the running load ends).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Gate {
    gen: u64,
    settled: bool,
    busy: bool,
}

impl Gate {
    /// The entry changed; `gen` (unique) names it from now on.
    pub(super) fn change(&mut self, gen: u64) {
        self.gen = gen;
        self.settled = false;
    }

    /// The debounce of `gen` elapsed: the generation to load now, if any.
    pub(super) fn settle(&mut self, gen: u64) -> Option<u64> {
        if gen != self.gen {
            return None;
        }
        self.settled = true;
        self.start()
    }

    fn start(&mut self) -> Option<u64> {
        if self.busy || !self.settled {
            return None;
        }
        self.busy = true;
        self.settled = false;
        Some(self.gen)
    }

    /// A load of `gen` ended: whether its result is current, and the
    /// generation to load next, if any.
    pub(super) fn finish(&mut self, gen: u64) -> (bool, Option<u64>) {
        self.busy = false;
        (gen == self.gen, self.start())
    }
}

/// The cursor entry a preview is for (a file's size and time too, so a
/// changed file is shown again).
#[derive(Debug, Clone, PartialEq)]
struct Want {
    path: PathBuf,
    kind: EntryKind,
    stamp: Option<(u64, Option<i64>)>,
}

impl Want {
    fn of(e: &Entry) -> Self {
        let stamp = (e.kind == EntryKind::File).then(|| (e.size, e.modified.map(|m| m.timestamp_millis())));
        Self { path: e.path.clone(), kind: e.kind, stamp }
    }
}

#[derive(Debug)]
pub(super) struct QuickView {
    /// The side whose area shows the preview.
    pub(super) side: usize,
    want: Option<Want>,
    gate: Gate,
    /// Stops the folder size count of the shown folder.
    cancel: Arc<AtomicBool>,
    content: Preview,
    /// First row of a text/hex preview.
    top: usize,
}

impl Drop for QuickView {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Builds the preview of `path` (on a worker thread).
fn preview_of(kind: Kind, path: &Path) -> Preview {
    let head = |path: &Path| match Document::open_head(path, HEAD) {
        Ok(doc) => Preview::Doc(Arc::new(doc)),
        Err(e) => Preview::Failed(e.to_string()),
    };
    match kind {
        Kind::Blank => Preview::Blank,
        Kind::Virtual => Preview::Note("No preview for files in archives or on servers".into()),
        Kind::Folder => match std::fs::read_dir(path) {
            Ok(items) => {
                let (mut dirs, mut files) = (0, 0);
                for item in items.flatten() {
                    if item.file_type().is_ok_and(|t| t.is_dir()) {
                        dirs += 1;
                    } else {
                        files += 1;
                    }
                }
                Preview::Folder { dirs, files, size: None }
            }
            Err(e) => Preview::Failed(e.to_string()),
        },
        Kind::Archive => match vfs::list(path, true) {
            Ok(mut entries) => {
                entries.retain(|e| e.kind != EntryKind::Parent);
                entries.sort_by_cached_key(|e| (e.kind != EntryKind::Dir, e.name.to_lowercase()));
                let all = vfs::open(path).and_then(|v| v.walk(path)).unwrap_or_default();
                let files: Vec<&Entry> = all.iter().filter(|e| e.kind == EntryKind::File).collect();
                let total = files.iter().map(|e| e.size).sum();
                Preview::Archive { entries: Arc::new(entries), files: files.len(), total }
            }
            Err(e) => Preview::Failed(e.to_string()),
        },
        Kind::Image => match viewer::decode_image(path, IMAGE_SIDE) {
            Some(Loaded::Image { handle, width, height }) => Preview::Image { handle, width, height },
            _ => head(path),
        },
        Kind::Markdown => {
            let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= markdown::MD_LIMIT);
            match small.then(|| Document::open(path)) {
                Some(Ok(doc)) if viewer::renders_markdown(path, &doc) => {
                    let md = markdown::parse_doc(doc.text());
                    Preview::Markdown(Arc::new(doc), Arc::new(md))
                }
                Some(Ok(doc)) => Preview::Doc(Arc::new(doc)),
                _ => head(path),
            }
        }
        Kind::Text => head(path),
    }
}

/// Loads the preview of `path` on a worker thread as generation `gen`; a
/// folder's size follows as a second message unless `cancel` is set first.
fn load(path: PathBuf, kind: EntryKind, gen: u64, cancel: Arc<AtomicBool>) -> Task<Message> {
    let (tx, rx) = iced::futures::channel::mpsc::unbounded();
    std::thread::spawn(move || {
        let k = kind_of(kind, &path, vfs::is_virtual(&path));
        let preview = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| preview_of(k, &path)))
            .unwrap_or_else(|_| Preview::Failed("preview failed".into()));
        let folder = matches!(preview, Preview::Folder { .. });
        if tx.unbounded_send(QvMsg::Loaded(gen, preview)).is_err() || !folder {
            return;
        }
        let size = rc_core::fs::dir_size(&path, &cancel);
        if !cancel.load(Ordering::Relaxed) {
            let _ = tx.unbounded_send(QvMsg::Size(gen, size));
        }
    });
    Task::run(rx, Message::QuickView)
}

impl App {
    /// `cm_QuickView`: turns the preview in the other panel's area on or off.
    pub(super) fn toggle_quick_view(&mut self) {
        if self.quick_view.take().is_some() {
            return;
        }
        if self.left_hidden() {
            self.info("Quick view needs both panels shown");
            return;
        }
        self.quick_view = Some(QuickView {
            side: 1 - self.active,
            want: None,
            gate: Gate::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            content: Preview::Blank,
            top: 0,
        });
    }

    /// After each update: leaves quick view when its side became active or
    /// hidden, and starts the debounce when the cursor entry changed.
    pub(super) fn quick_view_check(&mut self) -> Task<Message> {
        let Some(q) = &self.quick_view else { return Task::none() };
        if self.active == q.side || (q.side == 0 && self.left_hidden()) {
            self.quick_view = None;
            return Task::none();
        }
        // Full-window tools cover it; catch up once they close.
        if self.screen.is_some() {
            return Task::none();
        }
        let want = self.panel(self.active).current().map(Want::of);
        if q.want == want {
            return Task::none();
        }
        self.next_listing += 1;
        let gen = self.next_listing;
        let Some(q) = self.quick_view.as_mut() else { return Task::none() };
        q.cancel.store(true, Ordering::Relaxed);
        q.cancel = Arc::new(AtomicBool::new(false));
        q.gate.change(gen);
        q.top = 0;
        let blank = want.as_ref().is_none_or(|w| w.kind == EntryKind::Parent);
        q.content = if blank { Preview::Blank } else { Preview::Loading };
        q.want = want;
        if blank {
            return Task::none();
        }
        delayed(DEBOUNCE, Message::QuickView(QvMsg::Settle(gen)))
    }

    /// Starts loading generation `gen` (the current entry).
    fn quick_view_start(&mut self, gen: Option<u64>) -> Task<Message> {
        let (Some(gen), Some(q)) = (gen, self.quick_view.as_ref()) else { return Task::none() };
        match &q.want {
            Some(w) => load(w.path.clone(), w.kind, gen, q.cancel.clone()),
            None => Task::none(),
        }
    }

    pub(super) fn quick_view_update(&mut self, msg: QvMsg) -> Task<Message> {
        let Some(q) = self.quick_view.as_mut() else { return Task::none() };
        match msg {
            QvMsg::Settle(gen) => {
                let start = q.gate.settle(gen);
                return self.quick_view_start(start);
            }
            QvMsg::Loaded(gen, preview) => {
                let (current, next) = q.gate.finish(gen);
                if current {
                    q.content = preview;
                    q.top = 0;
                }
                return self.quick_view_start(next);
            }
            QvMsg::Size(gen, size) => {
                if let (true, Preview::Folder { size: s, .. }) = (q.gate.gen == gen, &mut q.content) {
                    *s = Some(size);
                }
            }
            QvMsg::Wheel(lines) => {
                if let Preview::Doc(doc) = &q.content {
                    let rows = doc.rows(doc.default_mode());
                    let top = q.top as isize - (lines * 3.0).round() as isize;
                    q.top = top.clamp(0, rows.saturating_sub(1) as isize) as usize;
                }
            }
            QvMsg::Click => {
                let side = q.side;
                self.quick_view = None;
                self.focus_panel(side);
            }
        }
        Task::none()
    }

    pub(super) fn quick_view_view<'a>(&'a self, q: &'a QuickView) -> Element<'a, Message> {
        let c = theme::c();
        let name = q.want.as_ref().map_or_else(String::new, |w| {
            w.path.file_name().map_or_else(|| w.path.display().to_string(), |n| n.to_string_lossy().into_owned())
        });
        let info = match &q.content {
            Preview::Doc(doc) => {
                let mut info = match doc.default_mode() {
                    Mode::Text => format!("{} · Text · {}", rc_core::format::size(doc.size), doc.encoding.label()),
                    Mode::Hex => format!("{} · Hex", rc_core::format::size(doc.size)),
                };
                if !doc.is_whole() {
                    info.push_str(&format!(" · first {}", rc_core::format::size(doc.end())));
                }
                info
            }
            Preview::Markdown(doc, _) => format!("{} · Markdown", rc_core::format::size(doc.size)),
            Preview::Image { width, height, .. } => format!("{width} × {height}"),
            Preview::Folder { .. } => "Folder".into(),
            Preview::Archive { .. } => "Archive".into(),
            _ => String::new(),
        };
        let header = container(
            row![
                text("Quick view").size(12).color(c.accent).wrapping(Wrapping::None),
                text(name).size(13).wrapping(Wrapping::None),
                Space::with_width(Length::Fill),
                text(info).size(12).color(c.muted).wrapping(Wrapping::None),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .width(Length::Fill)
        .clip(true)
        .style(filled(c.header));

        let muted = |s: String| -> Element<'a, Message> { center(text(s).size(13).color(c.muted)).into() };
        let body: Element<'a, Message> = match &q.content {
            Preview::Blank => Space::new(Length::Fill, Length::Fill).into(),
            Preview::Loading => muted("Loading…".into()),
            Preview::Note(note) => muted(note.clone()),
            Preview::Failed(e) => center(text(format!("Can't preview: {e}")).size(13).color(c.error)).into(),
            Preview::Doc(doc) => text_page(doc, q.top),
            Preview::Image { handle, .. } => center(
                image(handle.clone()).content_fit(ContentFit::ScaleDown).filter_method(image::FilterMethod::Linear),
            )
            .padding(8)
            .into(),
            Preview::Markdown(_, md) => {
                let key = (Arc::as_ptr(md) as usize, markdown::theme_key());
                let marks = || markdown::Marks { sel: None, hits: &[], current: None };
                scrolled(lazy(key, move |_| markdown::view(&md.blocks, marks())).into())
            }
            Preview::Folder { dirs, files, size } => {
                let size = size.map_or_else(|| "counting…".to_string(), rc_core::format::size);
                let line = |label: &'static str, value: String| {
                    row![text(label).size(13).color(c.muted).width(Length::Fixed(80.0)), text(value).size(13)]
                };
                container(column![
                    line("Folders", dirs.to_string()),
                    line("Files", files.to_string()),
                    line("Size", size),
                ]
                .spacing(6))
                .padding(16)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
            }
            Preview::Archive { entries, files, total } => archive_body(entries, *files, *total),
        };
        let body = mouse_area(container(body).width(Length::Fill).height(Length::Fill).style(filled(c.panel)))
            .on_press(Message::QuickView(QvMsg::Click))
            .on_scroll(|delta| {
                Message::QuickView(QvMsg::Wheel(match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / LINE_H / 3.0,
                }))
            });
        let strip = container(Space::new(Length::Fill, Length::Fixed(2.0))).style(filled(c.header));
        column![strip, header, body].width(Length::Fill).height(Length::Fill).into()
    }
}

/// `content` scrolling vertically.
fn scrolled(content: Element<'_, Message>) -> Element<'_, Message> {
    scrollable::Scrollable::new(content)
        .direction(Direction::Vertical(scrollbar::bar()))
        .style(scrollbar::style)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The rows of `doc` from `top` that fit, as the F3 viewer draws them
/// (text without wrapping, or hex for a binary file).
fn text_page(doc: &Document, top: usize) -> Element<'_, Message> {
    let mode = doc.default_mode();
    responsive(move |size| {
        let rows = (size.height / LINE_H).ceil() as usize;
        let cols = ((size.width - 24.0) / viewer::char_w()).floor().max(1.0) as usize;
        let end = (top + rows).min(doc.rows(mode));
        let lines = (top..end).map(|i| match mode {
            Mode::Text => viewer::row_text(rc_core::viewer::display_line(doc.line(i), 0, cols), &[], &[]),
            Mode::Hex => viewer::row_text(doc.hex_row(i), &[], &[]),
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

/// An archive's top-level listing with a summary line.
fn archive_body(entries: &[Entry], files: usize, total: u64) -> Element<'_, Message> {
    let c = theme::c();
    let summary = format!("{} entries · {files} files · {} unpacked", entries.len(), rc_core::format::size(total));
    let rows = entries.iter().take(ARCHIVE_ROWS).map(|e| {
        let size = if e.kind == EntryKind::Dir { "<DIR>".to_string() } else { rc_core::format::size(e.size) };
        row![
            text(e.name.as_str()).size(13).wrapping(Wrapping::None).width(Length::Fill),
            text(size).size(12).color(c.muted).wrapping(Wrapping::None),
        ]
        .spacing(8)
        .into()
    });
    let mut list = Column::with_children(rows).spacing(2).padding([4, 12]);
    if entries.len() > ARCHIVE_ROWS {
        list = list.push(text(format!("… and {} more", entries.len() - ARCHIVE_ROWS)).size(12).color(c.muted));
    }
    column![container(text(summary).size(12).color(c.muted)).padding([6, 12]), scrolled(list.into())].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_kind_of_entries() {
        let k = |kind, name: &str, virtual_| kind_of(kind, Path::new(name), virtual_);
        assert_eq!(k(EntryKind::Parent, "C:\\x", false), Kind::Blank);
        assert_eq!(k(EntryKind::Dir, "C:\\x", false), Kind::Folder);
        assert_eq!(k(EntryKind::Dir, "C:\\a.zip\\x", true), Kind::Virtual);
        assert_eq!(k(EntryKind::File, "C:\\a.zip\\b.txt", true), Kind::Virtual);
        assert_eq!(k(EntryKind::File, "C:\\a.zip", false), Kind::Archive);
        assert_eq!(k(EntryKind::File, "C:\\a.7z", false), Kind::Archive);
        assert_eq!(k(EntryKind::File, "C:\\p.PNG", false), Kind::Image);
        assert_eq!(k(EntryKind::File, "C:\\readme.md", false), Kind::Markdown);
        assert_eq!(k(EntryKind::File, "C:\\a.txt", false), Kind::Text);
        assert_eq!(k(EntryKind::File, "C:\\a.exe", false), Kind::Text);
    }

    #[test]
    fn gate_loads_only_the_newest_entry_one_at_a_time() {
        let mut g = Gate::default();
        // Fast moves: only the last debounce starts a load.
        g.change(1);
        g.change(2);
        g.change(3);
        assert_eq!(g.settle(1), None);
        assert_eq!(g.settle(2), None);
        assert_eq!(g.settle(3), Some(3));
        // Moves while loading: nothing more starts until it ends.
        g.change(4);
        assert_eq!(g.settle(4), None);
        g.change(5);
        assert_eq!(g.settle(5), None);
        // The stale result is dropped and the newest entry loads.
        assert_eq!(g.finish(3), (false, Some(5)));
        assert_eq!(g.finish(5), (true, None));
        // A move whose debounce hasn't elapsed doesn't start on finish.
        assert_eq!(g.settle(5), Some(5));
        g.change(6);
        assert_eq!(g.finish(5), (false, None));
        assert_eq!(g.settle(6), Some(6));
        assert_eq!(g.finish(6), (true, None));
    }

    #[test]
    fn previews_folders_and_text() {
        let dir = std::env::temp_dir().join(format!("rc-quick-view-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(dir.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        assert!(matches!(preview_of(Kind::Folder, &dir), Preview::Folder { dirs: 1, files: 2, size: None }));
        match preview_of(Kind::Text, &dir.join("a.txt")) {
            Preview::Doc(d) => assert_eq!((d.default_mode(), d.rows(Mode::Text)), (Mode::Text, 2)),
            other => panic!("{other:?}"),
        }
        match preview_of(Kind::Text, &dir.join("b.bin")) {
            Preview::Doc(d) => assert_eq!(d.default_mode(), Mode::Hex),
            other => panic!("{other:?}"),
        }
        assert!(matches!(preview_of(Kind::Text, &dir.join("missing")), Preview::Failed(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
