//! FTP and FTPS (explicit TLS) sessions via `suppaftp` (blocking, rustls
//! with the ring provider and the Windows certificate store).

use super::{err, refused, Item, Login, Protocol, Session, CONNECT_TIMEOUT, IO_TIMEOUT, LOGIN_FAILED};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::SystemTime;
use suppaftp::list::{File as ListedFile, ListParser};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Mode, RustlsConnector, RustlsFtpStream, Status};

pub(super) struct Ftp {
    stream: RustlsFtpStream,
    /// The server lists with `MLSD` (exact sizes and UTC dates).
    mlsd: bool,
}

/// Maps an FTP error to an `io::Error` about `path`: 550 (no such file /
/// not allowed) → NotFound or PermissionDenied, the rest as they are.
fn io_err(path: &str, e: FtpError) -> io::Error {
    match e {
        FtpError::ConnectionError(e) => err(e.kind(), path, e),
        FtpError::UnexpectedResponse(r) => {
            let text = r.as_string().unwrap_or_default();
            let text = text.trim();
            let kind = match r.status {
                Status::FileUnavailable | Status::BadFilename => {
                    let lower = text.to_lowercase();
                    if lower.contains("exist") && !lower.contains("not exist") && !lower.contains("no such") {
                        io::ErrorKind::AlreadyExists
                    } else if lower.contains("denied") || lower.contains("permission") {
                        io::ErrorKind::PermissionDenied
                    } else {
                        io::ErrorKind::NotFound
                    }
                }
                Status::NotLoggedIn => io::ErrorKind::PermissionDenied,
                Status::RequestedActionNotTaken | Status::RequestFileActionIgnored => io::ErrorKind::PermissionDenied,
                Status::NotImplemented | Status::CommandNotImplemented | Status::BadSequence => {
                    io::ErrorKind::Unsupported
                }
                _ => io::ErrorKind::Other,
            };
            refused(kind, path, format!("Server says {text}"))
        }
        other => err(io::ErrorKind::Other, path, other),
    }
}

fn tls_connector() -> io::Result<RustlsConnector> {
    let mut roots = rustls::RootCertStore::empty();
    let found = rustls_native_certs::load_native_certs();
    for cert in found.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(io::Error::other("No trusted root certificates found in the system store"));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(RustlsConnector::from(Arc::new(config)))
}

fn resolve(host: &str, port: u16) -> io::Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Unknown host: {host}")))
}

impl Ftp {
    pub(super) fn connect(login: &Login) -> io::Result<Ftp> {
        let where_ = format!("{}:{}", login.host, login.port);
        let addr = resolve(&login.host, login.port)?;
        let conn = |e: FtpError| io_err(&where_, e);
        let mut stream = RustlsFtpStream::connect_timeout(addr, CONNECT_TIMEOUT).map_err(conn)?;
        set_timeouts(&stream.get_ref())?;
        if login.protocol == Protocol::Ftps {
            stream = stream.into_secure(tls_connector()?, &login.host).map_err(conn)?;
            set_timeouts(&stream.get_ref())?;
        }
        stream = stream.passive_stream_builder(|addr| {
            let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(FtpError::ConnectionError)?;
            set_timeouts(&s).map_err(FtpError::ConnectionError)?;
            Ok(s)
        });
        stream.set_mode(Mode::Passive);
        // The server may announce its private address behind NAT.
        stream.set_passive_nat_workaround(true);
        let user = if login.user.is_empty() { "anonymous" } else { login.user.as_str() };
        let password = login.password.clone().unwrap_or_default();
        if let Err(e) = stream.login(user, password.as_str()) {
            let e = conn(e);
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{LOGIN_FAILED} ({e})")));
        }
        stream.transfer_type(FileType::Binary).map_err(conn)?;
        let mlsd = stream.feat().is_ok_and(|f| f.contains_key("MLSD") || f.contains_key("MLST"));
        Ok(Ftp { stream, mlsd })
    }
}

