//! SFTP sessions via `russh` (ring crypto) + `russh-sftp`. Both are async:
//! each session's calls are run on a small shared tokio runtime with
//! `block_on` from the calling (worker) thread, each under a timeout.

use super::{check_host_key, err, refused, Item, Login, Session, CONNECT_TIMEOUT, IO_TIMEOUT, LOGIN_FAILED};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, StatusCode};
use std::future::Future;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::runtime::Runtime;

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sftp")
            .enable_all()
            .build()
            .expect("tokio runtime for SFTP")
    })
}

/// Runs `fut` to completion on the runtime, failing with `TimedOut` after
/// `limit`.
fn block<T>(limit: Duration, what: &str, fut: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    runtime().block_on(async {
        match tokio::time::timeout(limit, fut).await {
            Ok(result) => result,
            Err(_) => Err(err(io::ErrorKind::TimedOut, what, "The server didn't answer in time")),
        }
    })
}

fn sftp_err(path: &str, e: SftpError) -> io::Error {
    match e {
        SftpError::Status(status) => {
            let kind = match status.status_code {
                StatusCode::NoSuchFile => io::ErrorKind::NotFound,
                StatusCode::PermissionDenied => io::ErrorKind::PermissionDenied,
                StatusCode::OpUnsupported => io::ErrorKind::Unsupported,
                _ => io::ErrorKind::Other,
            };
            let text = if status.error_message.is_empty() { status.status_code.to_string() } else { status.error_message };
            refused(kind, path, format!("Server says {text}"))
        }
        SftpError::Timeout => err(io::ErrorKind::TimedOut, path, "The server didn't answer in time"),
        other => err(io::ErrorKind::Other, path, other),
    }
}

fn ssh_err(what: &str, e: russh::Error) -> io::Error {
    let kind = match &e {
        russh::Error::IO(e) => e.kind(),
        russh::Error::ConnectionTimeout | russh::Error::KeepaliveTimeout | russh::Error::InactivityTimeout => {
            io::ErrorKind::TimedOut
        }
        _ => io::ErrorKind::Other,
    };
    err(kind, what, e)
}

/// Checks the server's host key against `known_hosts`; the reason for a
/// refusal is left in `problem`.
struct Client {
    login: Login,
    problem: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, offered: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let data = match offered {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.key_data().clone(),
            PublicKeyOrCertificate::Certificate(cert) => cert.public_key().clone(),
        };
        let key = PublicKey::new(data, "");
        let line = key.to_openssh().map_err(|e| russh::Error::IO(io::Error::other(e)))?;
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        match check_host_key(&self.login, line.trim(), &fingerprint) {
            Ok(()) => Ok(true),
            Err(problem) => {
                *self.problem.lock().unwrap_or_else(|e| e.into_inner()) = Some(problem);
                Ok(false)
            }
        }
    }
}

pub(super) struct Sftp {
    handle: Option<client::Handle<Client>>,
    sftp: SftpSession,
}

impl Sftp {
    pub(super) fn connect(login: &Login) -> io::Result<Sftp> {
        let where_ = format!("{}:{}", login.host, login.port);
        let problem = Arc::new(Mutex::new(None));
        let config = client::Config {
            inactivity_timeout: None,
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            ..Default::default()
        };
        let handler = Client { login: login.clone(), problem: problem.clone() };
        let host = login.host.clone();
        let port = login.port;
        let connected = block(CONNECT_TIMEOUT, &where_, async {
            client::connect(Arc::new(config), (host.as_str(), port), handler).await.map_err(|e| ssh_err(&where_, e))
        });
        let mut handle = match connected {
            Ok(h) => h,
            Err(e) => {
                let problem = problem.lock().unwrap_or_else(|e| e.into_inner()).take();
                return Err(match problem {
                    Some(text) => io::Error::new(io::ErrorKind::PermissionDenied, text),
                    None => e,
                });
            }
        };
        let user = login.user.clone();
        let password = login.password.clone();
        let key_file = login.key_file.clone();
        let accepted = block(CONNECT_TIMEOUT, &where_, async {
            let auth = match &key_file {
                Some(file) => {
                    let key = russh::keys::load_secret_key(file, password.as_deref()).map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("Can't read the private key {}: {e}", file.display()),
                        )
                    })?;
                    let hash = handle.best_supported_rsa_hash().await.map_err(|e| ssh_err(&where_, e))?.flatten();
                    let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
                    handle.authenticate_publickey(user, key).await
                }
                None => handle.authenticate_password(user, password.unwrap_or_default()).await,
            };
            auth.map(|r| r.success()).map_err(|e| ssh_err(&where_, e))
        })?;
        if !accepted {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{LOGIN_FAILED}: {where_} refused the user name, password or key"),
            ));
        }
        let sftp = block(CONNECT_TIMEOUT, &where_, async {
            let channel = handle.channel_open_session().await.map_err(|e| ssh_err(&where_, e))?;
            channel.request_subsystem(true, "sftp").await.map_err(|e| ssh_err(&where_, e))?;
            SftpSession::new(channel.into_stream()).await.map_err(|e| sftp_err(&where_, e))
        })?;
        sftp.set_timeout(IO_TIMEOUT.as_secs());
        Ok(Sftp { handle: Some(handle), sftp })
    }

    fn item(name: String, attrs: &FileAttributes) -> Item {
        Item {
            dir: attrs.is_dir(),
            link: false,
            size: attrs.size.unwrap_or(0),
            modified: attrs.mtime.map(|t| SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(t))),
            name,
        }
    }
}

