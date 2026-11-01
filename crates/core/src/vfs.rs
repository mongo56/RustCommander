//! Virtual file systems: what a panel can show besides local folders.
//!
//! Paths stay plain `PathBuf`s. A path that runs through an archive file
//! (`C:\x\a.zip\sub\f.txt`) is served by that archive's [`Vfs`]; anything
//! else is the local file system ([`LocalFs`]). Archives can be listed,
//! walked and read (copy out, view); zips can also be changed (see
//! `zip_edit`, driven by the jobs in `ops`), the others are read-only.
//! Supported: zip (`.zip`, `.jar`) via the `zip` crate, 7z via
//! `sevenz-rust2`, tar (`.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`) via
//! `tar` + `flate2`/`bzip2`, and a single gzipped file (`.gz`). Parsed
//! archive indexes are cached by path, size and modification time, so
//! browsing an archive reads its directory once (a tar has no directory:
//! listing reads it all, and so does reading files from it).

use crate::entry::{Entry, EntryKind};
use chrono::{DateTime, Local, TimeZone};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

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
enum Format {
    Zip,
    SevenZ,
    Tar,
    TarGz,
    TarBz2,
    /// One gzipped file (`x.txt.gz` holds `x.txt`).
    Gz,
}

/// The archive format a file name stands for (by extension), if any.
fn format_of(name: &OsStr) -> Option<Format> {
    let name = name.to_string_lossy().to_ascii_lowercase();
    let ext = Path::new(&name).extension()?.to_str()?;
    let inner_tar = name.trim_end_matches(&format!(".{ext}")).ends_with(".tar");
    match ext {
        "zip" | "jar" => Some(Format::Zip),
        "7z" => Some(Format::SevenZ),
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
/// Only names with an archive extension are checked on disk.
pub fn split_archive(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut archive = PathBuf::new();
    let mut parts = path.components();
    while let Some(part) = parts.next() {
        archive.push(part);
        if let Component::Normal(name) = part {
            if format_of(name).is_some() && archive.is_file() {
                return Some((archive, parts.as_path().to_path_buf()));
            }
        }
    }
    None
}

/// True for a path *inside* an archive (not the archive file itself).
pub fn is_virtual(path: &Path) -> bool {
    split_archive(path).is_some_and(|(_, inner)| !inner.as_os_str().is_empty())
}

/// True for an archive file or a path inside one: anything not listed by
/// the local file system.
pub fn in_archive(path: &Path) -> bool {
    split_archive(path).is_some()
}

/// For an archive or a path inside one: whether the archive can be changed
/// (zip yes, 7z no). `None` for local paths.
pub fn archive_writable(path: &Path) -> Option<bool> {
    let (file, _) = split_archive(path)?;
    Some(file.file_name().and_then(format_of) == Some(Format::Zip))
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
    match split_archive(path) {
        Some((file, _)) => Ok(archive(&file)?),
        None => Ok(Arc::new(LocalFs)),
    }
}

/// Lists the folder `path` wherever it lives (local folder or a folder in
/// an archive), leaving out hidden entries unless `show_hidden`.
pub fn list(path: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    match split_archive(path) {
        None => crate::fs::list_dir(path, show_hidden),
        Some((file, _)) => {
            let mut entries = archive(&file)?.list(path)?;
            if !show_hidden {
                entries.retain(|e| !e.hidden);
            }
            Ok(entries)
        }
    }
}

/// Where [`extract_temp`] puts the copy of a file inside an archive (the
/// path itself for local files): a folder per archive under the temp dir.
pub fn temp_path(path: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let Some((file, inner)) = split_archive(path).filter(|(_, inner)| !inner.as_os_str().is_empty()) else {
        return path.to_path_buf();
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file.hash(&mut hasher);
    let mut out = std::env::temp_dir().join("RustCommander").join(format!("{:016x}", hasher.finish()));
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
    file: PathBuf,
    format: Format,
    items: Vec<Item>,
    /// Item index by lowercased inner path.
    by_path: HashMap<String, usize>,
    /// Children of each folder (lowercased inner path, `""` = root).
    children: HashMap<String, Vec<usize>>,
}

/// A raw entry as the format reader reports it.
struct Raw {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<DateTime<Local>>,
    hidden: bool,
    source: usize,
}

/// Cached archives: (file, size, mtime) → parsed index; most recent first.
type Cache = Vec<(PathBuf, u64, Option<SystemTime>, Arc<Archive>)>;
static CACHE: Mutex<Cache> = Mutex::new(Vec::new());
const CACHE_SIZE: usize = 4;

/// The parsed archive `file`, from the cache if it hasn't changed.
fn archive(file: &Path) -> io::Result<Arc<Archive>> {
    let format = file.file_name().and_then(format_of).ok_or_else(|| io::Error::other("Not an archive"))?;
    let meta = fs::metadata(file)?;
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
        Format::Zip => read_zip_index(file)?,
        Format::SevenZ => read_7z_index(file)?,
        Format::Tar | Format::TarGz | Format::TarBz2 => read_tar_index(file, format)?,
        Format::Gz => read_gz_index(file)?,
    };
    let archive = Arc::new(Archive::build(file.to_path_buf(), format, raw));
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
        });
    }
    Ok(raw)
}

