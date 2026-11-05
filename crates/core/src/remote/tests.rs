//! Remote file systems against servers started in-process on 127.0.0.1:
//! FTP from `libunftp` + `unftp-sbe-fs`, SFTP from `russh` +
//! `russh-sftp`'s server side with a small handler over a temp folder.
//! Nothing here connects anywhere else.

use super::*;
use crate::ops::{self, Conflict, Control, Job, Outcome, Progress, Reporter, Resolution, Transfer};
use std::fs::{self, File};
use std::net::TcpListener;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("rc-remote-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The settings folder (known_hosts) shared by all tests of this process. Like
/// [`TempDir`] it cleans up: the guard deletes the folder once the last test
/// that asked for it is done (its `Drop` runs even when a test panics). The
/// count and the create/delete share one lock: with a plain counter, a test
/// starting while the last one was deleting the folder lost its known_hosts.
static SETTINGS_USERS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);

struct Settings(PathBuf);

impl Drop for Settings {
    fn drop(&mut self) {
        let mut users = SETTINGS_USERS.lock().unwrap_or_else(|e| e.into_inner());
        *users -= 1;
        if *users == 0 {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn settings() -> Settings {
    let mut users = SETTINGS_USERS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("rc-remote-settings-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    set_settings_dir(dir.clone());
    *users += 1;
    Settings(dir)
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Runs `server` on a runtime of its own on a background thread (for the
/// rest of the test process).
fn serve<F: std::future::Future<Output = ()> + Send + 'static>(server: impl FnOnce() -> F + Send + 'static) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.block_on(server());
    });
}

struct Answer {
    answer: Resolution,
    asked: usize,
}

impl Reporter for Answer {
    fn progress(&mut self, _: &Progress) {}
    fn conflict(&mut self, _: Conflict) -> Resolution {
        self.asked += 1;
        self.answer
    }
}

fn run_job(job: Job, answer: Resolution) -> (Outcome, usize) {
    let mut reporter = Answer { answer, asked: 0 };
    let outcome = ops::run(&job, &Control::default(), &mut reporter);
    (outcome, reporter.asked)
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        crate::vfs::list(dir, true).unwrap().into_iter().filter(|e| !e.is_parent()).map(|e| e.name).collect();
    names.sort();
    names
}

/// Browses, uploads, downloads, renames, deletes on a connected server
/// whose files live in `disk` (to check behind its back), starting at
/// `root` (the server path of `disk`).
fn exercise(root: &Path, disk: &Path) {
    let local = TempDir::new(&format!("local-{}", disk.file_name().unwrap().to_string_lossy()));
    fs::create_dir_all(disk.join("docs")).unwrap();
    fs::write(disk.join("docs/readme.txt"), b"hello").unwrap();
    fs::write(disk.join(".hidden"), b"h").unwrap();

    // Listing: `..` only below the root, hidden flagged.
    let entries = crate::vfs::list(root, true).unwrap();
    assert_eq!(entries.iter().any(|e| e.is_parent()), split(root).unwrap().1 != "/");
    assert!(entries.iter().any(|e| e.name == ".hidden" && e.hidden));
    assert!(!crate::vfs::list(root, false).unwrap().iter().any(|e| e.name == ".hidden"));
    let docs = root.join("docs");
    let listed = crate::vfs::list(&docs, true).unwrap();
    let readme = listed.iter().find(|e| e.name == "readme.txt").unwrap();
    assert_eq!(readme.size, 5);
    assert!(listed[0].is_parent() && listed[0].path == *root);
    assert!(crate::vfs::is_virtual(&docs) && crate::vfs::in_archive(&docs));

    // Upload a tree (copy), with a big file.
    let src = local.0.join("up");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), b"aaa").unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(src.join("sub/big.bin"), &big).unwrap();
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    File::options().write(true).open(src.join("a.txt")).unwrap().set_modified(old).unwrap();
    let transfers = ops::plan_transfer(std::slice::from_ref(&src), &root.join("")).unwrap();
    assert_eq!(transfers[0].dst, root.join("up"));
    let (outcome, asked) = run_job(Job::Copy(transfers.clone()), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(asked, 0);
    assert_eq!(fs::read(disk.join("up/sub/big.bin")).unwrap(), big);
    assert_eq!(fs::read(disk.join("up/a.txt")).unwrap(), b"aaa");
    assert_eq!(names(&root.join("up")), ["a.txt", "sub"]);
    // Dates are kept where the server allows (SFTP setstat; FTP MFMT).
    let stat = crate::vfs::open(root).unwrap().stat(&root.join("up").join("a.txt")).unwrap().unwrap();
    if disk.to_string_lossy().contains("sftp") {
        assert_eq!(SystemTime::from(stat.modified.unwrap()), old);
    }

    // Again: every file conflicts; skipped, then overwritten.
    let (outcome, asked) = run_job(Job::Copy(transfers.clone()), Resolution::SkipAll);
    assert_eq!((asked, outcome.skipped), (1, 2));
    fs::write(src.join("a.txt"), b"new").unwrap();
    let (outcome, asked) = run_job(Job::Copy(transfers), Resolution::Overwrite);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(asked, 2);
    assert_eq!(fs::read(disk.join("up/a.txt")).unwrap(), b"new");

    // Download (copy out), then view-style temp extract.
    let down = local.0.join("down");
    fs::create_dir_all(&down).unwrap();
    let transfers = ops::plan_transfer(&[root.join("up")], &down).unwrap();
    let (outcome, _) = run_job(Job::Copy(transfers), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(fs::read(down.join("up/sub/big.bin")).unwrap(), big);
    let temp = crate::vfs::extract_temp(&docs.join("readme.txt")).unwrap();
    assert_eq!(fs::read(&temp).unwrap(), b"hello");
    // Only this server's temp folder (and its marker): its parent is the
    // temp root that parallel tests (nested archives) stage in.
    let key = split(root).unwrap().0;
    let server_temp = crate::vfs::temp_path(&path_of(&key, "/x")).parent().unwrap().to_path_buf();
    let _ = fs::remove_dir_all(&server_temp);
    let mut marker = server_temp.into_os_string();
    marker.push(".used");
    let _ = fs::remove_file(marker);

    // New folder (with parents), rename, move on the server.
    let (outcome, _) = run_job(Job::MakeDir(root.join("new").join("deeper")), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(disk.join("new/deeper").is_dir());
    let (outcome, _) = run_job(Job::MakeDir(root.join("new")), Resolution::Skip);
    assert!(outcome.errors[0].1.contains("already exists"));
    let (outcome, _) = run_job(Job::Rename { from: root.join("new"), to: root.join("renamed") }, Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(disk.join("renamed/deeper").is_dir());
    let moved = vec![Transfer { src: root.join("renamed"), dst: root.join("up").join("renamed") }];
    let (outcome, _) = run_job(Job::Move(moved), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(disk.join("up/renamed/deeper").is_dir());
    // (Copies on and between servers: `copy_between_servers`, which alone
    // makes relay temp folders, so it can check none is left.)

    // Move up (local source removed), move out (server copy removed).
    let mover = local.0.join("mover.txt");
    fs::write(&mover, b"m").unwrap();
    let transfers = ops::plan_transfer(std::slice::from_ref(&mover), &root.join("docs")).unwrap();
    let (outcome, _) = run_job(Job::Move(transfers), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(!mover.exists() && disk.join("docs/mover.txt").is_file());
    let transfers = ops::plan_transfer(&[root.join("docs").join("mover.txt")], &local.0).unwrap();
    let (outcome, _) = run_job(Job::Move(transfers), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(mover.is_file() && !disk.join("docs/mover.txt").exists());

    // Delete a tree.
    let (outcome, _) = run_job(Job::Delete { paths: vec![root.join("up")], to_trash: false }, Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(!disk.join("up").exists());
    assert_eq!(names(root), [".hidden", "docs"]);

    // Cancelled upload: no partial file left.
    let cancel = Control::default();
    cancel.cancel();
    let transfers = ops::plan_transfer(&[src.join("sub")], &root.join("docs")).unwrap();
    let outcome = ops::run(&Job::Copy(transfers), &cancel, &mut Answer { answer: Resolution::Skip, asked: 0 });
    assert!(outcome.cancelled);
    assert!(!disk.join("docs/sub/big.bin").exists());

    // Missing paths.
    let err = crate::vfs::list(&root.join("nope"), true).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

// ---- FTP -------------------------------------------------------------------------

#[test]
fn ftp_against_local_server() {
    let disk = TempDir::new("ftp-server");
    let port = free_port();
    let root = disk.0.clone();
    serve(move || async move {
        let server = libunftp::ServerBuilder::new(Box::new(move || unftp_sbe_fs::Filesystem::new(root.clone()).unwrap()))
            .passive_ports(41000..=41999)
            .build()
            .unwrap();
        let _ = server.listen(format!("127.0.0.1:{port}")).await;
    });
    let login = Login {
        protocol: Protocol::Ftp,
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        password: Some("secret".into()),
        key_file: None,
    };
    let mut path = Err(io::Error::other("not tried"));
    for _ in 0..50 {
        path = connect(login.clone(), "");
        if path.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let root = path.unwrap();
    assert_eq!(display(&root), format!("ftp://tester@127.0.0.1:{port}/"));
    assert!(is_connected(&root));
    exercise(&root, &disk.0);
    // FTP has no way to ask for free space.
    assert_eq!(free_space(&root), None);

    // Disconnected: listings fail; reconnecting starts in the given folder.
    assert!(disconnect(&root));
    assert!(!is_connected(&root));
    let err = crate::vfs::list(&root, true).unwrap_err();
    assert!(err.to_string().contains("Not connected"), "{err}");
    let root = connect(login, "/docs").unwrap();
    assert_eq!(display(&root), format!("ftp://tester@127.0.0.1:{port}/docs"));
    assert_eq!(names(&root), ["readme.txt"]);
    disconnect(&root);
}

#[test]
fn ftp_connection_refused_fails_fast() {
    let login = Login {
        protocol: Protocol::Ftp,
        host: "127.0.0.1".into(),
        port: free_port(),
        user: String::new(),
        password: None,
        key_file: None,
    };
    let started = std::time::Instant::now();
    assert!(connect(login, "").is_err());
    assert!(started.elapsed() < CONNECT_TIMEOUT);
}

// ---- SFTP ------------------------------------------------------------------------

mod sftp_server {
    use russh::server::{Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use russh_sftp::extensions::{self, Statvfs};
    use russh_sftp::protocol::{
        Attrs, Data, ExtendedReply, File as SftpFile, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode,
        Version,
    };
    use std::collections::HashMap;
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    pub const USER: &str = "tester";
    pub const PASSWORD: &str = "secret";
    /// A file of this name in the served folder turns the statvfs extension off.
    pub const NO_STATVFS: &str = ".no-statvfs";
    /// With a file of this name in the served folder, reads and writes of a
    /// file start at its beginning whatever offset the client asks for first
    /// (like an FTP server that ignores `REST`).
    pub const IGNORE_OFFSET: &str = ".ignore-offset";
    /// A file of this name holding a number makes reads and writes at or past
    /// that offset fail (a transfer that breaks off).
    pub const FAIL_AT: &str = ".fail-at";

    pub fn host_key(seed: u8) -> russh::keys::PrivateKey {
        let pair = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]);
        russh::keys::PrivateKey::from(pair)
    }

    /// The only public key the server accepts (for `USER`).
    pub fn user_key() -> russh::keys::PrivateKey {
        host_key(77)
    }

    pub struct Ssh {
        pub root: PathBuf,
        channels: HashMap<ChannelId, Channel<Msg>>,
    }

    impl russh::server::Handler for Ssh {
        type Error = russh::Error;

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(if user == USER && password == PASSWORD { Auth::Accept } else { Auth::reject() })
        }

        async fn auth_publickey(
            &mut self,
            user: &str,
            key: russh::server::VerifiedPublicKey<'_>,
        ) -> Result<Auth, Self::Error> {
            Ok(if user == USER && *key.public_key == *user_key().public_key() { Auth::Accept } else { Auth::reject() })
        }

        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            reply: russh::server::ChannelOpenHandle,
            _: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn channel_eof(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
            session.close(channel)?;
            Ok(())
        }

        async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut Session)
            -> Result<(), Self::Error> {
            match self.channels.remove(&id).filter(|_| name == "sftp") {
                Some(channel) => {
                    session.channel_success(id)?;
                    let sftp = Sftp {
                        root: self.root.clone(),
                        files: HashMap::new(),
                        skew: HashMap::new(),
                        dirs: HashMap::new(),
                        next: 0,
                    };
                    russh_sftp::server::run(channel.into_stream(), sftp).await;
                }
                None => session.channel_failure(id)?,
            }
            Ok(())
        }
    }

    /// Serves `root` as `/`.
    struct Sftp {
        root: PathBuf,
        files: HashMap<String, File>,
        /// With [`IGNORE_OFFSET`]: per open file, the offset of its first read
        /// or write, taken off all of them.
        skew: HashMap<String, u64>,
        /// Open folders: Some(path) until read once.
        dirs: HashMap<String, Option<String>>,
        next: u32,
    }

    fn status(id: u32) -> Status {
        Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
    }

    fn code(e: std::io::Error) -> StatusCode {
        match e.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        }
    }

    impl Sftp {
        fn disk(&self, path: &str) -> PathBuf {
            let mut out = self.root.clone();
            for part in path.split('/').filter(|p| !p.is_empty() && *p != ".") {
                if part == ".." {
                    if out != self.root {
                        out.pop();
                    }
                } else {
                    out.push(part);
                }
            }
            out
        }

        fn handle(&mut self) -> String {
            self.next += 1;
            format!("h{}", self.next)
        }

        /// Where a read or write at `offset` of `handle` really goes (see
        /// [`IGNORE_OFFSET`]); fails at [`FAIL_AT`].
        fn position(&mut self, handle: &str, offset: u64) -> Result<u64, StatusCode> {
            let fail_at = fs::read_to_string(self.root.join(FAIL_AT)).ok().and_then(|t| t.trim().parse::<u64>().ok());
            if fail_at.is_some_and(|at| offset >= at) {
                return Err(StatusCode::Failure);
            }
            if !self.root.join(IGNORE_OFFSET).exists() {
                return Ok(offset);
            }
            let skew = *self.skew.entry(handle.to_owned()).or_insert(offset);
            Ok(offset.saturating_sub(skew))
        }
    }

    impl russh_sftp::server::Handler for Sftp {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
            let mut version = Version::new();
            if !self.root.join(NO_STATVFS).exists() {
                version.extensions.insert(extensions::STATVFS.into(), "2".into());
            }
            Ok(version)
        }

        async fn extended(&mut self, id: u32, request: String, _: Vec<u8>) -> Result<Packet, Self::Error> {
            if request != extensions::STATVFS {
                return Err(StatusCode::OpUnsupported);
            }
            let info = Statvfs {
                block_size: 4096,
                fragment_size: 4096,
                blocks: 1000,
                blocks_free: 300,
                blocks_avail: 250,
                inodes: 0,
                inodes_free: 0,
                inodes_avail: 0,
                fs_id: 1,
                flags: 0,
                name_max: 255,
            };
            let data = russh_sftp::ser::to_bytes(&info).map_err(|_| StatusCode::Failure)?.to_vec();
            Ok(Packet::ExtendedReply(ExtendedReply { id, data }))
        }

        async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
            let disk = self.disk(&path);
            let rel = disk.strip_prefix(&self.root).unwrap().to_string_lossy().replace('\\', "/");
            Ok(Name { id, files: vec![SftpFile::dummy(format!("/{rel}"))] })
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
            if !self.disk(&path).is_dir() {
                return Err(StatusCode::NoSuchFile);
            }
            let handle = self.handle();
            self.dirs.insert(handle.clone(), Some(path));
            Ok(Handle { id, handle })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
            let Some(path) = self.dirs.get_mut(&handle).and_then(Option::take) else { return Err(StatusCode::Eof) };
            let mut files = Vec::new();
            for item in fs::read_dir(self.disk(&path)).map_err(code)?.flatten() {
                let meta = item.metadata().map_err(code)?;
                files.push(SftpFile::new(item.file_name().to_string_lossy(), FileAttributes::from(&meta)));
            }
            Ok(Name { id, files })
        }

        async fn open(&mut self, id: u32, path: String, flags: OpenFlags, _: FileAttributes)
            -> Result<Handle, Self::Error> {
            let file = File::options()
                .read(flags.contains(OpenFlags::READ))
                .write(flags.contains(OpenFlags::WRITE))
                .create(flags.contains(OpenFlags::CREATE))
                .truncate(flags.contains(OpenFlags::TRUNCATE))
                .open(self.disk(&path))
                .map_err(code)?;
            let handle = self.handle();
            self.files.insert(handle.clone(), file);
            Ok(Handle { id, handle })
        }

        async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
            let offset = self.position(&handle, offset)?;
            let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
            file.seek(SeekFrom::Start(offset)).map_err(code)?;
            let mut data = vec![0; len as usize];
            let n = file.read(&mut data).map_err(code)?;
            if n == 0 {
                return Err(StatusCode::Eof);
            }
            data.truncate(n);
            Ok(Data { id, data })
        }

        async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
            let offset = self.position(&handle, offset)?;
            let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
            file.seek(SeekFrom::Start(offset)).map_err(code)?;
            file.write_all(&data).map_err(code)?;
            Ok(status(id))
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
            self.files.remove(&handle);
            self.skew.remove(&handle);
            self.dirs.remove(&handle);
            Ok(status(id))
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            let meta = fs::metadata(self.disk(&path)).map_err(code)?;
            Ok(Attrs { id, attrs: FileAttributes::from(&meta) })
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.stat(id, path).await
        }

        async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
            if let Some(mtime) = attrs.mtime {
                let file = File::options().write(true).open(self.disk(&path)).map_err(code)?;
                file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(mtime))).map_err(code)?;
            }
            Ok(status(id))
        }

