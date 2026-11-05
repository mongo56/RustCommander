//! Changing zip archives. A zip can't be edited in place, so every change
//! rewrites it: entries that stay are copied raw (no recompression) into a
//! temp file next to the archive, new entries are added, and the temp file
//! then replaces the archive in one rename. Entries encrypted with ZipCrypto
//! are copied encrypted and get their encryption flag put back at `commit`.
//! Until [`ZipEdit::commit`] the archive is untouched; dropping an
//! uncommitted edit removes the temp file.

use crate::gz_edit::GzEdit;
use crate::sevenz_edit::SevenZEdit;
use crate::tar_edit::TarEdit;
use crate::vfs::{self, clean_parts, Format};
use chrono::{DateTime, Datelike, Local, Timelike};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Seek, SeekFrom, Write};
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
    /// Stored name bytes of entries copied raw while encrypted (ZipCrypto).
    encrypted: Vec<Vec<u8>>,
    /// New files are encrypted with this (AES-256); only for a new zip.
    password: Option<String>,
}

impl ZipEdit {
    /// Starts rewriting `archive` (which may not exist yet: a new zip). With
    /// a `password` the files added are encrypted with it (AES-256;
    /// folder entries and names stay plain, as zip has no way to hide them).
    /// Only for a new zip: [`Self::copy_existing`] fails if it exists.
    pub fn begin(archive: &Path, password: Option<&str>) -> io::Result<ZipEdit> {
        let name = archive.file_name().ok_or_else(|| io::Error::other("Invalid archive path"))?;
        let temp = archive.with_file_name(format!(".{}.{}.rctmp", name.to_string_lossy(), std::process::id()));
        let file = File::create(&temp)?;
        let writer = ZipWriter::new(BufWriter::new(file));
        let password = password.map(str::to_owned);
        Ok(ZipEdit { archive: archive.to_path_buf(), temp, writer: Some(writer), dirs: HashSet::new(), encrypted: Vec::new(), password })
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
        if self.password.is_some() {
            return Err(io::Error::other("A password can only be set on a new archive, and this one exists"));
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
        // The stored name bytes the writer will put down for this entry.
        let renamed = name.map(|n| if is_dir { format!("{n}/") } else { n });
        // The stored name bytes the writer will put down for this entry.
        let stored = match renamed.as_ref() {
            Some(n) => n.as_bytes().to_owned(),
            None => file.name_raw().to_owned(),
        };
        let written = renamed.clone().unwrap_or_else(|| file.name().unwrap_or_default().into_owned());
        // The zip crate's raw copy writes ZipCrypto entries without their
        // encryption flag (AES ones keep theirs through their AE-x field); the
        // encrypted bytes themselves are copied as they are, so the flag is
        // put back over those entries in the temp file at `commit`.
        if file.encrypted() && !file.extra_data_fields().any(|e| matches!(e, zip::ExtraField::AeXEncryption(_))) {
            self.encrypted.push(stored);
        }
        if is_dir {
            self.dirs.insert(key(&written));
        }
        match renamed {
            Some(name) => self.writer().raw_copy_file_rename(file, name),
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
        let writer = self.writer.as_mut().expect("writer is present until commit");
        match &self.password {
            Some(password) => writer.start_file(inner, opts.with_aes_encryption(zip::AesMode::Aes256, password)),
            None => writer.start_file(inner, opts),
        }
        .map_err(io_err)
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
        self.restore_encrypted_flags(file)?;
        fs::rename(&self.temp, &self.archive)?;
        crate::vfs::forget(&self.archive);
        Ok(())
    }

    /// Sets the encryption flag (bit 0) back on the local header and central
    /// directory record of the entries in `encrypted` (stored name bytes, in
    /// the order copied): their data was copied raw, still encrypted, but the
    /// zip crate wrote them unflagged. `file` is the finished temp archive,
    /// the handle the entries were written through.
    fn restore_encrypted_flags(&mut self, file: File) -> io::Result<()> {
        let mut names: Vec<Vec<u8>> = self.encrypted.clone();
        self.encrypted.clear();
        if names.is_empty() {
            return Ok(());
        }
        // The flag bytes as the reader sees them (through a plain read
        // handle), encrypted bit set.
        let mut flags: Vec<(u64, u8)> = Vec::new();
        {
            let mut source = ZipArchive::new(BufReader::new(File::open(&self.temp)?)).map_err(io_err)?;
            for i in 0..source.len() {
                let entry = source.by_index_raw(i).map_err(io_err)?;
                if let Some(j) = names.iter().position(|n| n == entry.name_raw()) {
                    let flag = (entry.flags().as_u16() | 1) as u8;
                    flags.extend([
                        (entry.central_header_start() + 8, flag),
                        (entry.header_start() + 6, flag),
                    ]);
                    names.remove(j);
                }
            }
        }
        let mut file = file;
        for (at, flag) in flags {
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&[flag])?;
        }
        file.sync_all()?;
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

/// A zip, 7z, tar or gz archive being rewritten (see [`ZipEdit`],
/// [`SevenZEdit`], [`TarEdit`], [`GzEdit`]).
pub(crate) enum Rewrite {
    Zip(Box<ZipEdit>),
    SevenZ(Box<SevenZEdit>),
    Tar(Box<TarEdit>),
    Gz(Box<GzEdit>),
}

impl Rewrite {
    /// Starts rewriting `archive`, which must be a zip, a 7z, a tar (plain,
    /// gz or bz2) or a plain gz on disk; it may not exist yet.
    /// With a `password` it must be a new zip or 7z, its files encrypted
    /// with it (other formats fail).
    pub fn begin(archive: &Path, password: Option<&str>) -> io::Result<Rewrite> {
        let format = archive.file_name().and_then(vfs::format_of);
        if password.is_some() && !matches!(format, Some(Format::Zip | Format::SevenZ)) {
            return Err(io::Error::other(vfs::NO_ENCRYPTION));
        }
        match format {
            Some(Format::Zip) => Ok(Rewrite::Zip(Box::new(ZipEdit::begin(archive, password)?))),
            Some(Format::SevenZ) => Ok(Rewrite::SevenZ(Box::new(SevenZEdit::begin(archive, password)?))),
            Some(f @ (Format::Tar | Format::TarGz | Format::TarBz2)) => Ok(Rewrite::Tar(Box::new(TarEdit::begin(archive, f)?))),
            Some(Format::Gz) => Ok(Rewrite::Gz(Box::new(GzEdit::begin(archive)?))),
            _ => Err(io::Error::other("Only zip, 7z, tar and gz archives can be changed")),
        }
    }

    pub fn copy_existing(&mut self, edits: &Edits, alive: &mut dyn FnMut() -> bool) -> io::Result<bool> {
        match self {
            Rewrite::Zip(e) => e.copy_existing(edits, alive),
            Rewrite::SevenZ(e) => e.copy_existing(edits, alive),
            Rewrite::Tar(e) => e.copy_existing(edits, alive),
            Rewrite::Gz(e) => e.copy_existing(edits, alive),
        }
    }

    pub fn add_dir(&mut self, inner: &str, modified: Option<SystemTime>) -> io::Result<()> {
        match self {
            Rewrite::Zip(e) => e.add_dir(inner, modified),
            Rewrite::SevenZ(e) => e.add_dir(inner, modified),
            Rewrite::Tar(e) => e.add_dir(inner, modified),
            Rewrite::Gz(e) => e.add_dir(inner, modified),
        }
    }

    pub fn start_file(&mut self, inner: &str, modified: Option<SystemTime>, size: u64) -> io::Result<()> {
        match self {
            Rewrite::Zip(e) => e.start_file(inner, modified, size),
            Rewrite::SevenZ(e) => e.start_file(inner, modified),
            Rewrite::Tar(e) => e.start_file(inner, modified),
            Rewrite::Gz(e) => e.start_file(inner, modified),
        }
    }

    pub fn output(&mut self) -> &mut dyn Write {
        match self {
            Rewrite::Zip(e) => e.output(),
            Rewrite::SevenZ(e) => e.output(),
            Rewrite::Tar(e) => e.output(),
            Rewrite::Gz(e) => e.output(),
        }
    }

    pub fn abort_file(&mut self) -> io::Result<()> {
        match self {
            Rewrite::Zip(e) => e.abort_file(),
            Rewrite::SevenZ(e) => e.abort_file(),
            Rewrite::Tar(e) => e.abort_file(),
            Rewrite::Gz(e) => e.abort_file(),
        }
    }

    pub fn commit(self) -> io::Result<()> {
        match self {
            Rewrite::Zip(e) => (*e).commit(),
            Rewrite::SevenZ(e) => (*e).commit(),
            Rewrite::Tar(e) => (*e).commit(),
            Rewrite::Gz(e) => (*e).commit(),
        }
    }
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
