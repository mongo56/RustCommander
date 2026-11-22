//! Disk usage scan: walks a folder once and keeps the whole tree in memory, with
//! folder sizes summed up and every folder's children sorted by size, for the
//! disk usage tool (treemap and list).
//!
//! The tree is a flat [`Vec`] of [`Node`]s linked by `u32` indices (index 0 is
//! the scanned folder), which keeps memory low enough for millions of files.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Index of the root node in [`Tree::nodes`].
pub const ROOT: u32 = 0;
/// `ext_id` of files without an extension (`exts[0]`, name "").
pub const EXT_NONE: u16 = 0;
/// `ext_id` of files whose extension didn't fit in the table any more
/// (`exts[1]`, name "*").
pub const EXT_OTHER: u16 = 1;
/// `ext_id` of folders (not an index into `exts`; folders aren't counted there).
pub const EXT_DIR: u16 = u16::MAX;

/// How often [`scan`] reports progress at most.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A file, or a link/junction (counted as itself, never followed).
    File,
    Dir,
}

#[derive(Debug, Clone)]
pub struct Node {
    /// File name; for the root, the scanned path as given.
    pub name: Box<str>,
    /// Bytes: a file's logical length, or the sum of everything under a folder.
    pub size: u64,
    /// Bytes allocated on disk: a file's own, or the sum under a folder.
    pub alloc: u64,
    /// Files under a folder, recursively (0 for a file).
    pub files: u32,
    /// Folders under a folder, recursively, not counting itself (0 for a file).
    pub dirs: u32,
    pub kind: NodeKind,
    /// Index into [`Tree::exts`], or [`EXT_DIR`] for folders.
    pub ext_id: u16,
    /// Index of the parent folder; the root's parent is itself ([`ROOT`]).
    pub parent: u32,
    /// Child indices, largest first (empty for files).
    pub children: Vec<u32>,
}

/// Result of [`scan`].
#[derive(Debug, Clone, Default)]
pub struct Tree {
    /// All nodes; `nodes[0]` is the root. A child's index is always greater
    /// than its parent's.
    pub nodes: Vec<Node>,
    /// Extension table: (lower-case extension without the dot, total bytes,
    /// file count). `exts[0]` is "no extension", `exts[1]` the "other" bucket.
    pub exts: Vec<(Box<str>, u64, u32)>,
    /// Number of folders that couldn't be read (fully or partly) and were skipped.
    pub errors: u32,
    /// Allocated bytes per extension, parallel to `exts`.
    pub ext_alloc: Vec<u64>,
    /// Which size children are sorted by and the UI shows.
    pub measure: Measure,
    /// File nodes that are further hard links of a file already counted at
    /// another node (they count 0 bytes everywhere), by node index.
    pub hard_links: HashMap<u32, HardLink>,
}

/// Which size to show and sort by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Measure {
    /// Logical length.
    #[default]
    Size,
    /// Bytes allocated on disk.
    Alloc,
}

/// `size` rounded up to whole clusters (0 stays 0).
pub fn round_to_cluster(size: u64, cluster: u64) -> u64 {
    let c = cluster.max(1);
    size.div_ceil(c).saturating_mul(c)
}

/// Bytes a file takes on disk. Compressed, sparse and cloud files ask the
/// system; the rest are rounded up to whole clusters (no extra call).
/// `attributes` are the Windows `FILE_ATTRIBUTE_*` bits (0 elsewhere).
fn allocated(len: u64, attributes: u32, path: impl FnOnce() -> PathBuf, cluster: u64) -> u64 {
    // COMPRESSED, SPARSE_FILE, OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS.
    const SPECIAL: u32 = 0x800 | 0x200 | 0x1000 | 0x40000 | 0x400000;
    if attributes & SPECIAL != 0 {
        if let Some(a) = rc_platform::allocated_size(&path()) {
            return a;
        }
    }
    round_to_cluster(len, cluster)
}

/// A hard link whose file is counted at another link in the tree: its own
/// node has 0 bytes, these are the bytes it would have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardLink {
    pub size: u64,
    pub alloc: u64,
    /// The node that counts the file's bytes.
    pub counted: u32,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub files: u64,
    pub bytes: u64,
    /// The folder being read.
    pub current: PathBuf,
}

impl Tree {
    /// Node `index`'s bytes by the active measure.
    pub fn bytes(&self, index: u32) -> u64 {
        let n = &self.nodes[index as usize];
        match self.measure {
            Measure::Size => n.size,
            Measure::Alloc => n.alloc,
        }
    }

    /// Bytes per extension (parallel to `exts`) by the active measure.
    pub fn ext_bytes(&self) -> Vec<u64> {
        match self.measure {
            Measure::Size => self.exts.iter().map(|e| e.1).collect(),
            Measure::Alloc => (0..self.exts.len()).map(|i| self.ext_alloc.get(i).copied().unwrap_or(0)).collect(),
        }
    }

    /// Switches the measure and sorts every folder's children again.
    pub fn set_measure(&mut self, measure: Measure) {
        if self.measure == measure {
            return;
        }
        self.measure = measure;
        for i in 0..self.nodes.len() {
            if self.nodes[i].children.len() >= 2 {
                self.sort_children(i as u32);
            }
        }
    }

    /// Set when node `index` is a hard link counted at another node.
    pub fn hard_link(&self, index: u32) -> Option<&HardLink> {
        if self.hard_links.is_empty() {
            return None;
        }
        self.hard_links.get(&index)
    }

    /// A hard link's own bytes by the active measure.
    pub fn link_bytes(&self, link: &HardLink) -> u64 {
        match self.measure {
            Measure::Size => link.size,
            Measure::Alloc => link.alloc,
        }
    }

    pub fn root(&self) -> &Node {
        &self.nodes[ROOT as usize]
    }

    pub fn node(&self, index: u32) -> &Node {
        &self.nodes[index as usize]
    }

    /// The parent of `index`, or `None` for the root.
    pub fn parent(&self, index: u32) -> Option<u32> {
        (index != ROOT).then(|| self.node(index).parent)
    }

    /// Full path of node `index`, given the path the tree was scanned from.
    pub fn path(&self, root: &Path, index: u32) -> PathBuf {
        let mut names = Vec::new();
        let mut i = index;
        while i != ROOT {
            let node = self.node(i);
            names.push(&*node.name);
            i = node.parent;
        }
        let mut path = root.to_path_buf();
        path.extend(names.iter().rev());
        path
    }

