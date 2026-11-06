//! Directory hotlist (Ctrl+D) model: favourite folders and named submenus,
//! in menu order. A menu is addressed by the indices of the submenus leading
//! to it from the top (`at`, empty = the top menu).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One hotlist entry. In `session.toml` a folder is a plain path string (the
/// format older versions wrote) unless it has a display name or comment, then
/// an inline table `{ path = "…", name = "…", comment = "…" }`; a submenu is
/// `{ name = "…", items = [...] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Stored", into = "Stored")]
pub enum HotItem {
    Folder {
        path: PathBuf,
        /// Shown instead of the folder's name (empty: the folder's own name).
        name: String,
        /// Shown as a tooltip (empty: none).
        comment: String,
    },
    Menu {
        name: String,
        /// Shown open in the hotlist tree (left panel); saved so the tree
        /// looks the same next time. Written only when set.
        expanded: bool,
        items: Vec<HotItem>,
    },
}

/// The saved form of [`HotItem`].
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Stored {
    Plain(PathBuf),
    // Before `Menu`: a folder table has a `name` too.
    Folder {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        comment: String,
    },
    Menu {
        name: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        expanded: bool,
        #[serde(default)]
        items: Vec<HotItem>,
    },
}

impl From<Stored> for HotItem {
    fn from(s: Stored) -> Self {
        match s {
            Stored::Plain(path) => HotItem::folder_at(path),
            Stored::Folder { path, name, comment } => HotItem::Folder { path, name, comment },
            Stored::Menu { name, expanded, items } => HotItem::Menu { name, expanded, items },
        }
    }
}

impl From<HotItem> for Stored {
    fn from(item: HotItem) -> Self {
        match item {
            HotItem::Folder { path, name, comment } if name.is_empty() && comment.is_empty() => Stored::Plain(path),
            HotItem::Folder { path, name, comment } => Stored::Folder { path, name, comment },
            HotItem::Menu { name, expanded, items } => Stored::Menu { name, expanded, items },
        }
    }
}

impl HotItem {
    /// A folder entry without a custom name or comment.
    pub fn folder_at(path: PathBuf) -> Self {
        HotItem::Folder { path, name: String::new(), comment: String::new() }
    }

    pub fn folder(&self) -> Option<&Path> {
        match self {
            HotItem::Folder { path, .. } => Some(path),
            HotItem::Menu { .. } => None,
        }
    }

    /// A new, empty group.
    pub fn group(name: String) -> Self {
        HotItem::Menu { name, expanded: true, items: Vec::new() }
    }

    /// How many entries the item holds, at any depth (0 for a folder).
    pub fn count(&self) -> usize {
        match self {
            HotItem::Folder { .. } => 0,
            HotItem::Menu { items, .. } => items.iter().map(|it| 1 + it.count()).sum(),
        }
    }

    /// The name shown for the item: a group's name, a folder's custom name or
    /// last component (the whole path for a drive root).
    pub fn label(&self) -> String {
        match self {
            HotItem::Folder { name, .. } if !name.is_empty() => name.clone(),
            HotItem::Folder { path, .. } => rc_core::vfs::folder_label(path),
            HotItem::Menu { name, .. } => name.clone(),
        }
    }

    /// A folder's tooltip: its comment, then its path; `None` when it has
    /// neither a custom name nor a comment (and for groups).
    pub fn tip(&self) -> Option<String> {
        let HotItem::Folder { path, name, comment } = self else { return None };
        if name.is_empty() && comment.is_empty() {
            return None;
        }
        let path = rc_core::remote::display(path);
        Some(if comment.is_empty() { path } else { format!("{comment}\n{path}") })
    }

