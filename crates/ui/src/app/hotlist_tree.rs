//! The hotlist tree: the directory hotlist (Ctrl+D) as a folder tree with
//! its groups, in a column left of both panels (setting `hotlist_tree`; the
//! button left of the left panel's drive buttons or Ctrl+Shift+D show and
//! hide it; its divider resizes it). It has a keyboard focus of its own
//! (`HotTree::focused`, Tab cycles tree, left, right); folders picked in it
//! open in the last active panel, and other commands act on that panel. It
//! edits the same list as Ctrl+D (`App::hotlist`), saved at once.

use super::*;
use crate::hotlist::{self, HotItem, TreeRow};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, image, mouse_area, opaque, responsive, row, text, text_input, tooltip, Column, Row, Space};
use iced::{Alignment, Color, Element, Length, Padding};
use crate::theme::filled;
use std::cell::{Cell, RefCell};

const ICON: f32 = 16.0;
/// Indent per group level.
const INDENT: f32 = 16.0;
/// Room for a group's open/closed triangle.
const CHEVRON: f32 = 14.0;
const MENU_W: f32 = 290.0;
const MENU_ROW_H: f32 = 26.0;

fn name_input_id() -> text_input::Id {
    text_input::Id::new("tree-name-input")
}

/// Tree state; the entries themselves are `App::hotlist`.
#[derive(Debug, Default)]
pub(super) struct HotTree {
    /// Shown left of the panels (saved with the session).
    pub shown: bool,
    /// Width as set with the divider (saved; `App::tree_width` keeps it
    /// within the window).
    pub width: f32,
    /// The left panel is hidden while the tree is shown (saved).
    pub hide_left: bool,
    /// Has the keyboard focus; `App::active` is then the last active
    /// panel, which commands other than the tree's own act on.
    pub focused: bool,
    /// The divider right of it is dragged; when it was last pressed
    /// (double click: default width).
    resizing: bool,
    resize_click: Option<Instant>,
    /// Path of the entry with the cursor (see [`hotlist::get`]).
    pub cursor: Vec<usize>,
    /// First row drawn; `detached` while the wheel or scrollbar moved it
    /// away from the cursor.
    scroll: usize,
    detached: bool,
    /// Rows that fit, as last drawn (page keys, keeping the cursor shown).
    fit: Cell<usize>,
    /// A group's name being typed (new group or F2).
    pub naming: Option<Naming>,
    /// Left press on an entry: a click on release, or a drag to move it.
    press: Option<Press>,
    /// While something is dragged over the list: mouse y below its top.
    over: Option<f32>,
    /// Right-click menu.
    menu: Option<TreeMenu>,
    /// Icons drawn in code, per shape and colour.
    glyphs: RefCell<HashMap<(Glyph, [u8; 4]), image::Handle>>,
}

impl HotTree {
    pub fn new(shown: bool, width: f32, hide_left: bool) -> Self {
        Self { shown, width, hide_left, ..Self::default() }
    }
}

#[derive(Debug)]
pub(super) struct Naming {
    path: Vec<usize>,
    text: String,
    /// A group just made: Esc takes it away again.
    new: bool,
}

#[derive(Debug)]
struct Press {
    path: Vec<usize>,
    start: Option<Point>,
    active: bool,
}

#[derive(Debug)]
struct TreeMenu {
    /// Top left corner (window coordinates).
    at: Point,
    /// Entry right-clicked; `None`: the empty part of the list.
    target: Option<Vec<usize>>,
    selected: usize,
}

/// What the context menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    OpenNewTab,
    Expand,
    Rename,
    NewGroup,
    AddCurrent,
    Remove,
}

#[derive(Debug, Clone)]
pub enum TreeMsg {
    /// The button left of the drive buttons.
    Toggle,
    /// The "hide / show left panel" button in the tree's title.
    ToggleLeft,
    /// Press on the divider right of the tree.
    ResizePress,
    Press(Vec<usize>),
    Middle(Vec<usize>),
    /// Right press on an entry, or on the empty part of the list.
    Right(Option<Vec<usize>>),
    MenuAt(Option<Vec<usize>>, Option<(f32, f32)>),
    MenuPick(usize),
    MenuClose,
    /// Left press on the empty part of the list.
    Background,
    /// Mouse y over the list while dragging; left it.
    Over(f32),
    Out,
    Wheel(f32),
    ScrollTo(usize),
    NameInput(String),
    NameSubmit,
    /// Header buttons.
    NewGroup,
    AddCurrent,
}

/// Where a drop goes: index `index` of the menu at `at`, and how to show it.
#[derive(Debug, Clone, PartialEq)]
struct Spot {
    at: Vec<usize>,
    index: usize,
    mark: Mark,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mark {
    /// A line above or below row `row`, indented to `depth`.
    Line { row: usize, below: bool, depth: usize },
    /// Into the group on row `row`.
    Into(usize),
    /// After the last row.
    End,
}

/// Commands (not handled by `tree_key`) that, run while the tree has the
/// focus, move the focus to the last active panel: they need typing there.
/// All others act on that panel and leave the focus in the tree.
pub(super) fn takes_focus(cmd: Command) -> bool {
    matches!(cmd, Command::QuickFilter | Command::Rename)
}

impl App {
    /// Keys of `tree_key`, for the F1 help. Keep in sync.
    pub(super) const TREE_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Ctrl+Shift+D, button left of the drives", "Show / hide the hotlist tree left of the panels (setting \"Hotlist tree\", F10)"),
        ("Drag its right edge", "Resize the tree (double-click: default width)"),
        ("Tab, Shift+Tab", "Focus tree, left panel, right panel in turn"),
        ("Up, Down, PgUp, PgDn, Home, End", "Move the cursor; letters jump to the next entry starting with them"),
        ("Enter, click", "Open the folder in the last active panel (the tree keeps the focus) / open or close the group"),
        ("Shift+Enter, middle-click", "Open the folder in a new tab of the last active panel"),
        ("Right / Left", "Open the group (then to its first entry) / close it, or go to the group holding the entry"),
        ("Space", "Open or close the group"),
        ("F7 (new folder key)", "New group after the cursor: type its name, Enter"),
        ("F2", "Rename the group; on a folder: edit its display name and comment (tooltip)"),
        ("+", "Add the last active panel's folder (into the group, or after the entry)"),
        ("Del", "Remove the entry (asks first for a group with entries)"),
        ("Ctrl+Up, Ctrl+Down", "Move the entry up / down"),
        ("Ctrl+Right / Ctrl+Left", "Move the entry into the group above it / out of its group"),
        ("Drag", "Move entries and groups, also into and out of groups; a folder dropped on a panel opens there"),
        ("Drag folders from a panel", "Add them to the hotlist where dropped (Explorer drops: at the end)"),
        ("Right-click", "Menu: open in a new tab, rename or comment, new group, add the last active panel's folder, remove"),
        ("Other keys", "Commands act on the last active panel as if it had the focus (Ctrl+S, Shift+F6 move the focus there)"),
    ];

