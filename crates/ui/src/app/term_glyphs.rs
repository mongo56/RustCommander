//! Block elements (U+2580..259F) and box drawing lines (U+2500..257F) of
//! the built-in terminal, drawn as rectangles on whole pixels instead of
//! font glyphs. Glyphs don't fill the cell height and sit on fractional cell
//! edges, which left thin seams between neighbouring cells (progress bars,
//! scroll bars, borders). Dashed lines and diagonals stay with the font.
//! Also the sextants (U+1FB00..1FB3B) and the Powerline separators
//! (U+E0B0..E0B7: triangles and half circles), as rows of 1 px rects.

use iced::Rectangle;

/// A part of a drawn character, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Piece {
    /// A filled rectangle at this opacity (the shades are see-through).
    Rect(Rectangle, f32),
    /// A rounded corner (╭╮╯╰): the border of `rect` (`width` thick, corners
    /// of `radius`), shown only inside `clip` (the cell).
    Arc { rect: Rectangle, radius: f32, width: f32, clip: Rectangle },
}

/// Whether `c` is drawn here (else it's text).
pub(super) fn drawn(c: char) -> bool {
    matches!(c as u32, 0x2580..=0x259f | 0x256d..=0x2570 | 0x1fb00..=0x1fb3b | 0xe0b0..=0xe0b7) || arms(c).is_some()
}

/// The sextants: 2x3 blocks, 60 of the 64 patterns (not empty, full, ▌, ▐).
pub(super) const SEXTANTS: std::ops::RangeInclusive<u32> = 0x1fb00..=0x1fb3b;
/// The Powerline separators: filled and outlined triangles, then half circles.
pub(super) const POWERLINE: std::ops::RangeInclusive<u32> = 0xe0b0..=0xe0b7;

/// The cells of a sextant, bit 0 = upper left, then right, then down by rows
/// (bit 5 = lower right). The code counts the patterns from 1, skipping the
/// left column (cells 1, 3, 5) and the right one (2, 4, 6).
fn sextant(code: u32) -> u8 {
    let mut p = code - 0x1fb00 + 1;
    for skipped in [0b010101, 0b101010] {
        if p >= skipped {
            p += 1;
        }
    }
    p as u8
}

/// How far a separator reaches across the cell (0..=1) at `u`, -1 (top) to
/// 1 (bottom): a triangle or a half circle.
fn reach(round: bool, u: f32) -> f32 {
    let u = u.clamp(-1.0, 1.0);
    if round { (1.0 - u * u).sqrt() } else { 1.0 - u.abs() }
}

/// The arms of a box drawing line from the cell centre: [left, right, up,
/// down], each 0 (none), 1 (light), 2 (heavy) or 3 (double).
const ARMS: [&str; 128] = [
    "1100", "2200", "0011", "0022", "", "", "", "", "", "", "", "", // ─━│┃ dashed
    "0101", "0201", "0102", "0202", "1001", "2001", "1002", "2002", // ┌┍┎┏┐┑┒┓
    "0110", "0210", "0120", "0220", "1010", "2010", "1020", "2020", // └┕┖┗┘┙┚┛
    "0111", "0211", "0121", "0112", "0122", "0221", "0212", "0222", // ├┝┞┟┠┡┢┣
    "1011", "2011", "1021", "1012", "1022", "2021", "2012", "2022", // ┤┥┦┧┨┩┪┫
    "1101", "2101", "1201", "2201", "1102", "2102", "1202", "2202", // ┬┭┮┯┰┱┲┳
    "1110", "2110", "1210", "2210", "1120", "2120", "1220", "2220", // ┴┵┶┷┸┹┺┻
    "1111", "2111", "1211", "2211", "1121", "1112", "1122", "2121", // ┼┽┾┿╀╁╂╃
    "1221", "2112", "1212", "2221", "2212", "2122", "1222", "2222", // ╄╅╆╇╈╉╊╋
    "", "", "", "", // dashed
    "3300", "0033", "0301", "0103", "0303", "3001", "1003", "3003", // ═║╒╓╔╕╖╗
    "0310", "0130", "0330", "3010", "1030", "3030", "0311", "0133", // ╘╙╚╛╜╝╞╟
    "0333", "3011", "1033", "3033", "3301", "1103", "3303", "3310", // ╠╡╢╣╤╥╦╧
    "1130", "3330", "3311", "1133", "3333", // ╨╩╪╫╬
    "", "", "", "", "", "", "", // rounded corners (`Piece::Arc`), diagonals
    "1000", "0010", "0100", "0001", "2000", "0020", "0200", "0002", // ╴╵╶╷╸╹╺╻
    "1200", "0012", "2100", "0021", // ╼╽╾╿
];

