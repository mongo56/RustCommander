//! The shares a server offers (`NetShareEnum`), for listing `\\server`.

use std::io;

/// One disk share of a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub name: String,
    /// The server's description of the share (may be empty).
    pub remark: String,
    /// Name ends in `$` (admin shares like `C$`, or ones hidden on purpose).
    pub hidden: bool,
}

/// `STYPE_DISKTREE`, and `STYPE_MASK` (the low byte: the share's base type;
/// above it are flags like `STYPE_SPECIAL`, `STYPE_TEMPORARY` and the cluster bits).
const STYPE_DISKTREE: u32 = 0;
const STYPE_MASK: u32 = 0xFF;

/// The share `name` of type `kind` (a `SHARE_INFO_1` `shi1_type`) if it is
/// shown: disk shares only (printers, devices and `IPC$` are left out).
fn keep_share(name: String, kind: u32, remark: String) -> Option<Share> {
    if kind & STYPE_MASK != STYPE_DISKTREE || name.is_empty() || name.eq_ignore_ascii_case("IPC$") {
        return None;
    }
    let hidden = name.ends_with('$');
    Some(Share { name, remark, hidden })
}

/// The error for a failed `NetShareEnum` on `server`: an unknown or
/// unreachable host says so; anything else keeps the OS code (so access
/// denied / logon failures can be recognised by it).
fn enum_error(server: &str, code: u32) -> io::Error {
    match code {
        // ERROR_REM_NOT_LIST, ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME.
        51 | 53 | 67 => io::Error::new(io::ErrorKind::NotFound, format!(r"Server not found: \\{server}")),
        _ => io::Error::from_raw_os_error(code as i32),
    }
}

/// The disk shares of `server` (a name or address, without `\\`), sorted by
/// name. Blocks (can take seconds on an unreachable host): call it from a worker.
#[cfg(windows)]
pub fn shares(server: &str) -> io::Result<Vec<Share>> {
    use windows::core::PCWSTR;
    use windows::Win32::NetworkManagement::NetManagement::{NetApiBufferFree, MAX_PREFERRED_LENGTH};
    use windows::Win32::Storage::FileSystem::{NetShareEnum, SHARE_INFO_1};

    let unc: Vec<u16> = format!(r"\\{server}").encode_utf16().chain(Some(0)).collect();
    let text = |p: windows::core::PWSTR| if p.is_null() { String::new() } else { unsafe { p.to_string() }.unwrap_or_default() };
    let mut out = Vec::new();
    let mut resume = 0u32;
    loop {
        let mut buf: *mut u8 = std::ptr::null_mut();
        let (mut read, mut total) = (0u32, 0u32);
        let code = unsafe {
            NetShareEnum(PCWSTR(unc.as_ptr()), 1, &mut buf, MAX_PREFERRED_LENGTH, &mut read, &mut total, Some(&mut resume))
        };
        // ERROR_MORE_DATA: this part was read, call again for the rest.
        if (code == 0 || code == 234) && !buf.is_null() {
            let items = unsafe { std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize) };
            for i in items {
                out.extend(keep_share(text(i.shi1_netname), i.shi1_type.0, text(i.shi1_remark)));
            }
        }
        if !buf.is_null() {
            unsafe { NetApiBufferFree(Some(buf as *const _)) };
        }
        match code {
            0 => break,
            234 if read > 0 => {}
            234 => break,
            _ => return Err(enum_error(server, code)),
        }
    }
    out.sort_by_key(|s| s.name.to_lowercase());
    Ok(out)
}

#[cfg(not(windows))]
pub fn shares(server: &str) -> io::Result<Vec<Share>> {
    let _ = (server, enum_error, keep_share);
    Err(io::Error::new(io::ErrorKind::Unsupported, "listing a server's shares is only supported on Windows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep(name: &str, kind: u32) -> Option<Share> {
        keep_share(name.into(), kind, String::new())
    }

    #[test]
    fn keeps_disk_shares_only() {
        assert_eq!(keep("data", 0), Some(Share { name: "data".into(), remark: String::new(), hidden: false }));
        // Printer, device, IPC.
        assert_eq!(keep("printer", 1), None);
        assert_eq!(keep("dev", 2), None);
        assert_eq!(keep("pipe", 3), None);
        // IPC$ (STYPE_IPC | STYPE_SPECIAL), also if it claimed to be a disk.
        assert_eq!(keep("IPC$", 0x8000_0003), None);
        assert_eq!(keep("ipc$", 0), None);
        assert_eq!(keep("", 0), None);
    }

    #[test]
    fn flags_are_masked_and_dollar_is_hidden() {
        // C$ / ADMIN$ are STYPE_DISKTREE | STYPE_SPECIAL.
        assert!(keep("C$", 0x8000_0000).is_some_and(|s| s.hidden));
        assert!(keep("ADMIN$", 0x8000_0000).is_some_and(|s| s.hidden));
        // STYPE_TEMPORARY and cluster bits don't hide a share.
        assert!(keep("tmp", 0x4000_0000).is_some_and(|s| !s.hidden));
        assert!(keep("clu", 0x0200_0000).is_some_and(|s| !s.hidden));
        assert!(keep("secret$", 0).is_some_and(|s| s.hidden));
    }

    #[test]
    fn unknown_host_is_not_found() {
        let e = enum_error("nas", 53);
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert_eq!(e.to_string(), r"Server not found: \\nas");
        // Access denied keeps its code (the login dialog looks for it).
        assert_eq!(enum_error("nas", 5).raw_os_error(), Some(5));
    }
}
