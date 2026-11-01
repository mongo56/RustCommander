//! Changing zip archives. A zip can't be edited in place, so every change
//! rewrites it: entries that stay are copied raw (no recompression) into a
//! temp file next to the archive, new entries are added, and the temp file
//! then replaces the archive in one rename. Until [`ZipEdit::commit`] the
//! archive is untouched; dropping an uncommitted edit removes the temp file.

use crate::tar_edit::TarEdit;
use crate::vfs::{self, clean_parts, Format};
use chrono::{DateTime, Datelike, Local, Timelike};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

/// What happens to the entries already in the archive. Paths are inner
/// paths (`/`-separated); matching ignores case, and a folder's path also
/// matches everything under it.
#[derive(Debug, Default, Clone)]
pub(crate) struct Edits {
    /// Dropped.
    pub remove: Vec<String>,
    /// `(from, to, keep)`: re-stored under `to` (moved or, with `keep`,
    /// copied).
    pub relocate: Vec<(String, String, bool)>,
}

/// The lowercased `/`-joined form of an inner path, for matching.
pub(crate) fn key(inner: &str) -> String {
    clean_parts(inner).map(|p| p.join("/").to_lowercase()).unwrap_or_default()
}

/// The number of components of `prefix` that `key` starts with, if it's
/// `prefix` itself or under it.
pub(crate) fn matches(key: &str, prefix: &str) -> Option<usize> {
    let under = key.len() > prefix.len() && key.starts_with(prefix) && key.as_bytes()[prefix.len()] == b'/';
    (key == prefix || under).then(|| prefix.split('/').count())
}

pub(crate) struct ZipEdit {
    archive: PathBuf,
    temp: PathBuf,
    writer: Option<ZipWriter<BufWriter<File>>>,
    /// Keys of the folder entries written so far.
    dirs: HashSet<String>,
}

impl ZipEdit {
    /// Starts rewriting `archive` (which may not exist yet: a new zip).
    pub fn begin(archive: &Path) -> io::Result<ZipEdit> {
        let name = archive.file_name().ok_or_else(|| io::Error::other("Invalid archive path"))?;
        let temp = archive.with_file_name(format!(".{}.{}.rctmp", name.to_string_lossy(), std::process::id()));
        let file = File::create(&temp)?;
        let writer = ZipWriter::new(BufWriter::new(file));
        Ok(ZipEdit { archive: archive.to_path_buf(), temp, writer: Some(writer), dirs: HashSet::new() })
    }

    fn writer(&mut self) -> &mut ZipWriter<BufWriter<File>> {
        self.writer.as_mut().expect("writer is present until commit")
    }

    /// Copies the archive's current entries (if it exists) as `edits` say.
    /// `alive` is asked between entries; Ok(false) means it said stop.
    pub fn copy_existing(&mut self, edits: &Edits, alive: &mut dyn FnMut() -> bool) -> io::Result<bool> {
        if !self.archive.exists() {
            return Ok(true);
        }
        let mut source = ZipArchive::new(BufReader::new(File::open(&self.archive)?)).map_err(io_err)?;
        let remove: Vec<String> = edits.remove.iter().map(|r| key(r)).collect();
        let relocate: Vec<(String, &str, bool)> = edits.relocate.iter().map(|(f, t, k)| (key(f), t.as_str(), *k)).collect();
        for i in 0..source.len() {
            if !alive() {
                return Ok(false);
            }
            let (parts, is_dir) = {
                let f = source.by_index_raw(i).map_err(io_err)?;
                let parts: Option<Vec<String>> =
                    f.name().ok().and_then(|n| clean_parts(&n).map(|p| p.into_iter().map(str::to_owned).collect()));
                (parts, f.is_dir())
            };
            // Names that can't be shown (`../x`) are kept as they are.
            let Some(parts) = parts else {
                self.copy_raw(&mut source, i, None, is_dir)?;
                continue;
            };
            let key = parts.join("/").to_lowercase();
            if remove.iter().any(|r| matches(&key, r).is_some()) {
                continue;
            }
            let moved = relocate.iter().find_map(|(from, to, keep)| matches(&key, from).map(|depth| (depth, *to, *keep)));
            match moved {
                Some((depth, to, keep)) => {
                    if keep {
                        self.copy_raw(&mut source, i, None, is_dir)?;
                    }
                    let mut name = to.trim_end_matches('/').to_owned();
                    for part in &parts[depth..] {
                        name.push('/');
                        name.push_str(part);
                    }
                    self.copy_raw(&mut source, i, Some(name), is_dir)?;
                }
                None => self.copy_raw(&mut source, i, None, is_dir)?,
            }
        }
        Ok(true)
    }

