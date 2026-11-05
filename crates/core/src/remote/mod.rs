//! Remote file systems: SFTP (`russh` + `russh-sftp`) and FTP/FTPS
//! (`suppaftp` with rustls), browsed like local folders.
//!
//! A connected server is a [`Vfs`] reached through paths like
//! `sftp://user@host/home/user` (`ftp://`, `ftps://`; `:port` after the host
//! when it isn't the default). Paths stay `PathBuf`s: joins made by the rest
//! of the program add `\`, which [`split`] reads like `/` (and `Path`
//! compares them equal); [`display`] shows them with `/`.
//!
//! [`connect`] logs in (on the caller's thread: never call it, or any
//! [`Vfs`] method of a remote path, on the UI thread) and registers the
//! connection; [`disconnect`] forgets it. Each connection keeps a small pool
//! of logged-in sessions, so a listing in one panel and a copy job can run
//! at the same time; an idle session the server dropped is replaced
//! transparently. Every network wait has a timeout.
//!
//! SFTP host keys are checked against `known_hosts` in the settings folder
//! ([`set_settings_dir`]): an unknown key fails with [`HOST_KEY_UNKNOWN`] in
//! the error (the UI shows the fingerprint and may call [`trust_host_key`]),
//! a changed one with [`HOST_KEY_CHANGED`] (never connected).
//!
//! FTPS certificates are checked against the Windows certificate store; one
//! that fails that check (self-signed, unknown issuer, other name, expired)
//! may be pinned by the user: its SHA-256 goes into the same `known_hosts`
//! as a line `host:port ftps-cert-sha256 <hex>` ([`CERT_PIN_TYPE`]; SFTP
//! ignores these lines, FTPS reads only them). An unpinned one fails with
//! [`CERT_UNTRUSTED`] (the UI may call [`trust_host_key`], which pins it), a
//! different one than pinned with [`CERT_CHANGED`].

mod ftp;
mod sftp;

use crate::entry::{Entry, EntryKind};
use crate::vfs::Vfs;
use chrono::{DateTime, Local};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// How long to wait for a server to accept a connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long any single network read or write may stall.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Logged-in sessions kept per connection while idle.
const IDLE_SESSIONS: usize = 4;

/// In the error of a connection to an SFTP server whose host key isn't
/// trusted yet.
pub const HOST_KEY_UNKNOWN: &str = "Unknown host key";
/// In the error of a connection to an SFTP server whose host key differs
/// from the trusted one.
pub const HOST_KEY_CHANGED: &str = "HOST KEY CHANGED";
/// In the error of a connection to an FTPS server whose certificate the
/// system doesn't trust and the user hasn't pinned yet.
pub const CERT_UNTRUSTED: &str = "Untrusted certificate";
/// In the error of a connection to an FTPS server whose untrusted
/// certificate differs from the pinned one.
pub const CERT_CHANGED: &str = "CERTIFICATE CHANGED";
/// The key type of an FTPS certificate pin in `known_hosts` (followed by the
/// lower-case hex SHA-256 of the certificate's DER).
pub const CERT_PIN_TYPE: &str = "ftps-cert-sha256";
/// In the error of a login the server refused.
pub const LOGIN_FAILED: &str = "Login failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    Sftp,
    Ftp,
    /// FTP with explicit TLS (`AUTH TLS`); the certificate is checked
    /// against the Windows certificate store, or else against the user's pin.
    Ftps,
}

impl Protocol {
    pub const ALL: [Protocol; 3] = [Protocol::Sftp, Protocol::Ftp, Protocol::Ftps];

    pub fn scheme(self) -> &'static str {
        match self {
            Protocol::Sftp => "sftp",
            Protocol::Ftp => "ftp",
            Protocol::Ftps => "ftps",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Sftp => 22,
            Protocol::Ftp | Protocol::Ftps => 21,
        }
    }

    pub fn parse(scheme: &str) -> Option<Protocol> {
        Protocol::ALL.into_iter().find(|p| p.scheme().eq_ignore_ascii_case(scheme))
    }
}

/// What it takes to log in to a server.
#[derive(Clone, PartialEq, Eq)]
pub struct Login {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// The password (for a private key: its passphrase, if it has one).
    pub password: Option<String>,
    /// SFTP: log in with this private key (OpenSSH/PEM file).
    pub key_file: Option<PathBuf>,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Login({})", self.key())
    }
}