/// A blocking reader/writer over an async remote file.
struct Blocking<'a, T>(&'a mut T);

impl<T: AsyncRead + Unpin> Read for Blocking<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        block(IO_TIMEOUT, "read", self.0.read(buf))
    }
}

impl<T: AsyncWrite + Unpin> Write for Blocking<'_, T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        block(IO_TIMEOUT, "write", self.0.write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        block(IO_TIMEOUT, "write", self.0.flush())
    }
}

impl Session for Sftp {
    fn list(&mut self, dir: &str) -> io::Result<Vec<Item>> {
        let sftp = &self.sftp;
        block(IO_TIMEOUT, dir, async {
            let entries = sftp.read_dir(dir).await.map_err(|e| sftp_err(dir, e))?;
            let mut items = Vec::new();
            for entry in entries {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let mut attrs = entry.metadata();
                if attrs.is_symlink() {
                    // A link to a folder behaves like the folder.
                    let target = super::remote_join(dir, &name);
                    if let Ok(followed) = sftp.metadata(target).await {
                        attrs = followed;
                    }
                }
                items.push(Self::item(name, &attrs));
            }
            Ok(items)
        })
    }

    fn stat(&mut self, path: &str) -> io::Result<Option<Item>> {
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let sftp = &self.sftp;
        block(IO_TIMEOUT, path, async {
            match sftp.metadata(path).await {
                Ok(attrs) => Ok(Some(Self::item(name, &attrs))),
                Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => Ok(None),
                Err(e) => Err(sftp_err(path, e)),
            }
        })
    }

    fn read(&mut self, path: &str, each: &mut dyn FnMut(&mut dyn Read) -> io::Result<bool>) -> io::Result<bool> {
        let sftp = &self.sftp;
        let mut file = block(IO_TIMEOUT, path, async { sftp.open(path).await.map_err(|e| sftp_err(path, e)) })?;
        let result = each(&mut Blocking(&mut file));
        let _ = block(IO_TIMEOUT, path, async { file.shutdown().await });
        result
    }

    fn write(
        &mut self,
        path: &str,
        modified: Option<SystemTime>,
        fill: &mut dyn FnMut(&mut dyn Write) -> io::Result<bool>,
    ) -> io::Result<bool> {
        let sftp = &self.sftp;
        let mut file = block(IO_TIMEOUT, path, async { sftp.create(path).await.map_err(|e| sftp_err(path, e)) })?;
        let result = fill(&mut Blocking(&mut file));
        let closed = block(IO_TIMEOUT, path, async { file.shutdown().await });
        match (result, closed) {
            (Ok(true), Ok(())) => {
                if let Some(secs) = modified.and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok()) {
                    let secs = u32::try_from(secs.as_secs()).unwrap_or(u32::MAX);
                    let attrs = FileAttributes { atime: Some(secs), mtime: Some(secs), ..FileAttributes::empty() };
                    let _ = block(IO_TIMEOUT, path, async { sftp.set_metadata(path, attrs).await.map_err(|e| sftp_err(path, e)) });
                }
                Ok(true)
            }
            (result, closed) => {
                let _ = block(IO_TIMEOUT, path, async { sftp.remove_file(path).await.map_err(|e| sftp_err(path, e)) });
                result.and(closed.map(|_| false))
            }
        }
    }

    fn make_dir(&mut self, path: &str) -> io::Result<()> {
        let sftp = &self.sftp;
        block(IO_TIMEOUT, path, async { sftp.create_dir(path).await.map_err(|e| sftp_err(path, e)) })
    }

    fn remove_file(&mut self, path: &str) -> io::Result<()> {
        let sftp = &self.sftp;
        block(IO_TIMEOUT, path, async { sftp.remove_file(path).await.map_err(|e| sftp_err(path, e)) })
    }

    fn remove_dir(&mut self, path: &str) -> io::Result<()> {
        let sftp = &self.sftp;
        block(IO_TIMEOUT, path, async { sftp.remove_dir(path).await.map_err(|e| sftp_err(path, e)) })
    }

    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        let sftp = &self.sftp;
        block(IO_TIMEOUT, from, async { sftp.rename(from, to).await.map_err(|e| sftp_err(from, e)) })
    }

    fn home(&mut self) -> io::Result<String> {
        let sftp = &self.sftp;
        block(IO_TIMEOUT, ".", async { sftp.canonicalize(".").await.map_err(|e| sftp_err(".", e)) })
    }

    fn free_space(&mut self, path: &str) -> io::Result<Option<(u64, u64)>> {
        let sftp = &self.sftp;
        let info = block(IO_TIMEOUT, path, async { sftp.fs_info(path).await.map_err(|e| sftp_err(path, e)) })?;
        // `blocks_avail` is what an unprivileged user can use (like `df`).
        Ok(info.map(|v| (v.blocks_avail.saturating_mul(v.fragment_size), v.blocks.saturating_mul(v.fragment_size))))
    }
}

impl Drop for Sftp {
    fn drop(&mut self) {
        // Not waited for: dropping may happen anywhere.
        if let Some(handle) = self.handle.take() {
            runtime().spawn(async move {
                let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
            });
        }
    }
}