    /// The extension of a file node ("" for none, "*" for the overflow bucket),
    /// or `None` for a folder.
    pub fn ext_name(&self, index: u32) -> Option<&str> {
        let id = self.node(index).ext_id;
        (id != EXT_DIR).then(|| &*self.exts[id as usize].0)
    }

    /// Takes node `index` (and everything under it) out of the tree, after it
    /// was deleted from disk: unlinks it from its folder, takes its size and
    /// counts off every folder above it, takes its files off the extension
    /// table and sorts the children of the changed folders again. The nodes
    /// stay in [`Tree::nodes`] (indices don't move) but can't be reached from
    /// the root any more. A removed hard link that counted a file still linked
    /// elsewhere in the tree passes its bytes on to that link. Returns false
    /// (and changes nothing) for the root, a node out of range, or one already
    /// removed.
    pub fn remove(&mut self, index: u32) -> bool {
        if index == ROOT || index as usize >= self.nodes.len() {
            return false;
        }
        let parent = self.node(index).parent;
        let Some(pos) = self.nodes[parent as usize].children.iter().position(|&c| c == index) else {
            return false;
        };
        self.nodes[parent as usize].children.remove(pos);
        let node = self.node(index);
        let (size, alloc, files, dirs) = match node.kind {
            NodeKind::File => (node.size, node.alloc, 1, 0),
            NodeKind::Dir => (node.size, node.alloc, node.files, node.dirs + 1),
        };
        // Its files leave the extension table.
        let track_links = !self.hard_links.is_empty();
        let mut removed_files = Vec::new();
        let mut stack = vec![index];
        while let Some(i) = stack.pop() {
            let n = &self.nodes[i as usize];
            match n.kind {
                NodeKind::File => {
                    if track_links {
                        removed_files.push(i);
                    }
                    if let Some(e) = self.exts.get_mut(n.ext_id as usize) {
                        e.1 = e.1.saturating_sub(n.size);
                        if let Some(a) = self.ext_alloc.get_mut(n.ext_id as usize) {
                            *a = a.saturating_sub(n.alloc);
                        }
                        e.2 = e.2.saturating_sub(1);
                    }
                }
                NodeKind::Dir => stack.extend(n.children.iter().copied()),
            }
        }
        // Up the chain: smaller folders, which may move down in their own folder.
        self.update_up(parent, |p| {
            p.size = p.size.saturating_sub(size);
            p.alloc = p.alloc.saturating_sub(alloc);
            p.files = p.files.saturating_sub(files);
            p.dirs = p.dirs.saturating_sub(dirs);
        });
        if track_links {
            self.pass_on_hard_links(&removed_files);
        }
        true
    }

    /// True when a hard link inside the subtree at `index` is counted at a
    /// node outside it, or one outside is counted inside. A rescan of that
    /// subtree alone can't tell which of its files are linked outside (the
    /// file ids aren't kept), so [`Tree::replace_subtree`] refuses then and the
    /// whole tree has to be scanned again.
    pub fn links_cross(&self, index: u32) -> bool {
        let inside = |mut i: u32| -> bool {
            loop {
                if i == index {
                    return true;
                }
                if i == ROOT {
                    return false;
                }
                i = self.nodes[i as usize].parent;
            }
        };
        self.hard_links.iter().any(|(&link, h)| inside(link) != inside(h.counted))
    }

    /// Replaces the folder `index` (and everything under it) with `sub`, a
    /// fresh [`scan`] of that folder, e.g. after a delete in it failed
    /// partway. The old nodes are taken out as by [`Tree::remove`] (they stay
    /// in [`Tree::nodes`], unreachable) and `sub`'s nodes are appended, so a
    /// child's index stays greater than its parent's. The folders above get
    /// the new sums and sort their children again, the new files go into the
    /// extension table, `sub`'s hard links (recomputed within the folder)
    /// join [`Tree::hard_links`] and its unreadable folders add to
    /// [`Tree::errors`] (the old subtree's errors aren't known, so they stay
    /// counted). Returns the folder's new index, or `None` (and changes
    /// nothing) for the root, a file, a removed node, a tree that would grow
    /// too large, or a folder with hard links across its boundary
    /// ([`Tree::links_cross`]).
    pub fn replace_subtree(&mut self, index: u32, mut sub: Tree) -> Option<u32> {
        if index == ROOT
            || index as usize >= self.nodes.len()
            || self.node(index).kind != NodeKind::Dir
            || self.surviving(index) != index
            || self.links_cross(index)
            || sub.nodes.is_empty()
        {
            return None;
        }
        let base = u32::try_from(self.nodes.len()).ok()?;
        base.checked_add(u32::try_from(sub.nodes.len()).ok()?)?;
        let parent = self.node(index).parent;
        let name = self.node(index).name.clone();
        self.remove(index);
        sub.set_measure(self.measure);
        // `sub`'s extension ids -> this tree's (by name; new ones are added).
        let mut by_name: HashMap<Box<str>, u16> =
            self.exts.iter().enumerate().skip(2).map(|(i, e)| (e.0.clone(), i as u16)).collect();
        let mut ext_map = Vec::with_capacity(sub.exts.len());
        for (i, e) in sub.exts.iter().enumerate() {
            let id = match i {
                0 => EXT_NONE,
                1 => EXT_OTHER,
                _ => match by_name.get(&e.0) {
                    Some(&id) => id,
                    None if self.exts.len() < EXT_DIR as usize => {
                        let id = self.exts.len() as u16;
                        self.exts.push((e.0.clone(), 0, 0));
                        self.ext_alloc.resize(self.exts.len(), 0);
                        by_name.insert(e.0.clone(), id);
                        id
                    }
                    None => EXT_OTHER,
                },
            };
            if let Some(t) = self.exts.get_mut(id as usize) {
                t.1 += e.1;
                t.2 += e.2;
            }
            if let (Some(a), Some(&b)) = (self.ext_alloc.get_mut(id as usize), sub.ext_alloc.get(i)) {
                *a += b;
            }
            ext_map.push(id);
        }
        let r = &sub.nodes[ROOT as usize];
        let (size, alloc, files, dirs) = (r.size, r.alloc, r.files, r.dirs.saturating_add(1));
        self.nodes.reserve(sub.nodes.len());
        for (i, mut n) in sub.nodes.into_iter().enumerate() {
            if i == ROOT as usize {
                n.name = name.clone();
                n.parent = parent;
            } else {
                n.parent += base;
            }
            for c in &mut n.children {
                *c += base;
            }
            if n.ext_id != EXT_DIR {
                n.ext_id = ext_map.get(n.ext_id as usize).copied().unwrap_or(EXT_OTHER);
            }
            self.nodes.push(n);
        }
        for (link, mut h) in sub.hard_links {
            h.counted += base;
            self.hard_links.insert(link + base, h);
        }
        self.errors = self.errors.saturating_add(sub.errors);
        self.nodes[parent as usize].children.push(base);
        self.update_up(parent, |p| {
            p.size += size;
            p.alloc += alloc;
            p.files = p.files.saturating_add(files);
            p.dirs = p.dirs.saturating_add(dirs);
        });
        Some(base)
    }