fn arms(c: char) -> Option<[u8; 4]> {
    let i = (c as u32).checked_sub(0x2500).filter(|&i| i < 128)?;
    let s = ARMS[i as usize].as_bytes();
    (s.len() == 4).then(|| [s[0] - b'0', s[1] - b'0', s[2] - b'0', s[3] - b'0'])
}

/// The line(s) of one weight across an axis: one span, or two for double
/// lines (`first` on the low side), centred in `lo..lo + size`.
#[derive(Debug, Clone, Copy)]
struct Band {
    first: (f32, f32),
    last: (f32, f32),
}

impl Band {
    fn new(lo: f32, size: f32, weight: u8, t: f32) -> Option<Band> {
        let thick = match weight {
            0 => return None,
            2 => 2.0 * t,
            3 => 3.0 * t,
            _ => t,
        };
        let s = lo + ((size - thick) / 2.0).floor();
        Some(if weight == 3 {
            Band { first: (s, s + t), last: (s + 2.0 * t, s + 3.0 * t) }
        } else {
            Band { first: (s, s + thick), last: (s, s + thick) }
        })
    }

    fn double(&self) -> bool {
        self.first != self.last
    }

    /// Where an arm heading to the high (`pos`) or low side starts at
    /// these lines: their far edge (across all of them).
    fn far(&self, pos: bool) -> f32 {
        if pos { self.first.0 } else { self.last.1 }
    }

    /// Like `far`, but at the line on the arm's own side.
    fn near(&self, pos: bool) -> f32 {
        if pos { self.last.0 } else { self.first.1 }
    }
}

/// One arm along an axis (`lo..hi`), heading to the high side if `pos`:
/// its spans (along, across). `own` is its band across; `side` the bands
/// (along this axis) of the perpendicular arms on the low and high sides
/// across; `opposite` whether the arm on the other side of the centre is
/// there; `light` a light band along this axis (the centre).
fn arm(
    (lo, hi): (f32, f32),
    pos: bool,
    own: Band,
    side: [Option<Band>; 2],
    opposite: bool,
    light: Band,
    emit: &mut impl FnMut((f32, f32), (f32, f32)),
) {
    let mut span = |start: f32, across: (f32, f32)| {
        let along = if pos { (start, hi) } else { (lo, start) };
        emit(along, across);
    };
    // The far edge of all the perpendicular lines.
    let far = |bands: &mut dyn Iterator<Item = Band>| {
        bands.map(|b| b.far(pos)).reduce(|a, b| if pos { a.min(b) } else { a.max(b) })
    };
    if !own.double() {
        let start = match side {
            // Through a double line with no arm opposite (╟): stop at its
            // near line.
            [Some(a), Some(b)] if !opposite && a.double() && b.double() => a.near(pos),
            _ => far(&mut side.into_iter().flatten()).unwrap_or(light.far(pos)),
        };
        span(start, own.first);
        return;
    }
    for (i, across) in [own.first, own.last].into_iter().enumerate() {
        // The line on one side meets the perpendicular arm on that side at
        // its near line (╦), else the arm on the other side at its far line
        // (╔).
        let start = match (side[i], side[1 - i]) {
            (Some(b), _) => b.near(pos),
            (None, Some(b)) => b.far(pos),
            (None, None) => light.far(pos),
        };
        span(start, across);
    }
}

