//! Windows implementation (shell icons, context menu, disk space).

use crate::{DragOutcome, Icon, MenuAction};
use std::cell::RefCell;
use std::ffi::{c_void, OsStr};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows::core::{Interface, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetLogicalDrives, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{CoInitializeEx, IDataObject, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DefSubclassProc, ILFree, SHDoDragDrop, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder,
    RemoveWindowSubclass, SHGetDesktopFolder, SHGetFileInfoW, SHParseDisplayName,
    SetWindowSubclass, CMF_CANRENAME, CMF_EXTENDEDVERBS, CMF_NORMAL, CMINVOKECOMMANDINFO,
    CMINVOKECOMMANDINFOEX, CMIC_MASK_PTINVOKE, GCS_VERBW, SHFILEINFOW,
    SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetIconInfo, TrackPopupMenuEx, ASFW_ANY, HICON, ICONINFO, SW_SHOWNORMAL, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

pub fn drives() -> Vec<String> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .collect()
}

pub fn allow_foreground() {
    // Lets the program we launch bring its window to the front.
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
}

pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let w = wide(path.as_os_str());
    let (mut free, mut total) = (0u64, 0u64);
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), Some(&mut total), None) }
        .ok()?;
    Some((free, total))
}

// ---- icons ------------------------------------------------------------------

pub fn file_icon(path: &Path, is_dir: bool, by_type_only: bool) -> Option<Icon> {
    let w = wide(path.as_os_str());
    let mut info = SHFILEINFOW::default();
    let mut flags = SHGFI_ICON | SHGFI_SMALLICON;
    if by_type_only {
        // Don't touch the file: look the icon up by extension/attributes.
        flags |= SHGFI_USEFILEATTRIBUTES;
    }
    let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            attrs,
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let icon = unsafe { hicon_to_rgba(info.hIcon) };
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    icon
}

unsafe fn hicon_to_rgba(hicon: HICON) -> Option<Icon> {
    let mut ii = ICONINFO::default();
    unsafe { GetIconInfo(hicon, &mut ii) }.ok()?;
    let (color, mask) = (ii.hbmColor, ii.hbmMask);

    let result = unsafe {
        let dc = GetDC(None);
        let icon = read_icon_bitmaps(dc, color, mask);
        ReleaseDC(None, dc);
        icon
    };

    unsafe {
        if !color.is_invalid() {
            let _ = DeleteObject(color.into());
        }
        if !mask.is_invalid() {
            let _ = DeleteObject(mask.into());
        }
    }
    result
}

unsafe fn read_icon_bitmaps(dc: HDC, color: HBITMAP, mask: HBITMAP) -> Option<Icon> {
    if color.is_invalid() {
        return None; // monochrome icon; rare, skip
    }
    let mut bm = BITMAP::default();
    let got = unsafe {
        GetObjectW(color.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void))
    };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);

    let read = |bitmap: HBITMAP| -> Option<Vec<u8>> {
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
            GetDIBits(dc, bitmap, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS)
        };
        (lines != 0).then_some(px)
    };

    let mut px = read(color)?;
    if !px.as_chunks::<4>().0.iter().any(|p| p[3] != 0) {
        // No alpha channel: derive it from the AND mask (black = opaque).
        if let Some(m) = (!mask.is_invalid()).then(|| read(mask)).flatten() {
            for (p, m) in px.as_chunks_mut::<4>().0.iter_mut().zip(m.as_chunks::<4>().0) {
                p[3] = if m[0] == 0 { 255 } else { 0 };
            }
        } else {
            px.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
        }
    }
    for p in px.as_chunks_mut::<4>().0 {
        p.swap(0, 2); // BGRA -> RGBA
    }
    Some(Icon { width: w as u32, height: h as u32, rgba: px })
}

// ---- context menu -------------------------------------------------------------

const FIRST_ID: u32 = 1;
/// Same value as SEE_MASK_UNICODE; not exported by windows-rs.
const CMIC_MASK_UNICODE: u32 = 0x4000;
const LAST_ID: u32 = 0x7FFF;
const SUBCLASS_ID: usize = 0x5243_4D4E; // "RCMN"

thread_local! {
    /// The menu being shown, so owner-drawn submenus ("Send to", "New",
    /// "Open with") can be populated from the window procedure.
    static ACTIVE_MENU: RefCell<Option<(Option<IContextMenu2>, Option<IContextMenu3>)>> =
        const { RefCell::new(None) };
}

pub fn show_context_menu(
    window: isize,
    folder: &Path,
    items: &[PathBuf],
    extended: bool,
) -> Result<MenuAction, String> {
    let hwnd = HWND(window as *mut c_void);
    unsafe {
        // Already initialised by the windowing library (for drag & drop);
        // this is just a safety net.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let menu = context_menu_for(hwnd, folder, items).map_err(|e| e.message())?;
        let hmenu = CreatePopupMenu().map_err(|e| e.message())?;

        let mut flags = CMF_NORMAL | CMF_CANRENAME;
        if extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        let hr = menu.QueryContextMenu(hmenu, 0, FIRST_ID, LAST_ID, flags);
        if hr.is_err() {
            let _ = DestroyMenu(hmenu);
            return Err(hr.message());
        }

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);

        ACTIVE_MENU.with_borrow_mut(|m| *m = Some((menu.cast().ok(), menu.cast().ok())));
        let subclassed = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0).as_bool();
        let picked = TrackPopupMenuEx(hmenu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, hwnd, None);
        if subclassed {
            let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
        }
        ACTIVE_MENU.with_borrow_mut(|m| *m = None);

        let picked = picked.0 as u32;
        let result = if picked < FIRST_ID {
            Ok(MenuAction::Nothing)
        } else {
            let offset = (picked - FIRST_ID) as usize;
            let verb = command_verb(&menu, offset);
            match verb.as_str() {
                "rename" => Ok(MenuAction::Rename),
                "delete" => Ok(MenuAction::Delete),
                _ => invoke(&menu, hwnd, offset, folder, pt)
                    .map(|_| MenuAction::Invoked(verb))
                    .map_err(|e| e.message()),
            }
        };
        let _ = DestroyMenu(hmenu);
        result
    }
}

