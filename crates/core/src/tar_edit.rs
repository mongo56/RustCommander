//! Changing tar archives (`.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`),
//! like [`crate::zip_edit`] does for zips: the archive is read from start to
//! end and written again into a temp file next to it, which then replaces
//! it in one rename. Members that stay are copied byte for byte (with their
//! long-name and pax headers, so every bit of metadata is kept); renamed
//! ones keep everything but the name. Until [`TarEdit::commit`] the archive
//! is untouched; dropping an uncommitted edit removes the temp files.
//!
//! Refused (an error, nothing changed): GNU sparse members, and removing or
//! moving a file that a hard link elsewhere in the archive points at.

use crate::vfs::{clean_parts, Format};
use crate::zip_edit::{key, matches, Edits};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tar::{EntryType, Header};

const BLOCK: usize = 512;
/// Long-name and pax header data is read into memory; more is refused.
const META_LIMIT: u64 = 1 << 20;

/// The compressed (or plain) output.
enum Out {
    Plain(BufWriter<File>),
    Gz(flate2::write::GzEncoder<BufWriter<File>>),
    Bz(bzip2::write::BzEncoder<BufWriter<File>>),
}

impl Out {
    fn new(file: File, format: Format) -> Out {
        let file = BufWriter::new(file);
        match format {
            Format::TarGz => Out::Gz(flate2::write::GzEncoder::new(file, flate2::Compression::default())),
            Format::TarBz2 => Out::Bz(bzip2::write::BzEncoder::new(file, bzip2::Compression::default())),
            _ => Out::Plain(file),
        }
    }

    fn finish(self) -> io::Result<File> {
        let buffered = match self {
            Out::Plain(w) => w,
            Out::Gz(w) => w.finish()?,
            Out::Bz(w) => w.finish()?,
        };
        buffered.into_inner().map_err(|e| e.into_error())
    }
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Out::Plain(w) => w.write(buf),
            Out::Gz(w) => w.write(buf),
            Out::Bz(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Out::Plain(w) => w.flush(),
            Out::Gz(w) => w.flush(),
            Out::Bz(w) => w.flush(),
        }
    }
}

/// A member of the source archive as read: its long-name / pax headers
/// (header, data) and its own header. Its data follows in the stream.
struct Member {
    exts: Vec<(Header, Vec<u8>)>,
    header: Header,
    /// Bytes of data (pax `size` wins over the header's).
    size: u64,
}

impl Member {
    fn pax(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.exts
            .iter()
            .filter(|(h, _)| h.entry_type().is_pax_local_extensions())
            .flat_map(|(_, data)| tar::PaxExtensions::new(data).filter_map(|e| e.ok()))
            .map(|e| (e.key_bytes(), e.value_bytes()))
    }

    fn long(&self, link: bool) -> Option<Vec<u8>> {
        let (_, data) = self.exts.iter().find(|(h, _)| match link {
            true => h.entry_type().is_gnu_longlink(),
            false => h.entry_type().is_gnu_longname(),
        })?;
        Some(data.split(|b| *b == 0).next().unwrap_or_default().to_vec())
    }

    /// The name, as readers see it (GNU long name, pax `path`, header).
    fn name(&self) -> Vec<u8> {
        if let Some(long) = self.long(false) {
            return long;
        }
        match self.pax().find(|(k, _)| *k == b"path") {
            Some((_, v)) => v.to_vec(),
            None => self.header.path_bytes().into_owned(),
        }
    }

    fn link_name(&self) -> Option<Vec<u8>> {
        if let Some(long) = self.long(true) {
            return Some(long);
        }
        match self.pax().find(|(k, _)| *k == b"linkpath") {
            Some((_, v)) => Some(v.to_vec()),
            None => self.header.link_name_bytes().map(|n| n.into_owned()),
        }
    }
}

/// Reads members from a decompressed tar stream.
struct Reader<R> {
    input: R,
}

