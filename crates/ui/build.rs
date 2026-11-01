//! Embeds the application icon (the same artwork as the window icon, from
//! `src/icon_art.rs`) in the Windows executable, so Explorer, the taskbar
//! and pinned shortcuts show it. The `.res` file is written directly (PNG
//! images in an icon group), so no resource compiler is needed; the MSVC
//! linker takes it as an input.

#[allow(dead_code)]
#[path = "src/icon_art.rs"]
mod icon_art;

use std::path::PathBuf;

const SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" {
        return;
    }
    if env != "msvc" {
        println!("cargo:warning=exe icon is only embedded with the MSVC toolchain");
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("icon.res");
    std::fs::write(&out, icon_res()).expect("write icon.res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

fn png(size: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut encoder = png::Encoder::new(&mut buf, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&icon_art::rgba(size)).expect("png data");
    drop(writer);
    buf
}

/// One entry of a `.res` file: header with numeric type and name, then the
/// data padded to 4 bytes.
fn resource(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(32u32.to_le_bytes()); // header size
    out.extend([0xFFFFu16, kind, 0xFFFF, id].iter().flat_map(|v| v.to_le_bytes()));
    out.extend(0u32.to_le_bytes()); // data version
    out.extend(flags.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // language neutral
    out.extend(0u32.to_le_bytes()); // version
    out.extend(0u32.to_le_bytes()); // characteristics
    out.extend(data);
    out.resize(out.len().next_multiple_of(4), 0);
}

fn icon_res() -> Vec<u8> {
    let mut out = Vec::new();
    // A .res file starts with an empty entry.
    resource(&mut out, 0, 0, 0, &[]);
    let mut group = Vec::new();
    group.extend([0u16, 1, SIZES.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
    for (i, &size) in SIZES.iter().enumerate() {
        let image = png(size);
        let id = i as u16 + 1;
        resource(&mut out, RT_ICON, id, 0x1010, &image);
        let side = if size >= 256 { 0 } else { size as u8 };
        group.extend([side, side, 0, 0]);
        group.extend(1u16.to_le_bytes()); // planes
        group.extend(32u16.to_le_bytes()); // bits per pixel
        group.extend((image.len() as u32).to_le_bytes());
        group.extend(id.to_le_bytes());
    }
    resource(&mut out, RT_GROUP_ICON, 1, 0x1030, &group);
    out
}
