//! Connecting to a network share with other credentials (`WNetAddConnection3W`),
//! and the pure helpers around it.

use std::fmt;
use std::path::Path;

/// Why a connection could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareError {
    /// The server refused the user name / password.
    BadCredentials,
    /// Windows already has a connection to this server with another user.
    CredentialConflict,
    /// Anything else: the Win32 code and its message.
    Other(u32, String),
}

impl fmt::Display for ShareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShareError::BadCredentials => f.write_str("The user name or password was not accepted"),
            ShareError::CredentialConflict => f.write_str(
                "Windows already has a connection to this server with another user; close it first (net use \\\\server /delete)",
            ),
            ShareError::Other(code, msg) => write!(f, "{} (error {code})", msg.trim_end()),
        }
    }
}

/// Maps a Win32 error code of `WNetAddConnection*` to a `ShareError`.
pub fn share_error(code: u32) -> ShareError {
    match code {
        5 | 86 | 1326 => ShareError::BadCredentials,
        1219 => ShareError::CredentialConflict,
        _ => ShareError::Other(code, std::io::Error::from_raw_os_error(code as i32).to_string()),
    }
}

/// True when the raw OS error `code` of listing a folder means "log in first":
/// access denied (5), bad password (86), not authenticated (1244), logon
/// failure (1326).
pub fn is_logon_code(code: i32) -> bool {
    matches!(code, 5 | 86 | 1244 | 1326)
}

/// `\\server\share` of a UNC path (`\\server\share\sub\dir`, also the
/// `\\?\UNC\server\share\...` form); None for anything else.
pub fn share_root(path: &Path) -> Option<String> {
    let s = path.to_string_lossy().replace('/', "\\");
    let rest = if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        r
    } else if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") {
        return None;
    } else {
        s.strip_prefix(r"\\")?
    };
    let mut parts = rest.split('\\');
    let server = parts.next().filter(|p| !p.is_empty())?;
    let share = parts.next().filter(|p| !p.is_empty())?;
    Some(format!(r"\\{server}\{share}"))
}

/// Connects to `remote` (`\\server\share`, no drive letter) as `user`. With
/// `remember` Windows keeps the connection for later logons and may store the
/// credentials in the Credential Manager; otherwise it lasts for this logon
/// session only. Blocks (can take seconds): call it from a worker.
#[cfg(windows)]
pub fn connect_share(
    remote: &str,
    user: Option<&str>,
    password: Option<&str>,
    remember: bool,
    hwnd: Option<isize>,
) -> Result<(), ShareError> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::NetworkManagement::WNet::{
        WNetAddConnection3W, CONNECT_CMD_SAVECRED, CONNECT_TEMPORARY, CONNECT_UPDATE_PROFILE, NETRESOURCEW,
        RESOURCETYPE_DISK,
    };

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let mut remote_w = wide(remote);
    let user_w = user.filter(|u| !u.is_empty()).map(wide);
    let mut pass_w = password.map(wide);
    let res = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpRemoteName: PWSTR(remote_w.as_mut_ptr()),
        ..Default::default()
    };
    let flags = if remember { CONNECT_UPDATE_PROFILE | CONNECT_CMD_SAVECRED } else { CONNECT_TEMPORARY };
    let pcw = |v: &Option<Vec<u16>>| v.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let code =
        unsafe { WNetAddConnection3W(hwnd.map(|h| HWND(h as *mut _)), &res, pcw(&pass_w), pcw(&user_w), flags).0 };
    if let Some(p) = pass_w.as_mut() {
        for c in p.iter_mut() {
            // Volatile so the wipe isn't optimised away.
            unsafe { std::ptr::write_volatile(c, 0) };
        }
    }
    if code == 0 {
        Ok(())
    } else {
        Err(share_error(code))
    }
}

#[cfg(not(windows))]
pub fn connect_share(
    _remote: &str,
    _user: Option<&str>,
    _password: Option<&str>,
    _remember: bool,
    _hwnd: Option<isize>,
) -> Result<(), ShareError> {
    Err(ShareError::Other(0, "network shares are only supported on Windows".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_root_of_unc_paths() {
        let root = |s: &str| share_root(Path::new(s));
        assert_eq!(root(r"\\srv\share\sub\dir").as_deref(), Some(r"\\srv\share"));
        assert_eq!(root(r"\\srv\share").as_deref(), Some(r"\\srv\share"));
        assert_eq!(root(r"\\srv\share\").as_deref(), Some(r"\\srv\share"));
        assert_eq!(root(r"\\?\UNC\srv\share\a\b").as_deref(), Some(r"\\srv\share"));
        assert_eq!(root("//srv/share/a").as_deref(), Some(r"\\srv\share"));
    }

    #[test]
    fn share_root_rejects_other_paths() {
        let root = |s: &str| share_root(Path::new(s));
        assert_eq!(root(r"C:\dir"), None);
        assert_eq!(root(r"\\srv"), None);
        assert_eq!(root(r"\\srv\"), None);
        assert_eq!(root(r"\\?\C:\dir"), None);
        assert_eq!(root(r"relative\path"), None);
        assert_eq!(root(""), None);
    }

    #[test]
    fn error_codes_map() {
        assert_eq!(share_error(5), ShareError::BadCredentials);
        assert_eq!(share_error(86), ShareError::BadCredentials);
        assert_eq!(share_error(1326), ShareError::BadCredentials);
        assert_eq!(share_error(1219), ShareError::CredentialConflict);
        assert!(matches!(share_error(53), ShareError::Other(53, _)));
        assert!(is_logon_code(1326) && is_logon_code(5) && !is_logon_code(2) && !is_logon_code(53));
    }
}
