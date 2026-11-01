//! The two folder popups: the directory hotlist (Ctrl+D, TC's
//! `cm_DirectoryHotlist`) with submenus and reordering, and the folder
//! history list (Alt+Down, `cm_DirectoryHistory`). Drawn in `view.rs`.

use super::*;
use crate::hotlist::{self, HotItem};

/// Hotlist popup state.
#[derive(Debug, Default)]
pub(super) struct HotlistMenu {
    /// The submenu shown (see [`crate::hotlist::menu`]).
    pub at: Vec<usize>,
    /// Index into the menu's entries; one past them is the add/remove item.
    pub selected: usize,
    /// Name being typed for a new submenu (N) or a rename (F2).
    pub naming: Option<String>,
    pub renaming: bool,
}

pub(super) fn popup_list_id() -> iced::widget::scrollable::Id {
    iced::widget::scrollable::Id::new("popup-list")
}

impl App {
    /// Scrolls the hotlist / history / command palette popup so the selected row shows: rows
    /// are evenly tall, so the relative offset of the row's index does.
    pub(super) fn popup_snap(&self) -> Task<Message> {
        let (selected, rows) = match &self.dialog {
            Some(Dialog::Hotlist(menu)) => {
                // The entries, a row being named and the add/remove row.
                let n = hotlist::menu(&self.hotlist, &menu.at).len();
                (menu.selected, n + 1 + usize::from(menu.naming.is_some() && !menu.renaming))
            }
            Some(Dialog::History { selected }) => (*selected, self.panel(self.active).history().0.len()),
            Some(Dialog::Palette(p)) => (p.selected, p.shown.len()),
            _ => return Task::none(),
        };
        let y = if rows > 1 { (selected as f32 / (rows - 1) as f32).min(1.0) } else { 0.0 };
        iced::widget::scrollable::snap_to(popup_list_id(), iced::widget::scrollable::RelativeOffset { x: 0.0, y })
    }