        async fn mkdir(&mut self, id: u32, path: String, _: FileAttributes) -> Result<Status, Self::Error> {
            fs::create_dir(self.disk(&path)).map_err(code)?;
            Ok(status(id))
        }

        async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
            fs::remove_dir(self.disk(&path)).map_err(code)?;
            Ok(status(id))
        }

        async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
            fs::remove_file(self.disk(&path)).map_err(code)?;
            Ok(status(id))
        }

        async fn rename(&mut self, id: u32, from: String, to: String) -> Result<Status, Self::Error> {
            if self.disk(&to).exists() {
                return Err(StatusCode::Failure);
            }
            fs::rename(self.disk(&from), self.disk(&to)).map_err(code)?;
            Ok(status(id))
        }
    }

    /// Serves `root` on `listener` with the host key from `seed`.
    pub async fn run(root: PathBuf, listener: std::net::TcpListener, seed: u8, preferred: russh::Preferred) {
        let config = Arc::new(russh::server::Config {
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![host_key(seed)],
            preferred,
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        loop {
            let Ok((socket, _)) = listener.accept().await else { return };
            let handler = Ssh { root: root.clone(), channels: HashMap::new() };
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(session) = russh::server::run_stream(config, socket, handler).await {
                    let _ = session.await;
                }
            });
        }
    }
}

