//! The application icon artwork, drawn in code so no image asset is needed:
//! a rounded square in Gruvbox dark colours made of two file panels (the
//! left one with a yellow cursor bar) split by a thin gap. Plain Rust with no dependencies: `build.rs` includes this
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

/// Gruvbox colour from a `0xRRGGBB` value.
const fn hex(v: u32) -> [f32; 3] {
    [((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0]
}

/// Colour of one sample point, or `None` for transparent. `framed` adds a
/// dark margin around the panels instead of letting them fill the square.
fn sample(x: f32, y: f32, framed: bool) -> Option<[f32; 3]> {
    const BG: RoundRect = rr(0.03, 0.03, 0.97, 0.97, 0.2);
    const LEFT: RoundRect = rr(0.12, 0.14, 0.49, 0.86, 0.05);
    const RIGHT: RoundRect = rr(0.51, 0.14, 0.88, 0.86, 0.05);
    const CURSOR: RoundRect = rr(0.15, 0.44, 0.46, 0.53, 0.02);
    const BACK: [f32; 3] = hex(0x1d2021);
    const PANEL: [f32; 3] = hex(0x282828);
    const LINE: [f32; 3] = hex(0x665c54);
    const ACCENT: [f32; 3] = hex(0xfabd2f);

    if !BG.contains(x, y) {
        return None;
    }
    let bottom = if framed { 0.83 } else { 0.9 };
    if CURSOR.contains(x, y) {
        return Some(ACCENT);
    }
    // Framed: panels inset in a dark margin. Unframed: the halves of the square itself,
    // split by a thin gap (so only the outer corners are rounded).
    for (text, half) in [(&LEFT, x < 0.495), (&RIGHT, x > 0.505)] {
        let on_panel = if framed { text.contains(x, y) } else { half };
        if on_panel {
            // Faint "file name" lines.
            let row = (y - 0.21) / 0.085;
            let in_line = row >= 0.0 && row.fract() < 0.35 && y < bottom;
            let line_end = text.x1 - 0.045 - 0.06 * (row.floor() % 3.0);
            let lined = in_line && x > text.x0 + 0.035 && x < line_end;
            return Some(if lined { LINE } else { PANEL });
        }
    }
    Some(BACK)
}

/// The icon at `size` × `size` pixels, RGBA, anti-aliased.
pub fn rgba(size: u32) -> Vec<u8> {
    render(size, false)
}

fn render(size: u32, framed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let n = (SS * SS) as f32;
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if let Some(c) = sample(x, y, framed) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    fn png(size: u32, rgba: &[u8]) -> Vec<u8> {
        let mut raw = Vec::new();
        for row in rgba.chunks((size * 4) as usize) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        // zlib with stored blocks.
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, b) in blocks.iter().enumerate() {
            z.push((i + 1 == blocks.len()) as u8);
            z.extend((b.len() as u16).to_le_bytes());
            z.extend((!(b.len() as u16)).to_le_bytes());
            z.extend_from_slice(b);
        }
        let (mut a, mut b2) = (1u32, 0u32);
        for &v in &raw {
            a = (a + v as u32) % 65521;
            b2 = (b2 + a) % 65521;
        }
        z.extend(((b2 << 16) | a).to_be_bytes());
        let mut out = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10];
        let mut chunk = |kind: &[u8; 4], data: &[u8]| {
            out.extend((data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend(crc32(&body).to_be_bytes());
        };
        let mut ihdr = Vec::new();
        ihdr.extend(size.to_be_bytes());
        ihdr.extend(size.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        chunk(b"IHDR", &ihdr);
        chunk(b"IDAT", &z);
        chunk(b"IEND", &[]);
        out
    }

    /// Writes the icon at several sizes into the folder named by `ICON_OUT_DIR`.
    #[test]
    #[ignore]
    fn write_pngs() {
        let dir = std::path::PathBuf::from(std::env::var("ICON_OUT_DIR").expect("set ICON_OUT_DIR"));
        for size in [16, 32, 48, 256] {
            std::fs::write(dir.join(format!("icon-{size}.png")), png(size, &render(size, false))).unwrap();
            std::fs::write(dir.join(format!("framed-{size}.png")), png(size, &render(size, true))).unwrap();
        }
    }
}
