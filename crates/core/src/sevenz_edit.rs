//! Changing 7z archives, the way `crate::zip_edit` does for zips: a 7z is
//! never edited in place, every change rewrites it into a temp file beside
//! the archive, which then replaces it in one rename. The blocks whose files
//! all stay are copied as they are: same coder, same bytes, so nothing is
//! recompressed and an encrypted block stays encrypted (its files can even be
//! renamed without touching their data). A block that loses a file or gets
//! one of its files copied has to be decoded whole and its remaining files
//! recompressed, one pack each with the writer's coder (LZMA2): a solid block
//! comes out as one block per file. New files and folders carry their name
//! and their date when one is given; the other entries keep their attributes
//! and dates. Encrypted 7z archives (encrypted files or names) are refused:
//! the writer can't keep the encryption, and dropping it silently would
//! leave the contents readable. Until
//! [`SevenZEdit::commit`] the archive is untouched; dropping an uncommitted
//! edit removes the temp files.
//!
//! A file being added goes through a spool file first: pushing an entry means
//! handing the writer a reader of its bytes, while `ops` hands them over as a
//! writer.

use crate::zip_edit::{key, matches, Edits};
use crate::vfs::{self, clean_parts};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use sevenz_rust2::{Archive, ArchiveEntry, ArchiveWriter, BlockDecoder, Error, NtTime, Password};

/// A file being added: its bytes go to a spool file first, since an entry is
/// pushed whole, once its data is known.
struct Pending {
    name: String,
    modified: Option<SystemTime>,
    spool: File,
}

pub(crate) struct SevenZEdit {
    archive: PathBuf,
    temp: PathBuf,
    spool: PathBuf,
    writer: Option<ArchiveWriter<BufWriter<File>>>,
    pending: Option<Pending>,
    /// Keys of the folder entries written so far.
    dirs: HashSet<String>,
    /// The output when no file was started: refuses writes.
    none: NoFile,
    /// Set for a new archive written encrypted (refuses to copy an old one).
    encrypted: bool,
}

impl SevenZEdit {
    /// Starts rewriting `archive` (which may not exist yet: a new 7z). With
    /// a `password` everything added is encrypted with it (AES-256),
    /// the names too (an encrypted header). Only for a new 7z:
    /// [`Self::copy_existing`] fails if it exists.
    pub fn begin(archive: &Path, password: Option<&str>) -> io::Result<SevenZEdit> {
        let name = archive.file_name().ok_or_else(|| io::Error::other("Invalid archive path"))?.to_string_lossy();
        let temp = archive.with_file_name(format!(".{name}.{}.rctmp", std::process::id()));
        let spool = archive.with_file_name(format!(".{name}.{}.rcspool", std::process::id()));
        let mut writer = ArchiveWriter::new(BufWriter::new(File::create(&temp)?)).map_err(io_err)?;
        // Names are written plain: an archive whose 7z hid them comes out with
        // them shown.
        writer.set_encrypt_header(false);
        if let Some(password) = password {
            use sevenz_rust2::{encoder_options::AesEncoderOptions, EncoderMethod};
            writer.set_content_methods(vec![AesEncoderOptions::new(password.into()).into(), EncoderMethod::LZMA2.into()]);
            writer.set_encrypt_header(true);
        }
        Ok(SevenZEdit {
            archive: archive.to_path_buf(),
            temp,
            spool,
            writer: Some(writer),
            pending: None,
            dirs: HashSet::new(),
            none: NoFile,
            encrypted: password.is_some(),
        })
    }

    fn writer(&mut self) -> &mut ArchiveWriter<BufWriter<File>> {
        self.writer.as_mut().expect("writer is present until commit")
    }