impl<R: Read> Reader<R> {
    /// The next header block; `None` at the end (zero block or end of
    /// data).
    fn header(&mut self) -> io::Result<Option<Header>> {
        let mut h = Header::new_old();
        let block = h.as_mut_bytes();
        let mut got = 0;
        while got < BLOCK {
            match self.input.read(&mut block[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if got == 0 || (got == BLOCK && block.iter().all(|b| *b == 0)) {
            return Ok(None);
        }
        if got < BLOCK {
            return Err(truncated());
        }
        // Unsigned sum as POSIX says, or signed as some old tars wrote.
        let stored = h.cksum().map_err(|_| corrupt())?;
        let bytes = h.as_bytes();
        let field = 148..156;
        let blank = 8 * u32::from(b' ');
        let unsigned: u32 = bytes.iter().enumerate().filter(|(i, _)| !field.contains(i)).map(|(_, b)| u32::from(*b)).sum();
        let signed: i64 = bytes.iter().enumerate().filter(|(i, _)| !field.contains(i)).map(|(_, b)| i64::from(*b as i8)).sum();
        if stored != unsigned + blank && i64::from(stored) != signed + i64::from(blank) {
            return Err(corrupt());
        }
        Ok(Some(h))
    }

    /// The next member (with the headers describing it); `None` at the end.
    fn member(&mut self) -> io::Result<Option<Member>> {
        let mut exts = Vec::new();
        loop {
            let Some(header) = self.header()? else {
                return match exts.is_empty() {
                    true => Ok(None),
                    false => Err(corrupt()),
                };
            };
            let kind = header.entry_type();
            if kind.is_gnu_longname() || kind.is_gnu_longlink() || kind.is_pax_local_extensions() {
                let size = header.entry_size()?;
                if size > META_LIMIT {
                    return Err(corrupt());
                }
                let mut data = Vec::with_capacity(size as usize);
                (&mut self.input).take(size).read_to_end(&mut data)?;
                if data.len() as u64 != size {
                    return Err(truncated());
                }
                self.skip(padding(size))?;
                exts.push((header, data));
                continue;
            }
            if kind.is_gnu_sparse() {
                return Err(io::Error::other("This tar has sparse files, which can't be kept when changing it"));
            }
            let mut member = Member { exts, header, size: 0 };
            let pax_size = member.pax().find(|(k, _)| *k == b"size").map(|(_, v)| v.to_vec());
            member.size = match pax_size {
                Some(v) => std::str::from_utf8(&v).ok().and_then(|s| s.parse().ok()).ok_or_else(corrupt)?,
                None => member.header.entry_size()?,
            };
            return Ok(Some(member));
        }
    }

    /// Copies `size` bytes of data and its padding to `out`.
    fn copy(&mut self, size: u64, out: &mut dyn Write) -> io::Result<()> {
        let total = size + padding(size);
        let copied = io::copy(&mut (&mut self.input).take(total), out)?;
        if copied != total {
            return Err(truncated());
        }
        Ok(())
    }

    fn skip(&mut self, n: u64) -> io::Result<()> {
        self.copy_exact(n, &mut io::sink())
    }

    fn copy_exact(&mut self, n: u64, out: &mut dyn Write) -> io::Result<()> {
        if io::copy(&mut (&mut self.input).take(n), out)? != n {
            return Err(truncated());
        }
        Ok(())
    }
}

fn padding(size: u64) -> u64 {
    (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "The tar archive is truncated")
}

fn corrupt() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "The tar archive is damaged")
}

/// Writes `header` (checksum set) and `data` with its padding.
fn write_block(out: &mut dyn Write, header: &mut Header, data: &[u8]) -> io::Result<()> {
    header.set_cksum();
    out.write_all(header.as_bytes())?;
    out.write_all(data)?;
    out.write_all(&[0; BLOCK][..padding(data.len() as u64) as usize])
}

/// Puts `name` into `header`; a name too long for it gets a GNU long-name
/// header first (written to `out`).
fn write_name(out: &mut dyn Write, header: &mut Header, name: &[u8]) -> io::Result<()> {
    let old = header.as_old_mut();
    old.name = [0; 100];
    let short = name.len().min(100);
    old.name[..short].copy_from_slice(&name[..short]);
    if let Some(ustar) = header.as_ustar_mut() {
        ustar.prefix = [0; 155];
    }
    if name.len() > 100 {
        let mut long = Header::new_gnu();
        long.as_old_mut().name[..13].copy_from_slice(b"././@LongLink");
        long.set_mode(0o644);
        long.set_mtime(0);
        long.set_uid(0);
        long.set_gid(0);
        long.set_entry_type(EntryType::GNULongName);
        let mut data = name.to_vec();
        data.push(0);
        long.set_size(data.len() as u64);
        write_block(out, &mut long, &data)?;
    }
    Ok(())
}

/// One pax record: `<length> <key>=<value>\n`, the length counting itself.
fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    let rest = key.len() + value.len() + 3;
    let mut len = rest + 1;
    while len.to_string().len() + rest > len {
        len += 1;
    }
    let mut record = format!("{len} ").into_bytes();
    record.extend_from_slice(key);
    record.push(b'=');
    record.extend_from_slice(value);
    record.push(b'\n');
    record
}