    /// The tree is drawn left of the panels.
    pub(super) fn tree_visible(&self) -> bool {
        self.config.hotlist_tree && self.tree.shown
    }

    /// The left panel is hidden (tree shown with "hide left panel" on).
    pub(super) fn left_hidden(&self) -> bool {
        self.tree.hide_left && self.tree_visible()
    }

    /// The tree has the keyboard focus.
    pub(super) fn tree_focused(&self) -> bool {
        self.tree.focused && self.tree_visible()
    }

    pub(super) fn tree_dragging(&self) -> bool {
        self.tree.press.as_ref().is_some_and(|p| p.active)
    }

    /// Mouse moves matter: an entry is pressed (a drag may start), or the
    /// divider is dragged.
    pub(super) fn tree_tracks_mouse(&self) -> bool {
        self.tree.press.is_some() || self.tree.resizing
    }

    /// Ctrl+Shift+D / the button: show or hide the tree (hiding it gives
    /// the focus back to the last active panel).
    pub(super) fn toggle_tree(&mut self) -> Task<Message> {
        if !self.config.hotlist_tree {
            self.info("Turn on \"Hotlist tree\" in the settings (F10) first");
            return Task::none();
        }
        self.leave_edits();
        let t = &mut self.tree;
        t.shown = !t.shown;
        t.menu = None;
        t.press = None;
        t.over = None;
        t.resizing = false;
        self.split_ghost = None;
        t.focused = t.shown;
        if t.shown {
            // Start on the active panel's folder if it is in the list.
            if let Some((mut at, i)) = hotlist::find(&self.hotlist, &self.panel(self.active).path) {
                at.push(i);
                if hotlist::rows(&self.hotlist).iter().any(|r| r.path == at) {
                    self.tree.cursor = at;
                }
            }
            self.tree_fix_cursor();
            self.tree.detached = false;
        }
        self.sync_grids();
        self.save_session();
        Task::none()
    }

    /// Hide or show the left panel (needs the tree shown); hiding it makes
    /// the right panel the active one.
    pub(super) fn toggle_hide_left(&mut self) -> Task<Message> {
        if !self.tree_visible() {
            self.info("Show the hotlist tree first (Ctrl+Shift+D)");
            return Task::none();
        }
        self.leave_edits();
        self.tree.hide_left = !self.tree.hide_left;
        self.tree.menu = None;
        if self.tree.hide_left {
            self.active = 1;
        }
        self.sync_grids();
        self.save_session();
        Task::none()
    }

    /// Keeps the cursor on a shown entry after the list changed: the same
    /// place, the last entry of its menu, or the nearest group around it.
    pub(super) fn tree_fix_cursor(&mut self) {
        let rows = hotlist::rows(&self.hotlist);
        let Some(first) = rows.first() else {
            self.tree.cursor.clear();
            return;
        };
        let mut p = self.tree.cursor.clone();
        loop {
            let Some((&last, at)) = p.split_last() else {
                self.tree.cursor = first.path.clone();
                return;
            };
            match hotlist::menu_at(&self.hotlist, at).map(<[HotItem]>::len) {
                Some(n) if n > 0 => {
                    let at = at.to_vec();
                    p = at;
                    p.push(last.min(n - 1));
                }
                _ => {
                    p.pop();
                    continue;
                }
            }
            if rows.iter().any(|r| r.path == p) {
                self.tree.cursor = p;
                return;
            }
            p.pop();
        }
    }

    fn tree_set_cursor(&mut self, path: Vec<usize>) {
        self.tree.cursor = path;
        self.tree.detached = false;
        let rows = hotlist::rows(&self.hotlist);
        if let Some(i) = rows.iter().position(|r| r.path == self.tree.cursor) {
            let fit = self.tree.fit.get().max(1);
            self.tree.scroll = rc_core::panel::visible_start(self.tree.scroll, i, fit, rows.len());
        }
    }

    /// First row to draw when `fit` rows fit.
    fn tree_start(&self, rows: &[TreeRow], fit: usize) -> usize {
        let len = rows.len();
        if self.tree.detached {
            return self.tree.scroll.min(len.saturating_sub(fit));
        }
        let cursor = rows.iter().position(|r| r.path == self.tree.cursor).unwrap_or(0);
        rc_core::panel::visible_start(self.tree.scroll, cursor, fit, len)
    }

    /// Ends typing a group's name, keeping it (a click elsewhere).
    pub(super) fn tree_leave_naming(&mut self) {
        if self.tree.naming.is_some() {
            self.tree_commit_naming();
        }
    }

    fn tree_commit_naming(&mut self) {
        let Some(n) = self.tree.naming.take() else { return };
        let name = n.text.trim().to_owned();
        if name.is_empty() {
            if n.new {
                hotlist::take(&mut self.hotlist, &n.path);
                self.tree_fix_cursor();
            }
        } else if let Some(HotItem::Menu { name: old, .. }) = hotlist::get_mut(&mut self.hotlist, &n.path) {
            *old = name;
        }
        self.save_session();
    }

    fn tree_cancel_naming(&mut self) {
        if let Some(n) = self.tree.naming.take() {
            if n.new {
                hotlist::take(&mut self.hotlist, &n.path);
                self.tree_fix_cursor();
                self.save_session();
            }
        }
    }

    fn tree_start_naming(&mut self, path: Vec<usize>, new: bool) -> Task<Message> {
        if matches!(hotlist::get(&self.hotlist, &path), Some(HotItem::Folder { .. })) {
            self.tree_set_cursor(path.clone());
            return self.hotlist_edit(path, None);
        }
        let Some(HotItem::Menu { name, .. }) = hotlist::get(&self.hotlist, &path) else { return Task::none() };
        self.tree.naming = Some(Naming { path: path.clone(), text: name.clone(), new });
        self.tree_set_cursor(path);
        Task::batch([text_input::focus(name_input_id()), text_input::select_all(name_input_id())])
    }

    /// New group after `target` (or at the end), then its name is typed.
    fn tree_new_group(&mut self, target: Option<Vec<usize>>) -> Task<Message> {
        self.leave_edits();
        self.tree.focused = true;
        let (at, index) = match target.as_deref().and_then(|p| p.split_last()) {
            Some((&last, at)) => (at.to_vec(), last + 1),
            None => (Vec::new(), self.hotlist.len()),
        };
        let Some(path) = hotlist::insert(&mut self.hotlist, &at, index, HotItem::group("New group".into())) else {
            return Task::none();
        };
        self.save_session();
        self.tree_start_naming(path, true)
    }

    /// Where `+` / "Add the current folder" put it: into a group,
    /// after a folder, or at the end.
    fn tree_add_spot(&self, target: Option<&[usize]>) -> (Vec<usize>, usize) {
        match target.and_then(|p| Some((p, hotlist::get(&self.hotlist, p)?))) {
            Some((p, HotItem::Menu { items, .. })) => (p.to_vec(), items.len()),
            Some((p, HotItem::Folder { .. })) => {
                let (&last, at) = p.split_last().unwrap_or((&0, &[]));
                (at.to_vec(), last + 1)
            }
            None => (Vec::new(), self.hotlist.len()),
        }
    }

