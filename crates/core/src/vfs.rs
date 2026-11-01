//! Virtual file systems: what a panel can show besides local folders.
//!
//! Paths stay plain `PathBuf`s. A path that runs through an archive file
//! (`C:\x\a.zip\sub\f.txt`) is served by that archive's [`Vfs`]; anything
//! else is the local file system ([`LocalFs`]). Archives can be listed,
//! walked and read (copy out, view); zips and tars can also be changed (see
//! `zip_edit`, `tar_edit`, driven by the jobs in `ops`), the others are
//! read-only.
//! Supported: zip (`.zip`, `.jar`) via the `zip` crate, 7z via
//! `sevenz-rust2`, tar (`.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`) via
//! `tar` + `flate2`/`bzip2`, a single gzipped file (`.gz`), and rar (rar4,
//! rar5, multi-volume; read-only) via UnRAR (`rar` module). Parsed
//! archive indexes are cached by path, size and modification time, so
//! browsing an archive reads its directory once (a tar has no directory:
//! listing reads it all, and so does reading files from it).
//!
//! An archive inside an archive (`a.zip\b.7z\x`) is browsed through a temp
//! copy of the inner one ([`temp_path`], extracted when a path first runs
//! through it and again when it changed): paths stay logical, only reads go
//! to the copy. Nested archives are read-only.
//!
//! Encrypted zips (ZipCrypto, AES), 7z and rar archives (also with
//! encrypted names) are read with a password given by [`try_password`],
//! kept per archive for the session. Without one, reading an encrypted
//! file (or listing a 7z or rar with encrypted names) fails with an error whose
//! text contains [`PASSWORD_REQUIRED`].

use crate::entry::{Entry, EntryKind};
use chrono::{DateTime, Local, TimeZone};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

mod rar;

/// A file system a panel can list and copy out of.
pub trait Vfs: Send + Sync {
    /// The folder `dir` (a path inside this file system), with `..` first
    /// when it has a parent. Hidden entries are included (flagged).
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    /// `path` itself and, for a folder, everything under it; a folder comes
    /// before its contents.
    fn walk(&self, path: &Path) -> io::Result<Vec<Entry>>;
    /// Reads the files `files`, calling `each(i, reader)` with the index in
    /// `files` of each one, in whatever order suits the format. `each`
    /// returns `Ok(false)` to stop early.
    fn read_files(&self, files: &[PathBuf], each: &mut dyn FnMut(usize, &mut dyn Read) -> io::Result<bool>)
        -> io::Result<()>;
    /// `path` itself (not followed into a folder); `None` if it doesn't exist.
    fn stat(&self, path: &Path) -> io::Result<Option<Entry>>;
    /// Whether files can be created, changed or deleted here.
    fn writable(&self) -> bool;

    // Direct changes, for file systems that support them (servers); the
    // others fail with `Unsupported` (zips are changed by `zip_edit`).

    /// Creates the folder `path` (its parent exists).
    fn make_dir(&self, path: &Path) -> io::Result<()> {
        Err(unsupported(path))
    }
    /// Removes a file, or an empty folder if `is_dir`.
    fn remove(&self, path: &Path, is_dir: bool) -> io::Result<()> {
        let _ = is_dir;
        Err(unsupported(path))
    }
    /// Renames or moves `from` to `to` within this file system.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let _ = to;
        Err(unsupported(from))
    }
    /// Creates or replaces the file `path` with what `fill` writes (Ok(false)
    /// from `fill` cancels: the partial file is removed) and gives it the
    /// date `modified` where possible.
    fn write_file(
        &self,
        path: &Path,
        modified: Option<SystemTime>,
        fill: &mut dyn FnMut(&mut dyn io::Write) -> io::Result<bool>,
    ) -> io::Result<bool> {
        let _ = (modified, fill);
        Err(unsupported(path))
    }
}

fn unsupported(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, format!("Can't change this directly: {}", path.display()))
}

/// The local file system.
pub struct LocalFs;

impl LocalFs {
    fn entry_of(path: &Path) -> io::Result<Entry> {
        let meta = fs::metadata(path)?;
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let kind = if meta.is_dir() { EntryKind::Dir } else { EntryKind::File };
        let size = if meta.is_dir() { 0 } else { meta.len() };
        let modified = meta.modified().ok().map(DateTime::<Local>::from);
        Ok(Entry { name, path: path.to_path_buf(), kind, size, modified, hidden: false })
    }
}

impl Vfs for LocalFs {
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        crate::fs::list_dir(dir, true)
    }

    fn walk(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let mut out = vec![Self::entry_of(path)?];
        let mut i = 0;
        while i < out.len() {
            if out[i].kind == EntryKind::Dir {
                let children = crate::fs::list_dir(&out[i].path, true)?;
                out.extend(children.into_iter().filter(|e| !e.is_parent()));
            }
            i += 1;
        }
        Ok(out)
    }

    fn read_files(&self, files: &[PathBuf], each: &mut dyn FnMut(usize, &mut dyn Read) -> io::Result<bool>)
        -> io::Result<()> {
        for (i, path) in files.iter().enumerate() {
            if !each(i, &mut File::open(path)?)? {
                break;
            }
        }
        Ok(())
    }

    fn stat(&self, path: &Path) -> io::Result<Option<Entry>> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(Some(Self::entry_of(path)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn writable(&self) -> bool {
        true
    }
}

// ---- paths -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Zip,
    SevenZ,
    Tar,
    TarGz,
    TarBz2,
    /// One gzipped file (`x.txt.gz` holds `x.txt`).
    Gz,
    /// rar4/rar5 (read-only).
    Rar,
}

impl Format {
    /// Archives of this format can be changed (rewritten).
    pub(crate) fn writable(self) -> bool {
        matches!(self, Format::Zip | Format::Tar | Format::TarGz | Format::TarBz2)
    }
}

/// The archive format a file name stands for (by extension), if any.
pub(crate) fn format_of(name: &OsStr) -> Option<Format> {
    let name = name.to_string_lossy().to_ascii_lowercase();
    let ext = Path::new(&name).extension()?.to_str()?;
    let inner_tar = name.trim_end_matches(&format!(".{ext}")).ends_with(".tar");
    match ext {
        "zip" | "jar" => Some(Format::Zip),
        "7z" => Some(Format::SevenZ),
        "rar" => Some(Format::Rar),
        "tar" => Some(Format::Tar),
        "tgz" => Some(Format::TarGz),
        "tbz2" | "tbz" => Some(Format::TarBz2),
        "gz" if inner_tar => Some(Format::TarGz),
        "bz2" if inner_tar => Some(Format::TarBz2),
        "gz" => Some(Format::Gz),
        _ => None,
    }
}

/// Whether a file with this name can be entered like a folder.
pub fn is_archive_name(name: &str) -> bool {
    format_of(OsStr::new(name)).is_some()
}

/// Splits a path that runs through an archive file into the archive and the
/// path inside it (empty for the archive's root). `None` for local paths.
/// Only names with an archive extension are checked on disk. For nested
/// archives the archive is the innermost one (a logical path, e.g.
/// `a.zip\b.zip`), mounted on the way.
pub fn split_archive(path: &Path) -> Option<(PathBuf, PathBuf)> {
    if crate::remote::is_remote(path) {
        return None;
    }
    let mut archive = PathBuf::new();
    let mut parts = path.components();
    while let Some(part) = parts.next() {
        archive.push(part);
        if let Component::Normal(name) = part {
            if format_of(name).is_some() && archive.is_file() {
                return Some(split_nested(archive, parts.as_path()));
            }
        }
    }
    None
}

/// `inner` inside `archive`: if it runs through an archive file stored in
/// `archive`, that one is mounted and the rest split in it.
fn split_nested(archive: PathBuf, inner: &Path) -> (PathBuf, PathBuf) {
    let mut logical = archive.clone();
    let mut parts = inner.components();
    while let Some(part) = parts.next() {
        logical.push(part);
        if let Component::Normal(name) = part {
            if format_of(name).is_some() && mount(&archive, &logical).is_some() {
                return split_nested(logical, parts.as_path());
            }
        }
    }
    (archive, inner.to_path_buf())
}