fn start_sftp(root: PathBuf, seed: u8) -> u16 {
    start_sftp_with(root, seed, russh::Preferred::DEFAULT)
}

fn start_sftp_with(root: PathBuf, seed: u8, preferred: russh::Preferred) -> u16 {
    // Bound here and handed over: a port from `free_port` can be handed out
    // twice to tests running in parallel, and the second server's bind fails
    // (its test then talks to another test's server with another host key).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    serve(move || sftp_server::run(root, listener, seed, preferred));
    // Wait for the listener.
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    port
}

fn sftp_login(port: u16, password: &str) -> Login {
    Login {
        protocol: Protocol::Sftp,
        host: "127.0.0.1".into(),
        port,
        user: sftp_server::USER.into(),
        password: Some(password.into()),
        key_file: None,
    }
}

/// An old server (only SHA-1 key exchange, CBC ciphers and SHA-1 MACs, like OpenSSH 5) still connects.
#[test]
fn sftp_legacy_algorithms() {
    use russh::{cipher, kex, mac};
    let _settings = settings();
    let disk = TempDir::new("sftp-legacy");
    fs::write(disk.0.join("a.txt"), b"old").unwrap();
    let old = russh::Preferred {
        kex: vec![kex::DH_G14_SHA1, kex::EXTENSION_SUPPORT_AS_SERVER].into(),
        cipher: vec![cipher::AES_128_CBC].into(),
        mac: vec![mac::HMAC_SHA1].into(),
        ..russh::Preferred::DEFAULT
    };
    let port = start_sftp_with(disk.0.clone(), 7, old);
    let login = sftp_login(port, sftp_server::PASSWORD);
    let e = connect(login.clone(), "").unwrap_err().to_string();
    assert!(e.contains(HOST_KEY_UNKNOWN), "{e}");
    trust_host_key(&login).unwrap();
    let root = connect(login, "").unwrap();
    let names: Vec<String> =
        crate::vfs::list(&root, true).unwrap().into_iter().filter(|e| !e.is_parent()).map(|e| e.name).collect();
    assert_eq!(names, ["a.txt"]);
}

#[test]
fn sftp_against_local_server() {
    let _settings = settings();
    let disk = TempDir::new("sftp-server");
    let port = start_sftp(disk.0.clone(), 1);
    let login = sftp_login(port, sftp_server::PASSWORD);

    // First contact: the host key must be trusted first.
    let e = connect(login.clone(), "").unwrap_err().to_string();
    assert!(e.contains(HOST_KEY_UNKNOWN) && e.contains("SHA256:"), "{e}");
    trust_host_key(&login).unwrap();

    let e = connect(sftp_login(port, "wrong"), "").unwrap_err().to_string();
    assert!(e.contains(LOGIN_FAILED), "{e}");

    let root = connect(login.clone(), "").unwrap();
    assert_eq!(display(&root), format!("sftp://tester@127.0.0.1:{port}/"));
    exercise(&root, &disk.0);
    // The test server answers statvfs with fixed numbers: 4096-byte blocks,
    // 1000 in all, 250 free for users (and 300 free for root).
    assert_eq!(free_space(&root), Some((250 * 4096, 1000 * 4096)));
    assert_eq!(free_space(&path_of("sftp://tester@127.0.0.1:1", "/")), None, "not connected");
    assert!(disconnect(&root));
}