    /// Adds `dirs` at index `index` of the menu at `at`, skipping folders
    /// already in that menu; the cursor goes to the last one.
    fn tree_add_folders(&mut self, dirs: Vec<PathBuf>, at: &[usize], index: usize) {
        if dirs.is_empty() {
            self.info("Only folders go into the hotlist");
            return;
        }
        let Some(menu) = hotlist::menu_at(&self.hotlist, at) else { return };
        let (new, known): (Vec<PathBuf>, Vec<PathBuf>) =
            dirs.into_iter().partition(|d| !menu.iter().any(|it| it.folder() == Some(d.as_path())));
        if new.is_empty() {
            let shown = known.first().map(|d| d.display().to_string()).unwrap_or_default();
            self.status = Some((format!("{shown} is in the hotlist already"), Tone::Info));
            if let (Some(d), true) = (known.first(), known.len() == 1) {
                if let Some(i) = menu.iter().position(|it| it.folder() == Some(d.as_path())) {
                    let mut path = at.to_vec();
                    path.push(i);
                    self.tree_set_cursor(path);
                }
            }
            return;
        }
        let msg = match new.as_slice() {
            [one] => format!("Added {} to the hotlist", one.display()),
            many => format!("Added {} folders to the hotlist", many.len()),
        };
        let mut last = None;
        for (k, dir) in new.into_iter().enumerate() {
            last = hotlist::insert(&mut self.hotlist, at, index + k, HotItem::folder_at(dir));
        }
        self.tree_expand_to(at);
        if let Some(path) = last {
            self.tree_set_cursor(path);
        }
        self.success(msg);
        self.save_session();
    }

    /// Opens the group at `at` and every group around it.
    fn tree_expand_to(&mut self, at: &[usize]) {
        for n in 1..=at.len() {
            hotlist::set_expanded(&mut self.hotlist, &at[..n], Some(true));
        }
    }

    fn tree_add_current(&mut self, target: Option<Vec<usize>>) {
        self.leave_edits();
        let (at, index) = self.tree_add_spot(target.as_deref());
        let dir = self.panel(self.active).path.clone();
        self.tree_add_folders(vec![dir], &at, index);
    }