/// Mounted nested archives: (logical path, temp copy, size and date of the
/// entry it was extracted from).
type Mounts = Vec<(PathBuf, PathBuf, u64, Option<DateTime<Local>>)>;
static MOUNTS: Mutex<Mounts> = Mutex::new(Vec::new());

/// The temp copy of the archive file `logical` stored in the archive
/// `parent`, extracted if missing or outdated. `None` if `logical` isn't a
/// file there (or can't be extracted).
fn mount(parent: &Path, logical: &Path) -> Option<PathBuf> {
    let index = archive(parent).ok()?;
    let entry = index.stat(logical).ok()??;
    if entry.is_dir() {
        return None;
    }
    let disk = temp_path_in(parent, logical.strip_prefix(parent).ok()?);
    {
        let mounts = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
        let same = |(l, d, s, m): &&(PathBuf, PathBuf, u64, Option<DateTime<Local>>)| {
            l == logical && *d == disk && *s == entry.size && *m == entry.modified
        };
        if mounts.iter().find(same).is_some() && disk.is_file() {
            return Some(disk);
        }
    }
    {
        let _lock = TEMP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        mark_used(&disk).ok()?;
    }
    fs::create_dir_all(disk.parent()?).ok()?;
    index
        .read_files(&[logical.to_path_buf()], &mut |_, reader| {
            io::copy(reader, &mut File::create(&disk)?)?;
            Ok(true)
        })
        .ok()?;
    let mut mounts = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
    mounts.retain(|(l, ..)| l != logical);
    mounts.push((logical.to_path_buf(), disk.clone(), entry.size, entry.modified));
    forget(logical);
    Some(disk)
}

/// Forgets the nested archive `logical` (and any mounted inside it) after
/// it was changed, so it's extracted again when next used.
pub(crate) fn unmount(logical: &Path) {
    let mut mounts = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
    mounts.retain(|(l, ..)| {
        let gone = l.starts_with(logical);
        if gone {
            forget(l);
        }
        !gone
    });
}

/// Where the archive `file` (a logical path) is on disk: itself, or the
/// temp copy of a nested one.
pub(crate) fn on_disk(file: &Path) -> io::Result<PathBuf> {
    if file.is_file() {
        return Ok(file.to_path_buf());
    }
    let mounts = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
    match mounts.iter().find(|(l, ..)| l == file) {
        Some((_, disk, ..)) => Ok(disk.clone()),
        None => Err(io::Error::new(io::ErrorKind::NotFound, format!("Archive not found: {}", file.display()))),
    }
}

/// True for a path *inside* an archive (not the archive file itself) or on
/// a server: anything read through a [`Vfs`] other than [`LocalFs`].
pub fn is_virtual(path: &Path) -> bool {
    crate::remote::is_remote(path) || split_archive(path).is_some_and(|(_, inner)| !inner.as_os_str().is_empty())
}

/// True for an archive file, a path inside one or a path on a server:
/// anything not listed by the local file system.
pub fn in_archive(path: &Path) -> bool {
    crate::remote::is_remote(path) || split_archive(path).is_some()
}

/// For an archive or a path inside one: whether the archive can be changed
/// (zip and tar yes, 7z, gz and rar no). One inside another archive can be
/// changed if that one can (it's rewritten and then put back). `None` for
/// local paths.
pub fn archive_writable(path: &Path) -> Option<bool> {
    let (file, _) = split_archive(path)?;
    if !file.file_name().and_then(format_of).is_some_and(Format::writable) {
        return Some(false);
    }
    Some(file.is_file() || file.parent().and_then(archive_writable) == Some(true))
}

/// The path inside its archive of `path` (`/`-separated, as typed), with
/// the archive file; `None` for local paths.
pub fn inner_path(path: &Path) -> Option<(PathBuf, String)> {
    let (file, inner) = split_archive(path)?;
    let parts: Vec<String> = inner
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    Some((file, parts.join("/")))
}

/// The file system serving `path`.
pub fn open(path: &Path) -> io::Result<Arc<dyn Vfs>> {
    if crate::remote::is_remote(path) {
        return crate::remote::open(path);
    }
    match split_archive(path) {
        Some((file, _)) => Ok(archive(&file)?),
        None => Ok(Arc::new(LocalFs)),
    }
}

/// Lists the folder `path` wherever it lives (local folder or a folder in
/// an archive), leaving out hidden entries unless `show_hidden`.
pub fn list(path: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let vfs = match split_archive(path) {
        None if crate::remote::is_remote(path) => crate::remote::open(path)?,
        None => return crate::fs::list_dir(path, show_hidden),
        Some((file, _)) => archive(&file)? as Arc<dyn Vfs>,
    };
    let mut entries = vfs.list(path)?;
    if !show_hidden {
        entries.retain(|e| !e.hidden);
    }
    Ok(entries)
}

/// In the error text of reads that need an archive's password.
pub const PASSWORD_REQUIRED: &str = "Password required";

/// Passwords given for archives (archive file, password).
static PASSWORDS: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

fn password_for(file: &Path) -> Option<String> {
    let passwords = PASSWORDS.lock().unwrap_or_else(|e| e.into_inner());
    passwords.iter().find(|(f, _)| f == file).map(|(_, p)| p.clone())
}

fn set_password(file: &Path, password: Option<&str>) {
    let mut passwords = PASSWORDS.lock().unwrap_or_else(|e| e.into_inner());
    passwords.retain(|(f, _)| f != file);
    if let Some(p) = password {
        passwords.push((file.to_path_buf(), p.to_owned()));
    }
}

fn password_error(file: &Path) -> io::Error {
    let text = match password_for(file) {
        Some(_) => "Wrong password".to_owned(),
        None => format!("{PASSWORD_REQUIRED}: the archive is encrypted (enter the password when opening it)"),
    };
    io::Error::new(io::ErrorKind::PermissionDenied, format!("{text}: {}", file.display()))
}

/// Whether `path` is (in) an archive with encrypted files for which no
/// password was given yet.
pub fn needs_password(path: &Path) -> bool {
    let Some((file, _)) = split_archive(path) else { return false };
    password_for(&file).is_none() && archive(&file).is_ok_and(|a| a.encrypted.is_some())
}

/// Tries `password` for the archive of `path`: kept if it opens the
/// archive and decrypts its smallest encrypted file, else forgotten
/// (Ok(false)).
pub fn try_password(path: &Path, password: &str) -> io::Result<bool> {
    let (file, _) = split_archive(path).ok_or_else(|| io::Error::other("Not an archive"))?;
    let old = password_for(&file);
    set_password(&file, Some(password));
    forget(&file);
    let works = archive(&file).and_then(|a| match a.encrypted {
        Some(i) => {
            let sample = a.entry(i).path;
            a.read_files(&[sample], &mut |_, r| io::copy(r, &mut io::sink()).map(|_| true))
        }
        None => Ok(()),
    });
    match works {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied || e.to_string().contains("password") => {
            set_password(&file, old.as_deref());
            forget(&file);
            Ok(false)
        }
        Err(e) => {
            set_password(&file, old.as_deref());
            Err(e)
        }
    }
}

/// Where [`extract_temp`] puts the copy of a file inside an archive (the
/// path itself for local files): a folder per archive under the temp dir.
pub fn temp_path(path: &Path) -> PathBuf {
    if let Some((key, remote)) = crate::remote::split(path) {
        return temp_path_in(Path::new(&key), Path::new(remote.trim_start_matches('/')));
    }
    match split_archive(path).filter(|(_, inner)| !inner.as_os_str().is_empty()) {
        Some((file, inner)) => temp_path_in(&file, &inner),
        None => path.to_path_buf(),
    }
}

/// The folder for temp copies: `RustCommander` in the temp dir, with a
/// folder per archive or server (named by a 16-digit hash, see
/// [`temp_path`]) and `copy-*` folders of jobs copying between archives.
pub fn temp_root() -> PathBuf {
    std::env::temp_dir().join("RustCommander")
}

/// How long temp copies are kept after they were last made: [`clean_temp`]
/// deletes older ones on startup.
pub const TEMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Suffix of the marker file next to each archive's temp folder in
/// [`temp_root`]: its date says when a copy was last made there.
const USED: &str = ".used";