    /// Copies the archive's current entries (if it exists) as `edits` say.
    /// `alive` is asked between entries; Ok(false) means it said stop.
    pub fn copy_existing(&mut self, edits: &Edits, alive: &mut dyn FnMut() -> bool) -> io::Result<bool> {
        if !self.archive.exists() {
            return Ok(true);
        }
        if self.encrypted {
            return Err(io::Error::other("A password can only be set on a new archive, and this one exists"));
        }
        let (archive, password) = vfs::open_7z(&self.archive, &self.archive)?;
        let aes = sevenz_rust2::EncoderMethod::ID_AES256_SHA256;
        if !password.is_empty() || archive.blocks.iter().any(|b| b.coders.iter().any(|c| c.encoder_method_id() == aes)) {
            return Err(io::Error::other("This 7z is encrypted, and its encryption can't be kept when changing it"));
        }
        if !archive.blocks.is_empty() && archive.stream_map.block_first_file_index.len() != archive.blocks.len() {
            return Err(io::Error::other("This 7z does not map its files to its blocks, so it can't be changed"));
        }
        // What becomes of each entry of the source archive: dropped, stored
        // under another name, and kept under its own name as well (a copy).
        let planned: Vec<(bool, bool, Option<String>)> = archive.files.iter().map(|e| Self::fate(e, edits)).collect();
        let mut source = BufReader::new(File::open(&self.archive)?);
        // The writer is held out of `self` while the archive is read, so that
        // the entries being written and the ones still to read can both be
        // looked at; it is put back however this ends.
        let mut writer = self.writer.take().expect("writer is present until commit");
        let stopped = self.copy_with(&mut writer, &archive, &password, &mut source, &planned, alive)?;
        self.writer = Some(writer);
        Ok(stopped)
    }

    /// What becomes of one entry of the source archive: `(gone, also, to)` —
    /// dropped, kept under its own name too (a copy of it), and the name it
    /// is stored under when the edit renames or moves it. Names the edit
    /// can't show (`../x`) are kept as they are.
    fn fate(entry: &ArchiveEntry, edits: &Edits) -> (bool, bool, Option<String>) {
        if entry.is_anti_item {
            return (true, false, None);
        }
        let Some(parts) = clean_parts(&entry.name).map(|p| p.into_iter().map(str::to_owned).collect::<Vec<String>>()) else {
            return (false, false, None);
        };
        let name = parts.join("/").to_lowercase();
        if edits.remove.iter().any(|r| matches(&name, &key(r)).is_some()) {
            return (true, false, None);
        }
        match edits.relocate.iter().find_map(|(from, to, keep)| matches(&name, &key(from)).map(|depth| (depth, to, keep))) {
            None => (false, false, None),
            Some((depth, to, keep)) => {
                let mut moved = to.trim_end_matches('/').to_owned();
                for part in &parts[depth..] {
                    moved.push('/');
                    moved.push_str(part);
                }
                (false, *keep, Some(moved))
            }
        }
    }

