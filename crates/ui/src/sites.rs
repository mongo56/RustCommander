//! Saved server connections (`connections.toml` in the settings folder).
//! Passwords are never written there: a site either asks for its password
//! when connecting (kept in memory for the session) or keeps it in the
//! Windows Credential Manager (`rc_platform::credentials`), under
//! "RustCommander:" + the server's address.

use rc_core::remote::{Login, Protocol};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub name: String,
    /// "sftp", "ftp" or "ftps".
    pub protocol: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// The password is in the Windows Credential Manager (else asked).
    #[serde(default)]
    pub save_password: bool,
    /// SFTP: private key file to log in with (the password is then its
    /// passphrase, if it has one).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_file: String,
    /// Remote folder to start in (empty: where the server puts us).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub folder: String,
}

#[derive(Default, Serialize, Deserialize)]
struct SitesFile {
    #[serde(default, rename = "site")]
    sites: Vec<Site>,
}

impl Site {
    pub fn protocol(&self) -> Protocol {
        Protocol::parse(&self.protocol).unwrap_or(Protocol::Sftp)
    }

    /// The login, with `password` (or none).
    pub fn login(&self, password: Option<String>) -> Login {
        let key_file = Some(self.key_file.trim()).filter(|k| !k.is_empty() && self.protocol() == Protocol::Sftp);
        Login {
            protocol: self.protocol(),
            host: self.host.trim().to_owned(),
            port: self.port,
            user: self.user.trim().to_owned(),
            password,
            key_file: key_file.map(PathBuf::from),
        }
    }

    /// `sftp://user@host`, as listed and as the start of its paths.
    pub fn address(&self) -> String {
        self.login(None).key()
    }

    /// Credential Manager entry of the saved password.
    pub fn credential_target(&self) -> String {
        format!("RustCommander:{}", self.address())
    }

    /// Whether connecting needs a password typed (none saved; a key file
    /// may still need its passphrase, but usually has none).
    pub fn asks_password(&self) -> bool {
        !self.save_password && (self.key_file.trim().is_empty() || self.protocol() != Protocol::Sftp)
    }
}

fn file() -> Option<PathBuf> {
    Some(crate::config::dir()?.join("connections.toml"))
}

pub fn load() -> Vec<Site> {
    let Some(text) = file().and_then(|f| std::fs::read_to_string(f).ok()) else { return Vec::new() };
    toml::from_str::<SitesFile>(&text).map(|f| f.sites).unwrap_or_default()
}

pub fn save(sites: &[Site]) -> std::io::Result<()> {
    let file = file().ok_or_else(|| std::io::Error::other("No settings folder"))?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(&SitesFile { sites: sites.to_vec() }).map_err(std::io::Error::other)?;
    std::fs::write(file, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_round_trip_without_passwords() {
        let site = Site {
            name: "box".into(),
            protocol: "sftp".into(),
            host: " example.org ".into(),
            port: 2222,
            user: "me".into(),
            save_password: true,
            key_file: String::new(),
            folder: "/srv".into(),
        };
        let text = toml::to_string(&SitesFile { sites: vec![site.clone()] }).unwrap();
        assert!(!text.to_lowercase().contains("password =") || text.contains("save_password = true"));
        let back: SitesFile = toml::from_str(&text).unwrap();
        assert_eq!(back.sites, std::slice::from_ref(&site));
        assert_eq!(site.address(), "sftp://me@example.org:2222");
        assert_eq!(site.credential_target(), "RustCommander:sftp://me@example.org:2222");
        let ftp = Site { protocol: "ftp".into(), port: 21, key_file: "k".into(), save_password: false, ..site };
        assert_eq!(ftp.login(None).key_file, None);
        assert!(ftp.asks_password());
    }
}