    /// Opens the folder at `path` in the last active panel (or a new tab
    /// there); the focus stays where it is. A group opens or closes instead.
    fn tree_open(&mut self, path: &[usize], new_tab: bool) -> Task<Message> {
        match hotlist::get(&self.hotlist, path) {
            Some(HotItem::Folder { path: dir, .. }) => {
                let dir = dir.clone();
                self.tree_set_cursor(path.to_vec());
                let side = self.active;
                if new_tab { self.open_tab(side, dir, true) } else { self.start_nav(side, Nav::to(dir)) }
            }
            Some(HotItem::Menu { .. }) if !new_tab => {
                self.tree_toggle_group(path, None);
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn tree_toggle_group(&mut self, path: &[usize], open: Option<bool>) {
        if hotlist::set_expanded(&mut self.hotlist, path, open).is_some() {
            self.tree_set_cursor(path.to_vec());
            self.save_session();
        }
    }

    /// Del: a folder or an empty group goes at once; a group with entries
    /// asks first.
    fn tree_remove(&mut self, path: Vec<usize>) -> Task<Message> {
        match hotlist::get(&self.hotlist, &path) {
            Some(group @ HotItem::Menu { name, .. }) if group.count() > 0 => {
                let dialog = Dialog::RemoveGroup { name: name.clone(), count: group.count(), path };
                self.open_dialog(dialog)
            }
            Some(_) => {
                self.tree_remove_now(&path);
                Task::none()
            }
            None => Task::none(),
        }
    }

    pub(super) fn tree_remove_now(&mut self, path: &[usize]) {
        let Some(item) = hotlist::take(&mut self.hotlist, path) else { return };
        let what = match &item {
            HotItem::Folder { path, .. } => path.display().to_string(),
            HotItem::Menu { name, .. } => format!("the group \"{name}\""),
        };
        self.info(format!("Removed {what} from the hotlist"));
        self.tree.cursor = path.to_vec();
        self.tree_fix_cursor();
        self.save_session();
    }

    /// Ctrl+arrows: move the entry up / down, into the group above it, or
    /// out of its group (like Ctrl+D).
    fn tree_shift(&mut self, key: Named) {
        let Some((&i, at)) = self.tree.cursor.split_last() else { return };
        let at = at.to_vec();
        let place = match key {
            Named::ArrowUp | Named::ArrowDown => {
                let delta = if key == Named::ArrowUp { -1 } else { 1 };
                hotlist::menu_at_mut(&mut self.hotlist, &at).map(|items| (at.clone(), hotlist::shift(items, i, delta)))
            }
            Named::ArrowRight => {
                hotlist::menu_at_mut(&mut self.hotlist, &at).and_then(|items| hotlist::indent(items, i)).map(|m| {
                    let mut group = at.clone();
                    group.push(m);
                    let n = hotlist::menu_at(&self.hotlist, &group).map_or(1, <[HotItem]>::len);
                    (group, n - 1)
                })
            }
            Named::ArrowLeft => hotlist::outdent(&mut self.hotlist, &at, i),
            _ => None,
        };
        if let Some((at, index)) = place {
            self.tree_expand_to(&at);
            let mut path = at;
            path.push(index);
            self.tree_set_cursor(path);
            self.save_session();
        }
    }

    /// Keys while the tree has the focus; `None`: not a tree key (the
    /// key map decides, see `in_tree`).
    pub(super) fn tree_key(
        &mut self,
        key: &keyboard::Key,
        named: Option<Named>,
        mods: keyboard::Modifiers,
        text: Option<&str>,
    ) -> Option<Task<Message>> {
        if named == Some(Named::Escape) {
            self.tree.press = None;
            return Some(Task::none());
        }
        self.tree_fix_cursor();
        let rows = hotlist::rows(&self.hotlist);
        let row = rows.iter().position(|r| r.path == self.tree.cursor);
        let cur = self.tree.cursor.clone();
        let (is_group, expanded, has_items) = match hotlist::get(&self.hotlist, &cur) {
            Some(HotItem::Menu { expanded, items, .. }) => (true, *expanded, !items.is_empty()),
            _ => (false, false, false),
        };
        let go = |this: &mut Self, i: isize| {
            if let (Some(row), false) = (row, rows.is_empty()) {
                let to = (row as isize + i).clamp(0, rows.len() as isize - 1) as usize;
                this.tree_set_cursor(rows[to].path.clone());
            }
        };
        let none = Some(Task::none());
        if mods.control() && !mods.alt() {
            if let Some(k @ (Named::ArrowUp | Named::ArrowDown | Named::ArrowLeft | Named::ArrowRight)) = named {
                self.tree_shift(k);
                return none;
            }
        }
        let plain = !mods.control() && !mods.alt() && !mods.logo();
        match named {
            Some(Named::ArrowRight) if plain => {
                if is_group && !expanded {
                    self.tree_toggle_group(&cur, Some(true));
                } else if is_group && has_items {
                    go(self, 1);
                }
                return none;
            }
            Some(Named::ArrowLeft) if plain => {
                if is_group && expanded {
                    self.tree_toggle_group(&cur, Some(false));
                } else if cur.len() > 1 {
                    self.tree_set_cursor(cur[..cur.len() - 1].to_vec());
                }
                return none;
            }
            Some(Named::Enter) if mods.shift() && !mods.control() => return Some(self.tree_open(&cur, true)),
            Some(Named::F2) if plain && row.is_some() => return Some(self.tree_start_naming(cur, false)),
            _ => {}
        }
        if let Some(t) = text.filter(|t| plain && !t.is_empty() && !t.chars().any(char::is_control)) {
            match t {
                "+" => self.tree_add_current(row.map(|_| cur)),
                " " if is_group => self.tree_toggle_group(&cur, None),
                " " => {}
                _ => {
                    // Jump to the next entry starting with the letter.
                    let t = t.to_lowercase();
                    let from = row.map_or(0, |r| r + 1);
                    let hit = (0..rows.len()).map(|k| (from + k) % rows.len()).find(|&k| {
                        hotlist::get(&self.hotlist, &rows[k].path).is_some_and(|it| it.label().to_lowercase().starts_with(&t))
                    });
                    if let Some(k) = hit {
                        self.tree_set_cursor(rows[k].path.clone());
                    }
                }
            }
            return none;
        }
        use Command::*;
        let page = self.tree.fit.get().saturating_sub(1).max(1) as isize;
        match self.keymap.command(key, mods)? {
            CursorUp => go(self, -1),
            CursorDown => go(self, 1),
            PageUp => go(self, -page),
            PageDown => go(self, page),
            CursorHome => go(self, -(rows.len() as isize)),
            CursorEnd => go(self, rows.len() as isize),
            Open if row.is_some() => return Some(self.tree_open(&cur, false)),
            OpenInNewTab if row.is_some() => return Some(self.tree_open(&cur, true)),
            GoParent if cur.len() > 1 => self.tree_set_cursor(cur[..cur.len() - 1].to_vec()),
            ToggleSelect { .. } if is_group => self.tree_toggle_group(&cur, None),
            Rename if row.is_some() => return Some(self.tree_start_naming(cur, false)),
            MakeDir => return Some(self.tree_new_group(row.map(|_| cur))),
            Delete | DeletePermanent if row.is_some() => return Some(self.tree_remove(cur)),
            Open | OpenInNewTab | GoParent | ToggleSelect { .. } | Rename | Delete | DeletePermanent => {}
            _ => return None,
        }
        none
    }

    /// Keys while the naming box or the context menu is open; `None` when
    /// neither is.
    pub(super) fn tree_modal_key(&mut self, named: Option<Named>) -> Option<Task<Message>> {
        if self.tree.naming.is_some() {
            if named == Some(Named::Escape) {
                self.tree_cancel_naming();
            }
            return Some(Task::none());
        }
        let target = self.tree.menu.as_ref()?.target.clone();
        let count = self.tree_actions(target.as_deref()).len();
        let menu = self.tree.menu.as_mut()?;
        match named {
            Some(Named::ArrowUp) => menu.selected = (menu.selected + count - 1) % count,
            Some(Named::ArrowDown) => menu.selected = (menu.selected + 1) % count,
            Some(Named::Home | Named::PageUp) => menu.selected = 0,
            Some(Named::End | Named::PageDown) => menu.selected = count - 1,
            Some(Named::Enter) => {
                let i = menu.selected;
                return Some(self.tree_menu_pick(i));
            }
            Some(n) if is_modifier_key(n) => {}
            _ => self.tree.menu = None,
        }
        Some(Task::none())
    }

    /// The context menu's entries for `target` (in `TreeMenu` order).
    fn tree_actions(&self, target: Option<&[usize]>) -> Vec<Action> {
        use Action::*;
        match target.and_then(|p| hotlist::get(&self.hotlist, p)) {
            Some(HotItem::Folder { .. }) => vec![Open, OpenNewTab, Rename, NewGroup, AddCurrent, Remove],
            Some(HotItem::Menu { .. }) => vec![Expand, Rename, NewGroup, AddCurrent, Remove],
            None => vec![NewGroup, AddCurrent],
        }
    }

    fn tree_menu_pick(&mut self, i: usize) -> Task<Message> {
        let Some(menu) = self.tree.menu.take() else { return Task::none() };
        let target = menu.target;
        let Some(&action) = self.tree_actions(target.as_deref()).get(i) else { return Task::none() };
        let path = target.clone().unwrap_or_default();
        match action {
            Action::Open => self.tree_open(&path, false),
            Action::OpenNewTab => self.tree_open(&path, true),
            Action::Expand => self.tree_open(&path, false),
            Action::Rename => self.tree_start_naming(path, false),
            Action::NewGroup => self.tree_new_group(target),
            Action::AddCurrent => {
                self.tree_add_current(target);
                Task::none()
            }
            Action::Remove => self.tree_remove(path),
        }
    }

    /// Where a drop at the last mouse position over the list goes.
    fn tree_spot(&self, rows: &[TreeRow]) -> Spot {
        let end = Spot { at: Vec::new(), index: self.hotlist.len(), mark: Mark::End };
        let Some(y) = self.tree.over else { return end };
        let fit = self.tree.fit.get().max(1);
        let i = self.tree_start(rows, fit) + (y.max(0.0) / ROW_H) as usize;
        let Some(r) = rows.get(i) else { return end };
        let frac = (y.max(0.0) % ROW_H) / ROW_H;
        let Some((&last, parent)) = r.path.split_last() else { return end };
        let line = |below: bool, depth: usize| Mark::Line { row: i, below, depth };
        let before = Spot { at: parent.to_vec(), index: last, mark: line(false, r.depth) };
        let after = Spot { at: parent.to_vec(), index: last + 1, mark: line(true, r.depth) };
        match hotlist::get(&self.hotlist, &r.path) {
            Some(HotItem::Menu { expanded, items, .. }) => {
                if frac < 0.25 {
                    before
                } else if frac > 0.75 && *expanded && !items.is_empty() {
                    // Below an open group: its first entry.
                    Spot { at: r.path.clone(), index: 0, mark: line(true, r.depth + 1) }
                } else if frac > 0.75 {
                    after
                } else {
                    Spot { at: r.path.clone(), index: items.len(), mark: Mark::Into(i) }
                }
            }
            _ if frac < 0.5 => before,
            _ => after,
        }
    }

    /// The spot an entry being dragged would move to (`None`: into itself,
    /// or not over the list).
    fn tree_move_spot(&self, rows: &[TreeRow]) -> Option<Spot> {
        let press = self.tree.press.as_ref().filter(|p| p.active)?;
        self.tree.over?;
        let spot = self.tree_spot(rows);
        (!spot.at.starts_with(&press.path)).then_some(spot)
    }

    /// The panel a folder dragged out of the tree would open in.
    pub(super) fn tree_drop_side(&self) -> Option<usize> {
        let press = self.tree.press.as_ref().filter(|p| p.active)?;
        let folder = matches!(hotlist::get(&self.hotlist, &press.path), Some(HotItem::Folder { .. }));
        (folder && self.tree.over.is_none() && !self.tree_under_mouse()).then(|| self.side_under_mouse())
    }

    /// The left button went up: a click on an entry, the end of a drag, or
    /// of resizing.
    pub(super) fn tree_release(&mut self) -> Task<Message> {
        if std::mem::take(&mut self.tree.resizing) {
            if self.split_ghost.take().is_some() {
                self.tree.width = self.mouse.x - SPLIT_W / 2.0;
                self.tree.width = self.tree_width();
                self.sync_grids();
            }
            self.save_session();
        }
        // A folder dragged onto a panel opens there, which gets the focus.
        if let Some(side) = self.tree_drop_side() {
            let press = self.tree.press.take();
            if let Some(HotItem::Folder { path: dir, .. }) = press.and_then(|p| hotlist::get(&self.hotlist, &p.path)) {
                let dir = dir.clone();
                self.focus_panel(side);
                return self.start_nav(side, Nav::to(dir));
            }
        }
        let Some(press) = self.tree.press.take() else { return Task::none() };
        if !press.active {
            self.tree.over = None;
            let new_tab = self.modifiers.shift() || self.modifiers.control();
            return self.tree_open(&press.path, new_tab);
        }
        // `tree_move_spot` looks at the press.
        self.tree.press = Some(press);
        let spot = self.tree_move_spot(&hotlist::rows(&self.hotlist));
        let press = self.tree.press.take();
        self.tree.over = None;
        if let (Some(spot), Some(press)) = (spot, press) {
            if let Some(path) = hotlist::move_to(&mut self.hotlist, &press.path, &spot.at, spot.index) {
                if matches!(spot.mark, Mark::Into(_)) {
                    self.tree_expand_to(&spot.at);
                }
                self.tree_set_cursor(path);
                self.save_session();
            }
        }
        Task::none()
    }

    /// Esc while an entry is pressed or dragged.
    pub(super) fn tree_cancel_drag(&mut self) {
        self.tree.press = None;
        self.tree.over = None;
    }

    /// Mouse moved with an entry pressed: past the threshold it is dragged.
    /// Or the divider is dragged: resize.
    pub(super) fn tree_mouse_moved(&mut self) {
        let mouse = self.mouse;
        if self.tree.resizing {
            let old = self.tree.width;
            self.tree.width = mouse.x - SPLIT_W / 2.0;
            self.tree.width = self.tree_width();
            if theme::software() {
                self.split_ghost = Some(self.tree.width + SPLIT_W / 2.0);
                self.tree.width = old;
            } else {
                self.sync_grids();
            }
            return;
        }
        if let Some(p) = self.tree.press.as_mut().filter(|p| !p.active) {
            let start = *p.start.get_or_insert(mouse);
            if mouse.distance(start) >= DRAG_THRESHOLD {
                p.active = true;
            }
        }
    }

    /// Folders dragged from a panel were dropped on the tree.
    pub(super) fn tree_drop_files(&mut self, side: usize, sources: &[PathBuf]) -> Task<Message> {
        let dirs = self.dragged_folders(side, sources);
        let rows = hotlist::rows(&self.hotlist);
        let spot = self.tree_spot(&rows);
        self.tree.over = None;
        self.tree.focused = true;
        self.tree_add_folders(dirs, &spot.at, spot.index);
        Task::none()
    }

    /// The folders among `sources` (dragged from `side`).
    fn dragged_folders(&self, side: usize, sources: &[PathBuf]) -> Vec<PathBuf> {
        let panel = self.panel(side);
        sources
            .iter()
            .filter(|p| panel.entries.iter().any(|e| &e.path == *p && e.is_dir()))
            .cloned()
            .collect()
    }

    /// Files dropped from other apps on the tree: folders go at the end.
    pub(super) fn tree_drop_external(&mut self, sources: Vec<PathBuf>) -> Task<Message> {
        let dirs: Vec<PathBuf> = sources.into_iter().filter(|p| p.is_dir()).collect();
        self.tree.focused = true;
        let n = self.hotlist.len();
        self.tree_add_folders(dirs, &[], n);
        Task::none()
    }

    /// The floating label while something is dragged over the tree.
    pub(super) fn tree_drag_text(&self) -> Option<String> {
        let rows = hotlist::rows(&self.hotlist);
        let into = |spot: &Spot| match spot.at.as_slice() {
            [] => String::new(),
            at => hotlist::get(&self.hotlist, at).map(|g| format!(" \u{2192} {}", g.label())).unwrap_or_default(),
        };
        if let Some(press) = self.tree.press.as_ref().filter(|p| p.active) {
            let what = hotlist::get(&self.hotlist, &press.path).map(HotItem::label).unwrap_or_default();
            if let Some(side) = self.tree_drop_side() {
                return Some(format!("Open {what} in the {} panel", if side == 0 { "left" } else { "right" }));
            }
            return Some(match self.tree_move_spot(&rows) {
                Some(spot) => format!("Move {what}{}", into(&spot)),
                None if self.tree.over.is_some() => "Not into itself".to_owned(),
                None => format!("Move {what}"),
            });
        }
        let drag = self.drag.as_ref().filter(|d| d.active)?;
        if !self.tree_under_mouse() {
            return None;
        }
        let dirs = self.dragged_folders(drag.side, &drag.sources);
        let spot = self.tree_spot(&rows);
        Some(match dirs.as_slice() {
            [] => "Only folders go into the hotlist".to_owned(),
            [one] => format!("Add {} to the hotlist{}", HotItem::folder_at(one.clone()).label(), into(&spot)),
            many => format!("Add {} folders to the hotlist{}", many.len(), into(&spot)),
        })
    }

    pub(super) fn tree_update(&mut self, msg: TreeMsg) -> Task<Message> {
        match msg {
            TreeMsg::Toggle => return self.toggle_tree(),
            TreeMsg::ToggleLeft => return self.toggle_hide_left(),
            TreeMsg::ResizePress => {
                let now = Instant::now();
                if matches!(self.tree.resize_click, Some(t) if now - t < DOUBLE_CLICK) {
                    self.tree.resize_click = None;
                    self.tree.resizing = false;
                    self.tree.width = crate::session::DEFAULT_TREE_WIDTH;
                    self.sync_grids();
                    self.save_session();
                } else {
                    self.tree.resize_click = Some(now);
                    self.tree.resizing = true;
                }
            }
            TreeMsg::Press(path) => {
                self.leave_edits();
                self.tree.menu = None;
                self.tree.focused = true;
                if hotlist::get(&self.hotlist, &path).is_some() {
                    self.tree.cursor = path.clone();
                    self.tree.press = Some(Press { path, start: None, active: false });
                }
            }
            TreeMsg::Middle(path) => {
                self.leave_edits();
                self.tree.menu = None;
                self.tree.focused = true;
                return self.tree_open(&path, true);
            }
            TreeMsg::Right(target) => {
                self.leave_edits();
                self.tree.focused = true;
                if let Some(p) = &target {
                    self.tree_set_cursor(p.clone());
                }
                return window::get_oldest()
                    .and_then(|id| window::run_with_handle(id, |h| rc_platform::cursor_in_window(native_handle(&h))))
                    .map(move |pos| Message::Tree(TreeMsg::MenuAt(target.clone(), pos)));
            }
            TreeMsg::MenuAt(target, pos) => {
                if let Some((x, y)) = pos {
                    self.tree.menu = Some(TreeMenu { at: Point::new(x, y), target, selected: 0 });
                }
            }
            TreeMsg::MenuPick(i) => {
                if let Some(menu) = &mut self.tree.menu {
                    menu.selected = i;
                }
                return self.tree_menu_pick(i);
            }
            TreeMsg::MenuClose => self.tree.menu = None,
            TreeMsg::Background => {
                self.leave_edits();
                self.tree.menu = None;
                self.tree.focused = true;
            }
            TreeMsg::Over(y) => self.tree.over = Some(y),
            TreeMsg::Out => self.tree.over = None,
            TreeMsg::Wheel(lines) => {
                let rows = hotlist::rows(&self.hotlist);
                let fit = self.tree.fit.get().max(1);
                let start = self.tree_start(&rows, fit) as isize;
                let max = rows.len().saturating_sub(fit) as isize;
                self.tree.scroll = (start - (lines * 3.0).round() as isize).clamp(0, max) as usize;
                self.tree.detached = true;
            }
            TreeMsg::ScrollTo(start) => {
                self.tree.scroll = start;
                self.tree.detached = true;
            }
            TreeMsg::NameInput(value) => {
                if let Some(n) = &mut self.tree.naming {
                    n.text = value;
                }
            }
            TreeMsg::NameSubmit => self.tree_commit_naming(),
            TreeMsg::NewGroup => {
                let target = (!self.tree.cursor.is_empty()).then(|| self.tree.cursor.clone());
                return self.tree_new_group(target);
            }
            TreeMsg::AddCurrent => {
                self.tree.focused = true;
                let target = (!self.tree.cursor.is_empty()).then(|| self.tree.cursor.clone());
                self.tree_add_current(target);
            }
        }
        Task::none()
    }

    // ---- drawing ------------------------------------------------------------

    /// An icon drawn in code in `color` (cached).
    fn glyph(&self, g: Glyph, color: Color) -> image::Handle {
        let key = (g, color.into_rgba8());
        self.tree
            .glyphs
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| image::Handle::from_rgba(GLYPH_PX, GLYPH_PX, glyph_pixels(g, color)))
            .clone()
    }

    /// The button left of the left panel's drive buttons.
    pub(super) fn tree_button(&self) -> Element<'_, Message> {
        let on = self.tree_visible();
        let color = if on { theme::c().accent } else { theme::c().muted };
        let icon = image(self.glyph(Glyph::Tree, color)).width(ICON).height(ICON);
        let keys = keymap::Keymap::keys_for(&self.keymap.bindings(), Command::HotlistTree).join(", ");
        let tip = if on { "Hide the hotlist tree" } else { "Show the hotlist tree" };
        let tip = if keys.is_empty() { tip.to_owned() } else { format!("{tip} ({keys})") };
        tooltip(
            button(icon).on_press(Message::Tree(TreeMsg::Toggle)).padding([2, 6]).style(theme::drive(on)),
            container(text(tip).size(12)).padding([3, 8]).style(theme::drag_label),
            tooltip::Position::Bottom,
        )
        .gap(4)
        .into()
    }

    /// Small button in the tree's title: hide or show the left panel.
    fn hide_left_button(&self) -> Element<'_, Message> {
        let hidden = self.tree.hide_left;
        let keys = keymap::Keymap::keys_for(&self.keymap.bindings(), Command::HotlistTreeHideLeft).join(", ");
        let tip = if hidden { "Show the left panel" } else { "Hide the left panel" };
        let tip = if keys.is_empty() { tip.to_owned() } else { format!("{tip} ({keys})") };
        tooltip(
            button(text(if hidden { "Show left" } else { "Hide left" }).size(11).wrapping(Wrapping::None))
                .on_press(Message::Tree(TreeMsg::ToggleLeft))
                .padding([1, 6])
                .style(theme::drive(hidden)),
            container(text(tip).size(12)).padding([3, 8]).style(theme::drag_label),
            tooltip::Position::Bottom,
        )
        .gap(4)
        .into()
    }

