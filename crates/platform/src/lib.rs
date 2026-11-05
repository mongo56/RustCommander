//! OS integration: drives, file icons, the shell context menu, disk space
//! and launching files. Windows gets the real thing via `windows-rs`; other
//! platforms get simple fallbacks for now.

use std::io;
use std::path::{Path, PathBuf};

mod attrib;
pub mod credentials;
mod editors;
mod fileid;
pub mod net_discover;
mod netuse;
mod process_info;
mod rename;
mod shares;
mod terminals;
pub use attrib::set_file_attributes;
pub use editors::{detect_editors, pick_program, FoundEditor};
pub use fileid::file_id;
pub use netuse::{connect_share, is_logon_code, share_error, share_root, ShareError};
pub use process_info::{foreground_child, process_cwd};
pub use rename::rename_no_replace;
pub use shares::{shares, Share};
pub use terminals::{detect_terminals, find_pwsh, FoundTerminal};
#[cfg(windows)]
mod clipboard_win;
#[cfg(windows)]
mod drive_win;
#[cfg(windows)]
mod thumb_win;
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
    /// Our own "Run in terminal" on a script (or its "Run with PowerShell"
    /// when [`ScriptMenu::take_run_with_powershell`]): the app runs it in
    /// the built-in terminal.
    RunInTerminal,
    /// Our own "Run in external terminal" on a script.
    RunExternal,
}

/// The context menu is for one script file (.ps1, .cmd, .bat): our own
/// "Run in terminal" and "Run in external terminal" go at the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptMenu {
    /// The shell's "Run with PowerShell" gives [`MenuAction::RunInTerminal`]
    /// instead of starting a PowerShell window.
    pub take_run_with_powershell: bool,
}

