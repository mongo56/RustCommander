//! Renaming without ever replacing what is already at the target.

use std::io;
use std::path::Path;

/// Renames (or moves within one volume) `from` to `to`, failing if `to`
/// exists. Unlike `std::fs::rename`, an existing file is never replaced,
/// not even in a race; a case-only rename of the same item works. Moves
/// between volumes fail too (nothing is copied).
#[cfg(windows)]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};

    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (src, dst) = (wide(from), wide(to));
    unsafe { MoveFileExW(PCWSTR(src.as_ptr()), PCWSTR(dst.as_ptr()), MOVE_FILE_FLAGS(0)) }
        .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

#[cfg(not(windows))]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    if to.symlink_metadata().is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "the target already exists"));
    }
    std::fs::rename(from, to)
}