/// A file being added: its contents go to a spool file first, so its size
/// is known (and it can be dropped) before its header is written.
struct Pending {
    name: String,
    modified: Option<SystemTime>,
    spool: File,
}

pub(crate) struct TarEdit {
    archive: PathBuf,
    format: Format,
    temp: PathBuf,
    spool: PathBuf,
    out: Option<Out>,
    pending: Option<Pending>,
    /// Keys of the folder entries written so far.
    dirs: HashSet<String>,
}

impl TarEdit {
    /// Starts rewriting `archive` (a tar of `format`; it may not exist yet).
    pub fn begin(archive: &Path, format: Format) -> io::Result<TarEdit> {
        let name = archive.file_name().ok_or_else(|| io::Error::other("Invalid archive path"))?.to_string_lossy();
        let temp = archive.with_file_name(format!(".{name}.{}.rctmp", std::process::id()));
        let spool = archive.with_file_name(format!(".{name}.{}.rcspool", std::process::id()));
        let out = Out::new(File::create(&temp)?, format);
        Ok(TarEdit {
            archive: archive.to_path_buf(),
            format,
            temp,
            spool,
            out: Some(out),
            pending: None,
            dirs: HashSet::new(),
        })
    }

    fn out(&mut self) -> &mut Out {
        self.out.as_mut().expect("output is present until commit")
    }