impl Login {
    /// `sftp://user@host[:port]`: the start of every path on this server.
    pub fn key(&self) -> String {
        let port = if self.port == self.protocol.default_port() { String::new() } else { format!(":{}", self.port) };
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", self.user) };
        format!("{}://{user}{}{port}", self.protocol.scheme(), self.host)
    }

    /// `host:port`, as written in `known_hosts`.
    fn host_id(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

// ---- paths -------------------------------------------------------------------

/// The path of `remote` (`/`-separated, absolute) on the server `key`.
pub fn path_of(key: &str, remote: &str) -> PathBuf {
    let remote = if remote.starts_with('/') { remote.to_owned() } else { format!("/{remote}") };
    PathBuf::from(format!("{key}{remote}"))
}

/// True for a path on a server (`sftp://…`, `ftp://…`, `ftps://…`),
/// connected or not.
pub fn is_remote(path: &Path) -> bool {
    split(path).is_some()
}

/// (server key, absolute `/`-separated path on the server) of a remote
/// path; `None` for anything else. `..` components are resolved.
pub fn split(path: &Path) -> Option<(String, String)> {
    let mut parts = path.components();
    let Some(Component::Normal(scheme)) = parts.next() else { return None };
    let scheme = scheme.to_str()?.strip_suffix(':')?;
    let protocol = Protocol::parse(scheme)?;
    let Some(Component::Normal(server)) = parts.next() else { return None };
    let server = server.to_str()?;
    let mut rest: Vec<String> = Vec::new();
    for part in parts {
        match part {
            Component::Normal(n) => rest.push(n.to_string_lossy().into_owned()),
            Component::ParentDir => {
                rest.pop();
            }
            _ => {}
        }
    }
    Some((format!("{}://{server}", protocol.scheme()), format!("/{}", rest.join("/"))))
}

/// A remote path as the user should see it (all `/`); other paths as they
/// are.
pub fn display(path: &Path) -> String {
    match split(path) {
        Some((key, remote)) => format!("{key}{remote}"),
        None => path.display().to_string(),
    }
}

/// The parent of a remote path on the server (`/a/b` → `/a`); `None` at the
/// root.
fn remote_parent(remote: &str) -> Option<&str> {
    if remote == "/" {
        return None;
    }
    Some(match remote.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &remote[..i],
    })
}

fn remote_join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

// ---- settings: known hosts -------------------------------------------------------

static SETTINGS_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where `known_hosts` lives (the program's settings folder).
pub fn set_settings_dir(dir: PathBuf) {
    *SETTINGS_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

fn known_hosts_file() -> io::Result<PathBuf> {
    let dir = SETTINGS_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    dir.map(|d| d.join("known_hosts")).ok_or_else(|| io::Error::other("No settings folder for known_hosts"))
}

/// True for a `known_hosts` key that is an FTPS certificate pin
/// (`ftps-cert-sha256 <hex>`), not an SSH host key.
fn is_cert_pin(key: &str) -> bool {
    key.split_whitespace().next() == Some(CERT_PIN_TYPE)
}

/// The trusted keys of `host:port`: SSH host keys (`ssh-ed25519 AAAA…`), or
/// with `pins` the FTPS certificate pins (`ftps-cert-sha256 <hex>`).
fn known_keys(host_id: &str, pins: bool) -> Vec<String> {
    let Ok(file) = known_hosts_file() else { return Vec::new() };
    let text = std::fs::read_to_string(file).unwrap_or_default();
    text.lines()
        .filter_map(|line| line.trim().split_once(' '))
        .filter(|(host, _)| *host == host_id)
        .map(|(_, key)| key.trim().to_owned())
        .filter(|key| is_cert_pin(key) == pins)
        .collect()
}

/// Host keys and certificate pins seen on connections that failed with
/// [`HOST_KEY_UNKNOWN`] or [`CERT_UNTRUSTED`]: (`host:port`, key), for
/// [`trust_host_key`].
static OFFERED_KEYS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Trusts what the server of `login` offered on the last connection attempt:
/// the SSH host key (which failed with [`HOST_KEY_UNKNOWN`]) or, for FTPS,
/// the certificate (which failed with [`CERT_UNTRUSTED`]). Appends it to
/// `known_hosts` (once: the offer is used up).
pub fn trust_host_key(login: &Login) -> io::Result<()> {
    let host_id = login.host_id();
    let pins = login.protocol == Protocol::Ftps;
    let key = {
        let offered = OFFERED_KEYS.lock().unwrap_or_else(|e| e.into_inner());
        offered.iter().rev().find(|(h, k)| *h == host_id && is_cert_pin(k) == pins).map(|(_, k)| k.clone())
    };
    let what = if pins { "certificate" } else { "host key" };
    let key = key.ok_or_else(|| io::Error::other(format!("No {what} to trust: connect first")))?;
    let file = known_hosts_file()?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    // One write: `writeln!` writes in pieces, which two appends at once
    // (parallel tests, two connects) interleave into broken lines.
    out.write_all(format!("{host_id} {key}\n").as_bytes())?;
    // Trusted once: a later refusal (e.g. a changed key) can't re-trust it.
    OFFERED_KEYS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(h, k)| !(*h == host_id && *k == key));
    Ok(())
}

/// Checks a host key (`ssh-ed25519 AAAA…`, fingerprint for messages):
/// Ok if trusted, else the error to fail the connection with.
fn check_host_key(login: &Login, key: &str, fingerprint: &str) -> Result<(), String> {
    let host_id = login.host_id();
    check_known(&host_id, key, |file| {
        format!(
            "{HOST_KEY_CHANGED}: the server {host_id} now identifies with {fingerprint}, not the key \
             trusted before. Someone may be intercepting the connection. Not connected; if the server's key \
             really changed, remove its line from {file}"
        )
    })
    .map_err(|e| e.unwrap_or_else(|| format!("{HOST_KEY_UNKNOWN}: {host_id} identifies with {fingerprint}")))
}

/// Checks an FTPS certificate (its DER) that the system's roots don't
/// trust (`reason`: why) against the pins of `host_id`: Ok if pinned, else
/// the error to fail the connection with.
fn check_cert_pin(host_id: &str, der: &[u8], reason: &str) -> Result<(), String> {
    let hash = ring::digest::digest(&ring::digest::SHA256, der);
    let hex: String = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let fingerprint = cert_fingerprint(hash.as_ref());
    check_known(host_id, &format!("{CERT_PIN_TYPE} {hex}"), |file| {
        format!(
            "{CERT_CHANGED}: the server {host_id} now shows a certificate with SHA-256 {fingerprint} ({reason}), \
             not the one trusted before. Someone may be intercepting the connection. Not connected; if the \
             server's certificate really changed, remove its {CERT_PIN_TYPE} line from {file}"
        )
    })
    .map_err(|e| {
        e.unwrap_or_else(|| {
            format!("{CERT_UNTRUSTED}: the certificate of {host_id} isn't trusted ({reason}). SHA-256: {fingerprint}")
        })
    })
}

/// `AB:CD:…`: a SHA-256 as browsers show certificate fingerprints.
fn cert_fingerprint(hash: &[u8]) -> String {
    hash.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// Checks `key` against the trusted keys of its kind for `host_id`: Ok if
/// trusted; `Err(Some(changed(known_hosts path)))` if others are trusted;
/// `Err(None)` if none is (the key is kept for [`trust_host_key`]).
fn check_known(host_id: &str, key: &str, changed: impl FnOnce(&str) -> String) -> Result<(), Option<String>> {
    let known = known_keys(host_id, is_cert_pin(key));
    if known.iter().any(|k| k == key) {
        return Ok(());
    }
    if !known.is_empty() {
        let file = known_hosts_file().map(|f| f.display().to_string()).unwrap_or_default();
        return Err(Some(changed(&file)));
    }
    let mut offered = OFFERED_KEYS.lock().unwrap_or_else(|e| e.into_inner());
    offered.retain(|(h, k)| !(h == host_id && is_cert_pin(k) == is_cert_pin(key)));
    offered.push((host_id.to_owned(), key.to_owned()));
    Err(None)
}

// ---- sessions and connections --------------------------------------------------------

/// A file or folder on a server, as a session reports it.
#[derive(Debug, Clone)]
struct Item {
    name: String,
    dir: bool,
    /// A symlink (`dir` says whether it leads to a folder). Only FTP sets it.
    link: bool,
    size: u64,
    modified: Option<SystemTime>,
}

/// One logged-in session (a control connection / SSH connection). Paths
/// are absolute and `/`-separated.
trait Session: Send {
    /// The folder's entries, without `.` and `..`.
    fn list(&mut self, dir: &str) -> io::Result<Vec<Item>>;
    /// `path` itself; `None` if it doesn't exist.
    fn stat(&mut self, path: &str) -> io::Result<Option<Item>>;
    /// Calls `each` with a reader of the file's contents; returns what
    /// `each` returned.
    fn read(&mut self, path: &str, each: &mut dyn FnMut(&mut dyn Read) -> io::Result<bool>) -> io::Result<bool>;
    /// Creates or replaces the file `path` with what `fill` writes, then
    /// sets its modification time where the server allows it. `fill`
    /// returns Ok(false) to cancel: the partial file is removed.
    fn write(
        &mut self,
        path: &str,
        modified: Option<SystemTime>,
        fill: &mut dyn FnMut(&mut dyn Write) -> io::Result<bool>,
    ) -> io::Result<bool>;
    fn make_dir(&mut self, path: &str) -> io::Result<()>;
    fn remove_file(&mut self, path: &str) -> io::Result<()>;
    /// Removes an empty folder.
    fn remove_dir(&mut self, path: &str) -> io::Result<()>;
    fn rename(&mut self, from: &str, to: &str) -> io::Result<()>;
    /// The folder the server puts us in after logging in.
    fn home(&mut self) -> io::Result<String>;
    /// (free, total) bytes of the file system holding `path`; `None` if the
    /// protocol or server can't tell (FTP, SFTP without `statvfs@openssh.com`).
    fn free_space(&mut self, _path: &str) -> io::Result<Option<(u64, u64)>> {
        Ok(None)
    }
    /// Tells the server we're still here (FTP `NOOP`) so it doesn't drop an
    /// idle session; an error means the session is dead. SFTP keeps itself
    /// alive with the SSH keep-alive in its config.
    fn keep_alive(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// False once the session can't be trusted with another command (FTP
    /// after an aborted download): it isn't put back in the pool.
    fn reusable(&self) -> bool {
        true
    }
}

fn open_session(login: &Login) -> io::Result<Box<dyn Session>> {
    match login.protocol {
        Protocol::Sftp => Ok(Box::new(sftp::Sftp::connect(login)?)),
        Protocol::Ftp | Protocol::Ftps => Ok(Box::new(ftp::Ftp::connect(login)?)),
    }
}

/// Whether an error leaves the session usable (the server answered).
fn session_survives(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::AlreadyExists
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::Unsupported
            | io::ErrorKind::DirectoryNotEmpty
    ) || e.get_ref().is_some_and(|inner| inner.is::<Refused>())
}

/// How often the keep-alive thread looks at the idle sessions; a session
/// unused for this long is pinged (servers drop idle ones after 60-300 s).
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(45);

/// A pooled session and when it was last used.
struct Idle {
    session: Box<dyn Session>,
    since: Instant,
}

/// A connected server: its login and idle sessions.
pub struct Connection {
    key: String,
    login: Login,
    idle: Mutex<Vec<Idle>>,
}

/// Whether the keep-alive thread runs; it ends when no connection is left.
static KEEPER: Mutex<bool> = Mutex::new(false);
static KEEPER_WAKE: Condvar = Condvar::new();

fn start_keeper() {
    let mut running = KEEPER.lock().unwrap_or_else(|e| e.into_inner());
    if *running {
        return;
    }
    let spawned = std::thread::Builder::new().name("remote-keep-alive".into()).spawn(|| loop {
        let mut running = KEEPER.lock().unwrap_or_else(|e| e.into_inner());
        running = KEEPER_WAKE.wait_timeout(running, KEEP_ALIVE_INTERVAL).unwrap_or_else(|e| e.into_inner()).0;
        let all: Vec<Arc<Connection>> = CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if all.is_empty() {
            *running = false;
            return;
        }
        drop(running);
        for conn in all {
            conn.ping_idle(KEEP_ALIVE_INTERVAL);
        }
    });
    *running = spawned.is_ok();
}

static CONNECTIONS: Mutex<Vec<Arc<Connection>>> = Mutex::new(Vec::new());

/// Logs in to the server (blocking, with timeouts: call it on a worker
/// thread) and registers the connection, replacing an older one to the
/// same server. Returns the path of the folder `folder` on it, or of the
/// login folder if `folder` is empty.
pub fn connect(login: Login, folder: &str) -> io::Result<PathBuf> {
    let mut session = open_session(&login)?;
    let start = if folder.trim().is_empty() { session.home()? } else { folder.trim().to_owned() };
    let start = if start.starts_with('/') { start } else { format!("/{start}") };
    let key = login.key();
    let conn = Arc::new(Connection { key: key.clone(), login, idle: Mutex::new(vec![Idle { session, since: Instant::now() }]) });
    let mut all = CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|c| c.key != key);
    all.push(conn);
    drop(all);
    start_keeper();
    Ok(path_of(&key, &start))
}

/// Forgets the connection serving `path` (sessions in use by running jobs
/// finish first). False if there was none.
pub fn disconnect(path: &Path) -> bool {
    let Some((key, _)) = split(path) else { return false };
    let mut all = CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner());
    let (gone, kept): (Vec<_>, Vec<_>) = all.drain(..).partition(|c| c.key.eq_ignore_ascii_case(&key));
    *all = kept;
    let found = !gone.is_empty();
    // Logging out talks to the server: not on the caller's (UI) thread.
    if found {
        std::thread::spawn(move || drop(gone));
        KEEPER_WAKE.notify_all();
    }
    found
}

/// Whether the server of `path` is connected.
pub fn is_connected(path: &Path) -> bool {
    connection(path).is_ok()
}

/// The login of the connected server of `path` (to reconnect with).
pub fn login_of(path: &Path) -> Option<Login> {
    connection(path).ok().map(|c| c.login.clone())
}

/// (free, total) bytes on the server file system holding the remote `path`.
/// `None` when the server can't tell or anything fails. Blocking (a request
/// to the server): call it on a worker thread.
pub fn free_space(path: &Path) -> Option<(u64, u64)> {
    let conn = connection(path).ok()?;
    let remote = conn.remote(path).ok()?;
    conn.with(|s| s.free_space(&remote)).ok().flatten().filter(|&(_, total)| total > 0)
}

fn connection(path: &Path) -> io::Result<Arc<Connection>> {
    let (key, _) = split(path).ok_or_else(|| io::Error::other("Not a remote path"))?;
    let all = CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner());
    all.iter().find(|c| c.key.eq_ignore_ascii_case(&key)).cloned().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, format!("Not connected to {key} (Ctrl+F7 connects)"))
    })
}

