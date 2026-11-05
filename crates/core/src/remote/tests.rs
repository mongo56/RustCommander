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
/// that asked for it is done (its `Drop` runs even when a test panics).
static SETTINGS_USERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Settings(PathBuf);

impl Drop for Settings {
    fn drop(&mut self) {
        if SETTINGS_USERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn settings() -> Settings {
    let dir = std::env::temp_dir().join(format!("rc-remote-settings-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    set_settings_dir(dir.clone());
    SETTINGS_USERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    let _ = fs::remove_dir_all(crate::vfs::temp_path(root).parent().unwrap());

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
    let copy = vec![Transfer { src: root.join("docs"), dst: root.join("docs2") }];
    let (outcome, _) = run_job(Job::Copy(copy), Resolution::Skip);
    assert!(outcome.errors[0].1.contains("isn't supported"));

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
    use russh_sftp::protocol::{Attrs, Data, File as SftpFile, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode};
    use std::collections::HashMap;
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    pub const USER: &str = "tester";
    pub const PASSWORD: &str = "secret";

    pub fn host_key(seed: u8) -> russh::keys::PrivateKey {
        let pair = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]);
        russh::keys::PrivateKey::from(pair)
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
                    let sftp = Sftp { root: self.root.clone(), files: HashMap::new(), dirs: HashMap::new(), next: 0 };
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
    }

    impl russh_sftp::server::Handler for Sftp {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
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
            let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
            file.seek(SeekFrom::Start(offset)).map_err(code)?;
            file.write_all(&data).map_err(code)?;
            Ok(status(id))
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
            self.files.remove(&handle);
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

    /// Serves `root` on 127.0.0.1:`port` with the host key from `seed`.
    pub async fn run(root: PathBuf, port: u16, seed: u8) {
        let config = Arc::new(russh::server::Config {
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![host_key(seed)],
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
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
    let port = free_port();
    serve(move || sftp_server::run(root, port, seed));
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
    writeln!(known, "127.0.0.1:{port} {other}").unwrap();
    let e = connect(sftp_login(port, sftp_server::PASSWORD), "").unwrap_err().to_string();
    assert!(e.contains(HOST_KEY_CHANGED), "{e}");
    assert!(trust_host_key(&sftp_login(port, sftp_server::PASSWORD)).is_err());
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

/// Not a test: serves `RC_TEST_ROOT` over FTP on 127.0.0.1:2121 and SFTP on
/// 127.0.0.1:2222 (user `tester`, password `secret`) until killed, for
/// trying the UI by hand: `cargo test -p rc-core local_servers -- --ignored`.
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
    serve(move || sftp_server::run(root, 2222, 9));
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
