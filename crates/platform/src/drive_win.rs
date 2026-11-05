//! Drive names as Explorer shows them.

use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_DISPLAYNAME};

/// Explorer's name of a drive root ("Local Disk (C:)", "DATA (E:)",
/// "share (\\server) (Z:)"). Can be slow for network and optical drives.
pub fn drive_label(root: &str) -> Option<String> {
    let w: Vec<u16> = std::ffi::OsStr::new(root).encode_wide().chain(Some(0)).collect();
    let mut info = SHFILEINFOW::default();
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_DISPLAYNAME,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = info.szDisplayName.iter().position(|&c| c == 0).unwrap_or(info.szDisplayName.len());
    let name = String::from_utf16_lossy(&info.szDisplayName[..len]);
    (!name.trim().is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_drive_has_a_label() {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let label = drive_label(&format!("{drive}\\")).unwrap();
        assert!(label.contains(&drive), "{label}");
    }
}