/// The file system serving the remote `path`.
pub(crate) fn open(path: &Path) -> io::Result<Arc<dyn Vfs>> {
    Ok(connection(path)?)
}

impl Connection {
    /// Runs `op` on an idle session (or a new one). If an idle session
    /// fails in a way that suggests the server dropped it, `op` is retried
    /// once on a fresh session, unless `retry_ok` says it's too late (data
    /// was handed out already).
    fn with_if<T>(
        &self,
        mut op: impl FnMut(&mut dyn Session) -> io::Result<T>,
        retry_ok: impl Fn() -> bool,
    ) -> io::Result<T> {
        let pooled = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop().map(|i| i.session);
        let fresh = pooled.is_none();
        let mut session = match pooled {
            Some(s) => s,
            None => open_session(&self.login)?,
        };
        let mut result = op(session.as_mut());
        if let Err(e) = &result {
            if session_survives(e) {
                self.put_back(session);
                return result;
            }
            if fresh || !retry_ok() {
                return result;
            }
            session = open_session(&self.login)?;
            result = op(session.as_mut());
            if result.as_ref().is_err_and(|e| !session_survives(e)) {
                return result;
            }
        }
        self.put_back(session);
        result
    }

    fn with<T>(&self, op: impl FnMut(&mut dyn Session) -> io::Result<T>) -> io::Result<T> {
        self.with_if(op, || true)
    }