/// Held while [`clean_temp`] checks and deletes a temp folder, and while a
/// copy marks its folder as used, so a copy made meanwhile isn't deleted.
static TEMP_LOCK: Mutex<()> = Mutex::new(());

/// Marks the folder in [`temp_root`] that holds `copy` as used now.
fn mark_used(copy: &Path) -> io::Result<()> {
    let root = temp_root();
    let Some(Component::Normal(dir)) = copy.strip_prefix(&root).ok().and_then(|rest| rest.components().next()) else {
        return Ok(());
    };
    let mut name = dir.to_os_string();
    name.push(USED);
    fs::create_dir_all(&root)?;
    let stamp = File::options().write(true).create(true).truncate(false).open(root.join(name))?;
    stamp.set_modified(SystemTime::now())
}

/// Marks the temp copy `copy` (from [`extract_temp`]) as still in use, so
/// [`clean_temp`] keeps it, e.g. while it's open in an editor.
pub fn keep_temp(copy: &Path) -> io::Result<()> {
    let _lock = TEMP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    mark_used(copy)
}

/// Deletes temp copies in [`temp_root`] (of earlier runs or other
/// instances) whose folder wasn't used for `max_age`. Files that can't be
/// deleted (open in another program) are left, and so is anything in
/// there this didn't make. Run it off the UI thread.
pub fn clean_temp(max_age: Duration) {
    clean_temp_in(&temp_root(), max_age);
}

fn clean_temp_in(root: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(root) else { return };
    let now = SystemTime::now();
    let stale = |t: Option<SystemTime>| t.is_none_or(|t| now.duration_since(t).is_ok_and(|age| age > max_age));
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else { continue };
        let _lock = TEMP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        let hashed = |n: &str| n.len() == 16 && n.bytes().all(|b| b.is_ascii_hexdigit());
        if meta.is_dir() && (hashed(&name) || name.starts_with("copy-")) {
            let stamp = root.join(format!("{name}{USED}"));
            // Folders from before the markers: the newest date in them.
            let used = match fs::metadata(&stamp).and_then(|m| m.modified()) {
                Ok(t) => Some(t),
                Err(_) => newest_date(&path),
            };
            if stale(used) && remove_tree(&path) {
                let _ = fs::remove_file(&stamp);
            }
        } else if meta.is_file() {
            let Some(dir) = name.strip_suffix(USED).filter(|d| hashed(d)) else { continue };
            // A marker whose folder is gone.
            if fs::symlink_metadata(root.join(dir)).is_err() && stale(meta.modified().ok()) {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// The latest creation or modification date of `dir` or anything in it.
fn newest_date(dir: &Path) -> Option<SystemTime> {
    let dates = |m: &fs::Metadata| [m.created().ok(), m.modified().ok()].into_iter().flatten().max();
    let mut newest = fs::symlink_metadata(dir).ok().as_ref().and_then(dates);
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let date = match entry.file_type() {
            Ok(t) if t.is_dir() => newest_date(&entry.path()),
            _ => fs::symlink_metadata(entry.path()).ok().as_ref().and_then(dates),
        };
        newest = newest.max(date);
    }
    newest
}

/// Deletes what it can in `dir` and then `dir` itself, deepest first;
/// links are removed, never followed. True if `dir` is gone.
fn remove_tree(dir: &Path) -> bool {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                remove_tree(&path);
            }
            // A link to a folder is removed with remove_dir on Windows.
            _ => {
                let _ = fs::remove_file(&path).or_else(|_| fs::remove_dir(&path));
            }
        }
    }
    fs::remove_dir(dir).is_ok()
}

/// [`temp_path`] of `inner` in the archive `file`.
fn temp_path_in(file: &Path, inner: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file.hash(&mut hasher);
    let mut out = temp_root().join(format!("{:016x}", hasher.finish()));
    out.extend(inner.components().filter_map(|c| match c {
        Component::Normal(n) => Some(n),
        _ => None,
    }));
    out
}

/// Copies a file inside an archive to [`temp_path`] (so it can be viewed or
/// opened by other programs) and returns that path. Local files are
/// returned as they are.
pub fn extract_temp(path: &Path) -> io::Result<PathBuf> {
    if !is_virtual(path) {
        return Ok(path.to_path_buf());
    }
    Ok(extract_temp_all(std::slice::from_ref(&path.to_path_buf()))?.remove(0))
}

/// [`extract_temp`] for several files or folders (with everything in
/// them) inside one archive, e.g. to hand them to the shell (drag out,
/// context menu). Returns their temp paths, in order.
pub fn extract_temp_all(paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let Some(first) = paths.iter().find(|p| is_virtual(p)) else { return Ok(paths.to_vec()) };
    let vfs = open(first)?;
    {
        let _lock = TEMP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        mark_used(&temp_path(first))?;
    }
    let (mut files, mut targets) = (Vec::new(), Vec::new());
    for path in paths {
        for entry in vfs.walk(path)? {
            let dst = temp_path(&entry.path);
            if entry.is_dir() {
                fs::create_dir_all(&dst)?;
                continue;
            }
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            files.push(entry.path);
            targets.push((dst, entry.modified));
        }
    }
    vfs.read_files(&files, &mut |i, reader| {
        let (dst, modified) = &targets[i];
        let mut out = File::create(dst)?;
        io::copy(reader, &mut out)?;
        if let Some(m) = modified {
            let _ = out.set_modified((*m).into());
        }
        Ok(true)
    })?;
    Ok(paths.iter().map(|p| temp_path(p)).collect())
}

// ---- archives ----------------------------------------------------------------

/// One file or folder of an archive.
#[derive(Debug)]
struct Item {
    /// Path inside the archive, `/`-separated, original case.
    inner: String,
    is_dir: bool,
    size: u64,
    modified: Option<DateTime<Local>>,
    hidden: bool,
    /// Index in the archive's own entry list; `None` for folders that only
    /// exist implicitly (as the parent of some entry).
    source: Option<usize>,
}

/// A parsed archive directory.
#[derive(Debug)]
struct Archive {
    /// The logical path (entries' paths start with it).
    file: PathBuf,
    /// Where it's read from: `file`, or a nested archive's temp copy.
    disk: PathBuf,
    format: Format,
    items: Vec<Item>,
    /// Item index by lowercased inner path.
    by_path: HashMap<String, usize>,
    /// Children of each folder (lowercased inner path, `""` = root).
    children: HashMap<String, Vec<usize>>,
    /// The smallest encrypted file (item index), if any is.
    encrypted: Option<usize>,
}

/// A raw entry as the format reader reports it.
struct Raw {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<DateTime<Local>>,
    hidden: bool,
    source: usize,
    encrypted: bool,
}

/// Cached archives: (file, size, mtime) → parsed index; most recent first.
type Cache = Vec<(PathBuf, u64, Option<SystemTime>, Arc<Archive>)>;
static CACHE: Mutex<Cache> = Mutex::new(Vec::new());
const CACHE_SIZE: usize = 4;

/// The parsed archive `file` (a logical path), from the cache if it hasn't
/// changed.
fn archive(file: &Path) -> io::Result<Arc<Archive>> {
    let format = file.file_name().and_then(format_of).ok_or_else(|| io::Error::other("Not an archive"))?;
    let disk = on_disk(file)?;
    let meta = fs::metadata(&disk)?;
    let (len, modified) = (meta.len(), meta.modified().ok());
    {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = cache.iter().position(|(f, l, m, _)| f == file && *l == len && *m == modified) {
            let hit = cache.remove(i);
            let archive = hit.3.clone();
            cache.insert(0, hit);
            return Ok(archive);
        }
    }
    let raw = match format {
        Format::Zip => read_zip_index(&disk)?,
        Format::SevenZ => read_7z_index(file, &disk)?,
        Format::Tar | Format::TarGz | Format::TarBz2 => read_tar_index(&disk, format)?,
        Format::Gz => read_gz_index(&disk)?,
        Format::Rar => rar::read_index(file, &disk)?,
    };
    let archive = Arc::new(Archive::build(file.to_path_buf(), disk, format, raw));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(f, ..)| f != file);
    cache.insert(0, (file.to_path_buf(), len, modified, archive.clone()));
    cache.truncate(CACHE_SIZE);
    Ok(archive)
}