#[test]
fn sftp_key_file_login() {
    use russh::keys::ssh_key::LineEnding;
    let _settings = settings();
    let disk = TempDir::new("sftp-keys");
    let keys = TempDir::new("sftp-keyfiles");
    fs::write(disk.0.join("hello.txt"), "hi").unwrap();
    let port = start_sftp(disk.0.clone(), 6);
    let mut login = sftp_login(port, "");
    login.password = None;
    connect(login.clone(), "").unwrap_err();
    trust_host_key(&login).unwrap();

    let good = sftp_server::user_key();
    let plain = keys.0.join("id_plain");
    fs::write(&plain, good.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    let wrong = keys.0.join("id_wrong");
    fs::write(&wrong, sftp_server::host_key(78).to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    let encrypted = keys.0.join("id_encrypted");
    let locked = good
        .encrypt_with(
            russh::keys::ssh_key::Cipher::Aes256Ctr,
            russh::keys::ssh_key::Kdf::Bcrypt { salt: vec![9; 16], rounds: 4 },
            7,
            "pass phrase",
        )
        .unwrap();
    fs::write(&encrypted, locked.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();

    // The right key logs in and lists files.
    login.key_file = Some(plain.clone());
    let root = connect(login.clone(), "").unwrap();
    assert!(crate::vfs::list(&root, true).unwrap().iter().any(|e| e.name == "hello.txt"));
    assert!(disconnect(&root));

    // A key the server doesn't know is refused.
    login.key_file = Some(wrong);
    let e = connect(login.clone(), "").unwrap_err().to_string();
    assert!(e.contains(LOGIN_FAILED), "{e}");

    // An encrypted key: right passphrase works, missing or wrong ones say "private key".
    login.key_file = Some(encrypted);
    login.password = Some("pass phrase".into());
    let root = connect(login.clone(), "").unwrap();
    assert!(disconnect(&root));
    for pass in [None, Some("nope".to_string())] {
        login.password = pass;
        let e = connect(login.clone(), "").unwrap_err().to_string();
        assert!(e.contains("private key"), "{e}");
    }

    // A key file that doesn't exist.
    login.key_file = Some(keys.0.join("missing"));
    let e = connect(login, "").unwrap_err().to_string();
    assert!(e.contains("private key") && e.contains("missing"), "{e}");
}

#[test]
fn sftp_without_statvfs_shows_no_free_space() {
    let _settings = settings();
    let disk = TempDir::new("sftp-nostatvfs");
    let port = start_sftp(disk.0.clone(), 5);
    let login = sftp_login(port, sftp_server::PASSWORD);
    // First contact fails but remembers the key to trust.
    connect(login.clone(), "").unwrap_err();
    trust_host_key(&login).unwrap();
    // This marker file makes the test server leave statvfs out of its extensions.
    fs::write(disk.0.join(sftp_server::NO_STATVFS), "").unwrap();
    let root = connect(login, "").unwrap();
    assert_eq!(free_space(&root), None);
    assert!(disconnect(&root));
}

#[test]
fn sftp_refuses_a_changed_host_key() {
    let settings = settings();
    let disk = TempDir::new("sftp-changed");
    let port = start_sftp(disk.0.clone(), 2);
    // Trust another key for this server.
    let other = sftp_server::host_key(3).public_key().to_openssh().unwrap();
    let mut known = fs::OpenOptions::new().create(true).append(true).open(settings.0.join("known_hosts")).unwrap();
    // One write (see `trust_host_key`): `writeln!` writes in pieces that
    // parallel tests' appends interleave into broken lines.
    known.write_all(format!("127.0.0.1:{port} {other}\n").as_bytes()).unwrap();
    let e = connect(sftp_login(port, sftp_server::PASSWORD), "").unwrap_err().to_string();
    assert!(e.contains(HOST_KEY_CHANGED), "{e}");
    assert!(trust_host_key(&sftp_login(port, sftp_server::PASSWORD)).is_err());
}

// ---- FTPS ------------------------------------------------------------------------

/// A server certificate for 127.0.0.1 as PEM files `<name>.crt` / `.key` in
/// `dir`: self-signed, or signed by `ca`; made out for `name_in_cert`.
fn write_cert(
    dir: &Path,
    name: &str,
    name_in_cert: &str,
    ca: Option<&rcgen::Issuer<'_, rcgen::KeyPair>>,
) -> (PathBuf, PathBuf, Vec<u8>) {
    let mut params = rcgen::CertificateParams::new(vec![name_in_cert.to_owned()]).unwrap();
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = match ca {
        Some(ca) => params.signed_by(&key, ca).unwrap(),
        None => params.self_signed(&key).unwrap(),
    };
    let (crt, key_file) = (dir.join(format!("{name}.crt")), dir.join(format!("{name}.key")));
    fs::write(&crt, cert.pem()).unwrap();
    fs::write(&key_file, key.serialize_pem()).unwrap();
    (crt, key_file, cert.der().to_vec())
}

/// The certificate (and key) files an FTPS test server uses for new control
/// connections; replaced to make the server show another certificate.
type CertFiles = Arc<Mutex<(PathBuf, PathBuf)>>;

/// Starts an FTPS server over `root` (TLS required for logins and data) on
/// a listener bound here (see [`start_sftp`]); returns its port.
fn start_ftps(root: PathBuf, cert: CertFiles) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    start_ftps_on(listener, root, cert);
    port
}

/// Serves FTPS over `root` on `listener` (non-blocking).
fn start_ftps_on(listener: TcpListener, root: PathBuf, cert: CertFiles) {
    serve(move || async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        loop {
            let Ok((socket, _)) = listener.accept().await else { return };
            let (crt, key) = cert.lock().unwrap().clone();
            let root = root.clone();
            let server =
                libunftp::ServerBuilder::new(Box::new(move || unftp_sbe_fs::Filesystem::new(root.clone()).unwrap()))
                    .passive_ports(43000..=43999)
                    .ftps(crt, key)
                    .ftps_required(true, true)
                    .build()
                    .unwrap();
            tokio::spawn(async move {
                let _ = server.service(socket).await;
            });
        }
    });
}

fn ftps_login(port: u16) -> Login {
    Login {
        protocol: Protocol::Ftps,
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        password: Some("secret".into()),
        key_file: None,
    }
}

/// The lines of `known_hosts` for `127.0.0.1:port`.
fn known_lines(settings: &Settings, port: u16) -> Vec<String> {
    let text = fs::read_to_string(settings.0.join("known_hosts")).unwrap_or_default();
    let prefix = format!("127.0.0.1:{port} ");
    text.lines().filter(|l| l.starts_with(&prefix)).map(str::to_owned).collect()
}

fn sha256_hex(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn ftps_self_signed_certificate_is_pinned() {
    let settings = settings();
    let disk = TempDir::new("ftps-server");
    let certs = TempDir::new("ftps-certs");
    let (crt, key, der) = write_cert(&certs.0, "first", "127.0.0.1", None);
    let files: CertFiles = Arc::new(Mutex::new((crt, key)));
    let port = start_ftps(disk.0.clone(), files.clone());
    let login = ftps_login(port);

    // First contact: refused, with the reason and the fingerprint.
    let e = connect(login.clone(), "").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
    let e = e.to_string();
    let fingerprint = cert_fingerprint(ring::digest::digest(&ring::digest::SHA256, &der).as_ref());
    assert!(e.contains(CERT_UNTRUSTED) && e.contains(&format!("SHA-256: {fingerprint}")), "{e}");
    assert!(e.contains("self-signed"), "{e}");
    assert!(known_lines(&settings, port).is_empty());
    // Not an SFTP host key to trust.
    let mut sftp = login.clone();
    sftp.protocol = Protocol::Sftp;
    assert!(trust_host_key(&sftp).is_err());

    // Pinned: browse, upload and download over TLS (control and data).
    trust_host_key(&login).unwrap();
    assert_eq!(known_lines(&settings, port), [format!("127.0.0.1:{port} {CERT_PIN_TYPE} {}", sha256_hex(&der))]);
    let root = connect(login.clone(), "").unwrap();
    assert_eq!(display(&root), format!("ftps://tester@127.0.0.1:{port}/"));
    exercise(&root, &disk.0);
    assert!(disconnect(&root));

    // Another self-signed certificate on the same address: refused, never
    // pinned on its own, and the old offer can't be trusted again.
    let (crt, key, _) = write_cert(&certs.0, "second", "127.0.0.1", None);
    *files.lock().unwrap() = (crt, key);
    let e = connect(login.clone(), "").unwrap_err().to_string();
    assert!(e.contains(CERT_CHANGED), "{e}");
    assert!(trust_host_key(&login).is_err());
    assert_eq!(known_lines(&settings, port).len(), 1);
}

#[test]
fn ftps_pins_and_ssh_host_keys_share_known_hosts() {
    let settings = settings();
    let append = |line: String| {
        let mut out = fs::OpenOptions::new().create(true).append(true).open(settings.0.join("known_hosts")).unwrap();
        out.write_all(line.as_bytes()).unwrap();
    };

    // An SSH key line for an FTPS server's address doesn't make its
    // certificate "changed", nor count as a pin.
    let disk = TempDir::new("ftps-shared");
    let certs = TempDir::new("ftps-shared-certs");
    let (crt, key, _) = write_cert(&certs.0, "cert", "127.0.0.1", None);
    let port = start_ftps(disk.0.clone(), Arc::new(Mutex::new((crt, key))));
    let other = sftp_server::host_key(4).public_key().to_openssh().unwrap();
    append(format!("127.0.0.1:{port} {other}\n"));
    let e = connect(ftps_login(port), "").unwrap_err().to_string();
    assert!(e.contains(CERT_UNTRUSTED), "{e}");
    trust_host_key(&ftps_login(port)).unwrap();
    let root = connect(ftps_login(port), "").unwrap();
    assert!(names(&root).is_empty());
    disconnect(&root);
    assert_eq!(known_lines(&settings, port).len(), 2);

    // A certificate pin for an SFTP server's address doesn't make its host
    // key "changed"; its key is trusted next to the pin.
    let sftp_disk = TempDir::new("sftp-shared");
    let port = start_sftp(sftp_disk.0.clone(), 5);
    append(format!("127.0.0.1:{port} {CERT_PIN_TYPE} {}\n", "00".repeat(32)));
    let login = sftp_login(port, sftp_server::PASSWORD);
    let e = connect(login.clone(), "").unwrap_err().to_string();
    assert!(e.contains(HOST_KEY_UNKNOWN), "{e}");
    // Not an FTPS certificate to trust.
    assert!(trust_host_key(&ftps_login(port)).is_err());
    trust_host_key(&login).unwrap();
    let root = connect(login, "").unwrap();
    disconnect(&root);
    assert_eq!(known_lines(&settings, port).len(), 2);
}

#[test]
fn ftps_certificate_from_a_trusted_authority_needs_no_pin() {
    let settings = settings();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::DigitalSignature];
    params.distinguished_name.push(rcgen::DnType::CommonName, "RustCommander test CA");
    let ca = rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap()).unwrap();
    ftp::TEST_ROOTS.lock().unwrap().push(ca.der().clone());

    // Signed by the CA for this address: connects without asking.
    let disk = TempDir::new("ftps-ca");
    let certs = TempDir::new("ftps-ca-certs");
    let (crt, key, _) = write_cert(&certs.0, "good", "127.0.0.1", Some(&ca));
    let port = start_ftps(disk.0.clone(), Arc::new(Mutex::new((crt, key))));
    let root = connect(ftps_login(port), "").unwrap();
    assert!(names(&root).is_empty());
    disconnect(&root);
    assert!(known_lines(&settings, port).is_empty());

    // Signed by the CA but for another name: may be pinned.
    let (crt, key, _) = write_cert(&certs.0, "other", "files.example.com", Some(&ca));
    let port = start_ftps(disk.0.clone(), Arc::new(Mutex::new((crt, key))));
    let e = connect(ftps_login(port), "").unwrap_err().to_string();
    assert!(e.contains(CERT_UNTRUSTED) && e.contains("another name"), "{e}");
    trust_host_key(&ftps_login(port)).unwrap();
    let root = connect(ftps_login(port), "").unwrap();
    disconnect(&root);
}

