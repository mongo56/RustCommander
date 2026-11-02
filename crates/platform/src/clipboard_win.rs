//! Files on the Windows clipboard (CF_HDROP + "Preferred DropEffect"), the
//! format Explorer uses for copy/cut/paste of files.

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use windows::core::{w, BOOL};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, POINT};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE};
use windows::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};

/// Closes the clipboard when dropped.
struct Open;

impl Open {
    /// Another app may hold the clipboard for a moment: retry briefly.
    fn new(owner: Option<HWND>) -> Result<Self, String> {
        for _ in 0..10 {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Ok(Open);
            }
            sleep(Duration::from_millis(20));
        }
        Err("The clipboard is in use by another program".into())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// Copies `bytes` into a movable global block (for `SetClipboardData`).
fn global(bytes: &[u8]) -> Result<HGLOBAL, String> {
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.to_string())?;
        let ptr = GlobalLock(mem);
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err("Out of memory".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(mem);
        Ok(mem)
    }
}

/// Hands `mem` to the clipboard as `format`; frees it if that fails.
fn put(format: u32, mem: HGLOBAL) -> Result<(), String> {
    unsafe {
        if let Err(e) = SetClipboardData(format, Some(HANDLE(mem.0))) {
            let _ = GlobalFree(Some(mem));
            return Err(e.to_string());
        }
    }
    Ok(())
}

/// Puts `paths` on the clipboard for pasting elsewhere; `cut` marks them
/// to be moved. `window` must be ours: the clipboard needs an owner.
pub fn set_files(window: isize, paths: &[PathBuf], cut: bool) -> Result<(), String> {
    // DROPFILES header, then NUL-terminated wide paths and a final NUL.
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: BOOL(0),
        fWide: BOOL(1),
    };
    let mut bytes = Vec::new();
    // SAFETY: DROPFILES is plain data.
    bytes.extend_from_slice(unsafe {
        std::slice::from_raw_parts((&header as *const DROPFILES).cast::<u8>(), size_of::<DROPFILES>())
    });
    for path in paths {
        for unit in path.as_os_str().encode_wide().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    let effect = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };

    let _open = Open::new(Some(HWND(window as *mut _)))?;
    unsafe {
        EmptyClipboard().map_err(|e| e.to_string())?;
        put(CF_HDROP.0 as u32, global(&bytes)?)?;
        let format = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        if format != 0 {
            put(format, global(&effect.0.to_le_bytes())?)?;
        }
    }
    Ok(())
}

/// Files on the clipboard and whether they were cut (to be moved), or
/// `None` if it holds no files.
pub fn get_files() -> Option<(Vec<PathBuf>, bool)> {
    let _open = Open::new(None).ok()?;
    unsafe {
        let drop = HDROP(GetClipboardData(CF_HDROP.0 as u32).ok()?.0);
        if drop.0.is_null() {
            return None;
        }
        // The data belongs to the clipboard: no DragFinish.
        let count = DragQueryFileW(drop, u32::MAX, None);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(drop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
            if got > 0 {
                paths.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
            }
        }
        if paths.is_empty() {
            return None;
        }
        let mut cut = false;
        let format = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        if let Ok(h) = GetClipboardData(format) {
            let mem = HGLOBAL(h.0);
            if !mem.0.is_null() && GlobalSize(mem) >= 4 {
                let ptr = GlobalLock(mem);
                if !ptr.is_null() {
                    let effect = ptr.cast::<u32>().read_unaligned();
                    let _ = GlobalUnlock(mem);
                    cut = effect & DROPEFFECT_MOVE.0 != 0 && effect & DROPEFFECT_COPY.0 == 0;
                }
            }
        }
        Some((paths, cut))
    }
}