    /// Applies a [`rescan`] of folder `index`: a folder gone from disk is
    /// removed ([`Tree::remove`]) and its parent returned; a scanned one
    /// replaces the old subtree ([`Tree::replace_subtree`]) and its new index
    /// is returned. `None`: nothing changed (see those two); scan everything
    /// again then.
    pub fn splice(&mut self, index: u32, rescan: Rescan) -> Option<u32> {
        match rescan {
            Rescan::Gone => {
                let parent = self.parent(index)?;
                self.remove(index).then_some(parent)
            }
            Rescan::Tree(sub) => self.replace_subtree(index, sub),
        }
    }

    /// The deepest node on the way to `path` (a path under `root`, the
    /// folder the tree was scanned from), by name: `path`'s own node when it
    /// is in the tree, else the nearest folder above it that is.
    pub fn locate(&self, root: &Path, path: &Path) -> u32 {
        let mut node = ROOT;
        let Ok(rel) = path.strip_prefix(root) else { return ROOT };
        for part in rel.components() {
            let name = part.as_os_str().to_string_lossy();
            match self.node(node).children.iter().find(|&&c| *self.node(c).name == *name) {
                Some(&c) => node = c,
                None => break,
            }
        }
        node
    }

    /// After [`Tree::remove`] took `removed` files out: a removed file that
    /// counted the bytes of a file with other links still in the tree passes
    /// them on to the first of those (deleting one link frees nothing while
    /// others remain), so totals stay right.
    fn pass_on_hard_links(&mut self, removed: &[u32]) {
        let removed: std::collections::HashSet<u32> = removed.iter().copied().collect();
        self.hard_links.retain(|link, _| !removed.contains(link));
        // Removed counting node -> the remaining link (lowest index) that takes over.
        let mut heirs: HashMap<u32, u32> = HashMap::new();
        for (&link, h) in &self.hard_links {
            if removed.contains(&h.counted) {
                heirs.entry(h.counted).and_modify(|n| *n = (*n).min(link)).or_insert(link);
            }
        }
        for &heir in heirs.values() {
            let Some(h) = self.hard_links.remove(&heir) else { continue };
            let node = &mut self.nodes[heir as usize];
            node.size = h.size;
            node.alloc = h.alloc;
            let (ext, parent) = (node.ext_id as usize, node.parent);
            if let Some(e) = self.exts.get_mut(ext) {
                e.1 += h.size;
            }
            if let Some(a) = self.ext_alloc.get_mut(ext) {
                *a += h.alloc;
            }
            self.update_up(parent, |p| {
                p.size += h.size;
                p.alloc += h.alloc;
            });
        }
        for h in self.hard_links.values_mut() {
            if let Some(&heir) = heirs.get(&h.counted) {
                h.counted = heir;
            }
        }
    }

    /// Applies `f` to folder `index` and every folder above it, sorting each
    /// one's children again.
    fn update_up(&mut self, mut index: u32, f: impl Fn(&mut Node)) {
        loop {
            f(&mut self.nodes[index as usize]);
            self.sort_children(index);
            if index == ROOT {
                break;
            }
            index = self.nodes[index as usize].parent;
        }
    }

    /// `index` if it can still be reached from the root, else the nearest
    /// folder above it that can (after [`Tree::remove`]).
    pub fn surviving(&self, index: u32) -> u32 {
        if index as usize >= self.nodes.len() {
            return ROOT;
        }
        let mut chain = vec![index];
        let mut i = index;
        while i != ROOT {
            i = self.node(i).parent;
            chain.push(i);
        }
        // From the root down: the last node still linked to its folder.
        let mut found = ROOT;
        for pair in chain.windows(2).rev() {
            let (child, parent) = (pair[0], pair[1]);
            if !self.node(parent).children.contains(&child) {
                break;
            }
            found = child;
        }
        found
    }

    /// Sorts the children of `index` largest first (ties by name).
    fn sort_children(&mut self, index: u32) {
        let mut children = std::mem::take(&mut self.nodes[index as usize].children);
        let nodes = &self.nodes;
        let by_alloc = self.measure == Measure::Alloc;
        children.sort_by(|&a, &b| {
            let (a, b) = (&nodes[a as usize], &nodes[b as usize]);
            let (x, y) = if by_alloc { (a.alloc, b.alloc) } else { (a.size, b.size) };
            y.cmp(&x).then_with(|| a.name.cmp(&b.name))
        });
        self.nodes[index as usize].children = children;
    }
}

/// Lower-case extension of a file name, without the dot ("" for none; a
/// leading dot alone, as in ".gitignore", isn't an extension).
fn extension(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?;
    Some(ext.to_lowercase())
}

/// State of a running [`scan`].
struct Scanner {
    tree: Tree,
    ext_ids: HashMap<String, u16>,
    cluster: u64,
    files: u64,
    bytes: u64,
    /// Folders still to read: (node, path).
    stack: Vec<(u32, PathBuf)>,
    /// (file id, node) of every file that may have other hard links: ids
    /// that fit in 64 bits (NTFS) here, wider ones (ReFS, Unix device+inode)
    /// in `wide`, so the common case takes 16 bytes per file.
    ids: Vec<(u64, u32)>,
    wide: Vec<(u128, u32)>,
}

impl Scanner {
    /// The next node's index, or `None` once the tree is full.
    fn next_index(&self) -> Option<u32> {
        u32::try_from(self.tree.nodes.len()).ok()
    }