    /// Copies entry `i` of `source`, renamed to `name` (inner path) if given.
    fn copy_raw(&mut self, source: &mut ZipArchive<BufReader<File>>, i: usize, name: Option<String>, is_dir: bool)
        -> io::Result<()> {
        let file = source.by_index_raw(i).map_err(io_err)?;
        // The zip crate copies ZipCrypto entries without their encryption
        // flag, which would leave them unreadable; AES ones survive.
        if file.encrypted() && !file.extra_data_fields().any(|e| matches!(e, zip::ExtraField::AeXEncryption(_))) {
            return Err(io::Error::other(
                "This zip has files encrypted with ZipCrypto, which can't be kept when changing it",
            ));
        }
        let written = name.clone().unwrap_or_else(|| file.name().unwrap_or_default().into_owned());
        if is_dir {
            self.dirs.insert(key(&written));
        }
        match name {
            Some(name) => {
                let name = if is_dir { format!("{name}/") } else { name };
                self.writer().raw_copy_file_rename(file, name)
            }
            None => self.writer().raw_copy_file(file),
        }
        .map_err(io_err)
    }

    /// Adds a folder entry (unless one was written already).
    pub fn add_dir(&mut self, inner: &str, modified: Option<SystemTime>) -> io::Result<()> {
        if !self.dirs.insert(key(inner)) {
            return Ok(());
        }
        let opts = options(modified, 0);
        self.writer().add_directory(format!("{}/", inner.trim_end_matches('/')), opts).map_err(io_err)
    }

    /// Starts a file entry; write its contents with [`Self::output`], then
    /// start the next one (or [`Self::abort_file`] on failure).
    pub fn start_file(&mut self, inner: &str, modified: Option<SystemTime>, size: u64) -> io::Result<()> {
        let opts = options(modified, size);
        self.writer().start_file(inner, opts).map_err(io_err)
    }

    pub fn output(&mut self) -> &mut dyn Write {
        self.writer()
    }

    /// Drops the file being written.
    pub fn abort_file(&mut self) -> io::Result<()> {
        self.writer().abort_file().map_err(io_err)
    }

    /// Finishes the new archive and puts it in place of the old one.
    pub fn commit(mut self) -> io::Result<()> {
        let writer = self.writer.take().expect("commit runs once");
        let file = writer.finish().map_err(io_err)?.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.temp, &self.archive)?;
        crate::vfs::forget(&self.archive);
        Ok(())
    }
}

impl Drop for ZipEdit {
    fn drop(&mut self) {
        drop(self.writer.take());
        let _ = fs::remove_file(&self.temp);
    }
}

/// Deflate, the file's time (zip stores local time, 2 s steps, 1980-2107)
/// and zip64 when it's big.
fn options(modified: Option<SystemTime>, size: u64) -> SimpleFileOptions {
    let mut opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(size >= 0xFFFF_FFFF);
    let time = modified.map(DateTime::<Local>::from).and_then(|t| {
        let year = u16::try_from(t.year()).ok()?;
        let (mo, d, h, mi, s) = (t.month() as u8, t.day() as u8, t.hour() as u8, t.minute() as u8, t.second() as u8);
        zip::DateTime::from_date_and_time(year, mo, d, h, mi, s).ok()
    });
    if let Some(time) = time {
        opts = opts.last_modified_time(time);
    }
    opts
}

/// A zip or tar archive being rewritten (see [`ZipEdit`], [`TarEdit`]).
pub(crate) enum Rewrite {
    Zip(Box<ZipEdit>),
    Tar(Box<TarEdit>),
}

impl Rewrite {
    /// Starts rewriting `archive`, which must be a zip or a tar (plain, gz
    /// or bz2) on disk; it may not exist yet.
    pub fn begin(archive: &Path) -> io::Result<Rewrite> {
        match archive.file_name().and_then(vfs::format_of) {
            Some(Format::Zip) => Ok(Rewrite::Zip(Box::new(ZipEdit::begin(archive)?))),
            Some(f @ (Format::Tar | Format::TarGz | Format::TarBz2)) => Ok(Rewrite::Tar(Box::new(TarEdit::begin(archive, f)?))),
            _ => Err(io::Error::other("Only zip and tar archives can be changed")),
        }
    }

    pub fn copy_existing(&mut self, edits: &Edits, alive: &mut dyn FnMut() -> bool) -> io::Result<bool> {
        match self {
            Rewrite::Zip(e) => e.copy_existing(edits, alive),
            Rewrite::Tar(e) => e.copy_existing(edits, alive),
        }
    }

    pub fn add_dir(&mut self, inner: &str, modified: Option<SystemTime>) -> io::Result<()> {
        match self {
            Rewrite::Zip(e) => e.add_dir(inner, modified),
            Rewrite::Tar(e) => e.add_dir(inner, modified),
        }
    }

    pub fn start_file(&mut self, inner: &str, modified: Option<SystemTime>, size: u64) -> io::Result<()> {
        match self {
            Rewrite::Zip(e) => e.start_file(inner, modified, size),
            Rewrite::Tar(e) => e.start_file(inner, modified),
        }
    }

    pub fn output(&mut self) -> &mut dyn Write {
        match self {
            Rewrite::Zip(e) => e.output(),
            Rewrite::Tar(e) => e.output(),
        }
    }

    pub fn abort_file(&mut self) -> io::Result<()> {
        match self {
            Rewrite::Zip(e) => e.abort_file(),
            Rewrite::Tar(e) => e.abort_file(),
        }
    }

    pub fn commit(self) -> io::Result<()> {
        match self {
            Rewrite::Zip(e) => (*e).commit(),
            Rewrite::Tar(e) => (*e).commit(),
        }
    }
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