    /// Copies the archive's current members (if it exists) as `edits` say.
    /// `alive` is asked between members; Ok(false) means it said stop.
    pub fn copy_existing(&mut self, edits: &Edits, alive: &mut dyn FnMut() -> bool) -> io::Result<bool> {
        if !self.archive.exists() {
            return Ok(true);
        }
        let mut reader = Reader { input: crate::vfs::open_stream(&self.archive, self.format)? };
        let remove: Vec<String> = edits.remove.iter().map(|r| key(r)).collect();
        let relocate: Vec<(String, &str, bool)> = edits.relocate.iter().map(|(f, t, k)| (key(f), t.as_str(), *k)).collect();
        // Removed, or moved away (a hard link to it would break).
        let gone = |key: &str| {
            remove.iter().any(|r| matches(key, r).is_some())
                || relocate.iter().any(|(from, _, keep)| !keep && matches(key, from).is_some())
        };
        while let Some(member) = reader.member()? {
            if !alive() {
                return Ok(false);
            }
            let kind = member.header.entry_type();
            let name = String::from_utf8_lossy(&member.name()).into_owned();
            // Global pax headers and names that can't be shown (`../x`)
            // are kept as they are.
            let parts = clean_parts(&name).filter(|_| !kind.is_pax_global_extensions());
            let Some(parts) = parts else {
                self.copy_member(&mut reader, &member, None)?;
                continue;
            };
            let key = parts.join("/").to_lowercase();
            let removed = remove.iter().any(|r| matches(&key, r).is_some());
            if kind.is_hard_link() && !removed {
                let target = member.link_name().map(|t| String::from_utf8_lossy(&t).into_owned()).unwrap_or_default();
                if gone(&crate::zip_edit::key(&target)) {
                    return Err(io::Error::other(format!(
                        "{name} in the archive is a hard link to {target}, which would be lost: not changed"
                    )));
                }
            }
            if removed {
                reader.skip(member.size + padding(member.size))?;
                continue;
            }
            if kind == EntryType::Directory {
                self.dirs.insert(key.clone());
            }
            let moved = relocate.iter().find_map(|(from, to, keep)| matches(&key, from).map(|depth| (depth, *to, *keep)));
            let Some((depth, to, keep)) = moved else {
                self.copy_member(&mut reader, &member, None)?;
                continue;
            };
            let mut renamed = to.trim_end_matches('/').to_owned();
            for part in &parts[depth..] {
                renamed.push('/');
                renamed.push_str(part);
            }
            if kind == EntryType::Directory {
                self.dirs.insert(crate::zip_edit::key(&renamed));
                renamed.push('/');
            }
            if !keep {
                self.copy_member(&mut reader, &member, Some(&renamed))?;
                continue;
            }
            // Written twice: the data goes through the spool file.
            let mut spool = File::options().read(true).write(true).create(true).truncate(true).open(&self.spool)?;
            reader.copy(member.size, &mut spool)?;
            spool.seek(SeekFrom::Start(0))?;
            self.copy_member(&mut Reader { input: BufReader::new(&spool) }, &member, None)?;
            spool.seek(SeekFrom::Start(0))?;
            self.copy_member(&mut Reader { input: BufReader::new(&spool) }, &member, Some(&renamed))?;
        }
        // To the end, so the gz/bz2 checksum is checked too.
        io::copy(&mut reader.input, &mut io::sink())?;
        Ok(true)
    }

    /// Writes `member` (its data read from `reader`), under `rename` if
    /// given: then its long-name header and pax `path` are dropped.
    fn copy_member<R: Read>(&mut self, reader: &mut Reader<R>, member: &Member, rename: Option<&str>) -> io::Result<()> {
        let out = self.out.as_mut().expect("output is present until commit");
        for (h, data) in &member.exts {
            let kind = h.entry_type();
            let mut h = h.clone();
            match rename {
                Some(_) if kind.is_gnu_longname() => {}
                Some(_) if kind.is_pax_local_extensions() => {
                    let kept: Vec<u8> = tar::PaxExtensions::new(data)
                        .filter_map(|e| e.ok())
                        .filter(|e| e.key_bytes() != b"path")
                        .flat_map(|e| pax_record(e.key_bytes(), e.value_bytes()))
                        .collect();
                    if !kept.is_empty() {
                        h.set_size(kept.len() as u64);
                        write_block(out, &mut h, &kept)?;
                    }
                }
                _ => {
                    out.write_all(h.as_bytes())?;
                    out.write_all(data)?;
                    out.write_all(&[0; BLOCK][..padding(data.len() as u64) as usize])?;
                }
            }
        }
        match rename {
            Some(name) => {
                let mut h = member.header.clone();
                write_name(out, &mut h, name.as_bytes())?;
                h.set_cksum();
                out.write_all(h.as_bytes())?;
            }
            None => out.write_all(member.header.as_bytes())?,
        }
        reader.copy(member.size, out)
    }