    /// Keys of `hotlist_key`, for the F1 help. Keep in sync.
    pub(super) const HOTLIST_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Up, Down, Home, End", "Pick an entry"),
        ("Enter, 1-9, click", "Open the folder or submenu"),
        ("Right / Left, Backspace", "Open the submenu / back to the menu above"),
        ("A, +", "Add / remove the current folder (in the menu shown)"),
        ("N", "New submenu: type its name, Enter"),
        ("F2", "Rename the submenu"),
        ("Ctrl+Up, Ctrl+Down", "Move the entry up / down"),
        ("Ctrl+Right", "Move the entry into the submenu above it"),
        ("Ctrl+Left", "Move the entry out of the submenu"),
        ("Del", "Remove the entry (a submenu's entries move up)"),
        ("Esc", "Close"),
    ];

    /// Keys of `history_key`, for the F1 help. Keep in sync.
    pub(super) const HISTORY_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Up, Down, Home, End", "Pick a folder (most recent first, current one marked)"),
        ("Enter, 1-9, click", "Go there: like that many steps of Alt+Left / Alt+Right"),
        ("Esc", "Close"),
    ];

    pub(super) fn open_hotlist(&mut self) {
        self.leave_edits();
        // Start on the current folder if it is in the list.
        let here = &self.panel(self.active).path;
        let (at, selected) = hotlist::find(&self.hotlist, here).unwrap_or_default();
        self.dialog = Some(Dialog::Hotlist(HotlistMenu { at, selected, ..HotlistMenu::default() }));
    }

    pub(super) fn open_history(&mut self) {
        self.leave_edits();
        let (list, current) = self.panel(self.active).history();
        // On the previous folder, so Alt+Down, Enter = Alt+Left.
        let selected = if current + 1 < list.len() { current + 1 } else { current };
        self.dialog = Some(Dialog::History { selected });
    }

    pub(super) fn hotlist_key(&mut self, named: Option<Named>, text: Option<&str>, mods: keyboard::Modifiers) -> Task<Message> {
        let Some(Dialog::Hotlist(menu)) = &mut self.dialog else { return Task::none() };
        if let Some(name) = &mut menu.naming {
            match named {
                Some(Named::Escape) => menu.naming = None,
                Some(Named::Backspace) => {
                    name.pop();
                }
                Some(Named::Enter) => {
                    let name = name.trim().to_owned();
                    let renaming = menu.renaming;
                    menu.naming = None;
                    if !name.is_empty() {
                        let (at, i) = (menu.at.clone(), menu.selected);
                        let items = hotlist::menu_mut(&mut self.hotlist, &at);
                        if renaming {
                            if let Some(HotItem::Menu { name: old, .. }) = items.get_mut(i) {
                                *old = name;
                            }
                        } else {
                            items.insert(i.min(items.len()), HotItem::Menu { name, items: Vec::new() });
                        }
                        self.save_session();
                    }
                }
                _ => {
                    if let Some(t) = text.filter(|t| !mods.control() && t.chars().all(|c| !c.is_control())) {
                        name.push_str(t);
                    }
                }
            }
            return Task::none();
        }

        let (at, selected) = (menu.at.clone(), menu.selected);
        let entries = hotlist::menu(&self.hotlist, &at);
        let count = entries.len() + 1;
        let is_menu = matches!(entries.get(selected), Some(HotItem::Menu { .. }));
        let mut select = |i: usize| menu.selected = i;
        if mods.control() {
            let items = hotlist::menu_mut(&mut self.hotlist, &at);
            let place = match named {
                Some(Named::ArrowUp) => Some((at, hotlist::shift(items, selected, -1))),
                Some(Named::ArrowDown) => Some((at, hotlist::shift(items, selected, 1))),
                Some(Named::ArrowRight) => hotlist::indent(items, selected).map(|m| (at, m)),
                Some(Named::ArrowLeft) => hotlist::outdent(&mut self.hotlist, &at, selected),
                _ => None,
            };
            if let Some((at, selected)) = place {
                if let Some(Dialog::Hotlist(menu)) = &mut self.dialog {
                    menu.at = at;
                    menu.selected = selected;
                }
                self.save_session();
            }
            return Task::none();
        }
        match named {
            Some(Named::Escape) => self.dialog = None,
            Some(Named::ArrowUp) => select((selected + count - 1) % count),
            Some(Named::ArrowDown) => select((selected + 1) % count),
            Some(Named::Home | Named::PageUp) => select(0),
            Some(Named::End | Named::PageDown) => select(count - 1),
            Some(Named::Enter) => return self.hotlist_pick(selected),
            Some(Named::ArrowRight) if is_menu => return self.hotlist_pick(selected),
            Some(Named::ArrowLeft | Named::Backspace) => self.hotlist_back(),
            Some(Named::F2) if is_menu => {
                let Some(HotItem::Menu { name, .. }) = entries.get(selected) else { unreachable!() };
                menu.naming = Some(name.clone());
                menu.renaming = true;
            }
            Some(Named::Delete) if selected < count - 1 => {
                hotlist::remove(hotlist::menu_mut(&mut self.hotlist, &at), selected);
                let count = hotlist::menu(&self.hotlist, &at).len() + 1;
                if let Some(Dialog::Hotlist(menu)) = &mut self.dialog {
                    menu.selected = selected.min(count - 1);
                }
                self.save_session();
            }
            _ => match text.map(|t| t.to_ascii_lowercase()).as_deref() {
                // 1-9 open the matching entry directly.
                Some(d @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")) => {
                    let i = d.parse::<usize>().unwrap_or(1) - 1;
                    if i < count - 1 {
                        return self.hotlist_pick(i);
                    }
                }
                Some("a" | "+") => return self.hotlist_pick(count - 1),
                Some("n") => {
                    menu.naming = Some(String::new());
                    menu.renaming = false;
                }
                _ => {}
            },
        }
        Task::none()
    }

    /// Back from a submenu to the menu above, on the submenu's entry.
    pub(super) fn hotlist_back(&mut self) {
        if let Some(Dialog::Hotlist(menu)) = &mut self.dialog {
            if let Some(i) = menu.at.pop() {
                menu.selected = i;
            }
        }
    }

    /// Opens entry `i` of the menu shown (a folder, or a submenu); the index
    /// past the entries adds or removes the active panel's folder there.
    pub(super) fn hotlist_pick(&mut self, i: usize) -> Task<Message> {
        let Some(Dialog::Hotlist(menu)) = &mut self.dialog else { return Task::none() };
        let at = menu.at.clone();
        match hotlist::menu(&self.hotlist, &at).get(i) {
            Some(HotItem::Menu { .. }) => {
                menu.at.push(i);
                menu.selected = 0;
                return Task::none();
            }
            Some(HotItem::Folder(path)) => {
                let path = path.clone();
                self.dialog = None;
                return self.start_nav(self.active, Nav::to(path));
            }
            None => {}
        }
        self.dialog = None;
        let here = self.panel(self.active).path.clone();
        let msg = if hotlist::toggle(hotlist::menu_mut(&mut self.hotlist, &at), &here) {
            format!("Added {} to the hotlist", here.display())
        } else {
            format!("Removed {} from the hotlist", here.display())
        };
        self.status = Some((msg, Tone::Info));
        self.save_session();
        Task::none()
    }

    pub(super) fn history_key(&mut self, named: Option<Named>, text: Option<&str>) -> Task<Message> {
        let Some(Dialog::History { selected }) = self.dialog else { return Task::none() };
        let count = self.panel(self.active).history().0.len();
        let mut select = |i: usize| self.dialog = Some(Dialog::History { selected: i });
        match named {
            Some(Named::Escape) => self.dialog = None,
            Some(Named::ArrowUp) => select((selected + count - 1) % count),
            Some(Named::ArrowDown) => select((selected + 1) % count),
            Some(Named::Home | Named::PageUp) => select(0),
            Some(Named::End | Named::PageDown) => select(count - 1),
            Some(Named::Enter) => return self.history_pick(selected),
            _ => {
                if let Some(d) = text.and_then(|t| t.parse::<usize>().ok()).filter(|d| (1..=9).contains(d)) {
                    if d <= count {
                        return self.history_pick(d - 1);
                    }
                }
            }
        }
        Task::none()
    }

    /// Goes to entry `i` of the history list, moving within the history
    /// like Alt+Left / Alt+Right would (so both still work afterwards).
    pub(super) fn history_pick(&mut self, i: usize) -> Task<Message> {
        self.dialog = None;
        match self.panel(self.active).history_nav(i) {
            Some(nav) => self.start_nav(self.active, nav),
            None => Task::none(),
        }
    }
}