    fn add_dir(&mut self, parent: u32, name: Box<str>, path: PathBuf) -> u32 {
        let index = self.tree.nodes.len() as u32;
        self.stack.push((index, path));
        self.tree.nodes.push(Node {
            name,
            size: 0,
            alloc: 0,
            files: 0,
            dirs: 0,
            kind: NodeKind::Dir,
            ext_id: EXT_DIR,
            parent,
            children: Vec::new(),
        });
        index
    }

    /// Adds a file; `id` identifies it on the volume when it may have other
    /// hard links in the tree.
    fn add_file(&mut self, parent: u32, name: Box<str>, size: u64, alloc: u64, id: Option<u128>) -> u32 {
        let index = self.tree.nodes.len() as u32;
        let tree = &mut self.tree;
        let ext_id = match extension(&name) {
            None => EXT_NONE,
            Some(ext) => match self.ext_ids.get(&ext) {
                Some(&id) => id,
                None if tree.exts.len() < EXT_DIR as usize => {
                    let id = tree.exts.len() as u16;
                    tree.exts.push((ext.as_str().into(), 0, 0));
                    tree.ext_alloc.push(0);
                    self.ext_ids.insert(ext, id);
                    id
                }
                None => EXT_OTHER,
            },
        };
        let entry = &mut tree.exts[ext_id as usize];
        entry.1 += size;
        entry.2 += 1;
        tree.ext_alloc[ext_id as usize] += alloc;
        self.files += 1;
        self.bytes += size;
        match id {
            Some(id) if id >> 64 == 0 => self.ids.push((id as u64, index)),
            Some(id) => self.wide.push((id, index)),
            None => {}
        }
        tree.nodes.push(Node {
            name,
            size,
            alloc,
            files: 0,
            dirs: 0,
            kind: NodeKind::File,
            ext_id,
            parent,
            children: Vec::new(),
        });
        index
    }

    /// Reads folder `dir` (node `dir_index`) with `std::fs::read_dir`.
    /// Returns its children and whether everything could be read, or `None`
    /// when it can't be listed at all.
    fn read_std(&mut self, dir_index: u32, dir: &Path, cancel: &AtomicBool) -> Option<(Vec<u32>, bool)> {
        let items = fs::read_dir(dir).ok()?;
        let mut complete = true;
        let mut children = Vec::new();
        for item in items {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Ok(item) = item else {
                complete = false;
                continue;
            };
            // `DirEntry::file_type` doesn't follow links: a link or junction to a
            // folder isn't `is_dir` here.
            let Ok(file_type) = item.file_type() else {
                complete = false;
                continue;
            };
            if self.next_index().is_none() {
                break;
            }
            let name: Box<str> = item.file_name().to_string_lossy().into();
            let index = if file_type.is_dir() {
                self.add_dir(dir_index, name, item.path())
            } else {
                // On Windows `DirEntry::metadata` is the link's own, without following it.
                let (size, alloc, id) = match item.metadata() {
                    Ok(meta) => {
                        #[cfg(windows)]
                        let attributes = std::os::windows::fs::MetadataExt::file_attributes(&meta);
                        #[cfg(not(windows))]
                        let attributes = 0;
                        (meta.len(), allocated(meta.len(), attributes, || item.path(), self.cluster), unix_id(&meta))
                    }
                    Err(_) => {
                        complete = false;
                        (0, 0, None)
                    }
                };
                self.add_file(dir_index, name, size, alloc, id)
            };
            children.push(index);
        }
        Some((children, complete))
    }

    /// Reads folder `dir` with [`rc_platform::read_dir_ids`], which also
    /// gives every file's id. Files get an id only when the folder is on
    /// `volume` (set from the first folder read), so ids never mix volumes.
    fn read_fast(
        &mut self,
        dir_index: u32,
        dir: &Path,
        volume: &mut Option<u64>,
        cancel: &AtomicBool,
    ) -> (rc_platform::DirRead, Vec<u32>) {
        let mut children = Vec::new();
        let read = rc_platform::read_dir_ids(dir, |item| {
            if cancel.load(Ordering::Relaxed) || self.next_index().is_none() {
                return false;
            }
            let name: Box<str> = String::from_utf16_lossy(item.name).into();
            let index = if item.is_dir() {
                self.add_dir(dir_index, name.clone(), dir.join(&*name))
            } else {
                if volume.is_none() {
                    *volume = item.volume;
                }
                let id = item.id.filter(|_| item.volume.is_some() && item.volume == *volume);
                let alloc = allocated(item.len, item.attributes, || dir.join(&*name), self.cluster);
                self.add_file(dir_index, name, item.len, alloc, id)
            };
            children.push(index);
            true
        });
        (read, children)
    }

    /// Counts every file with several hard links in the tree once: the link
    /// found first keeps its bytes, the others get 0 and an entry in
    /// [`Tree::hard_links`]. Their bytes also leave the extension table.
    fn count_hard_links_once(&mut self) {
        let mut pairs = Vec::new();
        duplicates(&mut self.ids, &mut pairs);
        duplicates(&mut self.wide, &mut pairs);
        let tree = &mut self.tree;
        for (link, counted) in pairs {
            let node = &mut tree.nodes[link as usize];
            let (size, alloc) = (node.size, node.alloc);
            node.size = 0;
            node.alloc = 0;
            let ext = node.ext_id as usize;
            tree.exts[ext].1 -= size;
            tree.ext_alloc[ext] -= alloc;
            tree.hard_links.insert(link, HardLink { size, alloc, counted });
        }
    }
}

/// Sorts `ids` and appends (later link, first link) for every node whose id
/// an earlier node (lower index, found first) has too. Frees `ids`.
fn duplicates<K: Ord + Copy>(ids: &mut Vec<(K, u32)>, out: &mut Vec<(u32, u32)>) {
    ids.sort_unstable();
    for run in ids.chunk_by(|a, b| a.0 == b.0) {
        out.extend(run[1..].iter().map(|&(_, link)| (link, run[0].1)));
    }
    *ids = Vec::new();
}

/// Device and inode of a file with more than one hard link (Unix); `None`
/// elsewhere or for a single link.
fn unix_id(meta: &fs::Metadata) -> Option<u128> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.nlink() > 1).then(|| ((meta.dev() as u128) << 64) | meta.ino() as u128)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Result of [`rescan`].
#[derive(Debug, Clone)]
pub enum Rescan {
    /// The folder isn't there (or isn't a folder) any more.
    Gone,
    Tree(Tree),
}

