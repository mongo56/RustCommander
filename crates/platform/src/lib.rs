//! OS integration: drives, file icons, the shell context menu, disk space
//! and launching files. Windows gets the real thing via `windows-rs`; other
//! platforms get simple fallbacks for now.

use std::io;
use std::path::{Path, PathBuf};

pub mod credentials;
mod editors;
pub use editors::{detect_editors, pick_program, FoundEditor};
#[cfg(windows)]
mod win;

/// An RGBA icon bitmap (straight alpha).
#[derive(Debug, Clone)]
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What the user picked in the shell context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    /// Menu dismissed.
    Nothing,
    /// A shell command ran (verb, possibly empty). Folders may have changed.
    Invoked(String),
    /// "Rename" — handled by the app, since there's no Explorer view to do it.
    Rename,
    /// "Delete" — handled by the app so it goes through the job queue.
    Delete,
}

/// How a drag to another application ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragOutcome {
    Cancelled,
    Copied,
    Moved,
    Linked,
}

/// Mounted roots: drive letters on Windows, `/` elsewhere.
pub fn roots() -> Vec<String> {
    #[cfg(windows)]
    {
        win::drives()
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Opens a file with its default application (or runs it), without blocking.
pub fn open_default(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    win::allow_foreground();
    open::that_detached(path)
}

/// Starts `program` with `args` in `dir` without waiting for it, letting
/// it take the foreground. The child can be polled to see when it exits
/// (dropping it doesn't stop the program).
pub fn spawn_detached(program: &str, args: &[std::ffi::OsString], dir: &Path) -> io::Result<std::process::Child> {
    #[cfg(windows)]
    win::allow_foreground();
    std::process::Command::new(program).args(args).current_dir(dir).spawn()
}

/// True for files that start a program rather than open a document.
pub fn is_executable(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if cfg!(windows) {
        matches!(ext.as_str(), "exe" | "com" | "bat" | "cmd" | "msi" | "lnk" | "ps1" | "appref-ms")
    } else {
        ext.is_empty() || ext == "sh" || ext == "appimage"
    }
}

/// (free, total) bytes on the volume containing `path`.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        win::disk_space(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// Cache key for [`file_icon`]: icons that differ per file (programs,
/// shortcuts, drives) get a per-path key, everything else shares one per
/// extension.
pub fn icon_key(path: &Path, is_dir: bool) -> String {
    if is_dir {
        return if path.parent().is_none() {
            format!("drive:{}", path.display())
        } else {
            "dir".into()
        };
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if has_own_icon(&ext) {
        format!("file:{}", path.display())
    } else {
        format!("ext:{ext}")
    }
}

fn has_own_icon(ext: &str) -> bool {
    matches!(ext, "exe" | "ico" | "lnk" | "url" | "cur" | "ani" | "msc" | "appref-ms" | "scr")
}

/// The system's small icon for a file or folder.
pub fn file_icon(path: &Path, is_dir: bool) -> Option<Icon> {
    #[cfg(windows)]
    {
        let per_file = if is_dir {
            path.parent().is_none()
        } else {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            has_own_icon(&ext)
        };
        win::file_icon(path, is_dir, !per_file)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, is_dir);
        None
    }
}

/// Shows the native context menu at the mouse position for `items` (all
/// inside `folder`), or the folder's background menu when `items` is empty.
/// Must run on the UI thread; `window` is the native window handle.
pub fn show_context_menu(
    window: isize,
    folder: &Path,
    items: &[PathBuf],
    extended: bool,
) -> Result<MenuAction, String> {
    #[cfg(windows)]
    {
        win::show_context_menu(window, folder, items, extended)
    }
    #[cfg(not(windows))]
    {
        let _ = (window, folder, items, extended);
        Err("Context menu is not available on this platform yet".into())
    }
}

/// Starts a native drag of `items` (all inside `folder`) so they can be
/// dropped on Explorer, the desktop or any other app. Blocks until the
/// drop; must run on the UI thread while the mouse button is held.
pub fn drag_out(window: isize, folder: &Path, items: &[PathBuf]) -> Result<DragOutcome, String> {
    #[cfg(windows)]
    {
        win::drag_out(window, folder, items)
    }
    #[cfg(not(windows))]
    {
        let _ = (window, folder, items);
        Err("Dragging to other apps is not available on this platform yet".into())
    }
}

/// Right-button drop of `items` (all inside `folder`) onto the folder
/// `target`: the OS shows its drop menu (copy/move/link here and handlers
/// like archive extraction). Blocks until the menu closes.
pub fn drop_menu(window: isize, folder: &Path, items: &[PathBuf], target: &Path) -> Result<DragOutcome, String> {
    #[cfg(windows)]
    {
        win::drop_menu(window, folder, items, target)
    }
    #[cfg(not(windows))]
    {
        let _ = (window, folder, items, target);
        Err("The drop menu is not available on this platform yet".into())
    }
}

/// Mouse position relative to the window's client area, in logical
/// (DPI-independent) pixels.
pub fn cursor_in_window(window: isize) -> Option<(f32, f32)> {
    #[cfg(windows)]
    {
        win::cursor_in_window(window)
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

/// A window's restored rectangle (screen pixels, Windows workspace
/// coordinates) and maximised state, saved between sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub maximized: bool,
}

/// Where the window is and whether it's maximised; `None` where unsupported.
pub fn window_placement(window: isize) -> Option<Placement> {
    #[cfg(windows)]
    {
        win::window_placement(window)
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

/// Puts the window back where [`window_placement`] found it and shows it.
pub fn restore_placement(window: isize, placement: &Placement) {
    #[cfg(windows)]
    win::restore_placement(window, placement);
    #[cfg(not(windows))]
    let _ = (window, placement);
}
