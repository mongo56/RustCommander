//! Identity of a file on disk: two paths with the same id are the same file
//! (hard links of each other).

use std::path::Path;

/// `(volume, file index)` of the file, or `None` when it can't be opened.
#[cfg(windows)]
pub fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; the handle is closed before returning.
    unsafe {
        let handle = CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .ok()?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let ok = GetFileInformationByHandle(handle, &mut info).is_ok();
        let _ = CloseHandle(handle);
        ok.then_some((
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        ))
    }
}

#[cfg(not(windows))]
pub fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

/// One entry of [`read_dir_ids`].
#[derive(Debug, Clone, Copy)]
pub struct DirItem<'a> {
    /// The name as UTF-16 (no NUL).
    pub name: &'a [u16],
    /// `FILE_ATTRIBUTE_*` bits.
    pub attributes: u32,
    /// Reparse tag (0 unless a reparse point).
    pub reparse_tag: u32,
    /// Logical length.
    pub len: u64,
    /// The file's id on its volume, `None` where the file system has none.
    pub id: Option<u128>,
    /// Serial number of the volume the folder is on, `None` if unknown.
    pub volume: Option<u64>,
}

impl DirItem<'_> {
    /// A folder to descend into: a directory that isn't a link or junction
    /// (the same rule as `std::fs::FileType::is_dir`).
    pub fn is_dir(&self) -> bool {
        const DIRECTORY: u32 = 0x10;
        const REPARSE_POINT: u32 = 0x400;
        const NAME_SURROGATE: u32 = 0x2000_0000;
        self.attributes & DIRECTORY != 0
            && !(self.attributes & REPARSE_POINT != 0 && self.reparse_tag & NAME_SURROGATE != 0)
    }
}

/// Outcome of [`read_dir_ids`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirRead {
    /// Listed; false if listing stopped part-way on an error.
    Done(bool),
    /// Nothing was listed: read the folder another way (`std::fs::read_dir`).
    /// True when this won't work on this file system at all, so the caller
    /// can stop trying.
    Fallback(bool),
}

/// Lists folder `dir` with its files' ids, without opening the files: one
/// `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` per 64 KiB of
/// entries. `f` gets every entry but "." and ".."; returning false stops.
#[cfg(windows)]
pub fn read_dir_ids(dir: &Path, mut f: impl FnMut(&DirItem) -> bool) -> DirRead {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_EXTD_DIR_INFO,
        FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = verbatim(dir).as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; the handle is closed before returning;
    // entries are read inside the buffer, at the offsets the system wrote.
    unsafe {
        let Ok(handle) = CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        ) else {
            return DirRead::Fallback(false);
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let volume = GetFileInformationByHandle(handle, &mut info).is_ok().then_some(info.dwVolumeSerialNumber as u64);
        // u64s keep the entries 8-byte aligned.
        let mut buf = vec![0u64; 64 * 1024 / 8];
        let bytes = buf.len() * 8;
        let name_at = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        let mut first = true;
        let result = loop {
            let class = if first { FileIdExtdDirectoryRestartInfo } else { FileIdExtdDirectoryInfo };
            if let Err(e) = GetFileInformationByHandleEx(handle, class, buf.as_mut_ptr().cast(), bytes as u32) {
                if e.code() == ERROR_NO_MORE_FILES.to_hresult() {
                    break DirRead::Done(true);
                }
                // Access denied etc. would fail any other way too; anything
                // else on the first call means the class isn't supported.
                break if first { DirRead::Fallback(true) } else { DirRead::Done(false) };
            }
            first = false;
            let base = buf.as_ptr().cast::<u8>();
            let mut at = 0usize;
            let mut stop = false;
            loop {
                if at + name_at > bytes {
                    break;
                }
                let e = std::ptr::read_unaligned(base.add(at).cast::<FILE_ID_EXTD_DIR_INFO>());
                let len = (e.FileNameLength / 2) as usize;
                if at + name_at + len * 2 > bytes {
                    break;
                }
                let name = std::slice::from_raw_parts(base.add(at + name_at).cast::<u16>(), len);
                let dots = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
                if !dots {
                    let raw = u128::from_le_bytes(e.FileId.Identifier);
                    let item = DirItem {
                        name,
                        attributes: e.FileAttributes,
                        reparse_tag: e.ReparsePointTag,
                        len: e.EndOfFile.max(0) as u64,
                        id: (raw != 0 && raw != u128::MAX && raw != u64::MAX as u128).then_some(raw),
                        volume,
                    };
                    if !f(&item) {
                        stop = true;
                        break;
                    }
                }
                if e.NextEntryOffset == 0 {
                    break;
                }
                at += e.NextEntryOffset as usize;
            }
            if stop {
                break DirRead::Done(true);
            }
        };
        let _ = CloseHandle(handle);
        result
    }
}

#[cfg(not(windows))]
pub fn read_dir_ids(_dir: &Path, _f: impl FnMut(&DirItem) -> bool) -> DirRead {
    DirRead::Fallback(true)
}

/// `\\?\` form of a long absolute path, so the API isn't limited to
/// MAX_PATH; anything else as it is.
#[cfg(windows)]
fn verbatim(path: &Path) -> std::borrow::Cow<'_, Path> {
    use std::path::Component;
    let Some(s) = path.to_str() else { return path.into() };
    let dots = path.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir));
    if s.len() < 248 || s.starts_with(r"\\?\") || s.starts_with(r"\\.\") || s.contains('/') || dots || !path.is_absolute() {
        return path.into();
    }
    match s.strip_prefix(r"\\") {
        Some(unc) => std::path::PathBuf::from(format!(r"\\?\UNC\{unc}")).into(),
        None => std::path::PathBuf::from(format!(r"\\?\{s}")).into(),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn lists_long_paths_with_ids_and_skips_dots() {
        let base = std::env::temp_dir().join(format!("rc-fileid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut dir = base.clone();
        for _ in 0..3 {
            dir.push("d".repeat(100));
        }
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), [0u8; 12]).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        assert!(dir.as_os_str().len() > 260);
        let mut seen = Vec::new();
        let read = read_dir_ids(&dir, |item| {
            seen.push((String::from_utf16_lossy(item.name), item.is_dir(), item.len, item.id.is_some()));
            true
        });
        assert_eq!(read, DirRead::Done(true));
        seen.sort();
        assert_eq!(seen, [("f.txt".to_owned(), false, 12, true), ("sub".to_owned(), true, 0, true)]);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
