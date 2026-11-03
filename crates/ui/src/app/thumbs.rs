//! Thumbnail view (TC's `cm_SrcThumbs`, Ctrl+Shift+F5): a grid of square
//! tiles per tab instead of the list. Pictures are decoded off the UI
//! thread, only for the tiles on screen plus a row of margin, a few at a
//! time, and kept in an LRU cache; everything else (folders, other files,
//! anything in archives or on servers) shows its 48 px system icon.
//!
//! Decoding goes through the Windows shell (`rc_platform::thumbnail`): it
//! uses the system's thumbnail cache and the pictures' embedded previews,
//! so even a 50 MP JPEG is quick, and it covers videos and PDFs too. The
//! `image` crate is the fallback for pictures the shell can't do (and other
//! platforms).
//!
//! Navigation reuses the list's: `Panel` keeps scrolling in grid rows while
//! `thumbnails` is on (`Panel::set_grid`), selection and clicks are the
//! same messages, and only the cursor keys move in two dimensions.

use super::*;
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{column, container, image as image_widget, mouse_area, responsive, Column, Row, Space};
use iced::{Alignment, Border, ContentFit, Element, Length};
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::hash::Hash;

/// Padding inside a tile, around the picture and name.
const PAD: f32 = 6.0;
/// Gap between tiles (outside their highlight).
const GAP: f32 = 4.0;
/// Height of the name line under the picture.
const LABEL_H: f32 = 18.0;
/// Edge of the system icons shown for files without a thumbnail.
const BIG_ICON: f32 = 48.0;
/// Decodes running at once.
const MAX_DECODES: usize = 4;
/// Memory for cached thumbnails (RGBA bytes).
const CACHE_BYTES: usize = 64 << 20;

/// (columns, fully visible rows) of a grid of `size` px pictures in an
/// area of `width`×`height`.
pub(super) fn grid_dims(width: f32, height: f32, size: f32) -> (usize, usize) {
    let (w, h) = tile_size(size);
    let cols = (width / w).floor().max(1.0) as usize;
    let rows = (height / h).floor().max(1.0) as usize;
    (cols, rows)
}

/// Smallest tile (width, height) for `size` px pictures, gap included.
fn tile_size(size: f32) -> (f32, f32) {
    (size + 2.0 * PAD + GAP, size + LABEL_H + 2.0 * PAD + GAP)
}

/// A small least-recently-used cache, bounded by entry count.
#[derive(Debug)]
pub(super) struct Lru<K, V> {
    map: HashMap<K, (V, u64)>,
    /// Use stamp -> key, oldest first.
    order: BTreeMap<u64, K>,
    tick: u64,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V> Lru<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self { map: HashMap::new(), order: BTreeMap::new(), tick: 0, capacity: capacity.max(1) }
    }

    /// The value for `key`, marking it as just used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.tick += 1;
        let (_, stamp) = self.map.get_mut(key)?;
        let old = std::mem::replace(stamp, self.tick);
        let k = self.order.remove(&old)?;
        self.order.insert(self.tick, k);
        self.map.get(key).map(|(v, _)| v)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Adds or replaces `key`, dropping the least recently used entries
    /// beyond the capacity.
    pub fn insert(&mut self, key: K, value: V) {
        self.tick += 1;
        if let Some((_, old)) = self.map.insert(key.clone(), (value, self.tick)) {
            self.order.remove(&old);
        }
        self.order.insert(self.tick, key);
        while self.map.len() > self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            self.map.remove(&oldest);
        }
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.map.len() > self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            self.map.remove(&oldest);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// What a thumbnail was made from: a changed file or size makes a new one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ThumbKey {
    path: PathBuf,
    /// Modification time, ms since the epoch.
    modified: Option<i64>,
    len: u64,
    size: u32,
}

impl ThumbKey {
    fn of(entry: &Entry, size: u32) -> Self {
        Self { path: entry.path.clone(), modified: entry.modified.map(|t| t.timestamp_millis()), len: entry.size, size }
    }
}