/// Drops `file` from the index cache (after changing it).
pub(crate) fn forget(file: &Path) {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).retain(|(f, ..)| f != file);
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// The normal components of an entry name (`/` or `\` separated); `None`
/// if it tries to leave the archive (`..`) or is empty.
pub(crate) fn clean_parts(name: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            p if p.contains(':') => return None,
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then_some(parts)
}

fn read_zip_index(file: &Path) -> io::Result<Vec<Raw>> {
    let mut zip = zip::ZipArchive::new(BufReader::new(File::open(file)?)).map_err(io_err)?;
    let mut raw = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let f = zip.by_index_raw(i).map_err(io_err)?;
        let modified = f.last_modified().and_then(|t| {
            let (y, mo, d) = (t.year().into(), t.month().into(), t.day().into());
            Local.with_ymd_and_hms(y, mo, d, t.hour().into(), t.minute().into(), t.second().into()).earliest()
        });
        let Ok(name) = f.name() else { continue };
        raw.push(Raw {
            name: name.to_string(),
            is_dir: f.is_dir(),
            size: f.size(),
            modified,
            hidden: f.external_attributes() & 0x2 != 0 && f.unix_mode().is_none(),
            source: i,
            encrypted: f.encrypted(),
        });
    }
    Ok(raw)
}

/// Opens the 7z `file` (stored at `disk`) with its password, if one was
/// given.
fn open_7z(file: &Path, disk: &Path) -> io::Result<(sevenz_rust2::Archive, sevenz_rust2::Password)> {
    let password = password_for(file).map_or_else(sevenz_rust2::Password::empty, |p| p.as_str().into());
    match sevenz_rust2::Archive::open_with_password(disk, &password) {
        Ok(archive) => Ok((archive, password)),
        Err(e) => Err(sevenz_err(file, e)),
    }
}

fn sevenz_err(file: &Path, e: sevenz_rust2::Error) -> io::Error {
    match e {
        sevenz_rust2::Error::PasswordRequired | sevenz_rust2::Error::MaybeBadPassword(_) => password_error(file),
        e => io_err(e),
    }
}

fn read_7z_index(file: &Path, disk: &Path) -> io::Result<Vec<Raw>> {
    let (archive, _) = open_7z(file, disk)?;
    // Files in blocks with an AES coder are encrypted.
    let aes = sevenz_rust2::EncoderMethod::ID_AES256_SHA256;
    let encrypted: Vec<bool> =
        archive.blocks.iter().map(|b| b.coders.iter().any(|c| c.encoder_method_id() == aes)).collect();
    let mut raw = Vec::with_capacity(archive.files.len());
    for (i, f) in archive.files.iter().enumerate() {
        if f.is_anti_item {
            continue;
        }
        let modified = f.has_last_modified_date.then(|| u64::from(f.last_modified_date)).and_then(|ticks| {
            // 100 ns ticks since 1601-01-01.
            let secs = (ticks / 10_000_000) as i64 - 11_644_473_600;
            Local.timestamp_opt(secs, (ticks % 10_000_000) as u32 * 100).single()
        });
        raw.push(Raw {
            name: f.name.clone(),
            is_dir: f.is_directory,
            size: f.size,
            modified,
            hidden: f.has_windows_attributes && f.windows_attributes & 0x6 != 0,
            source: i,
            encrypted: archive.stream_map.file_block_index.get(i).copied().flatten().is_some_and(|b| encrypted[b]),
        });
    }
    Ok(raw)
}

/// The decompressed contents of a tar-like or gz `file`.
pub(crate) fn open_stream(file: &Path, format: Format) -> io::Result<Box<dyn Read>> {
    let input = BufReader::new(File::open(file)?);
    Ok(match format {
        Format::TarGz | Format::Gz => Box::new(flate2::read::MultiGzDecoder::new(input)),
        Format::TarBz2 => Box::new(bzip2::read::MultiBzDecoder::new(input)),
        _ => Box::new(input),
    })
}

/// A tar entry's kind: `Some(is_dir)` for folders and files; links and
/// special files are left out.
fn tar_kind(entry: &tar::Entry<'_, Box<dyn Read>>) -> Option<bool> {
    use tar::EntryType;
    match entry.header().entry_type() {
        EntryType::Directory => Some(true),
        EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => Some(false),
        _ => None,
    }
}

/// `source` numbers the entries the `tar` crate yields (meta entries such
/// as long names are folded in by it), counting the left out ones too.
fn read_tar_index(file: &Path, format: Format) -> io::Result<Vec<Raw>> {
    let mut tar = tar::Archive::new(open_stream(file, format)?);
    let mut raw = Vec::new();
    for (i, entry) in tar.entries()?.enumerate() {
        let entry = entry?;
        let Some(is_dir) = tar_kind(&entry) else { continue };
        let header = entry.header();
        let modified = header.mtime().ok().and_then(|s| Local.timestamp_opt(s as i64, 0).single());
        raw.push(Raw {
            name: entry.path()?.to_string_lossy().into_owned(),
            is_dir,
            size: if is_dir { 0 } else { entry.size() },
            modified,
            hidden: false,
            source: i,
            encrypted: false,
        });
    }
    Ok(raw)
}

/// A `.gz` holds one file, named like the archive without `.gz`. Its size
/// is in the trailer (modulo 4 GiB), its date is the archive's.
fn read_gz_index(file: &Path) -> io::Result<Vec<Raw>> {
    use std::io::{Seek, SeekFrom};
    let mut f = File::open(file)?;
    let mut size = [0u8; 4];
    let len = f.seek(SeekFrom::End(0))?;
    if len >= 18 {
        f.seek(SeekFrom::End(-4))?;
        f.read_exact(&mut size)?;
    }
    let modified = f.metadata()?.modified().ok().map(DateTime::<Local>::from);
    let name = file.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let size = u32::from_le_bytes(size).into();
    Ok(vec![Raw { name, is_dir: false, size, modified, hidden: false, source: 0, encrypted: false }])
}

impl Archive {
    fn build(file: PathBuf, disk: PathBuf, format: Format, raw: Vec<Raw>) -> Self {
        let mut archive = Archive {
            file,
            disk,
            format,
            items: Vec::new(),
            by_path: HashMap::new(),
            children: HashMap::new(),
            encrypted: None,
        };
        archive.children.insert(String::new(), Vec::new());
        // The smallest encrypted file: (size, key).
        let mut smallest: Option<(u64, String)> = None;
        for r in raw {
            let Some(parts) = clean_parts(&r.name) else { continue };
            if r.encrypted && !r.is_dir && smallest.as_ref().is_none_or(|(size, _)| r.size < *size) {
                smallest = Some((r.size, parts.join("/").to_lowercase()));
            }
            let parts: Vec<String> = parts.into_iter().map(str::to_owned).collect();
            // Implicit parent folders.
            for depth in 1..parts.len() {
                archive.add(&parts[..depth], None);
            }
            archive.add(&parts, Some(r));
        }
        archive.encrypted = smallest.and_then(|(_, key)| archive.by_path.get(&key).copied());
        archive
    }

    /// Adds (or completes) the item at `parts`. `raw = None` adds a folder
    /// if it's missing.
    fn add(&mut self, parts: &[String], raw: Option<Raw>) {
        let inner = parts.join("/");
        let key = inner.to_lowercase();
        if let Some(&i) = self.by_path.get(&key) {
            // A folder seen implicitly first, then explicitly: take its
            // details. Duplicate files: the later one wins.
            if let Some(r) = raw {
                let item = &mut self.items[i];
                if item.is_dir == r.is_dir {
                    item.size = r.size;
                    item.modified = r.modified;
                    item.hidden = r.hidden;
                    item.source = Some(r.source);
                }
            }
            return;
        }
        let item = match raw {
            Some(r) => Item {
                inner,
                is_dir: r.is_dir,
                size: if r.is_dir { 0 } else { r.size },
                modified: r.modified,
                hidden: r.hidden,
                source: Some(r.source),
            },
            None => Item { inner, is_dir: true, size: 0, modified: None, hidden: false, source: None },
        };
        let index = self.items.len();
        let parent = key.rfind('/').map_or("", |i| &key[..i]).to_owned();
        if item.is_dir {
            self.children.entry(key.clone()).or_default();
        }
        self.items.push(item);
        self.by_path.insert(key, index);
        self.children.entry(parent).or_default().push(index);
    }