    /// Sets a folder's custom name and comment (trimmed; empty clears).
    pub fn set_note(&mut self, new_name: &str, new_comment: &str) {
        if let HotItem::Folder { name, comment, .. } = self {
            *name = new_name.trim().to_owned();
            *comment = new_comment.trim().to_owned();
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
        HotItem::Folder { .. } => None,
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
            items.push(HotItem::folder_at(folder.to_path_buf()));
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

// ---- the hotlist tree (left panel) ------------------------------------------
//
// Entries there are addressed by their full index path: the submenus leading
// to them, then their own index (`[2, 0]`: the first entry of the third).

/// The entry at `path`.
pub fn get<'a>(top: &'a [HotItem], path: &[usize]) -> Option<&'a HotItem> {
    let (&last, at) = path.split_last()?;
    menu_at(top, at)?.get(last)
}

/// Mutable [`get`].
pub fn get_mut<'a>(top: &'a mut [HotItem], path: &[usize]) -> Option<&'a mut HotItem> {
    let (&first, rest) = path.split_first()?;
    let item = top.get_mut(first)?;
    if rest.is_empty() {
        return Some(item);
    }
    match item {
        HotItem::Menu { items, .. } => get_mut(items, rest),
        HotItem::Folder { .. } => None,
    }
}

/// The menu at `at`, if every step of it is a submenu (unlike [`menu`]).
pub fn menu_at<'a>(top: &'a [HotItem], at: &[usize]) -> Option<&'a [HotItem]> {
    let mut items = top;
    for &i in at {
        match items.get(i)? {
            HotItem::Menu { items: sub, .. } => items = sub,
            HotItem::Folder { .. } => return None,
        }
    }
    Some(items)
}

/// Mutable [`menu_at`].
pub fn menu_at_mut<'a>(top: &'a mut Vec<HotItem>, at: &[usize]) -> Option<&'a mut Vec<HotItem>> {
    let mut items = top;
    for &i in at {
        match items.get_mut(i)? {
            HotItem::Menu { items: sub, .. } => items = sub,
            HotItem::Folder { .. } => return None,
        }
    }
    Some(items)
}

/// Takes the entry at `path` out of the list (a group with its entries).
pub fn take(top: &mut Vec<HotItem>, path: &[usize]) -> Option<HotItem> {
    let (&last, at) = path.split_last()?;
    let items = menu_at_mut(top, at)?;
    (last < items.len()).then(|| items.remove(last))
}

/// Inserts `item` into the menu at `at` (a top-level or group path) at
/// `index` (clamped to its end). Returns the item's path.
pub fn insert(top: &mut Vec<HotItem>, at: &[usize], index: usize, item: HotItem) -> Option<Vec<usize>> {
    let items = menu_at_mut(top, at)?;
    let index = index.min(items.len());
    items.insert(index, item);
    let mut path = at.to_vec();
    path.push(index);
    Some(path)
}

/// Moves the entry at `from` to `index` in the menu at `at`, as seen
/// before the move (so dropping right after itself changes nothing).
/// Refuses moving a group into itself or one of its subgroups. Returns the
/// entry's new path.
pub fn move_to(top: &mut Vec<HotItem>, from: &[usize], at: &[usize], index: usize) -> Option<Vec<usize>> {
    let (&i, from_menu) = from.split_last()?;
    if at.starts_with(from) || get(top, from).is_none() || menu_at(top, at).is_none() {
        return None;
    }
    // Taking the entry out shifts what follows it in its menu: the target
    // menu's path (if it runs through a later sibling) or the index.
    let mut at = at.to_vec();
    let mut index = index;
    let depth = from_menu.len();
    if at.len() > depth && at[..depth] == *from_menu && at[depth] > i {
        at[depth] -= 1;
    } else if at == from_menu && index > i {
        index -= 1;
    }
    let item = take(top, from)?;
    insert(top, &at, index, item)
}

/// One line of the hotlist tree: an entry and how deep it is nested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub path: Vec<usize>,
    pub depth: usize,
}