/// The pieces of `c` in `cell` (whole-pixel edges), lines `t` thick (heavy
/// ones twice that). Nothing for characters that aren't `drawn`.
pub(super) fn pieces(c: char, cell: Rectangle, t: f32, out: &mut impl FnMut(Piece)) {
    let (x0, y0, w, h) = (cell.x, cell.y, cell.width, cell.height);
    let (x1, y1) = (x0 + w, y0 + h);
    let mut rect = |l: f32, t: f32, r: f32, b: f32, a: f32| {
        out(Piece::Rect(Rectangle { x: l, y: t, width: r - l, height: b - t }, a));
    };
    let eighths = |size: f32, n: u32| (size * n as f32 / 8.0).round();
    let (xm, ym) = (x0 + (w / 2.0).round(), y0 + (h / 2.0).round());
    let code = c as u32;
    match code {
        0x2580 => rect(x0, y0, x1, ym, 1.0),
        0x2581..=0x2587 => rect(x0, y1 - eighths(h, code - 0x2580), x1, y1, 1.0),
        0x2588 => rect(x0, y0, x1, y1, 1.0),
        0x2589..=0x258f => rect(x0, y0, x0 + eighths(w, 0x2590 - code), y1, 1.0),
        0x2590 => rect(xm, y0, x1, y1, 1.0),
        0x2591..=0x2593 => rect(x0, y0, x1, y1, (code - 0x2590) as f32 / 4.0),
        0x2594 => rect(x0, y0, x1, y0 + eighths(h, 1), 1.0),
        0x2595 => rect(x1 - eighths(w, 1), y0, x1, y1, 1.0),
        0x2596..=0x259f => {
            // Quadrants: upper left, upper right, lower left, lower right.
            const Q: [u8; 10] = [0b0100, 0b1000, 0b0001, 0b1101, 0b1001, 0b0111, 0b1011, 0b0010, 0b0110, 0b1110];
            let q = Q[(code - 0x2596) as usize];
            let parts = [(x0, y0, xm, ym), (xm, y0, x1, ym), (x0, ym, xm, y1), (xm, ym, x1, y1)];
            for (bit, (l, t, r, b)) in parts.into_iter().enumerate() {
                if q & (1 << bit) != 0 {
                    rect(l, t, r, b, 1.0);
                }
            }
        }
        c if SEXTANTS.contains(&c) => {
            let bits = sextant(c);
            let ys = [y0, y0 + (h / 3.0).round(), y0 + (h * 2.0 / 3.0).round(), y1];
            for bit in 0..6 {
                if bits & (1 << bit) != 0 {
                    let (l, r) = if bit % 2 == 0 { (x0, xm) } else { (xm, x1) };
                    rect(l, ys[bit / 2], r, ys[bit / 2 + 1], 1.0);
                }
            }
        }
        c if POWERLINE.contains(&c) => {
            // A staircase of 1 px rows (no layer, so the selection stays on top).
            let (outline, left, round) = (c % 2 == 1, matches!(c, 0xe0b2 | 0xe0b3 | 0xe0b6 | 0xe0b7), c >= 0xe0b4);
            for row in 0..h as i32 {
                let (top, bottom) = (y0 + row as f32, y0 + row as f32 + 1.0);
                let at = |y: f32| (reach(round, (y - y0) / h * 2.0 - 1.0) * w).round();
                let mid = at((top + bottom) / 2.0);
                // The edge reaches `hi`; an outline is `t` thick and spans the row's slope.
                let (lo, hi) = if outline {
                    let hi = at(top).max(at(bottom)).max(mid);
                    (at(top).min(at(bottom)).min(hi - t).max(0.0), hi)
                } else {
                    (0.0, mid)
                };
                if hi > lo {
                    if left {
                        rect(x1 - hi, top, x1 - lo, bottom, 1.0);
                    } else {
                        rect(x0 + lo, top, x0 + hi, bottom, 1.0);
                    }
                }
            }
        }
        0x256d..=0x2570 => {
            // The corner of a rounded rectangle whose straight edges run on
            // the light lines and out of the cell (clipped).
            let (Some(xs), Some(ys)) = (Band::new(x0, w, 1, t), Band::new(y0, h, 1, t)) else { return };
            let (l, r) = if matches!(code, 0x256d | 0x2570) { (xs.first.0, x1 + w) } else { (x0 - w, xs.first.1) };
            let (top, b) = if code <= 0x256e { (ys.first.0, y1 + h) } else { (y0 - h, ys.first.1) };
            let rect = Rectangle { x: l, y: top, width: r - l, height: b - top };
            out(Piece::Arc { rect, radius: (w.min(h) / 2.0).round(), width: t, clip: cell });
        }
        _ => {
            let Some([left, right, up, down]) = arms(c) else { return };
            let (Some(light_x), Some(light_y)) = (Band::new(x0, w, 1, t), Band::new(y0, h, 1, t)) else { return };
            let vx = |weight| Band::new(x0, w, weight, t);
            let hy = |weight| Band::new(y0, h, weight, t);
            let mut horizontal = |(l, r): (f32, f32), (t, b): (f32, f32)| rect(l, t, r, b, 1.0);
            for (weight, pos, opposite) in [(left, false, right), (right, true, left)] {
                if let Some(own) = hy(weight) {
                    arm((x0, x1), pos, own, [vx(up), vx(down)], opposite > 0, light_x, &mut horizontal);
                }
            }
            let mut vertical = |(t, b): (f32, f32), (l, r): (f32, f32)| rect(l, t, r, b, 1.0);
            for (weight, pos, opposite) in [(up, false, down), (down, true, up)] {
                if let Some(own) = vx(weight) {
                    arm((y0, y1), pos, own, [hy(left), hy(right)], opposite > 0, light_y, &mut vertical);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: Rectangle = Rectangle { x: 10.0, y: 20.0, width: 9.0, height: 18.0 };

    fn rects(c: char) -> Vec<(f32, f32, f32, f32)> {
        let mut v = Vec::new();
        pieces(c, CELL, 1.0, &mut |p| match p {
            Piece::Rect(r, _) => v.push((r.x, r.y, r.x + r.width, r.y + r.height)),
            Piece::Arc { .. } => panic!("arc"),
        });
        v
    }

    /// The pixels covered, as rows of '#' and '.' over the cell.
    fn pixels(c: char) -> Vec<String> {
        let rs = rects(c);
        (0..CELL.height as i32)
            .map(|y| {
                (0..CELL.width as i32)
                    .map(|x| {
                        let (px, py) = (CELL.x + x as f32 + 0.5, CELL.y + y as f32 + 0.5);
                        let hit = rs.iter().any(|&(l, t, r, b)| px > l && px < r && py > t && py < b);
                        if hit { '#' } else { '.' }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn table_covers_the_box_drawing_block() {
        assert_eq!(arms('─'), Some([1, 1, 0, 0]));
        assert_eq!(arms('┼'), Some([1, 1, 1, 1]));
        assert_eq!(arms('╋'), Some([2, 2, 2, 2]));
        assert_eq!(arms('╬'), Some([3, 3, 3, 3]));
        assert_eq!(arms('╿'), Some([0, 0, 2, 1]));
        assert_eq!(arms('╳'), None);
        assert_eq!(arms('┄'), None);
        assert!(drawn('╭') && drawn('█') && drawn('▟') && !drawn('╌') && !drawn('a'));
        for s in ARMS {
            assert!(s.is_empty() || (s.len() == 4 && s.bytes().all(|b| (b'0'..=b'3').contains(&b))), "{s}");
        }
    }

    #[test]
    fn blocks_fill_whole_pixels_to_the_cell_edges() {
        assert_eq!(rects('█'), vec![(10.0, 20.0, 19.0, 38.0)]);
        assert_eq!(rects('▀'), vec![(10.0, 20.0, 19.0, 29.0)]);
        assert_eq!(rects('▄'), vec![(10.0, 29.0, 19.0, 38.0)]);
        assert_eq!(rects('▌'), vec![(10.0, 20.0, 15.0, 38.0)]);
        assert_eq!(rects('▐'), vec![(15.0, 20.0, 19.0, 38.0)]);
        assert_eq!(rects('▁'), vec![(10.0, 36.0, 19.0, 38.0)]);
        assert_eq!(rects('▏'), vec![(10.0, 20.0, 11.0, 38.0)]);
        assert_eq!(rects('▚'), vec![(10.0, 20.0, 15.0, 29.0), (15.0, 29.0, 19.0, 38.0)]);
        let mut shade = Vec::new();
        pieces('▒', CELL, 1.0, &mut |p| shade.push(p));
        assert_eq!(shade, vec![Piece::Rect(CELL, 0.5)]);
    }

    #[test]
    fn every_arm_reaches_its_edge_and_no_other() {
        for code in 0x2500..0x2580 {
            let c = char::from_u32(code).unwrap();
            let Some([l, r, u, d]) = arms(c) else { continue };
            let rs = rects(c);
            let (x1, y1) = (CELL.x + CELL.width, CELL.y + CELL.height);
            for &(left, top, right, bottom) in &rs {
                assert!(left >= CELL.x && top >= CELL.y && right <= x1 && bottom <= y1, "{c} outside");
                assert!(left < right && top < bottom, "{c} empty");
            }
            assert_eq!(rs.iter().any(|r| r.0 == CELL.x), l > 0, "{c} left");
            assert_eq!(rs.iter().any(|r| r.2 == x1), r > 0, "{c} right");
            assert_eq!(rs.iter().any(|r| r.1 == CELL.y), u > 0, "{c} up");
            assert_eq!(rs.iter().any(|r| r.3 == y1), d > 0, "{c} down");
        }
    }

    #[test]
    fn lines_meet_at_the_centre() {
        let row = |c, y: usize| pixels(c)[y].clone();
        // Light lines are 1 px, in the same place in every cell.
        assert_eq!(row('─', 8), "#########");
        assert_eq!(row('─', 9), ".........");
        assert_eq!(row('│', 0), "....#....");
        assert_eq!(row('┼', 8), "#########");
        assert_eq!(row('┼', 17), "....#....");
        assert_eq!(row('┃', 0), "...##....");
        // Corners: the arms overlap at the centre.
        assert_eq!(&pixels('┌')[7..10], [".........", "....#####", "....#...."]);
        assert_eq!(&pixels('┘')[7..10], ["....#....", "#####....", "........."]);
        assert_eq!(&pixels('╴')[8..9], ["#####...."]);
    }

    #[test]
    fn double_lines_join_like_the_glyphs() {
        assert_eq!(
            &pixels('╔')[6..11],
            [".........", "...######", "...#.....", "...#.####", "...#.#..."]
        );
        assert_eq!(
            &pixels('╬')[6..11],
            ["...#.#...", "####.####", ".........", "####.####", "...#.#..."]
        );
        assert_eq!(
            &pixels('╦')[6..11],
            [".........", "#########", ".........", "####.####", "...#.#..."]
        );
        // A light arm stops at the near line of a double one going through.
        assert_eq!(&pixels('╟')[7..10], ["...#.#...", "...#.####", "...#.#..."]);
        // A light line under a double one starts at its lower line (╤), at
        // the upper one at a corner (╕).
        assert_eq!(&pixels('╤')[7..11], ["#########", ".........", "#########", "....#...."]);
        assert_eq!(&pixels('╕')[6..11], [".........", "#####....", "....#....", "#####....", "....#...."]);
    }

    #[test]
    fn sextants_cover_exactly_their_cells() {
        // SEXTANT-1, -2, -12 and -23456 are the first three and the last.
        assert_eq!(sextant(0x1fb00), 0b000001);
        assert_eq!(sextant(0x1fb01), 0b000010);
        assert_eq!(sextant(0x1fb02), 0b000011);
        assert_eq!(sextant(0x1fb3b), 0b111110);
        // ▌ and ▐ (SEXTANT-135, -246) are left out: 1FB13 is SEXTANT-35, 1FB14 SEXTANT-235.
        assert_eq!(sextant(0x1fb13), 0b010100);
        assert_eq!(sextant(0x1fb14), 0b010110);
        let mut seen = std::collections::HashSet::new();
        for code in SEXTANTS {
            let bits = sextant(code);
            assert!(!matches!(bits, 0 | 63 | 0b010101 | 0b101010), "{code:x}");
            assert!(seen.insert(bits), "{code:x} twice");
            // Each of the six cells is filled or empty as its bit says.
            let px = pixels(char::from_u32(code).unwrap());
            for bit in 0..6 {
                let (x, y) = (if bit % 2 == 0 { 1 } else { 7 }, [1, 7, 13][bit / 2]);
                assert_eq!(px[y].as_bytes()[x] == b'#', bits & (1 << bit) != 0, "{code:x} cell {bit}");
            }
            // With the complement (or ▌ ▐) it fills the cell exactly.
            let rest = !bits & 63;
            let other = match rest {
                0b010101 => pixels('▌'),
                0b101010 => pixels('▐'),
                _ => pixels(char::from_u32(SEXTANTS.clone().find(|&c| sextant(c) == rest).unwrap()).unwrap()),
            };
            for (a, b) in px.iter().zip(&other) {
                for (p, q) in a.bytes().zip(b.bytes()) {
                    assert!((p == b'#') != (q == b'#'), "{code:x}");
                }
            }
        }
        assert_eq!(seen.len(), 60);
        assert!(drawn('\u{1fb00}') && drawn('\u{1fb3b}') && !drawn('\u{1fb3c}'));
    }

    #[test]
    fn powerline_triangles_and_half_circles() {
        let (tri, circle) = (pixels('\u{e0b0}'), pixels('\u{e0b4}'));
        // Full width at the middle, narrow at the top and bottom, hugging the left edge.
        for p in [&tri, &circle] {
            assert!(p[0].starts_with('#') && p[17].starts_with('#'));
            assert_eq!(p[8].matches('#').count(), 9);
            assert!(p[0].matches('#').count() < 4);
            assert!(p.iter().all(|r| r.starts_with('#')));
        }
        assert!(circle[2].matches('#').count() > tri[2].matches('#').count());
        // The left ones are the mirror image.
        let mirror = |v: &Vec<String>| v.iter().map(|r| r.chars().rev().collect::<String>()).collect::<Vec<_>>();
        assert_eq!(pixels('\u{e0b2}'), mirror(&tri));
        assert_eq!(pixels('\u{e0b6}'), mirror(&circle));
        // Outlines are thin and reach the point at the middle.
        for c in ['\u{e0b1}', '\u{e0b3}', '\u{e0b5}', '\u{e0b7}'] {
            let p = pixels(c);
            assert!(p.iter().all(|r| r.contains('#')), "{c}");
            // Half circles are flat at the ends, so only the triangles are thin there.
            assert!(c > '\u{e0b4}' || (p[0].matches('#').count() <= 3 && p[4].matches('#').count() <= 3), "{c}");
        }
        assert!(pixels('\u{e0b1}')[8].ends_with('#') && pixels('\u{e0b3}')[8].starts_with('#'));
    }

    #[test]
    fn rounded_corners_are_clipped_borders() {
        let mut v = Vec::new();
        pieces('╭', CELL, 1.0, &mut |p| v.push(p));
        let [Piece::Arc { rect, width, clip, radius }] = v[..] else { panic!("{v:?}") };
        assert_eq!((width, clip, radius), (1.0, CELL, 5.0));
        // Its top and left edges are the light lines; it goes on past the cell.
        assert_eq!((rect.x, rect.y), (14.0, 28.0));
        assert!(rect.x + rect.width > CELL.x + CELL.width && rect.y + rect.height > CELL.y + CELL.height);
        let mut v = Vec::new();
        pieces('╯', CELL, 1.0, &mut |p| v.push(p));
        let [Piece::Arc { rect, .. }] = v[..] else { panic!("{v:?}") };
        assert_eq!((rect.x + rect.width, rect.y + rect.height), (15.0, 29.0));
        assert!(rect.x < CELL.x && rect.y < CELL.y);
    }
}