/// suppaftp knows `type=link` only; servers send `type=OS.unix=slink:<target>`
/// (or `OS.unix=symlink`) for symlinks, which it would drop from the listing.
fn mlsd_link_type(line: &str) -> std::borrow::Cow<'_, str> {
    let (facts, name) = match line.find("; ") {
        Some(i) => line.split_at(i),
        None => return line.into(),
    };
    let mut out = String::with_capacity(line.len());
    let mut changed = false;
    for (n, fact) in facts.split(';').enumerate() {
        if n > 0 {
            out.push(';');
        }
        let lower = fact.to_ascii_lowercase();
        if lower.starts_with("type=os.unix=slink") || lower.starts_with("type=os.unix=symlink") {
            out.push_str("type=link");
            changed = true;
        } else {
            out.push_str(fact);
        }
    }
    if !changed {
        return line.into();
    }
    out.push_str(name);
    out.into()
}

/// Turns LIST / MLSD lines into items. A symlink is reported as a link
/// (`link`) and, until its target is checked, as a file.
fn parse_lines(mlsd: bool, lines: &[String]) -> Vec<Item> {
    lines
        .iter()
        .filter_map(|line| {
            let file = if mlsd { ListParser::parse_mlsd(&mlsd_link_type(line)).ok() } else { ListedFile::try_from(line.as_str()).ok() };
            let file = file?;
            let link = file.is_symlink();
            let mut name = file.name().to_owned();
            if link {
                // Some parsers leave " -> target" on the name.
                if let Some(target) = file.symlink().and_then(|t| t.to_str()).filter(|t| !t.is_empty()) {
                    if let Some(stripped) = name.strip_suffix(&format!(" -> {target}")) {
                        name = stripped.to_owned();
                    }
                }
            }
            let name = name.rsplit('/').next().unwrap_or(&name).to_owned();
            if name.is_empty() || name == "." || name == ".." {
                return None;
            }
            Some(Item {
                dir: file.is_directory(),
                link,
                size: file.size() as u64,
                modified: Some(file.modified()).filter(|m| *m > SystemTime::UNIX_EPOCH),
                name,
            })
        })
        .collect()
}

/// Marks the links that lead to a folder as folders; `is_folder` is given
/// the link's absolute path. A broken link or one to a file stays a file.
fn resolve_links(dir: &str, items: &mut [Item], mut is_folder: impl FnMut(&str) -> bool) {
    for item in items.iter_mut().filter(|i| i.link && !i.dir) {
        item.dir = is_folder(&super::remote_join(dir, &item.name));
    }
}

fn set_timeouts(stream: &TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))
}

impl Session for Ftp {
    fn list(&mut self, dir: &str) -> io::Result<Vec<Item>> {
        let lines = if self.mlsd { self.stream.mlsd(Some(dir)) } else { self.stream.list(Some(dir)) };
        let lines = lines.map_err(|e| io_err(dir, e))?;
        let mut items = parse_lines(self.mlsd, &lines);
        if items.iter().any(|i| i.link) {
            // Tell linked folders from linked files by trying CWD, then go back.
            let back = self.stream.pwd().ok();
            let stream = &mut self.stream;
            resolve_links(dir, &mut items, |path| stream.cwd(path).is_ok());
            if let Some(back) = back {
                let _ = self.stream.cwd(back);
            }
        }
        Ok(items)
    }