/// Whether a picked context-menu command is "Run with PowerShell": the
/// `.ps1` verb `0` (what Windows registers it as), or, for a handler that
/// names it differently, its English text (`&` mnemonics ignored).
pub fn is_run_with_powershell(verb: &str, text: &str) -> bool {
    let text = text.replace('&', "").trim().to_ascii_lowercase();
    verb == "0" || text.starts_with("run with powershell")
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

/// The name the OS shows for a root from `roots` ("Local Disk (C:)"), if
/// any. May block on network or optical drives: call it off the UI thread.
pub fn drive_label(root: &str) -> Option<String> {
    #[cfg(windows)]
    {
        drive_win::drive_label(root)
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        None
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

/// Mounts a disk image (iso, vhd, ...) the way Windows does when you
/// double-click it. Must run on the UI thread (COM menu).
pub fn mount_disk_image(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        win::mount_iso(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("not supported on this platform".to_string())
    }
}

/// Starts `program` with `args` in `dir` without waiting for it, letting
/// it take the foreground. The child can be polled to see when it exits
/// (dropping it doesn't stop the program).
pub fn spawn_detached(program: &str, args: &[std::ffi::OsString], dir: &Path) -> io::Result<std::process::Child> {
    #[cfg(windows)]
    win::allow_foreground();
    std::process::Command::new(program).args(args).current_dir(dir).spawn()
}

/// Starts `program` in a new console window (Windows: CREATE_NEW_CONSOLE,
/// also when we have a console ourselves) in `dir`, with `raw_args` put on
/// its command line as they are (cmd.exe's quoting isn't the usual one).
/// Doesn't wait for it.
pub fn spawn_console(program: &str, raw_args: &str, dir: &Path) -> io::Result<()> {
    let mut command = std::process::Command::new(program);
    command.current_dir(dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        win::allow_foreground();
        command.raw_arg(raw_args).creation_flags(CREATE_NEW_CONSOLE);
    }
    #[cfg(not(windows))]
    command.args(raw_args.split_whitespace());
    command.spawn().map(|_| ())
}

/// Starts the program `path` with the command line `args` through the
/// shell, so a program that needs administrator rights gets the UAC prompt
/// (a plain `CreateProcess` fails for those). Doesn't wait for it.
pub fn run_program(path: &Path, args: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows::core::{HSTRING, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        win::allow_foreground();
        let (file, params) = (HSTRING::from(path.as_os_str()), HSTRING::from(args));
        let dir = path.parent().map(|d| HSTRING::from(d.as_os_str()));
        // SAFETY: all strings live until the call returns.
        let result = unsafe {
            ShellExecuteW(
                None,
                &HSTRING::from("open"),
                &file,
                &params,
                dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
                SW_SHOWNORMAL,
            )
        };
        // Values above 32 mean success; below are old-style error codes.
        match result.0 as isize {
            n if n > 32 => Ok(()),
            5 => Err(io::Error::new(io::ErrorKind::PermissionDenied, "access denied or cancelled")),
            2 | 3 => Err(io::Error::new(io::ErrorKind::NotFound, "program not found")),
            n => Err(io::Error::other(format!("ShellExecute failed ({n})"))),
        }
    }
    #[cfg(not(windows))]
    {
        let dir = path.parent().unwrap_or(Path::new("."));
        let args: Vec<std::ffi::OsString> = args.split_whitespace().map(Into::into).collect();
        std::process::Command::new(path).args(args).current_dir(dir).spawn().map(drop)
    }
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

/// The shell's thumbnail of a file (pictures, videos, PDFs, ... whatever
/// Explorer previews), fitting `size`×`size`. `None` when there is none:
/// it never falls back to an icon. Slow (decodes the file unless the
/// system's thumbnail cache has it): call it off the UI thread.
pub fn thumbnail(path: &Path, size: u32) -> Option<Icon> {
    #[cfg(windows)]
    {
        thumb_win::thumbnail(path, size)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, size);
        None
    }
}

/// The system's large (48 px) icon for a file or folder; cache it by
/// [`icon_key`] like [`file_icon`]. Works for paths that don't exist
/// (inside archives) unless their icon is per file.
pub fn large_icon(path: &Path, is_dir: bool) -> Option<Icon> {
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let per_file = if is_dir { path.parent().is_none() } else { has_own_icon(&ext) };
        thumb_win::large_icon(path, is_dir, !per_file || !path.exists())
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
/// `script`: the one item is a script (own items at the top, see [`ScriptMenu`]).
pub fn show_context_menu(
    window: isize,
    folder: &Path,
    items: &[PathBuf],
    extended: bool,
    script: Option<ScriptMenu>,
) -> Result<MenuAction, String> {
    #[cfg(windows)]
    {
        win::show_context_menu(window, folder, items, extended, script)
    }
    #[cfg(not(windows))]
    {
        let _ = (window, folder, items, extended, script);
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

/// Puts files on the system clipboard (Explorer's format) for pasting in
/// other apps; `cut` marks them to be moved.
pub fn set_clipboard_files(window: isize, paths: &[PathBuf], cut: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        clipboard_win::set_files(window, paths, cut)
    }
    #[cfg(not(windows))]
    {
        let _ = (window, paths, cut);
        Err("Copying files to the clipboard is not available on this platform yet".into())
    }
}

/// Files on the system clipboard and whether they were cut, or `None`.
pub fn clipboard_files() -> Option<(Vec<PathBuf>, bool)> {
    #[cfg(windows)]
    {
        clipboard_win::get_files()
    }
    #[cfg(not(windows))]
    {
        None
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

/// What the user picked in the small menu shown after a right-button drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropChoice {
    Copy,
    Move,
    /// Fall back to the OS drop menu (handlers such as archive extraction).
    More,
    Cancel,
}

/// Our own right-drop menu (Copy here / Move here / More options / Cancel)
/// at the cursor. Blocks until the menu closes.
pub fn drop_choice(window: isize) -> Result<DropChoice, String> {
    #[cfg(windows)]
    {
        win::drop_choice(window)
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        Ok(DropChoice::More)
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

/// Whether the logical right mouse button is down.
pub fn right_button_down() -> bool {
    #[cfg(windows)]
    {
        win::right_button_down()
    }
    #[cfg(not(windows))]
    {
        false
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

#[cfg(test)]
mod tests {
    use super::is_run_with_powershell;

    #[test]
    fn run_with_powershell_is_recognised() {
        assert!(is_run_with_powershell("0", "Run with PowerShell"));
        assert!(is_run_with_powershell("0", "Mit PowerShell ausführen"));
        assert!(is_run_with_powershell("", "&Run with PowerShell"));
        assert!(is_run_with_powershell("runps", " Run with PowerShell 7 "));
        assert!(!is_run_with_powershell("open", "Open"));
        assert!(!is_run_with_powershell("edit", "Edit"));
        assert!(!is_run_with_powershell("", "Windows PowerShell"));
        assert!(!is_run_with_powershell("runas", "Run as administrator"));
    }
}
