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
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub files: u64,
    pub bytes: u64,
    /// The folder being read.
    pub current: PathBuf,
}

impl Tree {
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
}

/// Lower-case extension of a file name, without the dot ("" for none; a
/// leading dot alone, as in ".gitignore", isn't an extension).
fn extension(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?;
    Some(ext.to_lowercase())
}

/// Scans `root` recursively. Links and junctions count as themselves and are
/// not followed; unreadable folders are skipped and counted in
/// [`Tree::errors`]. Sizes are logical lengths: hard links count once per
/// link, and compressed or sparse files show their full size, not the space
/// they take on disk. `progress` is called at most every ~100 ms. Returns
/// `None` once `cancel` is set.
pub fn scan(root: &Path, cancel: &AtomicBool, mut progress: impl FnMut(Progress)) -> Option<Tree> {
    let mut tree = Tree {
        nodes: vec![Node {
            name: root.to_string_lossy().into(),
            size: 0,
            files: 0,
            dirs: 0,
            kind: NodeKind::Dir,
            ext_id: EXT_DIR,
            parent: ROOT,
            children: Vec::new(),
        }],
        exts: vec![("".into(), 0, 0), ("*".into(), 0, 0)],
        errors: 0,
    };
    let mut ext_ids: HashMap<String, u16> = HashMap::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut last_report = Instant::now();
    let mut stack = vec![(ROOT, root.to_path_buf())];
    while let Some((dir_index, dir)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if last_report.elapsed() >= PROGRESS_EVERY {
            progress(Progress { files, bytes, current: dir.clone() });
            last_report = Instant::now();
        }
        let Ok(items) = fs::read_dir(&dir) else {
            tree.errors += 1;
            continue;
        };
        let mut failed = false;
        let mut children = Vec::new();
        for item in items {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let Ok(item) = item else {
                failed = true;
                continue;
            };
            // `DirEntry::file_type` doesn't follow links: a link or junction to a
            // folder isn't `is_dir` here.
            let Ok(file_type) = item.file_type() else {
                failed = true;
                continue;
            };
            let Ok(index) = u32::try_from(tree.nodes.len()) else { break };
            let name: Box<str> = item.file_name().to_string_lossy().into();
            if file_type.is_dir() {
                stack.push((index, item.path()));
                tree.nodes.push(Node {
                    name,
                    size: 0,
                    files: 0,
                    dirs: 0,
                    kind: NodeKind::Dir,
                    ext_id: EXT_DIR,
                    parent: dir_index,
                    children: Vec::new(),
                });
            } else {
                // On Windows `DirEntry::metadata` is the link's own, without following it.
                let size = match item.metadata() {
                    Ok(meta) => meta.len(),
                    Err(_) => {
                        failed = true;
                        0
                    }
                };
                let ext_id = match extension(&name) {
                    None => EXT_NONE,
                    Some(ext) => match ext_ids.get(&ext) {
                        Some(&id) => id,
                        None if tree.exts.len() < EXT_DIR as usize => {
                            let id = tree.exts.len() as u16;
                            tree.exts.push((ext.as_str().into(), 0, 0));
                            ext_ids.insert(ext, id);
                            id
                        }
                        None => EXT_OTHER,
                    },
                };
                let entry = &mut tree.exts[ext_id as usize];
                entry.1 += size;
                entry.2 += 1;
                files += 1;
                bytes += size;
                tree.nodes.push(Node {
                    name,
                    size,
                    files: 0,
                    dirs: 0,
                    kind: NodeKind::File,
                    ext_id,
                    parent: dir_index,
                    children: Vec::new(),
                });
            }
            children.push(index);
        }
        if failed {
            tree.errors += 1;
        }
        children.shrink_to_fit();
        tree.nodes[dir_index as usize].children = children;
    }
    // Children always come after their parent, so one pass from the end sums
    // every folder before its own parent sees it.
    for i in (1..tree.nodes.len()).rev() {
        let node = &tree.nodes[i];
        let (size, parent) = (node.size, node.parent as usize);
        let (files, dirs) = match node.kind {
            NodeKind::File => (1, 0),
            NodeKind::Dir => (node.files, node.dirs + 1),
        };
        let p = &mut tree.nodes[parent];
        p.size += size;
        p.files = p.files.saturating_add(files);
        p.dirs = p.dirs.saturating_add(dirs);
    }
    for i in 0..tree.nodes.len() {
        if tree.nodes[i].children.len() < 2 {
            continue;
        }
        let mut children = std::mem::take(&mut tree.nodes[i].children);
        let nodes = &tree.nodes;
        children.sort_by(|&a, &b| {
            let (a, b) = (&nodes[a as usize], &nodes[b as usize]);
            b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name))
        });
        tree.nodes[i].children = children;
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
}