    /// The lowercased inner path of `path` (`""` = root).
    fn key(&self, path: &Path) -> io::Result<String> {
        let rest = path.strip_prefix(&self.file).map_err(|_| io::Error::other("Path is not in this archive"))?;
        let parts: Vec<String> = rest
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
                _ => None,
            })
            .collect();
        Ok(parts.join("/"))
    }

    fn item(&self, path: &Path) -> io::Result<(String, Option<usize>)> {
        let key = self.key(path)?;
        if key.is_empty() {
            return Ok((key, None));
        }
        match self.by_path.get(&key) {
            Some(&i) => Ok((key, Some(i))),
            None => Err(io::Error::new(io::ErrorKind::NotFound, format!("Not found in the archive: {}", path.display()))),
        }
    }

    fn entry(&self, i: usize) -> Entry {
        let item = &self.items[i];
        let mut path = self.file.clone();
        path.extend(item.inner.split('/'));
        Entry {
            name: item.inner.rsplit('/').next().unwrap_or(&item.inner).to_owned(),
            path,
            kind: if item.is_dir { EntryKind::Dir } else { EntryKind::File },
            size: item.size,
            modified: item.modified,
            hidden: item.hidden,
        }
    }

    /// Indexes of the files to read, in the archive's own numbering.
    fn sources(&self, files: &[PathBuf]) -> io::Result<Vec<(usize, Option<usize>)>> {
        files
            .iter()
            .enumerate()
            .map(|(i, path)| match self.item(path)? {
                (_, Some(item)) if !self.items[item].is_dir => Ok((i, self.items[item].source)),
                _ => Err(io::Error::other(format!("Not a file: {}", path.display()))),
            })
            .collect()
    }

    fn read_zip(&self, files: &[PathBuf], each: &mut dyn FnMut(usize, &mut dyn Read) -> io::Result<bool>)
        -> io::Result<()> {
        let wanted = self.sources(files)?;
        let mut zip = zip::ZipArchive::new(BufReader::new(File::open(&self.disk)?)).map_err(io_err)?;
        let password = password_for(&self.file);
        for (i, source) in wanted {
            let Some(source) = source else { continue };
            let encrypted = zip.by_index_raw(source).map_err(io_err)?.encrypted();
            let found = match (encrypted, &password) {
                (false, _) => zip.by_index(source),
                (true, Some(p)) => zip.by_index_decrypt(source, p.as_bytes()),
                (true, None) => return Err(password_error(&self.file)),
            };
            let mut f = match found {
                Ok(f) => f,
                Err(zip::result::ZipError::InvalidPassword) => return Err(password_error(&self.file)),
                Err(e) => return Err(io_err(e)),
            };
            if !each(i, &mut f)? {
                break;
            }
        }
        Ok(())
    }

    /// Tar: reads through the archive, handing over the wanted entries.
    fn read_tar(&self, files: &[PathBuf], each: &mut dyn FnMut(usize, &mut dyn Read) -> io::Result<bool>)
        -> io::Result<()> {
        let mut want: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, src) in self.sources(files)? {
            if let Some(src) = src {
                want.entry(src).or_default().push(i);
            }
        }
        if self.format == Format::Gz {
            if let Some(indexes) = want.remove(&0) {
                let mut stream = open_stream(&self.disk, self.format)?;
                for i in indexes {
                    if !each(i, &mut stream)? {
                        break;
                    }
                }
            }
            return Ok(());
        }
        let mut tar = tar::Archive::new(open_stream(&self.disk, self.format)?);
        for (n, entry) in tar.entries()?.enumerate() {
            if want.is_empty() {
                break;
            }
            let mut entry = entry?;
            // A file listed twice is read once (the later copy wins in the
            // index, as `source` points at it).
            if let Some(indexes) = want.remove(&n) {
                for i in indexes {
                    if !each(i, &mut entry)? {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// 7z: decodes only the blocks holding wanted files (a solid block is
    /// read from its start up to the last wanted file in it).
    fn read_7z(&self, files: &[PathBuf], each: &mut dyn FnMut(usize, &mut dyn Read) -> io::Result<bool>)
        -> io::Result<()> {
        let wanted = self.sources(files)?;
        let (archive, password) = open_7z(&self.file, &self.disk)?;
        let mut source = BufReader::new(File::open(&self.disk)?);
        let map = &archive.stream_map;
        // Wanted files by archive index, grouped by block.
        let mut blocks: BTreeMap<usize, HashMap<usize, usize>> = BTreeMap::new();
        for (i, src) in wanted {
            let Some(src) = src else { continue };
            match map.file_block_index.get(src).copied().flatten() {
                Some(block) => {
                    blocks.entry(block).or_default().insert(src, i);
                }
                // Empty file: no data.
                None => {
                    if !each(i, &mut io::empty())? {
                        return Ok(());
                    }
                }
            }
        }
        for (block, mut want) in blocks {
            let mut index = map.block_first_file_index[block];
            let mut stop = false;
            let mut failure = None;
            let mut on_entry = |_: &sevenz_rust2::ArchiveEntry, reader: &mut dyn Read| {
                let here = index;
                index += 1;
                if let Some(i) = want.remove(&here) {
                    match each(i, reader) {
                        Ok(true) => {}
                        Ok(false) => stop = true,
                        Err(e) => failure = Some(e),
                    }
                }
                Ok::<bool, sevenz_rust2::Error>(!stop && failure.is_none() && !want.is_empty())
            };
            let decoder = sevenz_rust2::BlockDecoder::new(1, block, &archive, &password, &mut source);
            let result = decoder.for_each_entries(&mut on_entry);
            if let Some(e) = failure {
                // A wrong password shows as garbage data.
                let garbled = e.to_string().contains("password") || e.kind() == io::ErrorKind::InvalidData;
                return Err(if garbled && self.encrypted.is_some() { password_error(&self.file) } else { e });
            }
            result.map_err(|e| sevenz_err(&self.file, e))?;
            if stop {
                break;
            }
        }
        Ok(())
    }
}

impl Vfs for Archive {
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let (key, item) = self.item(dir)?;
        if item.is_some_and(|i| !self.items[i].is_dir) {
            return Err(io::Error::other(format!("Not a folder: {}", dir.display())));
        }
        let mut entries = Vec::new();
        if let Some(parent) = dir.parent() {
            entries.push(Entry::parent(parent.to_path_buf()));
        }
        let children = self.children.get(&key).map_or(&[][..], Vec::as_slice);
        entries.extend(children.iter().map(|&i| self.entry(i)));
        Ok(entries)
    }

    fn walk(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let (key, item) = self.item(path)?;
        let mut out = Vec::new();
        if let Some(i) = item {
            out.push(self.entry(i));
            if !self.items[i].is_dir {
                return Ok(out);
            }
        }
        let mut stack = vec![key];
        while let Some(key) = stack.pop() {
            for &i in self.children.get(&key).map_or(&[][..], Vec::as_slice) {
                out.push(self.entry(i));
                if self.items[i].is_dir {
                    stack.push(self.items[i].inner.to_lowercase());
                }
            }
        }
        Ok(out)
    }

    fn read_files(&self, files: &[PathBuf], each: &mut dyn FnMut(usize, &mut dyn Read) -> io::Result<bool>)
        -> io::Result<()> {
        match self.format {
            Format::Zip => self.read_zip(files, each),
            Format::SevenZ => self.read_7z(files, each),
            Format::Rar => self.read_rar(files, each),
            Format::Tar | Format::TarGz | Format::TarBz2 | Format::Gz => self.read_tar(files, each),
        }
    }

    fn stat(&self, path: &Path) -> io::Result<Option<Entry>> {
        match self.item(path) {
            Ok((_, Some(i))) => Ok(Some(self.entry(i))),
            Ok((_, None)) => {
                let name = self.file.file_name().unwrap_or_default().to_string_lossy().into_owned();
                Ok(Some(Entry { name, path: self.file.clone(), kind: EntryKind::Dir, size: 0, modified: None, hidden: false }))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn writable(&self) -> bool {
        self.format.writable() && (self.disk == self.file || archive_writable(&self.file) == Some(true))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rc-vfs-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A zip with `sub/` (explicit), `sub/deep/x.txt` (implicit folder),
    /// `a.txt` and `Readme.md`.
    pub(crate) fn make_zip(path: &Path) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.add_directory("sub/", opts).unwrap();
        zip.start_file("sub/deep/x.txt", opts).unwrap();
        zip.write_all(b"deep file").unwrap();
        zip.start_file("a.txt", opts).unwrap();
        zip.write_all(b"hello zip").unwrap();
        zip.start_file("Readme.md", opts).unwrap();
        zip.write_all(&[b'r'; 5000]).unwrap();
        zip.finish().unwrap();
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        let mut names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        names
    }

    fn read_all(vfs: &dyn Vfs, files: &[PathBuf]) -> Vec<(usize, Vec<u8>)> {
        let mut out = Vec::new();
        vfs.read_files(files, &mut |i, r| {
            let mut buf = Vec::new();
            r.read_to_end(&mut buf)?;
            out.push((i, buf));
            Ok(true)
        })
        .unwrap();
        out.sort();
        out
    }

    #[test]
    fn splits_archive_paths() {
        let dir = TempDir::new("split");
        let zip = dir.0.join("a.zip");
        make_zip(&zip);
        fs::create_dir(dir.0.join("folder.zip")).unwrap();
        assert_eq!(split_archive(&dir.0), None);
        assert_eq!(split_archive(&zip), Some((zip.clone(), PathBuf::new())));
        assert_eq!(split_archive(&zip.join("sub").join("x")), Some((zip.clone(), PathBuf::from("sub").join("x"))));
        // A folder named like an archive is just a folder.
        assert_eq!(split_archive(&dir.0.join("folder.zip").join("x")), None);
        assert!(is_virtual(&zip.join("a.txt")));
        assert!(!is_virtual(&zip));
        assert!(in_archive(&zip));
        assert!(is_archive_name("X.ZIP") && is_archive_name("b.7z") && !is_archive_name("zip"));
        assert_eq!(format_of(OsStr::new("a.TAR.GZ")), Some(Format::TarGz));
        assert_eq!(format_of(OsStr::new("a.tbz2")), Some(Format::TarBz2));
        assert_eq!(format_of(OsStr::new("a.txt.gz")), Some(Format::Gz));
        assert_eq!(format_of(OsStr::new("a.txt.bz2")), None);
    }

    #[test]
    fn browses_zip() {
        let dir = TempDir::new("zip");
        let zip = dir.0.join("a.zip");
        make_zip(&zip);

        let root = list(&zip, true).unwrap();
        assert_eq!(names(&root), ["..", "Readme.md", "a.txt", "sub"]);
        assert_eq!(root[0].path, dir.0);
        let a = root.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!((a.kind, a.size, a.path.clone()), (EntryKind::File, 9, zip.join("a.txt")));
        assert!(a.modified.is_some());

        let sub = list(&zip.join("sub"), true).unwrap();
        assert_eq!(names(&sub), ["..", "deep"]);
        assert_eq!(sub[0].path, zip);
        // Implicit folder, and lookups ignore case.
        assert_eq!(names(&list(&zip.join("SUB").join("Deep"), true).unwrap()), ["..", "x.txt"]);
        assert!(list(&zip.join("nope"), true).is_err());
        assert!(list(&zip.join("a.txt"), true).is_err());

        let vfs = open(&zip).unwrap();
        assert!(vfs.writable());
        assert_eq!(vfs.stat(&zip.join("SUB").join("deep")).unwrap().map(|e| e.kind), Some(EntryKind::Dir));
        assert_eq!(vfs.stat(&zip.join("a.txt")).unwrap().map(|e| e.size), Some(9));
        assert!(vfs.stat(&zip.join("nope")).unwrap().is_none());
        let walked = vfs.walk(&zip.join("sub")).unwrap();
        assert_eq!(names(&walked), ["deep", "sub", "x.txt"]);
        assert_eq!(walked[0].name, "sub");
        assert_eq!(vfs.walk(&zip).unwrap().len(), 5);

        let files = [zip.join("Readme.md"), zip.join("sub").join("deep").join("x.txt")];
        let read = read_all(&*vfs, &files);
        assert_eq!(read, [(0, vec![b'r'; 5000]), (1, b"deep file".to_vec())]);
        assert!(vfs.read_files(&[zip.join("sub")], &mut |_, _| Ok(true)).is_err());
    }

    #[test]
    fn index_is_refreshed_when_the_archive_changes() {
        let dir = TempDir::new("cache");
        let zip = dir.0.join("c.zip");
        make_zip(&zip);
        assert_eq!(list(&zip, true).unwrap().len(), 4);
        let mut w = zip::ZipWriter::new(File::create(&zip).unwrap());
        w.start_file("only.txt", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b"x").unwrap();
        w.finish().unwrap();
        assert_eq!(names(&list(&zip, true).unwrap()), ["..", "only.txt"]);
    }

    #[test]
    fn extracts_to_temp() {
        let dir = TempDir::new("temp");
        let zip = dir.0.join("t.zip");
        make_zip(&zip);
        let inner = zip.join("sub").join("deep").join("x.txt");
        let local = extract_temp(&inner).unwrap();
        assert_eq!(local, temp_path(&inner));
        assert!(local.ends_with(Path::new("sub").join("deep").join("x.txt")));
        assert_eq!(fs::read(&local).unwrap(), b"deep file");
        // A folder with its contents, and a file.
        let all = extract_temp_all(&[zip.join("sub"), zip.join("a.txt")]).unwrap();
        assert_eq!(all, [temp_path(&zip.join("sub")), temp_path(&zip.join("a.txt"))]);
        assert_eq!(fs::read(all[0].join("deep").join("x.txt")).unwrap(), b"deep file");
        assert_eq!(fs::read(&all[1]).unwrap(), b"hello zip");
        // The archive's temp folder is marked as used, for clean_temp.
        let folder = local.parent().unwrap().parent().unwrap().parent().unwrap();
        let mut stamp = folder.as_os_str().to_os_string();
        stamp.push(USED);
        let age = fs::metadata(&stamp).unwrap().modified().unwrap().elapsed().unwrap_or_default();
        assert!(age < Duration::from_secs(60));
        let _ = fs::remove_dir_all(folder);
        let _ = fs::remove_file(&stamp);
        assert_eq!(extract_temp(&zip).unwrap(), zip);
    }

    #[test]
    fn cleans_old_temp_copies() {
        let dir = TempDir::new("clean");
        let root = &dir.0;
        let file = |rel: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
        };
        let stamp = |name: &str, age: Duration| {
            let f = File::create(root.join(format!("{name}{USED}"))).unwrap();
            f.set_modified(SystemTime::now() - age).unwrap();
        };
        let day = Duration::from_secs(24 * 60 * 60);
        file("aaaaaaaaaaaaaaaa/sub/f.txt");
        stamp("aaaaaaaaaaaaaaaa", 2 * day);
        file("bbbbbbbbbbbbbbbb/f.txt");
        stamp("bbbbbbbbbbbbbbbb", Duration::ZERO);
        stamp("cccccccccccccccc", 2 * day);
        file("copy-9-9/0/f.txt");
        file("other/f.txt");
        file("notes.txt");
        let exists = |rel: &str| root.join(rel).exists();

        clean_temp_in(root, TEMP_MAX_AGE);
        // Used two days ago: gone with its marker; a marker without a folder too.
        assert!(!exists("aaaaaaaaaaaaaaaa") && !exists("aaaaaaaaaaaaaaaa.used") && !exists("cccccccccccccccc.used"));
        // Used now, or (no marker) made now: kept. Not ours: kept.
        assert!(exists("bbbbbbbbbbbbbbbb/f.txt") && exists("bbbbbbbbbbbbbbbb.used") && exists("copy-9-9/0/f.txt"));
        assert!(exists("other/f.txt") && exists("notes.txt"));

        // A file in use (not shared for deleting) stays, with its folders.
        #[cfg(windows)]
        let open = {
            use std::os::windows::fs::OpenOptionsExt;
            File::options().read(true).share_mode(0).open(root.join("bbbbbbbbbbbbbbbb/f.txt")).unwrap()
        };
        std::thread::sleep(Duration::from_millis(20));
        clean_temp_in(root, Duration::ZERO);
        assert!(!exists("copy-9-9") && exists("other/f.txt") && exists("notes.txt"));
        #[cfg(windows)]
        {
            assert!(exists("bbbbbbbbbbbbbbbb/f.txt") && exists("bbbbbbbbbbbbbbbb.used"));
            drop(open);
            clean_temp_in(root, Duration::ZERO);
        }
        assert!(!exists("bbbbbbbbbbbbbbbb") && !exists("bbbbbbbbbbbbbbbb.used"));
    }

    #[test]
    fn browses_7z() {
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
        let dir = TempDir::new("7z");
        let path = dir.0.join("b.7z");
        let mut w = ArchiveWriter::create(&path).unwrap();
        w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory("docs"), None).unwrap();
        w.push_archive_entry(ArchiveEntry::new_file("docs/one.txt"), Some(&b"first"[..])).unwrap();
        w.push_archive_entry(ArchiveEntry::new_file("docs/two.txt"), Some(&b"second!"[..])).unwrap();
        w.push_archive_entry(ArchiveEntry::new_file("empty.txt"), Some(&b""[..])).unwrap();
        w.finish().unwrap();

        assert_eq!(names(&list(&path, true).unwrap()), ["..", "docs", "empty.txt"]);
        let docs = list(&path.join("docs"), true).unwrap();
        assert_eq!(names(&docs), ["..", "one.txt", "two.txt"]);
        assert_eq!(docs.iter().find(|e| e.name == "two.txt").unwrap().size, 7);

        let vfs = open(&path).unwrap();
        let files = [path.join("docs").join("two.txt"), path.join("empty.txt"), path.join("docs").join("one.txt")];
        let read = read_all(&*vfs, &files);
        assert_eq!(read, [(0, b"second!".to_vec()), (1, Vec::new()), (2, b"first".to_vec())]);

        // Solid block: a file in the middle and the last one.
        let solid = dir.0.join("s.7z");
        let mut w = ArchiveWriter::create(&solid).unwrap();
        let entries = ["s/1", "s/2", "s/3"].map(ArchiveEntry::new_file).to_vec();
        let data: [&[u8]; 3] = [b"one", b"two", b"three"];
        w.push_archive_entries(entries, data.into_iter().map(Into::into).collect()).unwrap();
        w.finish().unwrap();
        let vfs = open(&solid).unwrap();
        let read = read_all(&*vfs, &[solid.join("s").join("3"), solid.join("s").join("2")]);
        assert_eq!(read, [(0, b"three".to_vec()), (1, b"two".to_vec())]);
    }

    /// A tar with `docs/` (explicit), `docs/one.txt`, `deep/x/two.txt`
    /// (implicit folders), a symlink (left out) and `top.txt`.
    pub(crate) fn make_tar(out: impl Write) {
        let mut b = tar::Builder::new(out);
        let mut add = |name: &str, kind: tar::EntryType, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(1_600_000_000);
            if kind == tar::EntryType::Symlink {
                h.set_link_name("top.txt").unwrap();
            }
            b.append_data(&mut h, name, data).unwrap();
        };
        add("docs/", tar::EntryType::Directory, b"");
        add("docs/one.txt", tar::EntryType::Regular, b"first");
        add("deep/x/two.txt", tar::EntryType::Regular, b"second!");
        add("link", tar::EntryType::Symlink, b"");
        add("top.txt", tar::EntryType::Regular, &[b't'; 3000]);
        b.into_inner().unwrap();
    }

    #[test]
    fn browses_tar_formats() {
        let dir = TempDir::new("tar");
        let plain = dir.0.join("a.tar");
        make_tar(File::create(&plain).unwrap());
        let gz = dir.0.join("b.tar.gz");
        make_tar(flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default()));
        let bz = dir.0.join("c.tbz2");
        make_tar(bzip2::write::BzEncoder::new(File::create(&bz).unwrap(), bzip2::Compression::default()));
        for path in [plain, gz, bz] {
            assert_eq!(names(&list(&path, true).unwrap()), ["..", "deep", "docs", "top.txt"], "{path:?}");
            let docs = list(&path.join("docs"), true).unwrap();
            let one = docs.iter().find(|e| e.name == "one.txt").unwrap();
            assert_eq!(one.size, 5);
            assert_eq!(one.modified.unwrap().timestamp(), 1_600_000_000);
            let vfs = open(&path).unwrap();
            assert!(vfs.writable());
            let files = [path.join("top.txt"), path.join("deep/x/two.txt"), path.join("docs/one.txt")];
            let read = read_all(&*vfs, &files);
            assert_eq!(read, [(0, vec![b't'; 3000]), (1, b"second!".to_vec()), (2, b"first".to_vec())]);
        }

        let single = dir.0.join("notes.txt.gz");
        let mut w = flate2::write::GzEncoder::new(File::create(&single).unwrap(), flate2::Compression::default());
        w.write_all(b"gzipped text").unwrap();
        w.finish().unwrap();
        let entries = list(&single, true).unwrap();
        assert_eq!(names(&entries), ["..", "notes.txt"]);
        assert_eq!(entries[1].size, 12);
        let vfs = open(&single).unwrap();
        assert_eq!(read_all(&*vfs, &[single.join("notes.txt")]), [(0, b"gzipped text".to_vec())]);
    }

    #[test]
    fn reads_encrypted_archives_with_a_password() {
        let dir = TempDir::new("password");
        let zip = dir.0.join("secret.zip");
        let mut w = zip::ZipWriter::new(File::create(&zip).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("plain.txt", opts).unwrap();
        w.write_all(b"open").unwrap();
        w.start_file("sub/hidden.txt", opts.with_aes_encryption(zip::AesMode::Aes256, "s3cret")).unwrap();
        w.write_all(b"classified").unwrap();
        w.finish().unwrap();

        // Listing works; reading the encrypted file needs the password.
        assert_eq!(names(&list(&zip, true).unwrap()), ["..", "plain.txt", "sub"]);
        assert!(needs_password(&zip));
        let vfs = open(&zip).unwrap();
        assert_eq!(read_all(&*vfs, &[zip.join("plain.txt")]), [(0, b"open".to_vec())]);
        let err = vfs.read_files(&[zip.join("sub/hidden.txt")], &mut |_, _| Ok(true)).unwrap_err();
        assert!(err.to_string().contains(PASSWORD_REQUIRED), "{err}");
        assert!(!try_password(&zip.join("sub"), "wrong").unwrap());
        assert!(needs_password(&zip));
        assert!(try_password(&zip, "s3cret").unwrap());
        assert!(!needs_password(&zip.join("sub")));
        let vfs = open(&zip).unwrap();
        assert_eq!(read_all(&*vfs, &[zip.join("sub/hidden.txt")]), [(0, b"classified".to_vec())]);

        // 7z with encrypted names: listing needs the password.
        use sevenz_rust2::{encoder_options::AesEncoderOptions, ArchiveEntry, ArchiveWriter, EncoderMethod};
        let seven = dir.0.join("secret.7z");
        let mut w = ArchiveWriter::create(&seven).unwrap();
        w.set_content_methods(vec![
            AesEncoderOptions::new("pw7".into()).into(),
            EncoderMethod::LZMA2.into(),
        ]);
        w.set_encrypt_header(true);
        w.push_archive_entry(ArchiveEntry::new_file("x.txt"), Some(&b"seven secret"[..])).unwrap();
        w.finish().unwrap();
        let err = list(&seven, true).unwrap_err();
        assert!(err.to_string().contains(PASSWORD_REQUIRED), "{err}");
        assert!(!try_password(&seven, "nope").unwrap());
        assert!(try_password(&seven, "pw7").unwrap());
        assert_eq!(names(&list(&seven, true).unwrap()), ["..", "x.txt"]);
        let vfs = open(&seven).unwrap();
        assert_eq!(read_all(&*vfs, &[seven.join("x.txt")]), [(0, b"seven secret".to_vec())]);
    }

    #[test]
    fn browses_nested_archives() {
        let dir = TempDir::new("nested");
        let outer = dir.0.join("outer.zip");
        let mut inner = Vec::new();
        make_tar(&mut inner);
        let mut w = zip::ZipWriter::new(File::create(&outer).unwrap());
        w.start_file("pack/inner.tar", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(&inner).unwrap();
        w.add_directory("pack/folder.zip/", zip::write::SimpleFileOptions::default()).unwrap();
        w.finish().unwrap();

        let tar = outer.join("pack").join("inner.tar");
        let deep = tar.join("docs").join("one.txt");
        assert_eq!(split_archive(&deep), Some((tar.clone(), PathBuf::from("docs").join("one.txt"))));
        assert_eq!(names(&list(&tar, true).unwrap()), ["..", "deep", "docs", "top.txt"]);
        assert_eq!(list(&tar, true).unwrap()[0].path, outer.join("pack"));
        let vfs = open(&deep).unwrap();
        assert_eq!(read_all(&*vfs, std::slice::from_ref(&deep)), [(0, b"first".to_vec())]);
        assert_eq!(fs::read(extract_temp(&deep).unwrap()).unwrap(), b"first");
        // Read-only; a folder named like an archive stays a folder.
        assert!(vfs.writable() && archive_writable(&deep) == Some(true));
        assert_eq!(split_archive(&outer.join("pack/folder.zip/x")), Some((outer.clone(), PathBuf::from("pack/folder.zip/x"))));
        let _ = fs::remove_dir_all(temp_path(&outer.join("pack")).parent().unwrap());
        let _ = fs::remove_dir_all(temp_path(&deep).parent().unwrap().parent().unwrap());
    }

    #[test]
    fn rejects_names_leaving_the_archive() {
        assert_eq!(clean_parts("a/./b\\c"), Some(vec!["a", "b", "c"]));
        assert_eq!(clean_parts("../evil"), None);
        assert_eq!(clean_parts("C:/evil"), None);
        assert_eq!(clean_parts("/"), None);
    }

    /// Copies the rar fixture `name` (from the `unrar` crate's test data,
    /// see testdata/rar/SOURCE.txt) into `dir`.
    fn rar_fixture(dir: &TempDir, name: &str) -> PathBuf {
        let path = dir.0.join(name);
        fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("rar").join(name), &path).unwrap();
        path
    }

    #[test]
    fn browses_rar() {
        let dir = TempDir::new("rar");
        assert!(is_archive_name("x.RAR") && is_archive_name("x.part1.rar"));
        // rar4, one file.
        let rar = rar_fixture(&dir, "version.rar");
        assert_eq!(names(&list(&rar, true).unwrap()), ["..", "VERSION"]);
        let vfs = open(&rar).unwrap();
        let version = vfs.stat(&rar.join("version")).unwrap().unwrap();
        assert_eq!((version.kind, version.size), (EntryKind::File, 11));
        assert!(version.modified.is_some_and(|d| d.format("%Y").to_string() == "2015"));
        assert_eq!(read_all(&*vfs, &[rar.join("VERSION")]), [(0, b"unrar-0.4.0".to_vec())]);
        assert_eq!(fs::read(extract_temp(&rar.join("VERSION")).unwrap()).unwrap(), b"unrar-0.4.0");
        // Read-only.
        assert!(!vfs.writable() && archive_writable(&rar) == Some(false));
        assert!(vfs.make_dir(&rar.join("new")).is_err());

        // rar5 (solid), and a Unicode name.
        let solid = rar_fixture(&dir, "solid.rar");
        let vfs = open(&solid).unwrap();
        assert_eq!(read_all(&*vfs, &[solid.join(".gitignore")]), [(0, b"target\nCargo.lock\n".to_vec())]);
        let unicode = rar_fixture(&dir, "unicode.rar");
        assert_eq!(names(&list(&unicode, true).unwrap()), ["..", "te\u{2026}\u{2015}st\u{270c}"]);
        let vfs = open(&unicode).unwrap();
        assert_eq!(read_all(&*vfs, &[unicode.join("te\u{2026}\u{2015}st\u{270c}")])[0].1.len(), 17);
    }

    #[test]
    fn browses_rar_volumes_and_folders() {
        // The first volume of a multi-volume rar4 whose second volume is
        // missing: what it holds is listed and read, the file running into
        // the missing volume fails clearly.
        let dir = TempDir::new("rar-volumes");
        let rar = rar_fixture(&dir, "archive.part1.rar");
        assert_eq!(names(&list(&rar, true).unwrap()), ["..", "Cargo.toml", "build.rs", "examples", "src", "vendor"]);
        let unrar = rar.join("vendor").join("unrar");
        assert_eq!(names(&list(&unrar, true).unwrap()), ["..", "acknow.txt", "arccmt.cpp", "archive.cpp"]);
        assert_eq!(list(&unrar, true).unwrap()[0].path, rar.join("vendor"));
        let vfs = open(&rar).unwrap();
        assert_eq!(vfs.walk(&rar.join("vendor")).unwrap().len(), 5);
        // Several files at once, out of archive order, keep their indexes.
        let files = [rar.join("examples/lister.rs"), rar.join("build.rs"), unrar.join("acknow.txt")];
        let read = read_all(&*vfs, &files);
        assert_eq!(read.iter().map(|(i, b)| (*i, b.len())).collect::<Vec<_>>(), [(0, 2693), (1, 2396), (2, 4280)]);
        // Stopping early is fine.
        let mut seen = 0;
        vfs.read_files(&files, &mut |_, _| {
            seen += 1;
            Ok(false)
        })
        .unwrap();
        assert_eq!(seen, 1);
        let err = vfs.read_files(&[unrar.join("archive.cpp")], &mut |_, r| io::copy(r, &mut io::sink()).map(|_| true));
        assert!(err.unwrap_err().to_string().contains("volume"));
        let copy = extract_temp(&rar.join("src").join("lib.rs")).unwrap();
        assert_eq!(fs::metadata(copy).unwrap().len(), 7870);
    }

    #[test]
    fn reads_encrypted_rars_with_a_password() {
        let dir = TempDir::new("rar-password");
        // rar4 with encrypted files: listing works, reading needs it.
        let rar = rar_fixture(&dir, "crypted.rar");
        assert_eq!(names(&list(&rar, true).unwrap()), ["..", ".gitignore"]);
        assert!(needs_password(&rar));
        let vfs = open(&rar).unwrap();
        let err = vfs.read_files(&[rar.join(".gitignore")], &mut |_, r| io::copy(r, &mut io::sink()).map(|_| true));
        assert!(err.unwrap_err().to_string().contains(PASSWORD_REQUIRED));
        assert!(!try_password(&rar, "wrong").unwrap());
        assert!(try_password(&rar, "unrar").unwrap());
        assert!(!needs_password(&rar));
        let vfs = open(&rar).unwrap();
        assert_eq!(read_all(&*vfs, &[rar.join(".gitignore")]), [(0, b"target\nCargo.lock\n".to_vec())]);

        // rar5 with encrypted names: listing needs it.
        let hidden = rar_fixture(&dir, "comment-hpw-password.rar");
        let err = list(&hidden, true).unwrap_err();
        assert!(err.to_string().contains(PASSWORD_REQUIRED), "{err}");
        assert!(!try_password(&hidden, "nope").unwrap());
        assert!(try_password(&hidden, "password").unwrap());
        assert_eq!(names(&list(&hidden, true).unwrap()), ["..", ".gitignore"]);
        let vfs = open(&hidden).unwrap();
        assert_eq!(read_all(&*vfs, &[hidden.join(".gitignore")]), [(0, b"target\nCargo.lock\n".to_vec())]);
    }
}
