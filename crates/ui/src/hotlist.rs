//! Directory hotlist (Ctrl+D) model: favourite folders and named submenus,
//! in menu order. A menu is addressed by the indices of the submenus leading
//! to it from the top (`at`, empty = the top menu).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One hotlist entry. In `session.toml` a folder is a plain path string (the
/// format older versions wrote) and a submenu an inline table
/// `{ name = "…", items = [...] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HotItem {
    Folder(PathBuf),
    Menu {
        name: String,
        #[serde(default)]
        items: Vec<HotItem>,
    },
}

impl HotItem {
    pub fn folder(&self) -> Option<&Path> {
        match self {
            HotItem::Folder(path) => Some(path),
            HotItem::Menu { .. } => None,
        }
    }
}

/// The menu at `at`; stops at the deepest valid submenu if `at` is stale.
pub fn menu<'a>(top: &'a [HotItem], at: &[usize]) -> &'a [HotItem] {
    let mut items = top;
    for &i in at {
        match items.get(i) {
            Some(HotItem::Menu { items: sub, .. }) => items = sub,
            _ => break,
        }
    }
    items
}

/// Mutable [`menu`].
pub fn menu_mut<'a>(top: &'a mut Vec<HotItem>, at: &[usize]) -> &'a mut Vec<HotItem> {
    let mut items = top;
    for &i in at {
        if !matches!(items.get(i), Some(HotItem::Menu { .. })) {
            break;
        }
        let Some(HotItem::Menu { items: sub, .. }) = items.get_mut(i) else { unreachable!() };
        items = sub;
    }
    items
}

/// Where `folder` is: the menu holding it and its index there (first match,
/// top menu first, then submenus in order).
pub fn find(items: &[HotItem], folder: &Path) -> Option<(Vec<usize>, usize)> {
    if let Some(i) = items.iter().position(|it| it.folder() == Some(folder)) {
        return Some((Vec::new(), i));
    }
    items.iter().enumerate().find_map(|(i, it)| match it {
        HotItem::Menu { items: sub, .. } => find(sub, folder).map(|(mut at, j)| {
            at.insert(0, i);
            (at, j)
        }),
        HotItem::Folder(_) => None,
    })
}

/// Adds `folder` to the menu, or removes it if it's there. True if added.
pub fn toggle(items: &mut Vec<HotItem>, folder: &Path) -> bool {
    match items.iter().position(|it| it.folder() == Some(folder)) {
        Some(i) => {
            items.remove(i);
            false
        }
        None => {
            items.push(HotItem::Folder(folder.to_path_buf()));
            true
        }
    }
}

/// Removes entry `i`; a submenu's entries take its place, so nothing but
/// the submenu itself is lost.
pub fn remove(items: &mut Vec<HotItem>, i: usize) {
    if i >= items.len() {
        return;
    }
    if let HotItem::Menu { items: sub, .. } = items.remove(i) {
        items.splice(i..i, sub);
    }
}

/// Moves entry `i` one place up (`delta` < 0) or down; returns its new index.
pub fn shift(items: &mut [HotItem], i: usize, delta: isize) -> usize {
    let Some(j) = i.checked_add_signed(delta).filter(|&j| j < items.len() && i < items.len()) else { return i };
    items.swap(i, j);
    j
}

/// Moves entry `i` to the end of the closest submenu above it. Returns that
/// submenu's index, or `None` if there is none above.
pub fn indent(items: &mut Vec<HotItem>, i: usize) -> Option<usize> {
    if i >= items.len() {
        return None;
    }
    let m = items[..i].iter().rposition(|it| matches!(it, HotItem::Menu { .. }))?;
    let item = items.remove(i);
    let HotItem::Menu { items: sub, .. } = &mut items[m] else { unreachable!() };
    sub.push(item);
    Some(m)
}

/// Moves entry `i` of the submenu at `at` out to the menu above, right after
/// the submenu. Returns the new place (menu, index), or `None` at the top.
pub fn outdent(top: &mut Vec<HotItem>, at: &[usize], i: usize) -> Option<(Vec<usize>, usize)> {
    let (&m, parent) = at.split_last()?;
    let sub = menu_mut(top, at);
    if i >= sub.len() {
        return None;
    }
    let item = sub.remove(i);
    let parent_items = menu_mut(top, parent);
    parent_items.insert(m + 1, item);
    Some((parent.to_vec(), m + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(p: &str) -> HotItem {
        HotItem::Folder(PathBuf::from(p))
    }

    fn sub(name: &str, items: Vec<HotItem>) -> HotItem {
        HotItem::Menu { name: name.into(), items }
    }

    #[test]
    fn reorders_and_nests() {
        let mut top = vec![f("a"), sub("Work", vec![f("w")]), f("b"), f("c")];
        assert_eq!(shift(&mut top, 3, -1), 2);
        assert_eq!(top, [f("a"), sub("Work", vec![f("w")]), f("c"), f("b")]);
        assert_eq!(shift(&mut top, 0, -1), 0);
        assert_eq!(shift(&mut top, 3, 1), 3);

        // Into the submenu above, then back out after it.
        assert_eq!(indent(&mut top, 3), Some(1));
        assert_eq!(indent(&mut top, 0), None);
        assert_eq!(top, [f("a"), sub("Work", vec![f("w"), f("b")]), f("c")]);
        assert_eq!(menu(&top, &[1]), [f("w"), f("b")]);
        assert_eq!(find(&top, Path::new("b")), Some((vec![1], 1)));
        assert_eq!(find(&top, Path::new("c")), Some((vec![], 2)));
        assert_eq!(find(&top, Path::new("x")), None);
        assert_eq!(outdent(&mut top, &[1], 0), Some((vec![], 2)));
        assert_eq!(top, [f("a"), sub("Work", vec![f("b")]), f("w"), f("c")]);
        assert_eq!(outdent(&mut top, &[], 0), None);

        // Toggling works per menu; removing a submenu keeps its entries.
        assert!(toggle(menu_mut(&mut top, &[1]), Path::new("a")));
        assert!(!toggle(&mut top, Path::new("w")));
        remove(&mut top, 1);
        assert_eq!(top, [f("a"), f("b"), f("a"), f("c")]);
        // A stale path stops at the deepest valid menu.
        assert_eq!(menu(&top, &[7, 2]).len(), 4);
    }
}