    /// The tree column: a title level with the tabs, a bar with actions
    /// level with the drives, the tree, a hint line.
    pub(super) fn tree_side_view(&self) -> Element<'_, Message> {
        let c = theme::c();
        let focused = self.tree_focused();
        let file_drop = self.dragging() && self.tree_under_mouse();
        let strip = if file_drop || self.tree_dragging() {
            c.success
        } else if focused {
            c.accent
        } else {
            c.header
        };
        let accent = container(Space::new(Length::Fill, Length::Fixed(2.0))).style(filled(strip));

        let folders = count_folders(&self.hotlist);
        let title = container(
            row![
                button(text("Hotlist").size(12).wrapping(Wrapping::None)).padding([3, 10]).style(theme::tab(true)),
                Space::with_width(Length::Fill),
                self.hide_left_button(),
                text(match folders {
                    1 => "1 folder".to_owned(),
                    n => format!("{n} folders"),
                })
                .size(11)
                .color(c.muted)
                .wrapping(Wrapping::None),
            ]
            .spacing(6)
            .align_y(Alignment::End),
        )
        .padding(Padding { top: 4.0, right: 10.0, bottom: 0.0, left: 8.0 })
        .width(Length::Fill)
        .clip(true)
        .style(filled(c.bg));

        let action = |label: &'static str, msg: TreeMsg| {
            button(text(label).size(12).wrapping(Wrapping::None))
                .on_press(Message::Tree(msg))
                .padding([2, 8])
                .style(theme::drive(false))
        };
        let bar = container(
            row![
                action("+ Group", TreeMsg::NewGroup),
                action("+ Current folder", TreeMsg::AddCurrent),
                Space::with_width(Length::Fill),
            ]
            .spacing(2)
            .align_y(Alignment::Center),
        )
        .padding([4, 8])
        .width(Length::Fill)
        .clip(true)
        .style(filled(c.header));

        let hint = if self.tree.naming.is_some() {
            "Type the name \u{b7} Enter OK \u{b7} Esc cancel"
        } else if self.tree_dragging() {
            "Drop between entries or on a group \u{b7} Esc cancel"
        } else {
            // As much as fits on one line.
            let room = ((self.tree_width() - 20.0) / 5.5) as usize;
            [
                "Enter open \u{b7} Shift+Enter new tab \u{b7} F7 group \u{b7} F2 rename \u{b7} Del remove \u{b7} drag to move",
                "Enter open \u{b7} Shift+Enter new tab \u{b7} F7 group \u{b7} F2 rename \u{b7} Del remove",
                "Enter open \u{b7} F7 group \u{b7} F2 rename \u{b7} Del remove",
                "Enter open \u{b7} F7 group \u{b7} Del remove",
            ]
            .into_iter()
            .find(|h| h.chars().count() <= room)
            .unwrap_or("F1 help")
        };
        let footer = container(text(hint).size(11).color(c.muted).wrapping(Wrapping::None))
            .padding([4, 10])
            .width(Length::Fill)
            .clip(true)
            .style(filled(c.header));

        column![accent, title, bar, self.tree_list(focused), footer].width(Length::Fill).height(Length::Fill).into()
    }

    fn tree_list(&self, focused: bool) -> Element<'_, Message> {
        let rows = hotlist::rows(&self.hotlist);
        let dragging = self.dragging() || self.tree_dragging();
        let spot = if self.tree_dragging() {
            self.tree_move_spot(&rows)
        } else if self.dragging() && self.tree.over.is_some() {
            Some(self.tree_spot(&rows))
        } else {
            None
        };
        let body: Element<'_, Message> = if rows.is_empty() {
            let c = theme::c();
            container(
                column![
                    text("No favourite folders yet").size(14).color(c.text),
                    text("Drag folders here from a panel, press + to add the current folder, or F7 for a group.")
                        .size(12)
                        .color(c.muted),
                ]
                .spacing(6),
            )
            .padding(16)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(c.panel))
            .into()
        } else {
            let rows = std::rc::Rc::new(rows);
            responsive(move |size| {
                let fit = ((size.height / ROW_H).floor() as usize).max(1);
                self.tree.fit.set(fit);
                let len = rows.len();
                let start = self.tree_start(&rows, fit);
                let end = (start + fit).min(len);
                let mut list = Column::with_children((start..end).map(|i| self.tree_row(&rows[i], i, focused, spot.as_ref())));
                if spot.as_ref().is_some_and(|s| s.mark == Mark::End) {
                    list = list.push(container(Space::new(Length::Fill, Length::Fixed(2.0))).style(filled(theme::c().accent)));
                }
                let view = container(list).width(Length::Fill).height(Length::Fill).clip(true);
                let bar = scrollbar::attach(view, start as f32, fit as f32, len as f32, move |at| {
                    Message::Tree(TreeMsg::ScrollTo(at.round() as usize))
                });
                container(bar).width(Length::Fill).height(Length::Fill).style(filled(theme::c().panel)).into()
            })
            .into()
        };
        let mut area = mouse_area(body)
            .on_scroll(|delta| {
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / ROW_H,
                };
                Message::Tree(TreeMsg::Wheel(lines))
            })
            .on_press(Message::Tree(TreeMsg::Background))
            .on_right_press(Message::Tree(TreeMsg::Right(None)));
        if dragging {
            area = area.on_move(|p| Message::Tree(TreeMsg::Over(p.y))).on_exit(Message::Tree(TreeMsg::Out));
        }
        area.into()
    }

    fn tree_row(&self, r: &TreeRow, i: usize, focused: bool, spot: Option<&Spot>) -> Element<'_, Message> {
        let c = theme::c();
        let Some(item) = hotlist::get(&self.hotlist, &r.path) else { return Space::with_height(ROW_H).into() };
        let cursor = r.path == self.tree.cursor;
        let mut content = Row::new().spacing(6).align_y(Alignment::Center).push(Space::with_width(r.depth as f32 * INDENT));
        let naming = self.tree.naming.as_ref().filter(|n| n.path == r.path);
        match item {
            HotItem::Menu { expanded, items, .. } => {
                let g = if *expanded { Glyph::Open } else { Glyph::Closed };
                content = content
                    .push(container(image(self.glyph(g, c.muted)).width(10).height(10)).center_x(Length::Fixed(CHEVRON)));
                content = match naming {
                    Some(n) => content.push(
                        text_input("Group name", &n.text)
                            .id(name_input_id())
                            .on_input(|v| Message::Tree(TreeMsg::NameInput(v)))
                            .on_submit(Message::Tree(TreeMsg::NameSubmit))
                            .size(13)
                            .padding([1, 4])
                            .width(Length::Fill)
                            .style(theme::input),
                    ),
                    None => content
                        .push(
                            text(item.label())
                                .size(13)
                                .color(c.text)
                                .font(iced::Font { weight: iced::font::Weight::Semibold, ..iced::Font::DEFAULT })
                                .wrapping(Wrapping::None),
                        )
                        .push(
                            text(match items.len() {
                                0 => "empty".to_owned(),
                                n => n.to_string(),
                            })
                            .size(11)
                            .color(c.muted)
                            .wrapping(Wrapping::None),
                        ),
                };
            }
            HotItem::Folder { path: dir, .. } => {
                content = content.push(Space::with_width(CHEVRON));
                content = content.push(match self.icon(dir, true) {
                    Some(icon) => Element::from(image(icon).width(ICON).height(ICON)),
                    None => Space::new(ICON, ICON).into(),
                });
                let here = self.sides.iter().any(|s| s.tabs[s.current].path == *dir);
                let color = if here { c.selected } else { c.dir };
                let parent = dir.parent().map(rc_core::remote::display).unwrap_or_default();
                let room = (self.tree_width() / 7.0) as usize;
                let label = item.label();
                let detail = super::view::elide_left(&parent, room.saturating_sub(label.chars().count() + r.depth * 2 + 6).max(8));
                content = content
                    .push(text(label).size(13).color(color).wrapping(Wrapping::None))
                    .push(text(detail).size(11).color(c.muted).wrapping(Wrapping::None));
            }
        }

        let into = spot.is_some_and(|s| s.mark == Mark::Into(i));
        let background = if into {
            c.drop
        } else if cursor {
            if focused { c.cursor } else { c.cursor_inactive }
        } else if i % 2 == 1 {
            c.zebra
        } else {
            c.panel
        };
        let line = |depth: usize| {
            container(container(Space::new(Length::Fill, Length::Fixed(2.0))).style(filled(c.accent)))
                .padding(Padding { left: 10.0 + depth as f32 * INDENT, ..Padding::ZERO })
                .width(Length::Fill)
        };
        let (above, below) = match spot.map(|s| s.mark) {
            Some(Mark::Line { row, below, depth }) if row == i => {
                if below { (None, Some(line(depth))) } else { (Some(line(depth)), None) }
            }
            _ => (None, None),
        };
        let height = if above.is_some() || below.is_some() { ROW_H - 2.0 } else { ROW_H };
        let body = column![]
            .push_maybe(above)
            .push(container(content.padding([0, 10]).height(height)).width(Length::Fill).clip(true))
            .push_maybe(below);
        let path = r.path.clone();
        let mut area = mouse_area(container(body).width(Length::Fill).height(ROW_H).style(filled(background)))
            .on_middle_press(Message::Tree(TreeMsg::Middle(path.clone())))
            .on_right_press(Message::Tree(TreeMsg::Right(Some(path.clone()))));
        if naming.is_none() {
            area = area.on_press(Message::Tree(TreeMsg::Press(path)));
        }
        match item.tip().filter(|_| naming.is_none() && self.tree.press.is_none()) {
            Some(tip) => tooltip(
                area,
                container(text(tip).size(12)).padding([3, 8]).max_width(420).style(theme::drag_label),
                tooltip::Position::Bottom,
            )
            .gap(2)
            .into(),
            None => area.into(),
        }
    }

    /// The right-click menu, over everything at the mouse.
    pub(super) fn tree_menu_view(&self) -> Option<Element<'_, Message>> {
        let menu = self.tree.menu.as_ref()?;
        let c = theme::c();
        let actions = self.tree_actions(menu.target.as_deref());
        let item = menu.target.as_deref().and_then(|p| hotlist::get(&self.hotlist, p));
        let current = HotItem::folder_at(self.panel(self.active).path.clone()).label();
        let panel = if self.active == 0 { "left panel" } else { "right panel" };
        let mut list = Column::new().spacing(1);
        let mut height = 10.0;
        for (i, &action) in actions.iter().enumerate() {
            let (label, key) = match action {
                Action::Open => (format!("Open in the {panel}"), "Enter"),
                Action::OpenNewTab => ("Open in a new tab".to_owned(), "Shift+Enter"),
                Action::Expand => {
                    let open = matches!(item, Some(HotItem::Menu { expanded: true, .. }));
                    (if open { "Collapse" } else { "Expand" }.to_owned(), "Enter")
                }
                Action::Rename => match item {
                    Some(HotItem::Menu { .. }) => ("Rename group".to_owned(), "F2"),
                    _ => ("Name and comment...".to_owned(), "F2"),
                },
                Action::NewGroup => ("New group".to_owned(), "F7"),
                Action::AddCurrent => match item {
                    Some(HotItem::Menu { .. }) => (format!("Add \"{current}\" ({panel}) here"), "+"),
                    _ => (format!("Add \"{current}\" ({panel})"), "+"),
                },
                Action::Remove => match item {
                    Some(HotItem::Menu { .. }) => ("Remove group".to_owned(), "Del"),
                    _ => ("Remove from the hotlist".to_owned(), "Del"),
                },
            };
            // Separators before the adding and the removing part.
            if i > 0 && matches!(action, Action::NewGroup | Action::Remove) {
                let rule = container(Space::new(Length::Fill, Length::Fixed(1.0))).style(filled(c.border));
                list = list.push(container(rule).padding([4, 6]));
                height += 9.0;
            }
            let color = if action == Action::Remove { c.error } else { c.text };
            let label = super::view::elide_end(&label, 34);
            list = list.push(
                button(
                    row![
                        text(label).size(13).color(color).wrapping(Wrapping::None),
                        Space::with_width(Length::Fill),
                        text(key).size(11).color(c.muted).wrapping(Wrapping::None),
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center),
                )
                .on_press(Message::Tree(TreeMsg::MenuPick(i)))
                .width(Length::Fill)
                .height(MENU_ROW_H)
                .padding([4, 10])
                .style(theme::suggestion(i == menu.selected)),
            );
            height += MENU_ROW_H + 1.0;
        }
        let card = container(list).padding(4).width(MENU_W).style(theme::dialog);
        let x = menu.at.x.min(self.window_width - MENU_W - 4.0).max(0.0);
        let y = if menu.at.y + height > self.window_height - 4.0 { (menu.at.y - height).max(0.0) } else { menu.at.y };
        let placed = container(card).padding(Padding { top: y, left: x, right: 0.0, bottom: 0.0 }).width(Length::Fill).height(Length::Fill);
        Some(
            opaque(
                mouse_area(placed)
                    .on_press(Message::Tree(TreeMsg::MenuClose))
                    .on_right_press(Message::Tree(TreeMsg::MenuClose)),
            ),
        )
    }
}

