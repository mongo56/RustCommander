//! OS integration. Shell context menus, drag-and-drop to other apps, system
//! icons and the recycle bin will live here (via `windows-rs` on Windows).

use std::io;
use std::path::{Path, PathBuf};

/// Mounted roots: drive letters on Windows, `/` elsewhere.
pub fn roots() -> Vec<String> {
    #[cfg(windows)]
    {
        // Skip A:/B: so we never poke a floppy controller.
        (b'C'..=b'Z')
            .map(|c| format!("{}:\\", c as char))
            .filter(|d| Path::new(d).exists())
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Opens a file with its default application, without blocking.
pub fn open_default(path: &Path) -> io::Result<()> {
    open::that_detached(path)
}