/// The thumbnail view's state in [`App`].
#[derive(Debug)]
pub(super) struct Thumbs {
    /// Decoded thumbnails; `None` = the file has none (shown as an icon).
    /// Read from `view`, hence the RefCell.
    cache: RefCell<Lru<ThumbKey, Option<image::Handle>>>,
    /// Being decoded right now.
    decoding: HashSet<ThumbKey>,
    /// 48 px system icons by `rc_platform::icon_key`, filled from `view`.
    icons: RefCell<HashMap<String, Option<image::Handle>>>,
    /// Size of each side's grid area as last drawn (0 = not drawn yet).
    area: [Cell<(f32, f32)>; 2],
}

impl Default for Thumbs {
    fn default() -> Self {
        Self {
            cache: RefCell::new(Lru::new(1024)),
            decoding: HashSet::new(),
            icons: RefCell::new(HashMap::new()),
            area: [Cell::new((0.0, 0.0)), Cell::new((0.0, 0.0))],
        }
    }
}

/// Decodes a thumbnail (on a worker thread): the shell's, else the `image`
/// crate's for the formats it knows.
fn decode(path: &Path, size: u32) -> Option<image::Handle> {
    if let Some(t) = rc_platform::thumbnail(path, size) {
        return Some(image::Handle::from_rgba(t.width, t.height, t.rgba));
    }
    ::image::ImageFormat::from_path(path).ok()?;
    let picture = ::image::open(path).ok()?.thumbnail(size, size).to_rgba8();
    let (w, h) = picture.dimensions();
    Some(image::Handle::from_rgba(w, h, picture.into_raw()))
}

impl App {
    fn thumb_size(&self) -> u32 {
        self.config.thumbnail_size()
    }

    /// Ctrl+Shift+F5: thumbnail view on / off for the active tab.
    pub(super) fn toggle_thumbnails(&mut self) {
        let side = self.active;
        let rows = self.page_rows();
        let panel = self.panel_mut(side);
        panel.thumbnails = !panel.thumbnails;
        self.sync_grids();
        self.panel_mut(side).ensure_visible(rows);
    }

    /// Gives every tab the grid layout of its side, from the area drawn
    /// last (estimated before the first draw).
    pub(super) fn sync_grids(&mut self) {
        let size = self.thumb_size() as f32;
        let bar = if self.config.show_fkeys { 0.0 } else { FKEYS_H };
        let estimate = (self.panel_width(1).min(self.panel_width(0)) - scrollbar::WIDTH, self.window_height - CHROME_H + bar);
        for side in 0..2 {
            let (w, h) = match self.thumbs.area[side].get() {
                (w, h) if w > 0.0 && h > 0.0 => (w, h),
                _ => estimate,
            };
            let (cols, rows) = grid_dims(w, h, size);
            for tab in &mut self.sides[side].tabs {
                tab.set_grid(cols, rows);
            }
        }
    }

    /// Starts decoding the thumbnails the visible grids need (the screen
    /// plus a row above and below), up to `MAX_DECODES` at once; the
    /// active side goes first.
    pub(super) fn pump_thumbs(&mut self) -> Task<Message> {
        if !self.sides.iter().any(|s| s.tabs[s.current].thumbnails) {
            return Task::none();
        }
        let size = self.thumb_size();
        let capacity = (CACHE_BYTES / (size as usize * size as usize * 4)).clamp(64, 4096);
        self.thumbs.cache.borrow_mut().set_capacity(capacity);
        let free = MAX_DECODES.saturating_sub(self.thumbs.decoding.len());
        if free == 0 {
            return Task::none();
        }
        let mut wanted = Vec::new();
        for side in [self.active, 1 - self.active] {
            let panel = self.panel(side);
            if !panel.thumbnails || vfs::in_archive(&panel.path) {
                continue;
            }
            let (cols, rows) = panel.grid();
            let start = panel.view_start(rows);
            // Visible rows first, then the margin below, then above.
            let visible = start * cols..(start + rows + 1) * cols;
            let below = visible.end..visible.end + cols;
            let above = start.saturating_sub(1) * cols..start * cols;
            for i in visible.chain(below).chain(above) {
                let Some(entry) = panel.entries.get(i).filter(|e| e.kind == EntryKind::File) else { continue };
                let key = ThumbKey::of(entry, size);
                if !self.thumbs.decoding.contains(&key)
                    && !self.thumbs.cache.borrow().contains(&key)
                    && !wanted.contains(&key)
                {
                    wanted.push(key);
                    if wanted.len() == free {
                        break;
                    }
                }
            }
            if wanted.len() == free {
                break;
            }
        }
        Task::batch(wanted.into_iter().map(|key| {
            self.thumbs.decoding.insert(key.clone());
            let (tx, rx) = iced::futures::channel::oneshot::channel();
            let (path, size) = (key.path.clone(), key.size);
            std::thread::spawn(move || {
                let _ = tx.send(decode(&path, size));
            });
            Task::perform(rx, move |r| Message::Thumb(key.clone(), r.ok().flatten()))
        }))
    }