fn count_folders(items: &[HotItem]) -> usize {
    items
        .iter()
        .map(|it| match it {
            HotItem::Folder { .. } => 1,
            HotItem::Menu { items, .. } => count_folders(items),
        })
        .sum()
}

// ---- icons drawn in code ----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Glyph {
    /// The toggle button: an indented outline.
    Tree,
    /// A closed / open group.
    Closed,
    Open,
}

/// Pixels per edge (drawn at 2x, shown at 16 or 10 px).
const GLYPH_PX: u32 = 32;

fn glyph_pixels(g: Glyph, color: Color) -> Vec<u8> {
    const SS: u32 = 4;
    let [r, gr, b, a] = color.into_rgba8();
    let mut out = Vec::with_capacity((GLYPH_PX * GLYPH_PX * 4) as usize);
    for py in 0..GLYPH_PX {
        for px in 0..GLYPH_PX {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / GLYPH_PX as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / GLYPH_PX as f32;
                    hits += u32::from(glyph_covers(g, x, y));
                }
            }
            let cover = hits as f32 / (SS * SS) as f32;
            out.extend_from_slice(&[r, gr, b, (cover * a as f32).round() as u8]);
        }
    }
    out
}

/// Whether point (x, y) in 0..1 is inside the shape.
fn glyph_covers(g: Glyph, x: f32, y: f32) -> bool {
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| x >= x0 && x <= x1 && y >= y0 && y <= y1;
    // Inside the triangle (a, b, c), either winding.
    let tri = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
        let side = |p: (f32, f32), q: (f32, f32)| (q.0 - p.0) * (y - p.1) - (q.1 - p.1) * (x - p.0);
        let (d1, d2, d3) = (side(a, b), side(b, c), side(c, a));
        !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
    };
    match g {
        Glyph::Tree => {
            // Top entry, two entries below it hanging off a connector.
            rect(0.06, 0.10, 0.28, 0.32)
                || rect(0.38, 0.16, 0.94, 0.26)
                || rect(0.15, 0.32, 0.20, 0.82)
                || rect(0.15, 0.48, 0.34, 0.53)
                || rect(0.15, 0.77, 0.34, 0.82)
                || rect(0.36, 0.40, 0.56, 0.61)
                || rect(0.64, 0.45, 0.94, 0.55)
                || rect(0.36, 0.69, 0.56, 0.90)
                || rect(0.64, 0.74, 0.94, 0.84)
        }
        Glyph::Closed => tri((0.30, 0.18), (0.30, 0.82), (0.78, 0.50)),
        Glyph::Open => tri((0.18, 0.30), (0.82, 0.30), (0.50, 0.78)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_are_drawn() {
        for g in [Glyph::Tree, Glyph::Closed, Glyph::Open] {
            let px = glyph_pixels(g, Color::WHITE);
            assert_eq!(px.len(), (GLYPH_PX * GLYPH_PX * 4) as usize);
            let opaque = px.chunks(4).filter(|p| p[3] == 255).count();
            assert!(opaque > 20, "{g:?}");
            // Corners stay clear.
            assert_eq!(px[3], 0, "{g:?}");
        }
    }

    #[test]
    fn typing_commands_take_the_focus_from_the_tree() {
        assert!(takes_focus(Command::QuickFilter));
        assert!(takes_focus(Command::Rename));
        assert!(!takes_focus(Command::Copy));
        assert!(!takes_focus(Command::NewTab));
        assert!(!takes_focus(Command::SwitchPanel));
    }
}
