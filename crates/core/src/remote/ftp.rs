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

    fn parse(&self, lines: &[String]) -> Vec<Item> {
        lines
            .iter()
            .filter_map(|line| {
                let file = if self.mlsd { ListParser::parse_mlsd(line).ok() } else { ListedFile::try_from(line.as_str()).ok() };
                let file = file?;
                let name = file.name().rsplit('/').next().unwrap_or(file.name()).to_owned();
                if name.is_empty() || name == "." || name == ".." {
                    return None;
                }
                Some(Item {
                    dir: file.is_directory(),
                    size: file.size() as u64,
                    modified: Some(file.modified()).filter(|m| *m > SystemTime::UNIX_EPOCH),
                    name,
                })
            })
            .collect()
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
        Ok(self.parse(&lines))
    }

    fn stat(&mut self, path: &str) -> io::Result<Option<Item>> {
        let (parent, name) = match path.rfind('/') {
            Some(0) => ("/", &path[1..]),
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("/", path),
        };
        if name.is_empty() {
            return Ok(Some(Item { name: "/".into(), dir: true, size: 0, modified: None }));
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