    fn put_back(&self, session: Box<dyn Session>) {
        if !session.reusable() {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < IDLE_SESSIONS {
            idle.push(Idle { session, since: Instant::now() });
        }
    }

    /// Pings the idle sessions unused for `older_than` (taken out of the
    /// pool meanwhile, so no job gets one mid-ping) and keeps those that
    /// answer; the others are dropped and the next job opens a new one.
    fn ping_idle(&self, older_than: Duration) {
        let stale: Vec<Idle> = {
            let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
            let (stale, fresh): (Vec<_>, Vec<_>) = idle.drain(..).partition(|i| i.since.elapsed() >= older_than);
            *idle = fresh;
            stale
        };
        for mut i in stale {
            if i.session.keep_alive().is_ok() {
                self.put_back(i.session);
            }
        }
    }

    fn remote(&self, path: &Path) -> io::Result<String> {
        match split(path) {
            Some((key, remote)) if key.eq_ignore_ascii_case(&self.key) => Ok(remote),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("Not on {}: {}", self.key, path.display()))),
        }
    }

    fn entry(&self, dir: &str, item: Item) -> Entry {
        let remote = remote_join(dir, &item.name);
        Entry {
            hidden: item.name.starts_with('.'),
            path: path_of(&self.key, &remote),
            kind: if item.dir { EntryKind::Dir } else { EntryKind::File },
            size: if item.dir { 0 } else { item.size },
            modified: item.modified.map(DateTime::<Local>::from),
            name: item.name,
        }
    }

    fn list_remote(&self, dir: &str) -> io::Result<Vec<Entry>> {
        let items = self.with(|s| s.list(dir))?;
        Ok(items.into_iter().map(|i| self.entry(dir, i)).collect())
    }
}