fn read_7z_index(file: &Path) -> io::Result<Vec<Raw>> {
    let archive = sevenz_rust2::Archive::open(file).map_err(io_err)?;
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
        });
    }
    Ok(raw)
}

/// The decompressed contents of a tar-like or gz `file`.
fn open_stream(file: &Path, format: Format) -> io::Result<Box<dyn Read>> {
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
    Ok(vec![Raw { name, is_dir: false, size: u32::from_le_bytes(size).into(), modified, hidden: false, source: 0 }])
}

impl Archive {
    fn build(file: PathBuf, format: Format, raw: Vec<Raw>) -> Self {
        let mut archive =
            Archive { file, format, items: Vec::new(), by_path: HashMap::new(), children: HashMap::new() };
        archive.children.insert(String::new(), Vec::new());
        for r in raw {
            let Some(parts) = clean_parts(&r.name) else { continue };
            let parts: Vec<String> = parts.into_iter().map(str::to_owned).collect();
            // Implicit parent folders.
            for depth in 1..parts.len() {
                archive.add(&parts[..depth], None);
            }
            archive.add(&parts, Some(r));
        }
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
        let mut zip = zip::ZipArchive::new(BufReader::new(File::open(&self.file)?)).map_err(io_err)?;
        for (i, source) in wanted {
            let Some(source) = source else { continue };
            let mut f = zip.by_index(source).map_err(io_err)?;
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
                let mut stream = open_stream(&self.file, self.format)?;
                for i in indexes {
                    if !each(i, &mut stream)? {
                        break;
                    }
                }
            }
            return Ok(());
        }
        let mut tar = tar::Archive::new(open_stream(&self.file, self.format)?);
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
        let password = sevenz_rust2::Password::empty();
        let archive = sevenz_rust2::Archive::open(&self.file).map_err(io_err)?;
        let mut source = BufReader::new(File::open(&self.file)?);
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
                return Err(e);
            }
            result.map_err(io_err)?;
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
        self.format == Format::Zip
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
        let _ = fs::remove_dir_all(local.parent().unwrap().parent().unwrap().parent().unwrap());
        assert_eq!(extract_temp(&zip).unwrap(), zip);
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
    fn make_tar(out: impl Write) {
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
            assert!(!vfs.writable());
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
    fn rejects_names_leaving_the_archive() {
        assert_eq!(clean_parts("a/./b\\c"), Some(vec!["a", "b", "c"]));
        assert_eq!(clean_parts("../evil"), None);
        assert_eq!(clean_parts("C:/evil"), None);
        assert_eq!(clean_parts("/"), None);
    }
}
