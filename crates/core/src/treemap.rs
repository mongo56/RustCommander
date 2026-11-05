//! Squarified treemap layout (Bruls, Huizing, van Wijk) over a `disk_usage::Tree`.
//! Pure geometry: no drawing, no iced.

use crate::disk_usage::{NodeKind, Tree};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
}

/// One rectangle to draw: a file, or a folder too small to show its contents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    pub node: u32,
    pub rect: Rect,
    /// Nesting level below the laid-out node (its direct children are 1; the node itself 0).
    pub depth: u8,
}

/// Lays out `node` inside `rect`, appending file tiles (and tiles of folders smaller than `min_px` in width or
/// height) to `out`. Subfolders are inset by 1 px per side when big enough. Zero-size entries get no tile.
pub fn layout(tree: &Tree, node: u32, rect: Rect, min_px: f32, out: &mut Vec<Tile>) {
    place(tree, node, rect, 0, min_px, out);
}

fn place(tree: &Tree, node: u32, rect: Rect, depth: u8, min_px: f32, out: &mut Vec<Tile>) {
    let n = tree.node(node);
    if n.size == 0 || !(rect.w > 0.0 && rect.h > 0.0) {
        return;
    }
    if n.kind == NodeKind::File || rect.w < min_px || rect.h < min_px {
        out.push(Tile { node, rect, depth });
        return;
    }
    let mut r = rect;
    if depth > 0 && r.w > 2.0 && r.h > 2.0 {
        r = Rect::new(r.x + 1.0, r.y + 1.0, r.w - 2.0, r.h - 2.0);
    }
    let items: Vec<(u32, f64)> =
        n.children.iter().map(|&c| (c, tree.node(c).size as f64)).filter(|&(_, s)| s > 0.0).collect();
    if items.is_empty() {
        out.push(Tile { node, rect, depth });
        return;
    }
    let total: f64 = items.iter().map(|i| i.1).sum();
    let scale = r.w as f64 * r.h as f64 / total;
    let child_depth = depth.saturating_add(1);
    let (mut x, mut y, mut w, mut h) = (r.x as f64, r.y as f64, r.w as f64, r.h as f64);
    let mut start = 0;
    while start < items.len() {
        let side = w.min(h);
        // Grow the row while the worst aspect ratio does not get worse.
        let mut sum = 0.0;
        let mut end = start;
        let mut worst = f64::INFINITY;
        let amax = items[start].1 * scale;
        while end < items.len() {
            let a = items[end].1 * scale;
            let s2 = sum + a;
            // Items are sorted descending: the first is the largest, the new one the smallest.
            let wr = (side * side * amax / (s2 * s2)).max(s2 * s2 / (side * side * a));
            if end > start && wr > worst {
                break;
            }
            worst = wr;
            sum = s2;
            end += 1;
        }
        // Fix the row: it spans the whole short side, with thickness sum / side. The last row takes all that is left.
        let last = end == items.len();
        let vertical_row = w >= h; // short side is vertical: the row is a column
        let thick = if last {
            if vertical_row { w } else { h }
        } else if side > 0.0 {
            sum / side
        } else {
            0.0
        };
        let mut pos = if vertical_row { y } else { x };
        let stop = if vertical_row { y + h } else { x + w };
        for (k, &(c, size)) in items[start..end].iter().enumerate() {
            let len = if start + k + 1 == end { stop - pos } else { size * scale / sum * side };
            let cr = if vertical_row {
                Rect::new(x as f32, pos as f32, thick as f32, len as f32)
            } else {
                Rect::new(pos as f32, y as f32, len as f32, thick as f32)
            };
            place(tree, c, cr, child_depth, min_px, out);
            pos += len;
        }
        if vertical_row {
            x += thick;
            w -= thick;
        } else {
            y += thick;
            h -= thick;
        }
        start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_usage::{Node, ROOT};

    fn node(name: &str, size: u64, kind: NodeKind, parent: u32, children: Vec<u32>) -> Node {
        Node { name: name.into(), size, files: 0, dirs: 0, kind, ext_id: 0, parent, children }
    }

    /// Root with the given file sizes (sorted descending here).
    fn flat(sizes: &[u64]) -> Tree {
        let mut sizes = sizes.to_vec();
        sizes.sort_by(|a, b| b.cmp(a));
        let mut nodes = vec![node("r", sizes.iter().sum(), NodeKind::Dir, 0, (1..=sizes.len() as u32).collect())];
        for (i, s) in sizes.iter().enumerate() {
            nodes.push(node(&format!("f{i}"), *s, NodeKind::File, ROOT, vec![]));
        }
        Tree { nodes, ..Default::default() }
    }

    /// root -> [dir d (a, b), c]; d is node 1.
    fn nested() -> Tree {
        let nodes = vec![
            node("r", 100, NodeKind::Dir, 0, vec![1, 4]),
            node("d", 60, NodeKind::Dir, 0, vec![2, 3]),
            node("a", 40, NodeKind::File, 1, vec![]),
            node("b", 20, NodeKind::File, 1, vec![]),
            node("c", 40, NodeKind::File, 0, vec![]),
        ];
        Tree { nodes, ..Default::default() }
    }

    fn area(r: &Rect) -> f32 {
        r.w * r.h
    }

    fn overlap(a: &Rect, b: &Rect) -> bool {
        let e = 0.01;
        a.x + a.w - e > b.x && b.x + b.w - e > a.x && a.y + a.h - e > b.y && b.y + b.h - e > a.y
    }

    fn run(t: &Tree, node: u32, r: Rect, min: f32) -> Vec<Tile> {
        let mut out = Vec::new();
        layout(t, node, r, min, &mut out);
        out
    }

    #[test]
    fn areas_proportional() {
        let t = flat(&[500, 300, 100, 60, 40]);
        let r = Rect::new(10.0, 20.0, 400.0, 250.0);
        let tiles = run(&t, ROOT, r, 0.0);
        assert_eq!(tiles.len(), 5);
        for tile in &tiles {
            let want = t.node(tile.node).size as f32 / 1000.0 * area(&r);
            assert!((area(&tile.rect) - want).abs() < want * 0.001 + 0.5, "{want} vs {:?}", tile.rect);
            assert_eq!(tile.depth, 1);
        }
        let total: f32 = tiles.iter().map(|t| area(&t.rect)).sum();
        assert!((total - area(&r)).abs() < 1.0);
    }

    #[test]
    fn inside_and_disjoint() {
        let t = flat(&[97, 80, 55, 41, 30, 22, 9, 9, 5, 3, 1, 1]);
        let r = Rect::new(0.0, 0.0, 333.0, 211.0);
        let tiles = run(&t, ROOT, r, 0.0);
        assert_eq!(tiles.len(), 12);
        for (i, a) in tiles.iter().enumerate() {
            let q = a.rect;
            assert!(q.x >= r.x - 0.01 && q.y >= r.y - 0.01);
            assert!(q.x + q.w <= r.x + r.w + 0.01 && q.y + q.h <= r.y + r.h + 0.01);
            for b in &tiles[i + 1..] {
                assert!(!overlap(&q, &b.rect), "{q:?} {:?}", b.rect);
            }
        }
    }

    #[test]
    fn equal_sizes_are_squarish() {
        let t = flat(&[10; 16]);
        let tiles = run(&t, ROOT, Rect::new(0.0, 0.0, 400.0, 400.0), 0.0);
        assert_eq!(tiles.len(), 16);
        for tile in tiles {
            let (w, h) = (tile.rect.w, tile.rect.h);
            assert!(w.max(h) / w.min(h) <= 2.0, "{:?}", tile.rect);
        }
    }

    #[test]
    fn nested_folder_is_inset() {
        let t = nested();
        let tiles = run(&t, ROOT, Rect::new(0.0, 0.0, 100.0, 100.0), 0.0);
        assert_eq!(tiles.len(), 3);
        let a = tiles.iter().find(|t| t.node == 2).unwrap();
        let b = tiles.iter().find(|t| t.node == 3).unwrap();
        assert_eq!((a.depth, b.depth), (2, 2));
        // The folder gets 60x100; its files fill that shrunk by 1 px per side.
        let inner = area(&a.rect) + area(&b.rect);
        assert!((inner - 58.0 * 98.0).abs() < 1.0, "{inner}");
    }

    #[test]
    fn min_px_emits_folder_tile() {
        let t = nested();
        let tiles = run(&t, ROOT, Rect::new(0.0, 0.0, 100.0, 100.0), 70.0);
        // The folder is 60 wide (< 70): one tile for it; the file c is 40 wide, also one tile.
        assert!(tiles.iter().any(|t| t.node == 1));
        assert!(tiles.iter().all(|t| t.node != 2 && t.node != 3));
        assert_eq!(tiles.len(), 2);
    }

    #[test]
    fn edge_cases() {
        let empty = Tree { nodes: vec![node("r", 0, NodeKind::Dir, 0, vec![])], ..Default::default() };
        assert!(run(&empty, ROOT, Rect::new(0.0, 0.0, 50.0, 50.0), 1.0).is_empty());
        let zeros = flat(&[0, 0]);
        assert!(run(&zeros, ROOT, Rect::new(0.0, 0.0, 50.0, 50.0), 1.0).is_empty());
        let one = flat(&[7]);
        let tiles = run(&one, ROOT, Rect::new(0.0, 0.0, 50.0, 30.0), 1.0);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].rect, Rect::new(0.0, 0.0, 50.0, 30.0));
        assert!(run(&one, ROOT, Rect::new(0.0, 0.0, 0.0, 30.0), 0.0).is_empty());
        assert!(run(&one, ROOT, Rect::new(0.0, 0.0, 50.0, 0.0), 0.0).is_empty());
        // A zero-size child gets no tile; a file as the laid-out node fills the rect at depth 0.
        let mixed = flat(&[5, 0]);
        assert_eq!(run(&mixed, ROOT, Rect::new(0.0, 0.0, 10.0, 10.0), 0.0).len(), 1);
        let f = run(&one, 1, Rect::new(1.0, 2.0, 3.0, 4.0), 0.0);
        assert_eq!((f.len(), f[0].depth), (1, 0));
    }
}