    /// Writes the file being added, if any.
    fn flush_pending(&mut self) -> io::Result<()> {
        let Some(mut pending) = self.pending.take() else { return Ok(()) };
        let size = pending.spool.seek(SeekFrom::End(0))?;
        pending.spool.seek(SeekFrom::Start(0))?;
        let mut h = new_header(EntryType::Regular, 0o644, pending.modified);
        h.set_size(size);
        let out = self.out.as_mut().expect("output is present until commit");
        write_name(out, &mut h, pending.name.as_bytes())?;
        h.set_cksum();
        out.write_all(h.as_bytes())?;
        Reader { input: BufReader::new(&pending.spool) }.copy_exact(size, out)?;
        out.write_all(&[0; BLOCK][..padding(size) as usize])
    }

    /// Adds a folder entry (unless one was written already).
    pub fn add_dir(&mut self, inner: &str, modified: Option<SystemTime>) -> io::Result<()> {
        self.flush_pending()?;
        if !self.dirs.insert(key(inner)) {
            return Ok(());
        }
        let mut h = new_header(EntryType::Directory, 0o755, modified);
        h.set_size(0);
        let name = format!("{}/", inner.trim_end_matches('/'));
        let out = self.out();
        write_name(out, &mut h, name.as_bytes())?;
        write_block(out, &mut h, &[])
    }

    /// Starts a file entry; write its contents with [`Self::output`], then
    /// start the next one (or [`Self::abort_file`] on failure).
    pub fn start_file(&mut self, inner: &str, modified: Option<SystemTime>) -> io::Result<()> {
        self.flush_pending()?;
        let spool = File::options().read(true).write(true).create(true).truncate(true).open(&self.spool)?;
        self.pending = Some(Pending { name: inner.trim_end_matches('/').to_owned(), modified, spool });
        Ok(())
    }

    pub fn output(&mut self) -> &mut dyn Write {
        match &mut self.pending {
            Some(p) => &mut p.spool,
            None => self.out.as_mut().expect("output is present until commit"),
        }
    }

    /// Drops the file being written.
    pub fn abort_file(&mut self) -> io::Result<()> {
        self.pending = None;
        Ok(())
    }

    /// Finishes the new archive and puts it in place of the old one.
    pub fn commit(mut self) -> io::Result<()> {
        self.flush_pending()?;
        let mut out = self.out.take().expect("commit runs once");
        out.write_all(&[0; 2 * BLOCK])?;
        let file = out.finish()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.temp, &self.archive)?;
        crate::vfs::forget(&self.archive);
        Ok(())
    }
}

impl Drop for TarEdit {
    fn drop(&mut self) {
        drop(self.out.take());
        drop(self.pending.take());
        let _ = fs::remove_file(&self.spool);
        let _ = fs::remove_file(&self.temp);
    }
}

