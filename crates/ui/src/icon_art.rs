//! The application icon artwork, drawn in code so no image asset is needed:
//! a blue rounded square holding two file panels, the left one with a
//! cursor bar. Plain Rust with no dependencies: `build.rs` includes this
//! file too, to embed the same icon in the Windows executable.

/// Samples per pixel along each axis (anti-aliasing).
const SS: u32 = 4;

/// Shapes in a 0..1 coordinate space, painted back to front.
struct RoundRect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: f32,
}

impl RoundRect {
    fn contains(&self, x: f32, y: f32) -> bool {
        let cx = x.clamp(self.x0 + self.r, self.x1 - self.r);
        let cy = y.clamp(self.y0 + self.r, self.y1 - self.r);
        let (dx, dy) = (x - cx, y - cy);
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1 && dx * dx + dy * dy <= self.r * self.r
    }
}

const fn rr(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> RoundRect {
    RoundRect { x0, y0, x1, y1, r }
}

/// Colour of one sample point, or `None` for transparent.
fn sample(x: f32, y: f32) -> Option<[f32; 3]> {
    const BG: RoundRect = rr(0.03, 0.03, 0.97, 0.97, 0.2);
    const LEFT: RoundRect = rr(0.16, 0.2, 0.48, 0.82, 0.05);
    const RIGHT: RoundRect = rr(0.52, 0.2, 0.84, 0.82, 0.05);
    const CURSOR: RoundRect = rr(0.19, 0.42, 0.45, 0.5, 0.02);
    const PANEL: [f32; 3] = [0.10, 0.11, 0.13];
    const LINE: [f32; 3] = [0.36, 0.40, 0.47];

    if !BG.contains(x, y) {
        return None;
    }
    if CURSOR.contains(x, y) {
        return Some([0.95, 0.72, 0.36]);
    }
    for panel in [&LEFT, &RIGHT] {
        if panel.contains(x, y) {
            // Faint "file name" lines.
            let row = (y - 0.26) / 0.08;
            let in_line = row >= 0.0 && row.fract() < 0.35 && y < 0.78;
            let line_end = panel.x1 - 0.05 - 0.06 * (row.floor() % 3.0);
            let lined = in_line && x > panel.x0 + 0.04 && x < line_end;
            return Some(if lined { LINE } else { PANEL });
        }
    }
    // Diagonal gradient from the accent blue to a deeper blue.
    let t = (x + y) / 2.0;
    let from = [0.42, 0.67, 1.0];
    let to = [0.16, 0.38, 0.82];
    Some([0, 1, 2].map(|i| from[i] + (to[i] - from[i]) * t))
}

/// The icon at `size` × `size` pixels, RGBA, anti-aliased.
pub fn rgba(size: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let n = (SS * SS) as f32;
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if let Some(c) = sample(x, y) {
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        acc[3] += 1.0;
                    }
                }
            }
            if acc[3] == 0.0 {
                out.extend([0, 0, 0, 0]);
            } else {
                // Average colour over covered samples; alpha = coverage.
                let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                out.extend([
                    to_u8(acc[0] / acc[3]),
                    to_u8(acc[1] / acc[3]),
                    to_u8(acc[2] / acc[3]),
                    to_u8(acc[3] / n),
                ]);
            }
        }
    }
    out
}