// ---- Keep-alive ------------------------------------------------------------------

/// An FTP server over `disk` on `port` that stops (closing its sockets)
/// when the returned sender is used or dropped.
fn start_stoppable_ftp(disk: &Path, port: u16, passive: std::ops::RangeInclusive<u16>) -> std::sync::mpsc::Sender<()> {
    let (stop, wait) = std::sync::mpsc::channel::<()>();
    let root = disk.to_path_buf();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.spawn(async move {
            let server = libunftp::ServerBuilder::new(Box::new(move || unftp_sbe_fs::Filesystem::new(root.clone()).unwrap()))
                .passive_ports(passive)
                .build()
                .unwrap();
            let _ = server.listen(format!("127.0.0.1:{port}")).await;
        });
        let _ = wait.recv();
        rt.shutdown_background();
    });
    stop
}

fn connect_retrying(login: &Login) -> PathBuf {
    let mut path = Err(io::Error::other("not tried"));
    for _ in 0..50 {
        path = connect(login.clone(), "");
        if path.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    path.unwrap()
}

fn idle_count(conn: &Connection) -> usize {
    conn.idle.lock().unwrap().len()
}

#[test]
fn keep_alive_pings_idle_ftp_sessions() {
    let disk = TempDir::new("ftp-keepalive");
    std::fs::write(disk.0.join("a.txt"), "x").unwrap();
    let port = free_port();
    let _stop = start_stoppable_ftp(&disk.0, port, 43000..=43499);
    let login = Login {
        protocol: Protocol::Ftp,
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        password: Some("secret".into()),
        key_file: None,
    };
    let root = connect_retrying(&login);
    let conn = connection(&root).unwrap();
    assert_eq!(idle_count(&conn), 1);
    // Used a moment ago: left alone. Idle long enough: pinged, kept, usable.
    conn.ping_idle(Duration::from_secs(3600));
    assert_eq!(idle_count(&conn), 1);
    conn.ping_idle(Duration::ZERO);
    assert_eq!(idle_count(&conn), 1);
    assert_eq!(names(&root), ["a.txt"]);
    disconnect(&root);
    assert!(!is_connected(&root));
}

#[test]
fn keep_alive_drops_dead_sessions_and_next_use_reconnects() {
    let disk = TempDir::new("ftp-keepalive-dead");
    std::fs::write(disk.0.join("b.txt"), "x").unwrap();
    let port = free_port();
    let stop = start_stoppable_ftp(&disk.0, port, 43500..=43749);
    let login = Login {
        protocol: Protocol::Ftp,
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        password: Some("secret".into()),
        key_file: None,
    };
    let root = connect_retrying(&login);
    let conn = connection(&root).unwrap();
    assert_eq!(names(&root), ["b.txt"]);
    assert_eq!(idle_count(&conn), 1);

    // The server goes away: the ping finds the session dead and drops it.
    drop(stop);
    std::thread::sleep(Duration::from_millis(500));
    conn.ping_idle(Duration::ZERO);
    assert_eq!(idle_count(&conn), 0);

    // It comes back (same address); the next operation opens a new session.
    let _stop = start_stoppable_ftp(&disk.0, port, 43750..=43999);
    let mut listed = Err(io::Error::other("not tried"));
    for _ in 0..50 {
        listed = crate::vfs::list(&root, true);
        if listed.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(listed.unwrap().len(), 1);
    assert_eq!(idle_count(&conn), 1);
    disconnect(&root);
}

// ---- Between servers -------------------------------------------------------------

/// Serves plain FTP over `root` on `listener` (non-blocking), with data
/// connections on the ports `passive`.
fn start_ftp_on(listener: TcpListener, root: PathBuf, passive: std::ops::RangeInclusive<u16>) {
    serve(move || async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        loop {
            let Ok((socket, _)) = listener.accept().await else { return };
            let root = root.clone();
            let server =
                libunftp::ServerBuilder::new(Box::new(move || unftp_sbe_fs::Filesystem::new(root.clone()).unwrap()))
                    .passive_ports(passive.clone())
                    .build()
                    .unwrap();
            tokio::spawn(async move {
                let _ = server.service(socket).await;
            });
        }
    });
}

/// Whether a temp folder of a copy between servers is left over.
fn relay_left() -> bool {
    let prefix = format!("copy-relay-{}-", std::process::id());
    fs::read_dir(crate::vfs::temp_root()).into_iter().flatten().flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
}

/// Cancels the job once `when` holds for the progress of the file `name`.
/// Until then it takes 55 ms per report, so that every next report reaches
/// it (they are throttled to one per 50 ms).
struct CancelAt {
    control: Arc<Control>,
    name: &'static str,
    when: Box<When>,
}

type When = dyn Fn(&Progress) -> bool;

impl Reporter for CancelAt {
    fn progress(&mut self, p: &Progress) {
        let on_file = p.current.as_ref().is_some_and(|c| c.file_name().is_some_and(|n| n == self.name));
        if self.control.is_cancelled() {
            return;
        }
        if on_file && (self.when)(p) {
            self.control.cancel();
        } else {
            std::thread::sleep(Duration::from_millis(55));
        }
    }
    fn conflict(&mut self, _: Conflict) -> Resolution {
        Resolution::Cancel
    }
}

#[test]
fn copy_between_servers() {
    let _settings = settings();
    let ftp_disk = TempDir::new("relay-ftp");
    let sftp_disk = TempDir::new("relay-sftp");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    start_ftp_on(listener, ftp_disk.0.clone(), 44000..=44999);
    let ftp = connect_retrying(&Login {
        protocol: Protocol::Ftp,
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        password: Some("secret".into()),
        key_file: None,
    });
    let sftp_port = start_sftp(sftp_disk.0.clone(), 7);
    let login = sftp_login(sftp_port, sftp_server::PASSWORD);
    assert!(connect(login.clone(), "").is_err());
    trust_host_key(&login).unwrap();
    let sftp = connect(login, "").unwrap();

    // A tree on the FTP server: two files, a subfolder and an empty folder.
    let tree = ftp_disk.0.join("tree");
    fs::create_dir_all(tree.join("sub/empty")).unwrap();
    fs::write(tree.join("a.txt"), b"aaa").unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(tree.join("sub/big.bin"), &big).unwrap();
    let total = 3 + big.len() as u64;
    let check = |dir: &Path, a: &[u8]| {
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), a, "{}", dir.display());
        assert_eq!(fs::read(dir.join("sub/big.bin")).unwrap(), big, "{}", dir.display());
        assert!(dir.join("sub/empty").is_dir(), "{}", dir.display());
    };

    // FTP -> SFTP: everything arrives, the source stays, the temp folder goes,
    // and the bytes are counted once (the progress counts both halves).
    let transfers = ops::plan_transfer(&[ftp.join("tree")], &sftp.join("")).unwrap();
    let (outcome, asked) = run_job(Job::Copy(transfers.clone()), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!((asked, outcome.done_items, outcome.done_bytes), (0, 5, total));
    check(&sftp_disk.0.join("tree"), b"aaa");
    check(&tree, b"aaa");
    assert!(!relay_left());

    // Again: both files conflict; skipped, then overwritten.
    let (outcome, asked) = run_job(Job::Copy(transfers.clone()), Resolution::SkipAll);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!((asked, outcome.skipped), (1, 2));
    fs::write(tree.join("a.txt"), b"new").unwrap();
    let (outcome, asked) = run_job(Job::Copy(transfers.clone()), Resolution::Overwrite);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(asked, 2);
    check(&sftp_disk.0.join("tree"), b"new");
    assert!(!relay_left());

    // SFTP -> FTP under a new name.
    let transfers = ops::plan_transfer(&[sftp.join("tree")], &ftp.join("back")).unwrap();
    assert_eq!(transfers[0].dst, ftp.join("back"));
    let (outcome, _) = run_job(Job::Copy(transfers), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    check(&ftp_disk.0.join("back"), b"new");

    // Folder to folder on one server (both protocols); not into itself.
    for (root, disk) in [(&sftp, &sftp_disk.0), (&ftp, &ftp_disk.0)] {
        let transfers = ops::plan_transfer(&[root.join("tree")], &root.join("copy")).unwrap();
        let (outcome, _) = run_job(Job::Copy(transfers), Resolution::Skip);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        check(&disk.join("copy"), b"new");
        let e = ops::plan_transfer(&[root.join("tree")], &root.join("tree").join("sub").join("")).unwrap_err();
        assert!(e.contains("into itself"), "{e}");
        // A sibling whose name starts the same is not inside.
        assert!(ops::plan_transfer(&[root.join("tree")], &root.join("tree2")).is_ok());
    }
    assert!(!relay_left());

    // Move between servers: a skipped file keeps the whole source.
    let transfers = vec![Transfer { src: ftp.join("back"), dst: sftp.join("tree") }];
    let (outcome, asked) = run_job(Job::Move(transfers), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!((asked, outcome.skipped), (2, 2));
    check(&ftp_disk.0.join("back"), b"new");
    // Without conflicts the source goes once everything is across.
    let transfers = vec![Transfer { src: ftp.join("back"), dst: sftp.join("moved") }];
    let (outcome, _) = run_job(Job::Move(transfers), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.done_bytes, total);
    check(&sftp_disk.0.join("moved"), b"new");
    assert!(!ftp_disk.0.join("back").exists());
    // A move on one server is a rename.
    let transfers = vec![Transfer { src: sftp.join("moved"), dst: sftp.join("renamed") }];
    let (outcome, _) = run_job(Job::Move(transfers), Resolution::Skip);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    check(&sftp_disk.0.join("renamed"), b"new");
    assert!(!sftp_disk.0.join("moved").exists());
    assert!(!relay_left());

    // Cancelled while the big file is downloaded, then while it is uploaded
    // (by the bytes done so far): no temp folder, no part
    // of it on the server, not counted as done; a cancelled move keeps its
    // source.
    // a.txt down and up is 6 bytes, the big file's download half `big.len()`.
    let downloaded = 6 + big.len() as u64;
    let phases: [Box<When>; 2] =
        [Box::new(|p: &Progress| p.done_bytes > 6), Box::new(move |p: &Progress| p.done_bytes > downloaded)];
    for (i, when) in phases.into_iter().enumerate() {
        let dst = sftp.join(format!("cancelled{i}"));
        let job = Job::Move(vec![Transfer { src: ftp.join("tree"), dst }]);
        let control = Arc::new(Control::default());
        let mut reporter = CancelAt { control: control.clone(), name: "big.bin", when };
        let outcome = ops::run(&job, &control, &mut reporter);
        assert!(outcome.cancelled, "{i}: {outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(outcome.done_items < 5, "{}", outcome.done_items);
        let part = sftp_disk.0.join(format!("cancelled{i}/sub/big.bin"));
        assert!(!part.exists(), "{}", part.display());
        assert!(!relay_left());
        check(&tree, b"new");
    }
    disconnect(&ftp);
    disconnect(&sftp);
}

// ---- Resuming transfers ----------------------------------------------------------

/// Answers every question with `answer` and keeps the questions.
struct Asked {
    answer: Resolution,
    conflicts: Vec<Conflict>,
}

impl Reporter for Asked {
    fn progress(&mut self, _: &Progress) {}
    fn conflict(&mut self, c: Conflict) -> Resolution {
        self.conflicts.push(c);
        self.answer
    }
}

fn run_asked(job: Job, answer: Resolution) -> (Outcome, Vec<Conflict>) {
    let mut reporter = Asked { answer, conflicts: Vec::new() };
    let outcome = ops::run(&job, &Control::default(), &mut reporter);
    (outcome, reporter.conflicts)
}

/// 3 MB that differ from the other tests' data.
fn resume_data() -> Vec<u8> {
    (0..3_000_000u32).map(|i| (i.wrapping_mul(7) % 253) as u8).collect()
}

/// Downloads and uploads that resume from a shorter target, on a connected
/// server whose root `root` is the folder `disk`; local files go in `local`.
fn exercise_resume(root: &Path, disk: &Path, local: &Path) {
    let data = resume_data();
    let small: Vec<u8> = data[..20_000].to_vec();
    fs::create_dir_all(disk.join("res")).unwrap();
    fs::write(disk.join("res/big.bin"), &data).unwrap();
    fs::write(disk.join("res/b.bin"), &small).unwrap();
    let res = root.join("res");
    let down = local.join("down");
    fs::create_dir_all(&down).unwrap();
    let get = |files: &[&str]| {
        let sources: Vec<PathBuf> = files.iter().map(|f| res.join(f)).collect();
        Job::Copy(ops::plan_transfer(&sources, &down).unwrap())
    };

    // Download: the existing part stays, the rest is appended; the date is
    // set as for any copy.
    fs::write(down.join("big.bin"), &data[..1_234_567]).unwrap();
    let (outcome, asked) = run_asked(get(&["big.bin"]), Resolution::Resume);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(asked.len(), 1);
    assert!(asked[0].resumable);
    assert_eq!((asked[0].src_size, asked[0].dst_size), (data.len() as u64, 1_234_567));
    assert!(fs::read(down.join("big.bin")).unwrap() == data, "download resumed wrong");
    let listed = crate::vfs::open(root).unwrap().stat(&res.join("big.bin")).unwrap().unwrap();
    let local_date = fs::metadata(down.join("big.bin")).unwrap().modified().unwrap();
    assert_eq!(listed.modified.map(SystemTime::from), Some(local_date));

    // Not offered when the existing file is as large or larger: Resume
    // then skips, nothing changes.
    for len in [data.len(), data.len() + 10] {
        fs::write(down.join("big.bin"), vec![1u8; len]).unwrap();
        let (outcome, asked) = run_asked(get(&["big.bin"]), Resolution::Resume);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(asked.len() == 1 && !asked[0].resumable, "{len}");
        assert_eq!(outcome.skipped, 1);
        assert_eq!(fs::read(down.join("big.bin")).unwrap(), vec![1u8; len]);
    }

    // Resume all: asked once; a later file that can't resume is asked.
    fs::write(down.join("big.bin"), &data[..100]).unwrap();
    fs::write(down.join("b.bin"), &small[..5]).unwrap();
    let (outcome, asked) = run_asked(get(&["big.bin", "b.bin"]), Resolution::ResumeAll);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(asked.len(), 1);
    assert!(fs::read(down.join("big.bin")).unwrap() == data);
    assert_eq!(fs::read(down.join("b.bin")).unwrap(), small);
    fs::write(down.join("big.bin"), &data[..100]).unwrap();
    let (outcome, asked) = run_asked(get(&["big.bin", "b.bin"]), Resolution::ResumeAll);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(asked.iter().map(|c| c.resumable).collect::<Vec<_>>(), [true, false]);
    assert_eq!(outcome.skipped, 1);
    assert!(fs::read(down.join("big.bin")).unwrap() == data);

    // Upload: the same the other way.
    let src = local.join("up.bin");
    fs::write(&src, &data).unwrap();
    let put = || Job::Copy(ops::plan_transfer(std::slice::from_ref(&src), &res.join("")).unwrap());
    fs::write(disk.join("res/up.bin"), &data[..777_777]).unwrap();
    let (outcome, asked) = run_asked(put(), Resolution::Resume);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(asked.len() == 1 && asked[0].resumable);
    assert_eq!(asked[0].dst_size, 777_777);
    assert!(fs::read(disk.join("res/up.bin")).unwrap() == data, "upload resumed wrong");
    // As large: not offered, skipped.
    let (outcome, asked) = run_asked(put(), Resolution::Resume);
    assert!(asked.len() == 1 && !asked[0].resumable);
    assert_eq!(outcome.skipped, 1);
    assert!(fs::read(disk.join("res/up.bin")).unwrap() == data);
    // A copy between folders of the server (through a temp file) can't resume.
    fs::write(disk.join("res/copy.bin"), &data[..10]).unwrap();
    let relay = Job::Copy(vec![Transfer { src: res.join("big.bin"), dst: res.join("copy.bin") }]);
    let (outcome, asked) = run_asked(relay, Resolution::Resume);
    assert!(asked.len() == 1 && !asked[0].resumable);
    assert_eq!(outcome.skipped, 1);
    assert_eq!(fs::read(disk.join("res/copy.bin")).unwrap(), &data[..10]);
}

#[test]
fn ftp_resumes_downloads_and_uploads() {
    let disk = TempDir::new("resume-ftp");
    let local = TempDir::new("resume-ftp-local");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    start_ftp_on(listener, disk.0.clone(), 45000..=45999);
    let root = connect_retrying(&Login {
        protocol: Protocol::Ftp,
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        password: Some("secret".into()),
        key_file: None,
    });
    exercise_resume(&root, &disk.0, &local.0);
    disconnect(&root);
}

#[test]
fn sftp_resumes_downloads_and_uploads() {
    let _settings = settings();
    let disk = TempDir::new("resume-sftp");
    let local = TempDir::new("resume-sftp-local");
    let port = start_sftp(disk.0.clone(), 9);
    let login = sftp_login(port, sftp_server::PASSWORD);
    assert!(connect(login.clone(), "").is_err());
    trust_host_key(&login).unwrap();
    let root = connect(login, "").unwrap();
    exercise_resume(&root, &disk.0, &local.0);

    let data = resume_data();
    let res = root.join("res");
    let down = local.0.join("down");
    let get = || Job::Copy(ops::plan_transfer(&[res.join("big.bin")], &down).unwrap());
    let src = local.0.join("up.bin");
    let put = || Job::Copy(ops::plan_transfer(std::slice::from_ref(&src), &res.join("")).unwrap());
    let flag = |name: &str, text: Option<&str>| match text {
        Some(t) => fs::write(disk.0.join(name), t).unwrap(),
        None => fs::remove_file(disk.0.join(name)).unwrap(),
    };

    // A server that ignores the offset sends (or takes) the whole file
    // again: the size check reports the damaged result.
    flag(sftp_server::IGNORE_OFFSET, Some(""));
    fs::write(down.join("big.bin"), &data[..1_000_000]).unwrap();
    let (outcome, _) = run_asked(get(), Resolution::Resume);
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
    assert!(outcome.errors[0].1.contains("After resuming"), "{:?}", outcome.errors);
    fs::write(disk.0.join("res/up.bin"), &data[..1_000_000]).unwrap();
    let (outcome, _) = run_asked(put(), Resolution::Resume);
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
    assert!(outcome.errors[0].1.contains("After resuming"), "{:?}", outcome.errors);
    flag(sftp_server::IGNORE_OFFSET, None);

    // A download that breaks off past 1 MB keeps its part (and says so);
    // copying again resumes it.
    let part = |path: &Path| {
        let got = fs::read(path).unwrap();
        assert!(got.len() as u64 >= KEEP_PARTIAL && got.len() < data.len(), "{}", got.len());
        assert!(data.starts_with(&got));
    };
    fs::remove_file(down.join("big.bin")).unwrap();
    flag(sftp_server::FAIL_AT, Some("2000000"));
    let (outcome, asked) = run_asked(get(), Resolution::Resume);
    assert!(asked.is_empty());
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
    assert!(outcome.errors[0].1.contains("was kept"), "{:?}", outcome.errors);
    part(&down.join("big.bin"));
    // An upload too.
    fs::remove_file(disk.0.join("res/up.bin")).unwrap();
    let (outcome, asked) = run_asked(put(), Resolution::Resume);
    assert!(asked.is_empty());
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
    assert!(outcome.errors[0].1.contains("was kept"), "{:?}", outcome.errors);
    part(&disk.0.join("res/up.bin"));
    flag(sftp_server::FAIL_AT, None);
    for job in [get(), put()] {
        let (outcome, asked) = run_asked(job, Resolution::Resume);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(asked.len() == 1 && asked[0].resumable);
    }
    assert!(fs::read(down.join("big.bin")).unwrap() == data);
    assert!(fs::read(disk.0.join("res/up.bin")).unwrap() == data);

    // One that breaks off before 1 MB is removed.
    fs::remove_file(down.join("big.bin")).unwrap();
    fs::remove_file(disk.0.join("res/up.bin")).unwrap();
    flag(sftp_server::FAIL_AT, Some("500000"));
    for job in [get(), put()] {
        let (outcome, _) = run_asked(job, Resolution::Resume);
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        assert!(!outcome.errors[0].1.contains("was kept"), "{:?}", outcome.errors);
    }
    assert!(!down.join("big.bin").exists() && !disk.0.join("res/up.bin").exists());
    flag(sftp_server::FAIL_AT, None);
    disconnect(&root);
}

// ---- Find files on a server ------------------------------------------------------

/// Starts an FTP server over `disk` and connects to its root.
fn start_ftp(disk: &Path) -> PathBuf {
    let port = free_port();
    let root = disk.to_path_buf();
    serve(move || async move {
        let server = libunftp::ServerBuilder::new(Box::new(move || unftp_sbe_fs::Filesystem::new(root.clone()).unwrap()))
            .passive_ports(42000..=42999)
            .build()
            .unwrap();
        let _ = server.listen(format!("127.0.0.1:{port}")).await;
    });
    let login = Login {
        protocol: Protocol::Ftp,
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        password: Some("secret".into()),
        key_file: None,
    };
    let mut path = Err(io::Error::other("not tried"));
    for _ in 0..50 {
        path = connect(login.clone(), "");
        if path.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    path.unwrap()
}

/// Runs a find on `root`; returns the hit names (sorted, `/`-separated) and the summary.
fn find(root: &Path, name: &str, regex: bool, text: &str, subfolders: bool) -> (Vec<String>, crate::search::Summary) {
    let query = crate::search::Query {
        root: root.to_path_buf(),
        name: name.into(),
        name_regex: regex,
        text: text.into(),
        subfolders,
        ..Default::default()
    };
    let mut hits = Vec::new();
    let summary = crate::search::search(&query, &std::sync::atomic::AtomicBool::new(false), |b| {
        hits.extend(b.hits.into_iter().map(|e| e.name.replace('\\', "/")));
        true
    })
    .unwrap();
    hits.sort();
    (hits, summary)
}

/// Find files over a small tree on a connected server whose files live in `disk`.
fn exercise_find(root: &Path, disk: &Path) {
    fs::create_dir_all(disk.join("sub/deep")).unwrap();
    fs::create_dir_all(disk.join("empty")).unwrap();
    fs::write(disk.join("a.txt"), b"hello world").unwrap();
    fs::write(disk.join("b.log"), b"hello").unwrap();
    fs::write(disk.join("sub/c.txt"), b"a needle here").unwrap();
    fs::write(disk.join("sub/deep/d.TXT"), b"NEEDLE").unwrap();

    // Name mask, all levels, case-insensitive.
    let (hits, summary) = find(root, "*.txt", false, "", true);
    assert_eq!(hits, ["a.txt", "sub/c.txt", "sub/deep/d.TXT"]);
    assert_eq!((summary.errors, summary.cancelled), (0, false));
    // Without subfolders only the top level.
    assert_eq!(find(root, "*.txt", false, "", false).0, ["a.txt"]);
    // Regex; folders match by name too.
    assert_eq!(find(root, r"^[ab]\.", true, "", true).0, ["a.txt", "b.log"]);
    assert_eq!(find(root, "deep", false, "", true).0, ["sub/deep"]);
    // Everything.
    assert_eq!(find(root, "", false, "", true).0.len(), 7);
    // Content: case-insensitive by default, files only.
    assert_eq!(find(root, "", false, "needle", true).0, ["sub/c.txt", "sub/deep/d.TXT"]);
    assert_eq!(find(root, "*.log", false, "hello", true).0, ["b.log"]);
    assert_eq!(find(root, "*.txt", false, "hello", true).0, ["a.txt"]);
    assert!(find(root, "", false, "absent", true).0.is_empty());

    // Cancelled before reading: stops at once, reports it, no hits.
    let query = crate::search::Query { root: root.to_path_buf(), text: "hello".into(), subfolders: true, ..Default::default() };
    let summary = crate::search::search(&query, &std::sync::atomic::AtomicBool::new(true), |_| true).unwrap();
    assert!(summary.cancelled);
    assert_eq!(summary.hits, 0);

    // A folder that cannot be read: reported, not a hang or a panic.
    let sub = root.join("sub");
    disconnect(root);
    let query = crate::search::Query { root: sub, subfolders: true, ..Default::default() };
    let summary = crate::search::search(&query, &std::sync::atomic::AtomicBool::new(false), |_| true).unwrap();
    assert_eq!((summary.hits, summary.errors), (0, 1));
}

#[test]
fn find_files_on_ftp_server() {
    let disk = TempDir::new("ftp-find");
    let root = start_ftp(&disk.0);
    exercise_find(&root, &disk.0);
}

#[test]
fn find_files_on_sftp_server() {
    let _settings = settings();
    let disk = TempDir::new("sftp-find");
    let port = start_sftp(disk.0.clone(), 4);
    let login = sftp_login(port, sftp_server::PASSWORD);
    assert!(connect(login.clone(), "").is_err());
    trust_host_key(&login).unwrap();
    let root = connect(login, "").unwrap();
    exercise_find(&root, &disk.0);
}

#[test]
fn partial_files_kept_only_when_resumable() {
    // A resumed transfer never removes what was there before.
    assert!(keeps_partial(10, 0, true) && keeps_partial(10, 0, false));
    // A new one: only after an error past KEEP_PARTIAL, never when cancelled.
    assert!(keeps_partial(0, KEEP_PARTIAL, false));
    assert!(!keeps_partial(0, KEEP_PARTIAL - 1, false));
    assert!(!keeps_partial(0, KEEP_PARTIAL * 10, true));
}

#[test]
fn paths() {
    let p = path_of("sftp://me@host", "/home/me");
    assert_eq!(split(&p), Some(("sftp://me@host".into(), "/home/me".into())));
    let joined = p.join("sub").join("f.txt");
    assert_eq!(display(&joined), "sftp://me@host/home/me/sub/f.txt");
    assert_eq!(joined.parent().unwrap(), p.join("sub"));
    assert_eq!(split(&p.join("..").join("x")).unwrap().1, "/home/x");
    assert_eq!(split(Path::new("ftp://u@h:2121")).unwrap(), ("ftp://u@h:2121".into(), "/".into()));
    assert_eq!(split(Path::new("FTPS://h/a")).unwrap().0, "ftps://h");
    assert!(split(Path::new(r"C:\ftp:\x")).is_none());
    assert!(split(Path::new("ftp:")).is_none());
    assert!(!is_remote(Path::new(r"C:\Users")));
    assert_eq!(remote_parent("/a/b"), Some("/a"));
    assert_eq!(remote_parent("/a"), Some("/"));
    assert_eq!(remote_parent("/"), None);
    let login = Login { protocol: Protocol::Sftp, host: "h".into(), port: 22, user: "u".into(), password: None, key_file: None };
    assert_eq!(login.key(), "sftp://u@h");
    assert_eq!(Login { port: 2222, ..login }.key(), "sftp://u@h:2222");
    // Remote paths are never treated as archives or local files.
    assert!(crate::vfs::split_archive(Path::new("sftp://u@h/a.zip/x")).is_none());
}

/// Not a test: serves `RC_TEST_ROOT` over FTP on 127.0.0.1:2121, SFTP on
/// 127.0.0.1:2222 and FTPS (TLS required, self-signed certificate) on
/// 127.0.0.1:2990 (user `tester`, password `secret`) until killed, for
/// trying the UI by hand: `cargo test -p rc-core local_servers -- --ignored`.
/// The certificate is kept in `%TEMP%\rc-manual-ftps` (delete it for a new
/// one, i.e. a changed certificate).
#[test]
#[ignore]
fn local_servers_for_manual_testing() {
    let root = PathBuf::from(std::env::var_os("RC_TEST_ROOT").expect("RC_TEST_ROOT"));
    let ftp_root = root.clone();
    serve(move || async move {
        let server = libunftp::ServerBuilder::new(Box::new(move || unftp_sbe_fs::Filesystem::new(ftp_root.clone()).unwrap()))
            .passive_ports(41000..=41999)
            .build()
            .unwrap();
        let _ = server.listen("127.0.0.1:2121").await;
    });
    let certs = std::env::temp_dir().join("rc-manual-ftps");
    let (crt, key) = (certs.join("server.crt"), certs.join("server.key"));
    if !crt.exists() || !key.exists() {
        fs::create_dir_all(&certs).unwrap();
        write_cert(&certs, "server", "127.0.0.1", None);
    }
    let ftps = TcpListener::bind("127.0.0.1:2990").unwrap();
    ftps.set_nonblocking(true).unwrap();
    start_ftps_on(ftps, root.clone(), Arc::new(Mutex::new((crt, key))));
    let sftp = TcpListener::bind("127.0.0.1:2222").unwrap();
    sftp.set_nonblocking(true).unwrap();
    serve(move || sftp_server::run(root, sftp, 9, russh::Preferred::DEFAULT));
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