/// A GNU header for a new entry (no owner, `mode`, the date if given).
fn new_header(kind: EntryType, mode: u32, modified: Option<SystemTime>) -> Header {
    let mut h = Header::new_gnu();
    h.set_entry_type(kind);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    let secs = modified.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    h.set_mtime(secs);
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{self, Conflict, Control, Job, Outcome, Progress, Reporter, Resolution};
    use crate::vfs::tests::TempDir;

    struct NoAsk;
    impl Reporter for NoAsk {
        fn progress(&mut self, _: &Progress) {}
        fn conflict(&mut self, _: Conflict) -> Resolution {
            Resolution::Cancel
        }
    }

    fn run(job: Job) -> Outcome {
        ops::run(&job, &Control::default(), &mut NoAsk)
    }

    fn ok(job: Job) {
        let o = run(job);
        assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
    }

    fn long_name() -> String {
        format!("long/{}.txt", "n".repeat(120))
    }

    fn pax_name() -> String {
        format!("pax/{}.txt", "p".repeat(150))
    }

    /// A tar with owners and modes, a GNU long name, a pax header (path,
    /// fractional mtime, an xattr), a symlink and a hard link.
    fn make(path: &Path, format: Format) {
        let mut out = Out::new(File::create(path).unwrap(), format);
        let mut add = |name: &str, kind: EntryType, data: &[u8], link: Option<&str>| {
            let mut h = Header::new_gnu();
            h.set_entry_type(kind);
            h.set_mode(if kind == EntryType::Directory { 0o750 } else { 0o640 });
            h.set_uid(1000);
            h.set_gid(100);
            h.set_username("alice").unwrap();
            h.set_mtime(1_600_000_000);
            h.set_size(data.len() as u64);
            if let Some(link) = link {
                h.set_link_name(link).unwrap();
            }
            write_name(&mut out, &mut h, name.as_bytes()).unwrap();
            write_block(&mut out, &mut h, data).unwrap();
        };
        add("docs/", EntryType::Directory, b"", None);
        add("docs/one.txt", EntryType::Regular, b"first", None);
        add(&long_name(), EntryType::Regular, b"long", None);
        add("link", EntryType::Symlink, b"", Some("docs/one.txt"));
        add("hard", EntryType::Link, b"", Some("docs/one.txt"));
        add("top.txt", EntryType::Regular, &[b't'; 3000], None);
        let mut pax: Vec<u8> = pax_record(b"path", pax_name().as_bytes());
        pax.extend(pax_record(b"mtime", b"1600000000.5"));
        pax.extend(pax_record(b"SCHILY.xattr.user.k", b"v"));
        let mut x = Header::new_ustar();
        x.set_entry_type(EntryType::XHeader);
        x.set_size(pax.len() as u64);
        x.as_old_mut().name[..14].copy_from_slice(b"PaxHeader/x.tx");
        write_block(&mut out, &mut x, &pax).unwrap();
        let mut h = Header::new_ustar();
        h.set_entry_type(EntryType::Regular);
        h.set_mode(0o600);
        h.set_mtime(1_600_000_000);
        h.set_size(3);
        h.as_old_mut().name[..8].copy_from_slice(b"pax/x.tx");
        write_block(&mut out, &mut h, b"pax").unwrap();
        out.write_all(&[0; 2 * BLOCK]).unwrap();
        out.finish().unwrap();
    }

    /// A member as the `tar` crate reads it: name, header bytes (name,
    /// checksum and ustar prefix blanked), pax records, data.
    type Seen = (String, Vec<u8>, Vec<(String, String)>, Vec<u8>);

    fn members(path: &Path, format: Format) -> Vec<Seen> {
        let mut tar = tar::Archive::new(crate::vfs::open_stream(path, format).unwrap());
        let mut all = Vec::new();
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            let mut header = entry.header().as_bytes().to_vec();
            header[..100].fill(0);
            header[148..156].fill(0);
            header[345..500].fill(0);
            let pax = match entry.pax_extensions().unwrap() {
                Some(p) => p.filter_map(|e| e.ok()).map(|e| (e.key().unwrap().into(), e.value().unwrap().into())).collect(),
                None => Vec::new(),
            };
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            all.push((name, header, pax, data));
        }
        all
    }

    fn find<'a>(all: &'a [Seen], name: &str) -> &'a Seen {
        all.iter().find(|m| m.0 == name).unwrap_or_else(|| panic!("{name} not in {:?}", all.iter().map(|m| &m.0).collect::<Vec<_>>()))
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let names = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned());
        names.filter(|n| n.contains(".rctmp") || n.contains(".rcspool")).collect()
    }

    #[test]
    fn changes_tars_keeping_the_rest() {
        let tmp = TempDir::new("tar-edit");
        let local = tmp.0.join("local.txt");
        fs::write(&local, b"from disk").unwrap();
        let (long, pax) = (long_name(), pax_name());
        let formats = [("a.tar", Format::Tar), ("b.tar.gz", Format::TarGz), ("c.tgz", Format::TarGz), ("d.tbz2", Format::TarBz2)];
        for (name, format) in formats {
            let tar = tmp.0.join(name);
            make(&tar, format);
            assert_eq!(crate::vfs::archive_writable(&tar), Some(true));
            let before = members(&tar, format);
            assert_eq!(before.len(), 7);
            assert_eq!(find(&before, &pax).3, b"pax");

            ok(Job::Copy(ops::plan_transfer(std::slice::from_ref(&local), &tar.join("docs")).unwrap()));
            ok(Job::MakeDir(tar.join("made").join("inner")));
            ok(Job::Rename { from: tar.join("top.txt"), to: tar.join("renamed.txt") });
            ok(Job::Rename { from: tar.join(&pax), to: tar.join("short.txt") });
            let far = format!("far/{}", "f".repeat(150));
            ok(Job::Copy(ops::plan_transfer(&[tar.join("renamed.txt")], &tar.join(&far)).unwrap()));
            let entry = tar.join("docs").join("local.txt");
            let stamp = ops::entry_stamp(&entry).unwrap();
            let copy = tmp.0.join("edited.txt");
            fs::write(&copy, b"edited").unwrap();
            ok(Job::Update { src: copy, dst: entry, expect: stamp });
            ok(Job::Delete { paths: vec![tar.join("made")], to_trash: false });

            let after = members(&tar, format);
            let names: Vec<&str> = after.iter().map(|m| m.0.as_str()).collect();
            // Copied in place, the moved file's folder kept, the updated file
            // re-added at the end.
            let copied = far;
            let expected =
                ["docs/", "docs/one.txt", &long, "link", "hard", "renamed.txt", &copied, "short.txt", "pax/", "docs/local.txt"];
            assert_eq!(names, expected, "{name}");
            // Untouched members: byte for byte (headers, pax records, data).
            for name in ["docs/", "docs/one.txt", &long, "link", "hard"] {
                assert_eq!(find(&after, name), find(&before, name), "{name}");
            }
            // Renamed: all but the name kept (pax path dropped, the rest kept).
            let (top, renamed) = (find(&before, "top.txt"), find(&after, "renamed.txt"));
            assert_eq!((&renamed.1, &renamed.3), (&top.1, &top.3));
            let short = find(&after, "short.txt");
            assert_eq!(short.3, b"pax");
            let keys: Vec<&str> = short.2.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(keys, ["mtime", "SCHILY.xattr.user.k"]);
            assert_eq!(find(&after, &copied).3, vec![b't'; 3000]);
            assert_eq!(find(&after, "docs/local.txt").3, b"edited");
            assert!(leftovers(&tmp.0).is_empty());
        }
    }

    #[test]
    fn refuses_breaking_hard_links() {
        let tmp = TempDir::new("tar-hard");
        let tar = tmp.0.join("a.tar");
        make(&tar, Format::Tar);
        let before = fs::read(&tar).unwrap();
        for job in [
            Job::Delete { paths: vec![tar.join("docs").join("one.txt")], to_trash: false },
            Job::Rename { from: tar.join("docs"), to: tar.join("papers") },
        ] {
            let o = run(job);
            assert!(o.errors.len() == 1 && o.errors[0].1.contains("hard link"), "{:?}", o.errors);
            assert_eq!(fs::read(&tar).unwrap(), before);
        }
        // Copying it is fine, and so is removing the link with it.
        ok(Job::Copy(ops::plan_transfer(&[tar.join("docs")], &tar.join("copy")).unwrap()));
        ok(Job::Delete { paths: vec![tar.join("hard"), tar.join("docs")], to_trash: false });
        assert!(leftovers(&tmp.0).is_empty());
    }

    /// Damaged sources: an error, the archive untouched, no temp files.
    #[test]
    fn leaves_damaged_tars_alone() {
        let tmp = TempDir::new("tar-damaged");
        let fail = |path: &Path, format: Format, what: &str| {
            let before = fs::read(path).unwrap();
            let mut edit = TarEdit::begin(path, format).unwrap();
            let result = edit.copy_existing(&Edits::default(), &mut || true);
            assert!(result.is_err(), "{what}: {result:?}");
            drop(edit);
            assert_eq!(fs::read(path).unwrap(), before, "{what}");
            assert!(leftovers(&tmp.0).is_empty(), "{what}");
        };
        let good = tmp.0.join("good.tar");
        make(&good, Format::Tar);
        let bytes = fs::read(&good).unwrap();

        // Cut inside the data of a member.
        let cut = tmp.0.join("cut.tar");
        fs::write(&cut, &bytes[..bytes.len() - 3000]).unwrap();
        fail(&cut, Format::Tar, "truncated");

        let mut bad = bytes.clone();
        bad[BLOCK + 10] ^= 0x55;
        let bad_sum = tmp.0.join("sum.tar");
        fs::write(&bad_sum, &bad).unwrap();
        fail(&bad_sum, Format::Tar, "checksum");

        // A gz whose CRC (in the trailer, after the tar's end) is wrong.
        let gz = tmp.0.join("crc.tar.gz");
        make(&gz, Format::TarGz);
        let mut zipped = fs::read(&gz).unwrap();
        let at = zipped.len() - 8;
        zipped[at] ^= 0xff;
        fs::write(&gz, &zipped).unwrap();
        fail(&gz, Format::TarGz, "gz crc");

        let mut sparse = bytes.clone();
        let mut h = Header::new_old();
        h.as_mut_bytes().copy_from_slice(&sparse[..BLOCK]);
        h.set_entry_type(EntryType::GNUSparse);
        h.set_cksum();
        sparse[..BLOCK].copy_from_slice(h.as_bytes());
        let sp = tmp.0.join("sparse.tar");
        fs::write(&sp, &sparse).unwrap();
        fail(&sp, Format::Tar, "sparse");

        // Cancelled midway, and a file added then dropped.
        let mut edit = TarEdit::begin(&good, Format::Tar).unwrap();
        assert!(!edit.copy_existing(&Edits::default(), &mut || false).unwrap());
        edit.start_file("x.txt", None).unwrap();
        edit.output().write_all(b"partial").unwrap();
        edit.abort_file().unwrap();
        drop(edit);
        assert_eq!(fs::read(&good).unwrap(), bytes);
        assert!(leftovers(&tmp.0).is_empty());
        // Through a job: the error is reported, nothing changed.
        let before = fs::read(&bad_sum).unwrap();
        let o = run(Job::MakeDir(bad_sum.join("x")));
        assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
        assert_eq!(fs::read(&bad_sum).unwrap(), before);
    }

    /// New files and folders; a dropped file leaves no trace; long names
    /// get a GNU long-name header.
    #[test]
    fn adds_files_and_folders() {
        let tmp = TempDir::new("tar-add");
        let tar = tmp.0.join("new.tar");
        let long = format!("{}/x.txt", "d".repeat(120));
        let mut edit = TarEdit::begin(&tar, Format::Tar).unwrap();
        assert!(edit.copy_existing(&Edits::default(), &mut || true).unwrap());
        edit.add_dir("folder", None).unwrap();
        edit.add_dir("FOLDER", None).unwrap();
        edit.start_file("folder/a.txt", Some(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000))).unwrap();
        edit.output().write_all(&[b'a'; 1000]).unwrap();
        edit.start_file("dropped.txt", None).unwrap();
        edit.output().write_all(b"no").unwrap();
        edit.abort_file().unwrap();
        edit.start_file(&long, None).unwrap();
        edit.output().write_all(b"long").unwrap();
        edit.commit().unwrap();
        let all = members(&tar, Format::Tar);
        let names: Vec<&str> = all.iter().map(|m| m.0.as_str()).collect();
        assert_eq!(names, ["folder/", "folder/a.txt", long.as_str()]);
        assert_eq!(find(&all, "folder/a.txt").3, vec![b'a'; 1000]);
        assert_eq!(find(&all, &long).3, b"long");
        let mut tar_crate = tar::Archive::new(File::open(&tar).unwrap());
        let mtime = tar_crate.entries().unwrap().nth(1).unwrap().unwrap().header().mtime().unwrap();
        assert_eq!(mtime, 1_700_000_000);
        assert!(leftovers(&tmp.0).is_empty());
    }
}