impl Vfs for Connection {
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let remote = self.remote(dir)?;
        let mut entries = Vec::new();
        if let Some(parent) = remote_parent(&remote) {
            entries.push(Entry::parent(path_of(&self.key, parent)));
        }
        entries.extend(self.list_remote(&remote)?);
        Ok(entries)
    }

    fn walk(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let first = self.stat(path)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("Not found: {}", display(path)))
        })?;
        let mut out = vec![first];
        // Linked folders (FTP) are listed but not entered: a link to a parent
        // would never end, so a copy or delete treats them as empty folders.
        let mut linked = vec![false];
        let mut i = 0;
        while i < out.len() {
            if out[i].kind == EntryKind::Dir && !linked[i] {
                let dir = self.remote(&out[i].path)?;
                // A subfolder that is forbidden or gone is skipped instead of
                // failing the whole walk (other errors, e.g. a lost
                // connection, still fail it).
                match self.with(|s| s.list(&dir)) {
                    Ok(children) => {
                        for item in children {
                            linked.push(item.link);
                            out.push(self.entry(&dir, item));
                        }
                    }
                    Err(e) if i > 0 && matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
            i += 1;
        }
        Ok(out)
    }

    fn read_files(&self, files: &[PathBuf], each: &mut dyn FnMut(usize, &mut dyn Read) -> io::Result<bool>)
        -> io::Result<()> {
        for (i, path) in files.iter().enumerate() {
            let remote = self.remote(path)?;
            let started = std::cell::Cell::new(false);
            let go_on = self.with_if(
                |s| {
                    s.read(&remote, &mut |r| {
                        started.set(true);
                        each(i, r)
                    })
                },
                || !started.get(),
            )?;
            if !go_on {
                break;
            }
        }
        Ok(())
    }

    fn stat(&self, path: &Path) -> io::Result<Option<Entry>> {
        let remote = self.remote(path)?;
        let Some(parent) = remote_parent(&remote) else {
            let name = self.key.clone();
            return Ok(Some(Entry { name, path: path.to_path_buf(), kind: EntryKind::Dir, size: 0, modified: None, hidden: false }));
        };
        let item = self.with(|s| s.stat(&remote))?;
        Ok(item.map(|i| self.entry(parent, i)))
    }

    fn writable(&self) -> bool {
        true
    }

    fn make_dir(&self, path: &Path) -> io::Result<()> {
        let remote = self.remote(path)?;
        self.with(|s| s.make_dir(&remote))
    }

    fn remove(&self, path: &Path, is_dir: bool) -> io::Result<()> {
        let remote = self.remote(path)?;
        self.with(|s| if is_dir { s.remove_dir(&remote) } else { s.remove_file(&remote) })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (self.remote(from)?, self.remote(to)?);
        self.with(|s| s.rename(&from, &to))
    }

    fn write_file(
        &self,
        path: &Path,
        modified: Option<SystemTime>,
        fill: &mut dyn FnMut(&mut dyn Write) -> io::Result<bool>,
    ) -> io::Result<bool> {
        let remote = self.remote(path)?;
        let started = std::cell::Cell::new(false);
        self.with_if(
            |s| {
                s.write(&remote, modified, &mut |w| {
                    started.set(true);
                    fill(w)
                })
            },
            || !started.get(),
        )
    }
}

/// An error with `kind` and a message naming `path`.
fn err(kind: io::ErrorKind, path: &str, what: impl std::fmt::Display) -> io::Error {
    io::Error::new(kind, format!("{what}: {path}"))
}

/// The server's answer to a request it refused: the session is still fine.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// [`err`] for a request the server refused.
fn refused(kind: io::ErrorKind, path: &str, what: impl std::fmt::Display) -> io::Error {
    io::Error::new(kind, Refused(format!("{what}: {path}")))
}

#[cfg(test)]
mod tests;