/// Builds the IContextMenu for `items` in `folder`, or the folder's
/// background menu if `items` is empty.
unsafe fn context_menu_for(
    hwnd: HWND,
    folder: &Path,
    items: &[PathBuf],
) -> windows::core::Result<IContextMenu> {
    unsafe {
        let shell_folder = shell_folder(folder)?;

        if items.is_empty() {
            return shell_folder.CreateViewObject(hwnd);
        }
        ui_object(hwnd, &shell_folder, items)
    }
}

/// Binds to `folder` in the shell namespace.
unsafe fn shell_folder(folder: &Path) -> windows::core::Result<IShellFolder> {
    unsafe {
        let desktop = SHGetDesktopFolder()?;
        let folder_w = wide(folder.as_os_str());
        let mut folder_pidl: *mut ITEMIDLIST = null_mut();
        SHParseDisplayName(PCWSTR(folder_w.as_ptr()), None, &mut folder_pidl, 0, None)?;
        let result = desktop.BindToObject(folder_pidl, None);
        ILFree(Some(folder_pidl));
        result
    }
}

/// A shell UI object (context menu, data object, ...) for items of one folder.
unsafe fn ui_object<T: Interface>(
    hwnd: HWND,
    shell_folder: &IShellFolder,
    items: &[PathBuf],
) -> windows::core::Result<T> {
    unsafe {
        let mut children: Vec<*mut ITEMIDLIST> = Vec::with_capacity(items.len());
        let mut result = Ok(());
        for item in items {
            let name = wide(item.file_name().unwrap_or(item.as_os_str()));
            let mut child: *mut ITEMIDLIST = null_mut();
            let mut attrs = 0u32;
            match shell_folder.ParseDisplayName(hwnd, None, PCWSTR(name.as_ptr()), None, &mut child, &mut attrs) {
                Ok(()) => children.push(child),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        let menu = result.and_then(|_| {
            let ptrs: Vec<*const ITEMIDLIST> = children.iter().map(|c| *c as *const _).collect();
            shell_folder.GetUIObjectOf::<T>(hwnd, &ptrs, None)
        });
        for child in children {
            ILFree(Some(child));
        }
        menu
    }
}

unsafe fn command_verb(menu: &IContextMenu, offset: usize) -> String {
    let mut buf = [0u16; 256];
    let ok = unsafe {
        menu.GetCommandString(offset, GCS_VERBW, None, PSTR(buf.as_mut_ptr() as *mut u8), buf.len() as u32)
    };
    if ok.is_err() {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).to_ascii_lowercase()
}

unsafe fn invoke(
    menu: &IContextMenu,
    hwnd: HWND,
    offset: usize,
    folder: &Path,
    pt: POINT,
) -> windows::core::Result<()> {
    let dir = wide(folder.as_os_str());
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd,
        // MAKEINTRESOURCE: the command offset smuggled in as a pointer.
        lpVerb: PCSTR(offset as *const u8),
        lpVerbW: PCWSTR(offset as *const u16),
        lpDirectoryW: PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: pt,
        ..Default::default()
    };
    allow_foreground();
    unsafe { menu.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO) }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        let handled = ACTIVE_MENU.with_borrow(|active| {
            let (cm2, cm3) = active.as_ref()?;
            if let Some(cm3) = cm3 {
                let mut result = LRESULT(0);
                if unsafe { cm3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)) }.is_ok() {
                    return Some(result);
                }
            }
            if let Some(cm2) = cm2 {
                if unsafe { cm2.HandleMenuMsg(msg, wparam, lparam) }.is_ok() {
                    return Some(LRESULT(0));
                }
            }
            None
        });
        if let Some(result) = handled {
            return result;
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

// ---- drag & drop to other apps -------------------------------------------------

pub fn drag_out(window: isize, folder: &Path, items: &[PathBuf]) -> Result<DragOutcome, String> {
    let hwnd = HWND(window as *mut c_void);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let data: IDataObject = shell_folder(folder)
            .and_then(|f| ui_object(hwnd, &f, items))
            .map_err(|e| e.message())?;
        let allowed = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0);
        // The shell supplies the drop source and the drag image.
        let effect = SHDoDragDrop(Some(hwnd), &data, None, allowed).map_err(|e| e.message())?;
        Ok(if effect.0 & DROPEFFECT_MOVE.0 != 0 {
            DragOutcome::Moved
        } else if effect.0 & DROPEFFECT_COPY.0 != 0 {
            DragOutcome::Copied
        } else if effect.0 & DROPEFFECT_LINK.0 != 0 {
            DragOutcome::Linked
        } else {
            DragOutcome::Cancelled
        })
    }
}

pub fn cursor_in_window(window: isize) -> Option<(f32, f32)> {
    let hwnd = HWND(window as *mut c_void);
    let mut pt = POINT::default();
    unsafe {
        GetCursorPos(&mut pt).ok()?;
        if !ScreenToClient(hwnd, &mut pt).as_bool() {
            return None;
        }
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        Some((pt.x as f32 / scale, pt.y as f32 / scale))
    }
}
