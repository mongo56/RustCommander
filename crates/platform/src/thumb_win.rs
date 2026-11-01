//! Windows thumbnails: the shell's own (`IShellItemImageFactory`, the same
//! previews and cache Explorer uses, for pictures, videos, PDFs, ...) and
//! large icons from the system image list.

use crate::Icon;
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, IBindCtx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHGetFileInfoW, SHGetImageList, SHFILEINFOW,
    SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHIL_EXTRALARGE, SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// COM for the calling thread, released on drop if this call set it up.
struct Com(bool);

impl Com {
    fn init() -> Self {
        Self(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// The shell's thumbnail of `path`, fitting `size`×`size`; `None` when the
/// shell has no thumbnail for it (it never falls back to an icon). Blocks
/// while the thumbnail is made: call it off the UI thread.
pub fn thumbnail(path: &Path, size: u32) -> Option<Icon> {
    let _com = Com::init();
    let w = wide(path);
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None::<&IBindCtx>).ok()?;
        let s = size as i32;
        let bitmap = factory.GetImage(SIZE { cx: s, cy: s }, SIIGBF_THUMBNAILONLY).ok()?;
        let icon = bitmap_rgba(bitmap, true);
        let _ = DeleteObject(bitmap.into());
        icon
    }
}

/// The 48 px system icon of a file or folder; `by_type_only` looks it up
/// by extension without touching the file (so any name works).
pub fn large_icon(path: &Path, is_dir: bool, by_type_only: bool) -> Option<Icon> {
    let w = wide(path);
    let mut info = SHFILEINFOW::default();
    let mut flags = SHGFI_SYSICONINDEX;
    if by_type_only {
        flags |= SHGFI_USEFILEATTRIBUTES;
    }
    let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    unsafe {
        let ok = SHGetFileInfoW(PCWSTR(w.as_ptr()), attrs, Some(&mut info), size_of::<SHFILEINFOW>() as u32, flags);
        if ok == 0 {
            return None;
        }
        let list: IImageList = SHGetImageList(SHIL_EXTRALARGE as i32).ok()?;
        let hicon = list.GetIcon(info.iIcon, ILD_TRANSPARENT.0).ok()?;
        let mut ii = ICONINFO::default();
        let icon = if GetIconInfo(hicon, &mut ii).is_ok() {
            let icon = if ii.hbmColor.is_invalid() { None } else { bitmap_rgba(ii.hbmColor, false) };
            for b in [ii.hbmColor, ii.hbmMask] {
                if !b.is_invalid() {
                    let _ = DeleteObject(b.into());
                }
            }
            icon
        } else {
            None
        };
        let _ = DestroyIcon(hicon);
        icon
    }
}

/// A 32-bit bitmap as straight-alpha RGBA. Without any alpha it is opaque;
/// `premultiplied` alpha (shell thumbnails) is divided out.
unsafe fn bitmap_rgba(bitmap: HBITMAP, premultiplied: bool) -> Option<Icon> {
    let mut bm = BITMAP::default();
    let got = unsafe { GetObjectW(bitmap.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; (w * h * 4) as usize];
    let lines = unsafe {
        let dc = GetDC(None);
        let lines =
            GetDIBits(dc, bitmap, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        lines
    };
    if lines == 0 {
        return None;
    }
    let pixels = px.as_chunks_mut::<4>().0;
    let has_alpha = pixels.iter().any(|p| p[3] != 0);
    for p in pixels {
        if !has_alpha {
            p[3] = 255;
        } else if premultiplied && p[3] != 0 && p[3] != 255 {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
        p.swap(0, 2); // BGRA -> RGBA
    }
    Some(Icon { width: w as u32, height: h as u32, rgba: px })
}
