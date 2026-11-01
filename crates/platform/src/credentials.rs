//! Secrets kept in the Windows Credential Manager (generic credentials of
//! the current user, "Windows Credentials" in the control panel), e.g.
//! saved passwords of server connections. Elsewhere nothing is stored.

use std::io;

/// Stores `secret` under `target` (replacing it); `user` is shown in the
/// Credential Manager next to it.
pub fn save(target: &str, user: &str, secret: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        win::save(target, user, secret)
    }
    #[cfg(not(windows))]
    {
        let _ = (target, user, secret);
        Err(io::Error::new(io::ErrorKind::Unsupported, "No credential store on this system"))
    }
}

/// The secret stored under `target`, if any.
pub fn load(target: &str) -> Option<String> {
    #[cfg(windows)]
    {
        win::load(target)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        None
    }
}

/// Removes the secret stored under `target` (fine if there is none).
pub fn delete(target: &str) {
    #[cfg(windows)]
    win::delete(target);
    #[cfg(not(windows))]
    let _ = target;
}

#[cfg(windows)]
mod win {
    use std::io;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn save(target: &str, user: &str, secret: &str) -> io::Result<()> {
        let mut target_w = wide(target);
        let mut user_w = wide(user);
        let mut blob: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target_w.as_mut_ptr()),
            UserName: PWSTR(user_w.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        let result = unsafe { CredWriteW(&credential, 0) };
        blob.fill(0);
        result.map_err(io::Error::other)
    }

    pub fn load(target: &str) -> Option<String> {
        let target_w = wide(target);
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        unsafe { CredReadW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None, &mut found) }.ok()?;
        if found.is_null() {
            return None;
        }
        let secret = unsafe {
            let c = &*found;
            if c.CredentialBlob.is_null() || c.CredentialBlobSize == 0 {
                CredFree(found as *const _);
                return Some(String::new());
            }
            let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
            let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
            String::from_utf16(&units).ok()
        };
        unsafe { CredFree(found as *const _) };
        secret
    }

    pub fn delete(target: &str) {
        let target_w = wide(target);
        let _ = unsafe { CredDeleteW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None) };
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn saves_loads_and_deletes() {
        let target = format!("RustCommander-test:{}", std::process::id());
        super::save(&target, "tester", "pässwörd").unwrap();
        assert_eq!(super::load(&target).as_deref(), Some("pässwörd"));
        super::save(&target, "tester", "other").unwrap();
        assert_eq!(super::load(&target).as_deref(), Some("other"));
        super::delete(&target);
        assert_eq!(super::load(&target), None);
    }
}