    /// Writes the entries of the source archive as `planned` says: a block
    /// whose files all stay is copied whole (its packed bytes, coder, sizes
    /// and dates as they are), a block that loses a file or gets one copied is
    /// decoded and its files recompressed, one pack each, and the folders and
    /// files without data in between are written one by one. Ok(false) means
    /// `alive` said stop.
    fn copy_with(
        &mut self,
        writer: &mut ArchiveWriter<BufWriter<File>>,
        archive: &Archive,
        password: &Password,
        source: &mut BufReader<File>,
        planned: &[(bool, bool, Option<String>)],
        alive: &mut dyn FnMut() -> bool,
    ) -> io::Result<bool> {
        let files = &archive.files;
        let block_of = |i: usize| archive.stream_map.file_block_index.get(i).copied().flatten();
        let mut i: usize = 0;
        while i < files.len() {
            if !alive() {
                return Ok(false);
            }
            let block = block_of(i);
            match (block, files[i].has_stream) {
                // A folder or a file stored without data: one entry at a time.
                (None, _) => {
                    if !planned[i].0 {
                        self.push_plain(writer, &files[i], planned, i)?;
                    }
                    i += 1
                }
                // An empty entry sitting inside a block: it carries no data of
                // its own, so it goes on its own too.
                (Some(..), false) => {
                    if !planned[i].0 {
                        self.push_plain(writer, &files[i], planned, i)?;
                    }
                    i += 1
                }
                (Some(b), true) => {
                    // The run of entries this block covers.
                    let mut past = i + 1;
                    while past < files.len() && block_of(past) == block {
                        past += 1
                    }
                    // Its files can be copied as they are only if none of them
                    // goes or is copied twice: a copied block cannot lose or
                    // gain one.
                    let stays = planned[i..past].iter().all(|p| !p.0 && !p.1);
                    if stays {
                        // Folders and empty files of the run are not in the
                        // copied bytes, so they are written before it.
                        for j in i..past {
                            if !files[j].has_stream && !planned[j].0 {
                                self.push_plain(writer, &files[j], planned, j)?;
                            }
                        }
                        // What the copied files are renamed to, by the name
                        // they have now.
                        let mut renames: HashMap<String, String> = HashMap::new();
                        for j in i..past {
                            if let Some(ref to) = planned[j].2 {
                                renames.insert(files[j].name.clone(), to.clone());
                            }
                        }
                        let rename = |entry: &ArchiveEntry| -> String {
                            match renames.get(&entry.name) {
                                Some(to) => to.clone(),
                                None => entry.name.clone(),
                            }
                        };
                        writer.push_raw_block(archive, b, source, rename).map_err(io_err)?;
                    } else {
                        // The block has to be read: its files are recompressed.
                        let mut k = i;
                        let mut stop = false;
                        let mut failure: Option<io::Error> = None;
                        let mut on_entry = |entry: &ArchiveEntry, mut reader: &mut dyn Read| -> Result<bool, Error> {
                            let here = k;
                            k += 1;
                            if !alive() {
                                stop = true;
                                return Ok(false)
                            }
                            if here < i || planned[here].0 {
                                return Ok(true)
                            }
                            if let Err(e) = self.push_decoded(writer, entry, &mut reader, planned, here) {
                                failure = Some(e);
                                stop = true
                            }
                            Ok(!stop)
                        };
                        let decoder = BlockDecoder::new(1, b, archive, password, source);
                        decoder.for_each_entries(&mut on_entry).map_err(io_err)?;
                        if let Some(e) = failure {
                            return Err(e)
                        }
                        if stop {
                            return Ok(false)
                        }
                    }
                    i = past
                }
            }
        }
        Ok(true)
    }

    /// Writes one entry that carries no data of its own (a folder, or a file
    /// stored without any): under the name the plan gives it, and, when the
    /// plan copies it, under its own name as well.
    fn push_plain(&mut self, writer: &mut ArchiveWriter<BufWriter<File>>, entry: &ArchiveEntry, planned: &[(bool, bool, Option<String>)], j: usize) -> io::Result<()> {
        let mut keep = |e: &ArchiveEntry, name: &str| -> io::Result<()> {
            let mut kept = e.clone();
            kept.name = name.into();
            if kept.is_directory {
                self.dirs.insert(crate::zip_edit::key(&kept.name));
            }
            writer.push_archive_entry(kept, Some(io::empty())).map_err(io_err)?;
            Ok(())
        };
        if planned[j].1 {
            keep(entry, &entry.name)?;
        }
        match planned[j].2 {
            Some(ref to) => keep(entry, to.as_str()),
            None => keep(entry, &entry.name),
        }
    }

