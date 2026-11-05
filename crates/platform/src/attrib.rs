//! Setting the attribute flags of a file or folder (read-only, hidden, ...).

use std::io;
use std::path::Path;

/// Sets the Win32 attribute bits of `path` to `attrs` (only the settable ones
/// count: read-only, hidden, system, archive, ...). A link is changed itself,
/// not its target. 0 is stored as "normal".
#[cfg(windows)]
pub fn set_file_attributes(path: &Path, attrs: u32) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attrs = if attrs == 0 { FILE_ATTRIBUTE_NORMAL.0 } else { attrs };
    unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(attrs)) }
        .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

#[cfg(not(windows))]
pub fn set_file_attributes(_path: &Path, _attrs: u32) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "file attributes are only supported on Windows"))
}