    fn stat(&mut self, path: &str) -> io::Result<Option<Item>> {
        let (parent, name) = match path.rfind('/') {
            Some(0) => ("/", &path[1..]),
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("/", path),
        };
        if name.is_empty() {
            return Ok(Some(Item { name: "/".into(), dir: true, link: false, size: 0, modified: None }));
        }
        match self.list(parent) {
            Ok(items) => Ok(items.into_iter().find(|i| i.name == name)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read(&mut self, path: &str, each: &mut dyn FnMut(&mut dyn Read) -> io::Result<bool>) -> io::Result<bool> {
        let mut transfer = self.stream.retr_as_stream(path).map_err(|e| io_err(path, e))?;
        let _ = set_timeouts(transfer.get_ref().get_ref());
        let result = each(&mut transfer);
        match result {
            Ok(true) => {
                // The reader may have stopped before the end.
                io::copy(&mut transfer, &mut io::sink())?;
                transfer.finish().map_err(|e| io_err(path, e))?;
                Ok(true)
            }
            other => {
                self.stream.abort(transfer).map_err(|e| io_err(path, e))?;
                other
            }
        }
    }

    fn write(
        &mut self,
        path: &str,
        modified: Option<SystemTime>,
        fill: &mut dyn FnMut(&mut dyn Write) -> io::Result<bool>,
    ) -> io::Result<bool> {
        let mut transfer = self.stream.put_with_stream(path).map_err(|e| io_err(path, e))?;
        let _ = set_timeouts(transfer.get_ref().get_ref());
        let result = fill(&mut transfer);
        let finished = transfer.finish().map_err(|e| io_err(path, e));
        match (result, finished) {
            (Ok(true), Ok(())) => {
                if let Some(m) = modified {
                    // MFMT is an extension; servers without it keep the upload time.
                    let stamp = chrono::DateTime::<chrono::Utc>::from(m).format("%Y%m%d%H%M%S");
                    let _ = self.stream.custom_command(format!("MFMT {stamp} {path}"), &[Status::File]);
                }
                Ok(true)
            }
            (result, finished) => {
                let _ = self.stream.rm(path);
                result.and(finished.map(|_| false))
            }
        }
    }

    fn make_dir(&mut self, path: &str) -> io::Result<()> {
        self.stream.mkdir(path).map_err(|e| io_err(path, e))
    }

    fn remove_file(&mut self, path: &str) -> io::Result<()> {
        self.stream.rm(path).map_err(|e| io_err(path, e))
    }

    fn remove_dir(&mut self, path: &str) -> io::Result<()> {
        self.stream.rmdir(path).map_err(|e| io_err(path, e))
    }

    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        self.stream.rename(from, to).map_err(|e| io_err(from, e))
    }

    fn keep_alive(&mut self) -> io::Result<()> {
        self.stream.noop().map_err(|e| io_err("keep-alive", e))
    }

    fn home(&mut self) -> io::Result<String> {
        self.stream.pwd().map_err(|e| io_err("PWD", e))
    }
}

impl Drop for Ftp {
    fn drop(&mut self) {
        let _ = self.stream.quit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    const LIST: &[&str] = &[
        "drwxr-xr-x   2 ftp ftp   4096 Jan 10  2024 docs",
        "-rw-r--r--   1 ftp ftp    120 Jan 10  2024 a.txt",
        "lrwxrwxrwx   1 ftp ftp      4 Jan 10  2024 to-docs -> docs",
        "lrwxrwxrwx   1 ftp ftp      7 Jan 10  2024 to-file -> a.txt",
        "lrwxrwxrwx   1 ftp ftp      9 Jan 10  2024 broken -> nowhere",
        "lrwxrwxrwx   1 ftp ftp     12 Jan 10  2024 my link -> /srv/a -> b",
    ];

    #[test]
    fn list_symlinks_keep_their_name_and_are_flagged() {
        let items = parse_lines(false, &lines(LIST));
        let got: Vec<_> = items.iter().map(|i| (i.name.as_str(), i.dir, i.link)).collect();
        assert_eq!(
            got,
            vec![
                ("docs", true, false),
                ("a.txt", false, false),
                ("to-docs", false, true),
                ("to-file", false, true),
                ("broken", false, true),
                ("my link", false, true),
            ]
        );
    }

    #[test]
    fn mlsd_symlinks_are_flagged() {
        let l = lines(&[
            "type=dir;modify=20240110120000;perm=el; docs",
            "type=OS.unix=slink:docs;size=4;modify=20240110120000; to-docs",
            "type=OS.unix=slink:/x/y;size=4;modify=20240110120000; with space",
            "type=file;size=3;modify=20240110120000; f.txt",
        ]);
        let items = parse_lines(true, &l);
        let got: Vec<_> = items.iter().map(|i| (i.name.as_str(), i.dir, i.link)).collect();
        assert_eq!(
            got,
            vec![("docs", true, false), ("to-docs", false, true), ("with space", false, true), ("f.txt", false, false)]
        );
    }

    #[test]
    fn links_become_folders_only_when_cwd_works() {
        let mut items = parse_lines(false, &lines(LIST));
        let mut tried = Vec::new();
        resolve_links("/home", &mut items, |p| {
            tried.push(p.to_owned());
            p == "/home/to-docs"
        });
        // Only links are tried; plain folders and files are not.
        assert_eq!(tried, ["/home/to-docs", "/home/to-file", "/home/broken", "/home/my link"]);
        let dirs: Vec<_> = items.iter().filter(|i| i.dir).map(|i| i.name.as_str()).collect();
        assert_eq!(dirs, ["docs", "to-docs"]);
    }
}