    /// A decode finished.
    pub(super) fn thumb_done(&mut self, key: ThumbKey, handle: Option<image::Handle>) {
        self.thumbs.decoding.remove(&key);
        self.thumbs.cache.borrow_mut().insert(key, handle);
    }

    /// Cached 48 px system icon.
    fn big_icon(&self, path: &Path, is_dir: bool) -> Option<image::Handle> {
        let key = rc_platform::icon_key(path, is_dir);
        if let Some(handle) = self.thumbs.icons.borrow().get(&key) {
            return handle.clone();
        }
        let handle = rc_platform::large_icon(path, is_dir)
            .map(|icon| image::Handle::from_rgba(icon.width, icon.height, icon.rgba));
        self.thumbs.icons.borrow_mut().insert(key, handle.clone());
        handle
    }

    /// The grid, virtualised like the list: only the rows on screen (and a
    /// partly visible one below them) are built.
    pub(super) fn thumb_grid(&self, side: usize, active: bool) -> Element<'_, Message> {
        let panel = self.panel(side);
        let size = self.thumb_size() as f32;
        let icons_only = vfs::in_archive(&panel.path);
        responsive(move |area| {
            let width = (area.width - scrollbar::WIDTH).max(1.0);
            self.thumbs.area[side].set((width, area.height));
            let (cols, rows) = grid_dims(width, area.height, size);
            let len = panel.entries.len();
            let total = len.div_ceil(cols);
            // The panel's layout is from the last draw; after a resize this
            // frame's may differ for a moment.
            let start = panel.view_start(rows).min(total.saturating_sub(rows));
            let tile_w = (width / cols as f32).floor();
            let tile_h = tile_size(size).1;
            let grid = Column::with_children((start..(start + rows + 1).min(total)).map(|r| {
                Row::with_children(
                    (r * cols..((r + 1) * cols).min(len))
                        .map(|i| self.tile(side, i, active, tile_w, tile_h, size, icons_only)),
                )
                .into()
            }));
            let grid = container(grid).width(Length::Fill).height(Length::Fill).clip(true);
            let bar = scrollbar::attach(grid, start as f32, rows as f32, total as f32, move |at| {
                Message::ScrollTo(side, at.round() as usize, rows)
            });
            container(bar).width(Length::Fill).height(Length::Fill).style(filled(theme::c().panel)).into()
        })
        .into()
    }

    #[allow(clippy::too_many_arguments)]
    fn tile(
        &self,
        side: usize,
        index: usize,
        active: bool,
        tile_w: f32,
        tile_h: f32,
        size: f32,
        icons_only: bool,
    ) -> Element<'_, Message> {
        let panel = self.panel(side);
        let entry = &panel.entries[index];
        let selected = panel.is_selected(entry);
        let thumb = (!icons_only && entry.kind == EntryKind::File)
            .then(|| self.thumbs.cache.borrow_mut().get(&ThumbKey::of(entry, size as u32)).cloned().flatten())
            .flatten();
        let opacity = if entry.hidden { 0.5_f32 } else { 1.0 };
        let picture: Element<'_, Message> = match thumb {
            Some(handle) => image_widget(handle).content_fit(ContentFit::ScaleDown).opacity(opacity).into(),
            None => match self.big_icon(&entry.path, entry.is_dir()).or_else(|| self.icon(&entry.path, entry.is_dir())) {
                Some(handle) => image_widget(handle).width(BIG_ICON).height(BIG_ICON).opacity(opacity).into(),
                None => Space::new(BIG_ICON, BIG_ICON).into(),
            },
        };
        let picture = container(picture).center(Length::Fixed(size));

        let color = if selected {
            theme::c().selected
        } else if entry.is_dir() {
            theme::c().dir
        } else if entry.hidden {
            theme::c().muted
        } else {
            theme::c().text
        };
        let renaming = self.rename.as_ref().filter(|r| r.side == side && r.src == entry.path);
        let label: Element<'_, Message> = match renaming {
            Some(r) => SelectRange::new(
                text_input("", &r.text)
                    .id(rename_input_id())
                    .on_input(Message::RenameInput)
                    .on_submit(Message::RenameSubmit)
                    .size(12)
                    .padding([0, 2])
                    .width(Length::Fill)
                    .style(theme::input),
                rename_input_id(),
                r.select,
                r.selection(),
            )
            .into(),
            None => {
                let chars = ((tile_w - GAP - 2.0 * PAD) / 6.5).max(3.0) as usize;
                iced::widget::text(view::elide_end(&entry.name, chars))
                    .size(12)
                    .color(color)
                    .wrapping(Wrapping::None)
                    .width(Length::Fill)
                    .align_x(Alignment::Center)
                    .into()
            }
        };

        let drop_target = self.drag.as_ref().is_some_and(|d| {
            d.active && self.hover == Some((side, index)) && entry.is_dir() && !d.sources.contains(&entry.path)
        });
        let background = if drop_target {
            Some(theme::c().drop)
        } else if index == panel.cursor {
            Some(if active { theme::c().cursor } else { theme::c().cursor_inactive })
        } else {
            None
        };
        let border = if selected { theme::c().selected } else { iced::Color::TRANSPARENT };
        let card = container(column![picture, label].spacing(2).align_x(Alignment::Center))
            .padding(PAD)
            .width(Length::Fill)
            .height(Length::Fill)
            .clip(true)
            .style(move |_| container::Style {
                background: background.map(Into::into),
                border: Border { color: border, width: 2.0, radius: 4.0.into() },
                ..container::Style::default()
            });
        let cell = container(card).padding(GAP / 2.0).width(tile_w).height(tile_h);

        let mut area = mouse_area(cell)
            .on_press(Message::Click(side, index))
            .on_middle_press(Message::MiddleClick(side, index))
            .on_right_press(Message::RightPress(side, index));
        if self.dragging() {
            area = area.on_enter(Message::Hover(side, index));
        }
        area.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_drops_the_least_recently_used() {
        let mut lru = Lru::new(3);
        lru.insert("a", 1);
        lru.insert("b", 2);
        lru.insert("c", 3);
        assert_eq!(lru.get(&"a"), Some(&1)); // a is now the newest
        lru.insert("d", 4); // drops b
        assert_eq!(lru.len(), 3);
        assert!(!lru.contains(&"b"));
        assert!(lru.contains(&"a") && lru.contains(&"c") && lru.contains(&"d"));
        // Replacing a value refreshes it without growing.
        lru.insert("c", 30);
        lru.insert("e", 5); // drops a (c was refreshed after a was used)
        assert!(!lru.contains(&"a"));
        assert_eq!(lru.get(&"c"), Some(&30));
        assert_eq!(lru.get(&"zz"), None);
        lru.set_capacity(1);
        assert_eq!(lru.len(), 1);
        assert!(lru.contains(&"c"));
    }

    #[test]
    fn grid_fits_whole_tiles() {
        let (w, h) = tile_size(128.0);
        assert_eq!(grid_dims(w * 3.0, h * 2.0, 128.0), (3, 2));
        assert_eq!(grid_dims(w * 3.0 - 1.0, h * 2.0 - 1.0, 128.0), (2, 1));
        // Always at least one tile.
        assert_eq!(grid_dims(10.0, 10.0, 128.0), (1, 1));
    }
}