/// Scans folder `path` again (a folder already in a tree, for
/// [`Tree::splice`]), or tells it is gone. `None` once `cancel` is set.
pub fn rescan(path: &Path, cancel: &AtomicBool, progress: impl FnMut(Progress)) -> Option<Rescan> {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.is_dir() => Some(Rescan::Gone),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Rescan::Gone),
        _ => scan(path, cancel, progress).map(Rescan::Tree),
    }
}

/// Scans `root` recursively. Links and junctions count as themselves and are
/// not followed; unreadable folders are skipped and counted in
/// [`Tree::errors`]. `size` is the logical length and `alloc` the space taken
/// on disk (whole clusters; the system's figure for compressed, sparse and
/// cloud files). A file with several hard links in the tree counts once (see
/// [`Tree::hard_links`]). `progress` is called at most every ~100 ms.
/// Returns `None` once `cancel` is set.
///
/// On Windows folders are listed with their files' ids
/// ([`rc_platform::read_dir_ids`]), which costs no extra call per file; hard
/// links are then found by sorting the ids once at the end (16 bytes per
/// file while scanning). Where that listing isn't supported (FAT, some
/// network shares) `std::fs::read_dir` is used and hard links aren't
/// detected. On Unix only files with a link count above 1 are tracked.
pub fn scan(root: &Path, cancel: &AtomicBool, mut progress: impl FnMut(Progress)) -> Option<Tree> {
    let tree = Tree {
        nodes: vec![Node {
            name: root.to_string_lossy().into(),
            size: 0,
            alloc: 0,
            files: 0,
            dirs: 0,
            kind: NodeKind::Dir,
            ext_id: EXT_DIR,
            parent: ROOT,
            children: Vec::new(),
        }],
        exts: vec![("".into(), 0, 0), ("*".into(), 0, 0)],
        ext_alloc: vec![0, 0],
        ..Tree::default()
    };
    let mut s = Scanner {
        tree,
        ext_ids: HashMap::new(),
        cluster: rc_platform::cluster_size(root),
        files: 0,
        bytes: 0,
        stack: vec![(ROOT, root.to_path_buf())],
        ids: Vec::new(),
        wide: Vec::new(),
    };
    let mut fast = cfg!(windows);
    let mut volume = None;
    let mut last_report = Instant::now();
    while let Some((dir_index, dir)) = s.stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if last_report.elapsed() >= PROGRESS_EVERY {
            progress(Progress { files: s.files, bytes: s.bytes, current: dir.clone() });
            last_report = Instant::now();
        }
        let mut listed = None;
        if fast {
            match s.read_fast(dir_index, &dir, &mut volume, cancel) {
                (rc_platform::DirRead::Done(complete), children) => listed = Some((children, complete)),
                (rc_platform::DirRead::Fallback(never), _) => fast = !never,
            }
        }
        let listed = match listed {
            Some(l) => Some(l),
            None => s.read_std(dir_index, &dir, cancel),
        };
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Some((mut children, complete)) = listed else {
            s.tree.errors += 1;
            continue;
        };
        if !complete {
            s.tree.errors += 1;
        }
        children.shrink_to_fit();
        s.tree.nodes[dir_index as usize].children = children;
    }
    s.count_hard_links_once();
    let mut tree = s.tree;
    // Children always come after their parent, so one pass from the end sums
    // every folder before its own parent sees it.
    for i in (1..tree.nodes.len()).rev() {
        let node = &tree.nodes[i];
        let (size, alloc, parent) = (node.size, node.alloc, node.parent as usize);
        let (files, dirs) = match node.kind {
            NodeKind::File => (1, 0),
            NodeKind::Dir => (node.files, node.dirs + 1),
        };
        let p = &mut tree.nodes[parent];
        p.size += size;
        p.alloc += alloc;
        p.files = p.files.saturating_add(files);
        p.dirs = p.dirs.saturating_add(dirs);
    }
    for i in 0..tree.nodes.len() {
        if tree.nodes[i].children.len() >= 2 {
            tree.sort_children(i as u32);
        }
    }
    Some(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rc-du-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn child<'a>(tree: &'a Tree, parent: u32, name: &str) -> (u32, &'a Node) {
        let i = *tree.node(parent).children.iter().find(|&&c| &*tree.node(c).name == name).unwrap();
        (i, tree.node(i))
    }

    #[test]
    fn sums_sizes_sorts_children_and_counts_extensions() {
        let root = temp("sum");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("x.TXT"), [0u8; 10]).unwrap();
        fs::write(root.join("noext"), [0u8; 5]).unwrap();
        fs::write(root.join(".hidden"), [0u8; 1]).unwrap();
        fs::write(root.join("a/y.txt"), [0u8; 20]).unwrap();
        fs::write(root.join("a/b/z.rs"), [0u8; 30]).unwrap();
        let tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(tree.errors, 0);
        let r = tree.root();
        assert_eq!((r.size, r.files, r.dirs), (66, 5, 3));
        let names: Vec<&str> = r.children.iter().map(|&c| &*tree.node(c).name).collect();
        assert_eq!(names, ["a", "x.TXT", "noext", ".hidden", "empty"]);
        let (ai, a) = child(&tree, ROOT, "a");
        assert_eq!((a.size, a.files, a.dirs, a.kind), (50, 2, 1, NodeKind::Dir));
        assert_eq!(a.children.iter().map(|&c| tree.node(c).size).collect::<Vec<_>>(), [30, 20]);
        let (bi, _) = child(&tree, ai, "b");
        let (zi, z) = child(&tree, bi, "z.rs");
        assert_eq!((z.size, z.kind, z.parent), (30, NodeKind::File, bi));
        assert_eq!(tree.path(&root, zi), root.join("a").join("b").join("z.rs"));
        assert_eq!(tree.path(&root, ROOT), root);
        assert_eq!(tree.parent(zi), Some(bi));
        assert_eq!(tree.parent(ROOT), None);
        assert_eq!(tree.ext_name(zi), Some("rs"));
        assert_eq!(tree.ext_name(ai), None);
        let ext = |name: &str| tree.exts.iter().find(|e| &*e.0 == name).map(|e| (e.1, e.2));
        assert_eq!(ext("txt"), Some((30, 2)));
        assert_eq!(ext("rs"), Some((30, 1)));
        assert_eq!(ext(""), Some((6, 2)));
        assert_eq!(tree.exts[EXT_OTHER as usize].2, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_a_file_updates_the_folders_above_and_the_extensions() {
        let root = temp("rm-file");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/b/big.rs"), [0u8; 40]).unwrap();
        fs::write(root.join("a/small.txt"), [0u8; 5]).unwrap();
        fs::write(root.join("c/mid.txt"), [0u8; 30]).unwrap();
        let mut tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        let names =
            |t: &Tree, i: u32| t.node(i).children.iter().map(|&c| t.node(c).name.to_string()).collect::<Vec<_>>();
        assert_eq!(names(&tree, ROOT), ["a", "c"]);
        let (ai, _) = child(&tree, ROOT, "a");
        let (bi, _) = child(&tree, ai, "b");
        let (fi, _) = child(&tree, bi, "big.rs");
        assert!(tree.remove(fi));
        let r = tree.root();
        assert_eq!((r.size, r.files, r.dirs), (35, 2, 3));
        let a = tree.node(ai);
        assert_eq!((a.size, a.files, a.dirs), (5, 1, 1));
        assert_eq!((tree.node(bi).size, tree.node(bi).files, tree.node(bi).children.len()), (0, 0, 0));
        // "a" shrank below "c"; inside "a" the empty "b" went below the file.
        assert_eq!(names(&tree, ROOT), ["c", "a"]);
        assert_eq!(names(&tree, ai), ["small.txt", "b"]);
        let ext = |t: &Tree, name: &str| t.exts.iter().find(|e| &*e.0 == name).map(|e| (e.1, e.2));
        assert_eq!(ext(&tree, "rs"), Some((0, 0)));
        assert_eq!(ext(&tree, "txt"), Some((35, 2)));
        // Again: already gone.
        assert!(!tree.remove(fi));
        assert_eq!((tree.surviving(fi), tree.surviving(bi)), (bi, bi));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_a_folder_takes_its_whole_subtree() {
        let root = temp("rm-dir");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/z.rs"), [0u8; 30]).unwrap();
        fs::write(root.join("a/y.txt"), [0u8; 20]).unwrap();
        fs::write(root.join("x.txt"), [0u8; 10]).unwrap();
        let mut tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        let (ai, _) = child(&tree, ROOT, "a");
        let (bi, _) = child(&tree, ai, "b");
        let (zi, _) = child(&tree, bi, "z.rs");
        assert!(tree.remove(ai));
        assert_eq!((tree.surviving(zi), tree.surviving(bi), tree.surviving(ai)), (ROOT, ROOT, ROOT));
        let r = tree.root();
        assert_eq!((r.size, r.files, r.dirs), (10, 1, 0));
        assert_eq!(r.children.iter().map(|&c| &*tree.node(c).name).collect::<Vec<_>>(), ["x.txt"]);
        let ext = |name: &str| tree.exts.iter().find(|e| &*e.0 == name).map(|e| (e.1, e.2));
        assert_eq!(ext("txt"), Some((10, 1)));
        assert_eq!(ext("rs"), Some((0, 0)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_root_is_never_removed() {
        let root = temp("rm-root");
        fs::write(root.join("f"), [0u8; 3]).unwrap();
        let mut tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(!tree.remove(ROOT));
        assert!(!tree.remove(99));
        assert_eq!((tree.root().size, tree.root().files), (3, 1));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cancel_returns_none() {
        let root = temp("cancel");
        fs::write(root.join("f"), [0u8; 3]).unwrap();
        assert!(scan(&root, &AtomicBool::new(true), |_| {}).is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_root_counts_an_error() {
        let root = temp("missing").join("nope");
        let tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!((tree.errors, tree.root().size, tree.nodes.len()), (1, 0, 1));
    }

    #[cfg(windows)]
    #[test]
    fn junctions_are_not_followed() {
        let root = temp("link");
        fs::create_dir_all(root.join("real")).unwrap();
        fs::write(root.join("real/big"), [0u8; 100]).unwrap();
        if junction::create(root.join("real"), root.join("jn")).is_err() {
            eprintln!("skipped: can't create a junction");
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        let r = tree.root();
        assert_eq!((r.size, r.files, r.dirs), (100, 2, 1));
        let (_, jn) = child(&tree, ROOT, "jn");
        assert_eq!((jn.kind, jn.children.len()), (NodeKind::File, 0));
        fs::remove_dir(root.join("jn")).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rounding_to_clusters() {
        assert_eq!(round_to_cluster(0, 4096), 0);
        assert_eq!(round_to_cluster(1, 4096), 4096);
        assert_eq!(round_to_cluster(4096, 4096), 4096);
        assert_eq!(round_to_cluster(4097, 4096), 8192);
        assert_eq!(round_to_cluster(5, 0), 5, "unknown cluster size");
    }

    #[test]
    fn alloc_is_summed_removed_and_sortable() {
        let root = temp("alloc");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/a"), [0u8; 10]).unwrap();
        fs::write(root.join("b"), [0u8; 3]).unwrap();
        fs::write(root.join("empty"), []).unwrap();
        let mut tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        let c = rc_platform::cluster_size(&root);
        let (di, d) = child(&tree, ROOT, "d");
        assert_eq!(d.alloc, round_to_cluster(10, c));
        assert_eq!(child(&tree, ROOT, "empty").1.alloc, 0);
        let want = round_to_cluster(10, c) + round_to_cluster(3, c);
        assert_eq!(tree.root().alloc, want);
        assert_eq!(tree.ext_bytes()[EXT_NONE as usize], 13);
        tree.set_measure(Measure::Alloc);
        assert_eq!(tree.ext_bytes()[EXT_NONE as usize], want);
        assert_eq!(tree.bytes(ROOT), want);
        assert!(tree.remove(di));
        assert_eq!(tree.root().alloc, round_to_cluster(3, c));
        assert_eq!((tree.root().size, tree.ext_bytes()[EXT_NONE as usize]), (3, round_to_cluster(3, c)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn measure_changes_the_child_order() {
        let mk = |name: &str, size, alloc, parent| Node {
            name: name.into(),
            size,
            alloc,
            files: 0,
            dirs: 0,
            kind: NodeKind::File,
            ext_id: 0,
            parent,
            children: Vec::new(),
        };
        let mut root = mk("r", 30, 20, ROOT);
        root.kind = NodeKind::Dir;
        root.children = vec![1, 2];
        let mut tree = Tree { nodes: vec![root, mk("big", 20, 4, ROOT), mk("small", 10, 16, ROOT)], ..Tree::default() };
        tree.set_measure(Measure::Alloc);
        assert_eq!(tree.root().children, [2, 1]);
        tree.set_measure(Measure::Size);
        assert_eq!(tree.root().children, [1, 2]);
    }

    #[test]
    fn duplicates_pair_later_links_with_the_first() {
        let mut ids = vec![(7u64, 5), (3, 1), (7, 2), (9, 4), (7, 8)];
        let mut out = Vec::new();
        duplicates(&mut ids, &mut out);
        assert_eq!(out, [(5, 2), (8, 2)]);
        assert!(ids.is_empty());
    }

    #[test]
    fn hard_links_count_once_and_pass_their_bytes_on_when_removed() {
        let root = temp("hardlink");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::create_dir_all(root.join("e")).unwrap();
        fs::write(root.join("a.bin"), [0u8; 100]).unwrap();
        fs::write(root.join("other.bin"), [0u8; 7]).unwrap();
        if fs::hard_link(root.join("a.bin"), root.join("d/b.bin")).is_err()
            || fs::hard_link(root.join("a.bin"), root.join("e/c.bin")).is_err()
        {
            eprintln!("skipped: can't create hard links here");
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let mut tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        if tree.hard_links.is_empty() {
            eprintln!("skipped: the file system gives no file ids");
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let r = tree.root();
        assert_eq!((r.size, r.files, r.dirs), (107, 4, 2));
        let (ai, _) = child(&tree, ROOT, "a.bin");
        let (di, _) = child(&tree, ROOT, "d");
        let (ei, _) = child(&tree, ROOT, "e");
        let (bi, _) = child(&tree, di, "b.bin");
        let (ci, _) = child(&tree, ei, "c.bin");
        let links = [ai, bi, ci];
        let counted: Vec<u32> = links.iter().copied().filter(|&i| tree.node(i).size == 100).collect();
        assert_eq!(counted.len(), 1, "one link keeps the bytes");
        assert_eq!(tree.hard_links.len(), 2);
        for &i in links.iter().filter(|&&i| i != counted[0]) {
            assert_eq!(tree.node(i).size, 0);
            let h = tree.hard_link(i).unwrap();
            assert_eq!((h.size, h.counted), (100, counted[0]));
            assert_eq!(tree.link_bytes(h), 100);
        }
        assert!(tree.hard_link(counted[0]).is_none());
        let bin = tree.exts.iter().find(|e| &*e.0 == "bin").map(|e| (e.1, e.2));
        assert_eq!(bin, Some((107, 4)));
        // The counting link goes: another one takes over, the total stays.
        assert!(tree.remove(counted[0]));
        assert_eq!((tree.root().size, tree.root().files), (107, 3));
        assert_eq!(tree.hard_links.len(), 1);
        let left: Vec<u32> = links.iter().copied().filter(|&i| i != counted[0]).collect();
        let heir = *left.iter().find(|&&i| tree.node(i).size == 100).unwrap();
        let other = *left.iter().find(|&&i| i != heir).unwrap();
        assert_eq!(tree.hard_link(other).map(|h| h.counted), Some(heir));
        assert_eq!(tree.bytes(tree.node(heir).parent), 100 + if tree.node(heir).parent == ROOT { 7 } else { 0 });
        // An uncounted link goes: nothing changes but the file count.
        assert!(tree.remove(other));
        assert_eq!((tree.root().size, tree.root().files), (107, 2));
        assert!(tree.hard_links.is_empty());
        // The last one frees the bytes.
        assert!(tree.remove(heir));
        assert_eq!(tree.root().size, 7);
        assert_eq!(tree.exts.iter().find(|e| &*e.0 == "bin").map(|e| (e.1, e.2)), Some((7, 1)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_a_folder_with_the_counting_link_passes_it_on() {
        let root = temp("hardlink-dir");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/a"), [0u8; 50]).unwrap();
        if fs::hard_link(root.join("d/a"), root.join("b")).is_err() {
            eprintln!("skipped: can't create hard links here");
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let mut tree = scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        if tree.hard_links.is_empty() {
            eprintln!("skipped: the file system gives no file ids");
            let _ = fs::remove_dir_all(&root);
            return;
        }
        assert_eq!(tree.root().size, 50);
        let (di, _) = child(&tree, ROOT, "d");
        let (bi, _) = child(&tree, ROOT, "b");
        assert!(tree.remove(di));
        assert_eq!((tree.root().size, tree.node(bi).size), (50, 50));
        assert!(tree.hard_links.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Everything reachable from `i`, in child order: name, sizes, counts.
    fn shape(tree: &Tree, i: u32, out: &mut Vec<String>) {
        let n = tree.node(i);
        out.push(format!("{} {} {} {} {} {:?}", n.name, n.size, n.alloc, n.files, n.dirs, tree.ext_name(i)));
        for &c in &n.children {
            shape(tree, c, out);
        }
    }

    /// The extensions with files: name -> (bytes, allocated, files).
    fn ext_table(tree: &Tree) -> Vec<(String, u64, u64, u32)> {
        let mut v: Vec<_> = (0..tree.exts.len())
            .filter(|&i| tree.exts[i].2 > 0)
            .map(|i| (tree.exts[i].0.to_string(), tree.exts[i].1, tree.ext_alloc[i], tree.exts[i].2))
            .collect();
        v.sort();
        v
    }

    fn assert_same(spliced: &Tree, fresh: &Tree) {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        shape(spliced, ROOT, &mut a);
        shape(fresh, ROOT, &mut b);
        assert_eq!(a, b);
        assert_eq!(ext_table(spliced), ext_table(fresh));
        assert_eq!(spliced.hard_links.len(), fresh.hard_links.len());
        assert_eq!(spliced.errors, fresh.errors);
    }

    fn subtree_fixture(name: &str) -> PathBuf {
        let root = temp(name);
        fs::create_dir_all(root.join("a/b/deep")).unwrap();
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("a/b/z.rs"), [0u8; 300]).unwrap();
        fs::write(root.join("a/b/deep/w.rs"), [0u8; 40]).unwrap();
        fs::write(root.join("a/y.txt"), [0u8; 20]).unwrap();
        fs::write(root.join("a/c.log"), [0u8; 5]).unwrap();
        fs::write(root.join("x.txt"), [0u8; 100]).unwrap();
        fs::write(root.join("d/k.md"), [0u8; 3]).unwrap();
        root
    }

    #[test]
    fn a_rescanned_subtree_matches_a_full_scan() {
        for measure in [Measure::Size, Measure::Alloc] {
            let root = subtree_fixture(&format!("splice-{measure:?}"));
            let no = AtomicBool::new(false);
            let mut tree = scan(&root, &no, |_| {}).unwrap();
            tree.set_measure(measure);
            assert_eq!(tree.node(tree.root().children[0]).name.as_ref(), "a");
            // Part of "a" goes (a delete that stopped partway), a new file appears.
            fs::remove_file(root.join("a/b/z.rs")).unwrap();
            fs::remove_dir_all(root.join("a/b/deep")).unwrap();
            fs::write(root.join("a/b/new.dat"), [0u8; 7]).unwrap();
            let ai = tree.locate(&root, &root.join("a"));
            let bi = tree.locate(&root, &root.join("a/b"));
            assert_eq!(tree.locate(&root, &root.join("a/b/nope/x")), bi, "nearest folder above");
            assert!(!tree.links_cross(ai));
            let Some(Rescan::Tree(sub)) = rescan(&root.join("a"), &no, |_| {}) else { panic!("a is there") };
            let new = tree.splice(ai, Rescan::Tree(sub)).unwrap();
            assert!(new > ai);
            assert_eq!((tree.node(new).name.as_ref(), tree.parent(new)), ("a", Some(ROOT)));
            assert_eq!(tree.surviving(ai), ROOT, "the old node is out");
            assert_eq!(tree.locate(&root, &root.join("a")), new);
            assert_eq!(tree.path(&root, tree.locate(&root, &root.join("a/b/new.dat"))), root.join("a/b/new.dat"));
            let mut fresh = scan(&root, &no, |_| {}).unwrap();
            fresh.set_measure(measure);
            assert_same(&tree, &fresh);
            if measure == Measure::Size {
                // "a" (32 bytes now) went below "x.txt".
                assert_eq!(tree.node(tree.root().children[0]).name.as_ref(), "x.txt");
            }
            // A second splice of the same folder works on the new index.
            let Some(again) = rescan(&root.join("a"), &no, |_| {}) else { panic!() };
            assert!(tree.splice(ai, again.clone()).is_none(), "the old index is gone");
            assert!(tree.splice(new, again).is_some());
            assert_same(&tree, &fresh);
            fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn a_rescanned_folder_that_is_gone_is_removed() {
        let root = subtree_fixture("splice-gone");
        let no = AtomicBool::new(false);
        let mut tree = scan(&root, &no, |_| {}).unwrap();
        let ai = tree.locate(&root, &root.join("a"));
        fs::remove_dir_all(root.join("a")).unwrap();
        let r = rescan(&root.join("a"), &no, |_| {}).unwrap();
        assert!(matches!(r, Rescan::Gone));
        assert_eq!(tree.splice(ai, r), Some(ROOT));
        assert_same(&tree, &scan(&root, &no, |_| {}).unwrap());
        // A file in its place is "gone" as a folder too.
        fs::write(root.join("a"), [0u8; 1]).unwrap();
        assert!(matches!(rescan(&root.join("a"), &no, |_| {}), Some(Rescan::Gone)));
        assert!(rescan(&root.join("d"), &AtomicBool::new(true), |_| {}).is_none(), "cancelled");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_root_and_files_are_not_replaced() {
        let root = subtree_fixture("splice-bad");
        let no = AtomicBool::new(false);
        let mut tree = scan(&root, &no, |_| {}).unwrap();
        let Some(Rescan::Tree(sub)) = rescan(&root, &no, |_| {}) else { panic!() };
        assert!(tree.replace_subtree(ROOT, sub.clone()).is_none());
        let xi = tree.locate(&root, &root.join("x.txt"));
        assert!(tree.replace_subtree(xi, sub.clone()).is_none());
        assert!(tree.replace_subtree(9999, sub).is_none());
        assert_same(&tree, &scan(&root, &no, |_| {}).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hard_links_inside_a_rescanned_folder_are_kept_across_ones_refuse() {
        let root = temp("splice-links");
        fs::create_dir_all(root.join("d/s")).unwrap();
        fs::write(root.join("d/a.bin"), [0u8; 50]).unwrap();
        fs::write(root.join("d/gone.bin"), [0u8; 9]).unwrap();
        fs::write(root.join("e.bin"), [0u8; 60]).unwrap();
        fs::create_dir_all(root.join("f")).unwrap();
        if fs::hard_link(root.join("d/a.bin"), root.join("d/s/b.bin")).is_err()
            || fs::hard_link(root.join("e.bin"), root.join("f/e2.bin")).is_err()
        {
            eprintln!("skipped: can't create hard links here");
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let no = AtomicBool::new(false);
        let mut tree = scan(&root, &no, |_| {}).unwrap();
        if tree.hard_links.is_empty() {
            eprintln!("skipped: the file system gives no file ids");
            let _ = fs::remove_dir_all(&root);
            return;
        }
        assert_eq!(tree.hard_links.len(), 2);
        // "f" holds a link of a file counted outside it: no splice.
        let fi = tree.locate(&root, &root.join("f"));
        assert!(tree.links_cross(fi));
        let Some(Rescan::Tree(sub)) = rescan(&root.join("f"), &no, |_| {}) else { panic!() };
        assert!(tree.replace_subtree(fi, sub).is_none());
        // "d" has both links of its file: recomputed inside, totals as a full scan.
        let di = tree.locate(&root, &root.join("d"));
        assert!(!tree.links_cross(di));
        fs::remove_file(root.join("d/gone.bin")).unwrap();
        let r = rescan(&root.join("d"), &no, |_| {}).unwrap();
        let new = tree.splice(di, r).unwrap();
        let fresh = scan(&root, &no, |_| {}).unwrap();
        assert_same(&tree, &fresh);
        assert_eq!((tree.root().size, tree.node(new).size), (110, 50));
        for (&link, h) in &tree.hard_links {
            assert!(tree.surviving(link) == link && tree.surviving(h.counted) == h.counted);
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