/// The lines of the tree: every entry of the top menu and, under an
/// expanded group, its entries.
pub fn rows(top: &[HotItem]) -> Vec<TreeRow> {
    fn walk(items: &[HotItem], prefix: &mut Vec<usize>, out: &mut Vec<TreeRow>) {
        for (i, item) in items.iter().enumerate() {
            prefix.push(i);
            out.push(TreeRow { path: prefix.clone(), depth: prefix.len() - 1 });
            if let HotItem::Menu { expanded: true, items: sub, .. } = item {
                walk(sub, prefix, out);
            }
            prefix.pop();
        }
    }
    let mut out = Vec::new();
    walk(top, &mut Vec::new(), &mut out);
    out
}

/// Opens or closes the group at `path`, or sets it to `open`. Returns its
/// new state (`None`: not a group).
pub fn set_expanded(top: &mut [HotItem], path: &[usize], open: Option<bool>) -> Option<bool> {
    match get_mut(top, path)? {
        HotItem::Menu { expanded, .. } => {
            *expanded = open.unwrap_or(!*expanded);
            Some(*expanded)
        }
        HotItem::Folder { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(p: &str) -> HotItem {
        HotItem::folder_at(PathBuf::from(p))
    }

    fn sub(name: &str, items: Vec<HotItem>) -> HotItem {
        HotItem::Menu { name: name.into(), expanded: false, items }
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

    fn open(name: &str, items: Vec<HotItem>) -> HotItem {
        HotItem::Menu { name: name.into(), expanded: true, items }
    }

    #[test]
    fn tree_paths_get_take_insert() {
        let mut top = vec![f("a"), open("G", vec![f("g1"), sub("H", vec![f("h1")])]), f("b")];
        assert_eq!(get(&top, &[1, 1, 0]), Some(&f("h1")));
        assert_eq!(get(&top, &[0, 0]), None);
        assert_eq!(get(&top, &[]), None);
        assert_eq!(get(&top, &[9]), None);
        assert_eq!(top[1].count(), 3);
        assert_eq!(top[1].label(), "G");
        assert_eq!(f(r"C:\x\y").label(), "y");
        assert_eq!(insert(&mut top, &[1, 1], 99, f("h2")), Some(vec![1, 1, 1]));
        assert_eq!(insert(&mut top, &[0], 0, f("x")), None);
        assert_eq!(take(&mut top, &[1, 1, 0]), Some(f("h1")));
        assert_eq!(take(&mut top, &[1, 1, 5]), None);
        assert_eq!(get(&top, &[1, 1, 0]), Some(&f("h2")));
        let group = take(&mut top, &[1]).unwrap();
        assert_eq!(group.count(), 3);
        assert_eq!(top, [f("a"), f("b")]);
    }

    #[test]
    fn tree_rows_follow_expanded_groups() {
        let mut top = vec![f("a"), open("G", vec![f("g1"), sub("H", vec![f("h1")])]), f("b")];
        let paths = |top: &[HotItem]| rows(top).into_iter().map(|r| (r.path, r.depth)).collect::<Vec<_>>();
        assert_eq!(paths(&top), [(vec![0], 0), (vec![1], 0), (vec![1, 0], 1), (vec![1, 1], 1), (vec![2], 0)]);
        assert_eq!(set_expanded(&mut top, &[1, 1], None), Some(true));
        assert_eq!(paths(&top)[4], (vec![1, 1, 0], 2));
        assert_eq!(set_expanded(&mut top, &[1], Some(false)), Some(false));
        assert_eq!(paths(&top), [(vec![0], 0), (vec![1], 0), (vec![2], 0)]);
        assert_eq!(set_expanded(&mut top, &[0], None), None);
    }

    #[test]
    fn tree_moves_within_and_across_groups() {
        let start = vec![f("a"), open("G", vec![f("g1"), f("g2")]), f("b"), sub("H", vec![])];

        // Down within the top menu: the index is as seen before the move.
        let mut top = start.clone();
        assert_eq!(move_to(&mut top, &[0], &[], 3), Some(vec![2]));
        assert_eq!(top[..3], [start[1].clone(), f("b"), f("a")]);
        // Dropping right after itself changes nothing.
        let mut top = start.clone();
        assert_eq!(move_to(&mut top, &[0], &[], 1), Some(vec![0]));
        assert_eq!(top, start);
        // Up.
        let mut top = start.clone();
        assert_eq!(move_to(&mut top, &[2], &[], 0), Some(vec![0]));
        assert_eq!(top[0], f("b"));

        // Into a group that comes after the entry: its path shifts up.
        let mut top = start.clone();
        assert_eq!(move_to(&mut top, &[0], &[3], 0), Some(vec![2, 0]));
        assert_eq!(top[2], sub("H", vec![f("a")]));
        // Into a group before it.
        let mut top = start.clone();
        assert_eq!(move_to(&mut top, &[2], &[1], 1), Some(vec![1, 1]));
        assert_eq!(top[1], open("G", vec![f("g1"), f("b"), f("g2")]));
        // Out of a group to the top.
        let mut top = start.clone();
        assert_eq!(move_to(&mut top, &[1, 1], &[], 0), Some(vec![0]));
        assert_eq!(top[..2], [f("g2"), f("a")]);
        assert_eq!(top[2], open("G", vec![f("g1")]));
        // A whole group into another one.
        let mut top = start.clone();
        assert_eq!(move_to(&mut top, &[1], &[3], 0), Some(vec![2, 0]));
        assert_eq!(top, [f("a"), f("b"), sub("H", vec![start[1].clone()])]);
        // Never into itself or below itself; stale paths do nothing.
        let mut top = start.clone();
        assert_eq!(move_to(&mut top, &[1], &[1], 0), None);
        assert_eq!(move_to(&mut top, &[1], &[1, 0], 0), None);
        assert_eq!(move_to(&mut top, &[7], &[], 0), None);
        assert_eq!(move_to(&mut top, &[0], &[0], 0), None);
        assert_eq!(top, start);
    }

    #[test]
    fn notes_round_trip_and_old_files_load() {
        #[derive(Serialize, Deserialize)]
        struct S {
            hotlist: Vec<HotItem>,
        }
        let mut named = f("c");
        named.set_note("  Mine ", "");
        let mut both = f("d");
        both.set_note("", " the notes ");
        let s = S { hotlist: vec![f("a"), named, open("G", vec![both.clone()])] };
        let text = toml::to_string_pretty(&s).unwrap();
        // Entries without a note stay plain strings.
        assert_eq!(text.matches("path").count(), 2, "{text}");
        assert_eq!(text.matches("comment").count(), 1, "{text}");
        let back: S = toml::from_str(&text).unwrap();
        assert_eq!(back.hotlist, s.hotlist);
        assert_eq!(back.hotlist[1].label(), "Mine");
        assert_eq!(back.hotlist[1].tip().as_deref(), Some("c"));
        assert_eq!(both.label(), "d");
        assert_eq!(both.tip().as_deref(), Some("the notes\nd"));
        assert_eq!(f("a").tip(), None);
        let old: S = toml::from_str("hotlist = ['a', { path = 'b', name = 'B' }, { name = 'G', items = ['g'] }]").unwrap();
        assert_eq!(old.hotlist[0], f("a"));
        assert_eq!(old.hotlist[1].label(), "B");
        assert_eq!(old.hotlist[2], sub("G", vec![f("g")]));
    }

    #[test]
    fn expanded_state_round_trips_and_defaults_off() {
        #[derive(Serialize, Deserialize)]
        struct S {
            hotlist: Vec<HotItem>,
        }
        let s = S { hotlist: vec![f("a"), open("G", vec![f("g")]), sub("H", vec![])] };
        let text = toml::to_string_pretty(&s).unwrap();
        assert_eq!(text.matches("expanded").count(), 1, "{text}");
        let back: S = toml::from_str(&text).unwrap();
        assert_eq!(back.hotlist, s.hotlist);
        let old: S = toml::from_str("hotlist = ['a', { name = 'G', items = ['g'] }]").unwrap();
        assert_eq!(old.hotlist, [f("a"), sub("G", vec![f("g")])]);
    }
}