    /// Writes one file decoded from a block that had to be read: its bytes
    /// come from `reader`, and, when the plan copies it, they go through the
    /// spool file so that the entry can be pushed twice.
    fn push_decoded(&mut self, writer: &mut ArchiveWriter<BufWriter<File>>, entry: &ArchiveEntry, mut reader: &mut dyn Read, planned: &[(bool, bool, Option<String>)], j: usize) -> io::Result<()> {
        let mut keep = |name: &str, r: &mut dyn Read| -> io::Result<()> {
            let mut kept = entry.clone();
            kept.name = name.into();
            writer.push_archive_entry(kept, Some(r)).map_err(io_err)?;
            Ok(())
        };
        if !planned[j].1 {
            let name = match planned[j].2 {
                Some(ref to) => to.as_str(),
                None => entry.name.as_str(),
            };
            return keep(name, &mut reader)
        }
        // Copied twice: an entry is pushed with a reader of its own, so the
        // bytes wait in the spool file and are read back for each name.
        let mut spool = File::options().read(true).write(true).create(true).truncate(true).open(&self.spool)?;
        io::copy(&mut reader, &mut spool)?;
        spool.seek(SeekFrom::Start(0))?;
        keep(&entry.name, &mut spool)?;
        match planned[j].2 {
            None => Ok(()),
            Some(ref to) => {
                spool.seek(SeekFrom::Start(0))?;
                keep(to.as_str(), &mut spool)?;
                Ok(())
            }
        }
    }

    /// Puts the file's date into a new entry, as 7z keeps it.
    fn dated(&mut self, mut entry: ArchiveEntry, modified: Option<SystemTime>) -> ArchiveEntry {
        if let Some(t) = modified {
            entry.has_last_modified_date = true;
            entry.last_modified_date = t.try_into().unwrap_or(NtTime::UNIX_EPOCH);
        }
        entry
    }

    /// Adds a folder entry (unless one was written already).
    pub fn add_dir(&mut self, inner: &str, modified: Option<SystemTime>) -> io::Result<()> {
        self.flush_pending()?;
        if !self.dirs.insert(key(inner)) {
            return Ok(())
        }
        let name = inner.trim_end_matches('/').to_owned();
        let entry = self.dated(ArchiveEntry::new_directory(&name), modified);
        self.writer().push_archive_entry(entry, Some(io::empty())).map_err(io_err)?;
        Ok(())
    }

    /// Starts a file entry; write its contents with [`Self::output`], then
    /// start the next one (or [`Self::abort_file`] on failure).
    pub fn start_file(&mut self, inner: &str, modified: Option<SystemTime>) -> io::Result<()> {
        self.flush_pending()?;
        let spool = File::options().read(true).write(true).create(true).truncate(true).open(&self.spool)?;
        self.pending = Some(Pending { name: inner.to_owned(), modified, spool });
        Ok(())
    }

    /// The bytes of the file being added, going to its spool file.
    pub fn output(&mut self) -> &mut dyn Write {
        match &mut self.pending {
            Some(pending) => &mut pending.spool,
            None => &mut self.none,
        }
    }

    /// Drops the file being written.
    pub fn abort_file(&mut self) -> io::Result<()> {
        self.pending = None;
        Ok(())
    }

    /// Writes the file that was being added, from its spool file.
    fn flush_pending(&mut self) -> io::Result<()> {
        let Some(mut pending) = self.pending.take() else { return Ok(()) };
        pending.spool.seek(SeekFrom::Start(0))?;
        let entry = self.dated(ArchiveEntry::new_file(&pending.name), pending.modified);
        let writer = self.writer.as_mut().expect("writer is present until commit");
        writer.push_archive_entry(entry, Some(&mut pending.spool)).map_err(io_err)?;
        Ok(())
    }

    /// Finishes the new archive and puts it in place of the old one.
    pub fn commit(mut self) -> io::Result<()> {
        self.flush_pending()?;
        let writer = self.writer.take().expect("commit runs once");
        let file = writer.finish()?.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.temp, &self.archive)?;
        crate::vfs::forget(&self.archive);
        Ok(())
    }
}

impl Drop for SevenZEdit {
    fn drop(&mut self) {
        drop(self.writer.take());
        drop(self.pending.take());
        let _ = fs::remove_file(&self.spool);
        let _ = fs::remove_file(&self.temp);
    }
}

/// The output when no file was started: refuses writes.
struct NoFile;

impl Write for NoFile {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("No file started in the 7z"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
