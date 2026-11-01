//! Changing tar archives (`.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`),
//! like [`crate::zip_edit`] does for zips: the archive is read from start to
//! end and written again into a temp file next to it, which then replaces
//! it in one rename. Members that stay are copied byte for byte (with their
//! long-name and pax headers, so every bit of metadata is kept); renamed
//! ones keep everything but the name. Until [`TarEdit::commit`] the archive
//! is untouched; dropping an uncommitted edit removes the temp files.
//!
//! GNU sparse members (old 'S' type with its extension blocks, or pax
//! `GNU.sparse.*` forms) are copied like any other; renamed, they stay
//! sparse. Hard links whose target is removed, moved or replaced are kept
//! working: the first link left takes the target's place (a regular member
//! with its old data, under the link's name) and later links point at it;
//! links to a moved target get its new name. Refused (an error, nothing
//! changed): removing or moving the target of a hard link that comes before
//! it in the archive.

use crate::vfs::{clean_parts, Format};
use crate::zip_edit::{key, matches, Edits};
use std::collections::{HashMap, HashSet};
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
#[derive(Clone)]
struct Member {
    exts: Vec<(Header, Vec<u8>)>,
    header: Header,
    /// Old GNU sparse members: the extension blocks after the header (more
    /// of the sparse map), as read.
    sparse: Vec<u8>,
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

    /// The real name of a pax-form sparse file (its header and `path` hold
    /// a made-up `…/GNUSparseFile.N/…` one).
    fn sparse_name(&self) -> Option<Vec<u8>> {
        self.pax().find(|(k, _)| *k == b"GNU.sparse.name").map(|(_, v)| v.to_vec())
    }

    /// The name, as readers see it (sparse name, GNU long name, pax
    /// `path`, header).
    fn name(&self) -> Vec<u8> {
        if let Some(name) = self.sparse_name() {
            return name;
        }
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
            // Old GNU sparse: more of the map may follow in extension
            // blocks (each says if another follows); then the data.
            let mut sparse = Vec::new();
            let mut extended = kind.is_gnu_sparse() && header.as_bytes()[482] != 0;
            while extended {
                if sparse.len() as u64 >= META_LIMIT {
                    return Err(corrupt());
                }
                self.copy_exact(BLOCK as u64, &mut sparse)?;
                extended = sparse[sparse.len() - BLOCK + 504] != 0;
            }
            let mut member = Member { exts, header, sparse, size: 0 };
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
    write_long(out, header, name, false)
}

/// Puts `name` into `header` as its name or, with `link`, its link target;
/// one too long for it gets a GNU long-name / long-link header first
/// (written to `out`).
fn write_long(out: &mut dyn Write, header: &mut Header, name: &[u8], link: bool) -> io::Result<()> {
    let old = header.as_old_mut();
    let field = match link {
        true => &mut old.linkname,
        false => &mut old.name,
    };
    *field = [0; 100];
    let short = name.len().min(100);
    field[..short].copy_from_slice(&name[..short]);
    if !link {
        if let Some(ustar) = header.as_ustar_mut() {
            ustar.prefix = [0; 155];
        }
    }
    if name.len() > 100 {
        let mut long = Header::new_gnu();
        long.as_old_mut().name[..13].copy_from_slice(b"././@LongLink");
        long.set_mode(0o644);
        long.set_mtime(0);
        long.set_uid(0);
        long.set_gid(0);
        long.set_entry_type(if link { EntryType::GNULongLink } else { EntryType::GNULongName });
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

/// The header name GNU tar gives a pax-form sparse file named `name`:
/// `<folder>/GNUSparseFile.0/<file>`, its real name being in a pax record.
fn made_up_sparse_name(name: &str) -> Vec<u8> {
    let (folder, file) = name.rsplit_once('/').unwrap_or((".", name));
    format!("{folder}/GNUSparseFile.0/{file}").into_bytes()
}

/// The name readers show for `member`, and its parts if it's one that
/// can be shown and changed (not a global pax header, nor like `../x`).
fn shown(member: &Member) -> (String, Option<Vec<String>>) {
    let name = String::from_utf8_lossy(&member.name()).into_owned();
    let parts = clean_parts(&name)
        .filter(|_| !member.header.entry_type().is_pax_global_extensions())
        .map(|parts| parts.into_iter().map(str::to_owned).collect());
    (name, parts)
}

/// The key of a hard link's target.
fn link_key(member: &Member) -> String {
    key(&String::from_utf8_lossy(&member.link_name().unwrap_or_default()))
}

/// What became of a member that hard links point at, as an edit goes.
enum Fate {
    /// It's stored under this name now.
    Moved(Vec<u8>),
    /// It was removed: its data waits in a spool file for the first link
    /// left, which takes its place.
    Spooled(PathBuf, Box<Member>),
    /// A removed link: links to it follow its target (this key).
    Alias(String),
}

/// The fate of the member `key` names (through removed links), and the key
/// it's stored under.
fn resolve<'a>(fates: &'a HashMap<String, Fate>, key: &str) -> Option<(&'a str, &'a Fate)> {
    let mut key = key;
    for _ in 0..64 {
        match fates.get_key_value(key)? {
            (_, Fate::Alias(next)) => key = next,
            (at, fate) => return Some((at, fate)),
        }
    }
    None
}

/// Hard links in an archive, found before changing it.
#[derive(Default)]
struct Links {
    /// Keys of the members that links which stay point at (directly or
    /// through other links).
    needed: HashSet<String>,
    /// Keys of all members.
    present: HashSet<String>,
}

/// Reads through `archive` for its hard links; `removed` says which
/// members the edit drops.
fn scan_links(archive: &Path, format: Format, removed: &dyn Fn(&str) -> bool) -> io::Result<Links> {
    match format {
        // Plain tars: data skipped by seeking.
        Format::Tar => {
            let reader = Reader { input: BufReader::new(File::open(archive)?) };
            scan_links_in(reader, |r, n| r.input.seek_relative(n as i64), removed)
        }
        _ => scan_links_in(Reader { input: crate::vfs::open_stream(archive, format)? }, |r, n| r.skip(n), removed),
    }
}

fn scan_links_in<R: Read>(
    mut reader: Reader<R>,
    skip: fn(&mut Reader<R>, u64) -> io::Result<()>,
    removed: &dyn Fn(&str) -> bool,
) -> io::Result<Links> {
    let mut links = Links::default();
    let mut target_of: HashMap<String, String> = HashMap::new();
    let mut staying = Vec::new();
    while let Some(member) = reader.member()? {
        skip(&mut reader, member.size + padding(member.size))?;
        let (_, Some(parts)) = shown(&member) else { continue };
        let key = parts.join("/").to_lowercase();
        if member.header.entry_type().is_hard_link() {
            let target = link_key(&member);
            if !removed(&key) {
                staying.push(target.clone());
            }
            target_of.insert(key.clone(), target);
        }
        links.present.insert(key);
    }
    for mut target in staying {
        for _ in 0..64 {
            if !links.needed.insert(target.clone()) {
                break;
            }
            match target_of.get(&target) {
                Some(next) => target = next.clone(),
                None => break,
            }
        }
    }
    Ok(links)
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
    /// Spool files of removed members that hard links point at.
    link_spools: Vec<PathBuf>,
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
            link_spools: Vec::new(),
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
        let remove: Vec<String> = edits.remove.iter().map(|r| key(r)).collect();
        let relocate: Vec<(String, &str, bool)> = edits.relocate.iter().map(|(f, t, k)| (key(f), t.as_str(), *k)).collect();
        let removed = |key: &str| remove.iter().any(|r| matches(key, r).is_some());
        let place = |key: &str| relocate.iter().find_map(|(from, to, keep)| matches(key, from).map(|depth| (depth, *to, *keep)));
        // Removed, or moved away.
        let gone = |key: &str| removed(key) || place(key).is_some_and(|(_, _, keep)| !keep);
        // Hard links to members that go are rewritten: find which members
        // the links left point at first.
        let links = match remove.is_empty() && relocate.iter().all(|(_, _, keep)| *keep) {
            true => Links::default(),
            false => scan_links(&self.archive, self.format, &removed)?,
        };
        let mut fates: HashMap<String, Fate> = HashMap::new();
        let mut reader = Reader { input: crate::vfs::open_stream(&self.archive, self.format)? };
        while let Some(member) = reader.member()? {
            if !alive() {
                return Ok(false);
            }
            let kind = member.header.entry_type();
            let (name, parts) = shown(&member);
            let Some(parts) = parts else {
                self.copy_member(&mut reader, &member, None, None)?;
                continue;
            };
            let key = parts.join("/").to_lowercase();
            let needed = links.needed.contains(&key);
            if removed(&key) {
                if !needed {
                    reader.skip(member.size + padding(member.size))?;
                } else if kind.is_hard_link() {
                    // Links to this one follow its target.
                    reader.skip(member.size + padding(member.size))?;
                    let target = link_key(&member);
                    let fate = match resolve(&fates, &target) {
                        Some(_) => Fate::Alias(target),
                        None => Fate::Moved(member.link_name().unwrap_or_default()),
                    };
                    fates.insert(key, fate);
                } else {
                    // Kept for the first link left, which takes its place.
                    let spool = self.archive.with_file_name(format!(
                        ".{}.{}.{}.rcspool",
                        self.archive.file_name().unwrap_or_default().to_string_lossy(),
                        std::process::id(),
                        self.link_spools.len()
                    ));
                    self.link_spools.push(spool.clone());
                    let mut file = BufWriter::new(File::create(&spool)?);
                    reader.copy(member.size, &mut file)?;
                    file.into_inner().map_err(|e| e.into_error())?;
                    fates.insert(key, Fate::Spooled(spool, Box::new(member)));
                }
                continue;
            }
            if kind == EntryType::Directory {
                self.dirs.insert(key.clone());
            }
            let moved = place(&key);
            let renamed = moved.map(|(depth, to, _)| {
                let mut renamed = to.trim_end_matches('/').to_owned();
                for part in &parts[depth..] {
                    renamed.push('/');
                    renamed.push_str(part);
                }
                if kind == EntryType::Directory {
                    self.dirs.insert(crate::zip_edit::key(&renamed));
                    renamed.push('/');
                }
                renamed
            });
            let keep = moved.is_some_and(|(_, _, keep)| keep);
            if let (Some(renamed), false, true) = (&renamed, keep, needed) {
                fates.insert(key.clone(), Fate::Moved(renamed.clone().into_bytes()));
            }
            // A hard link whose target went: pointed at its new name, or
            // given its data.
            let mut relink = None;
            if kind.is_hard_link() {
                let target = link_key(&member);
                match resolve(&fates, &target) {
                    Some((_, Fate::Moved(to))) => relink = Some(to.clone()),
                    Some((at, Fate::Spooled(..))) => {
                        let at = at.to_owned();
                        reader.skip(member.size + padding(member.size))?;
                        let first = match (&renamed, keep) {
                            (Some(renamed), false) => renamed.clone(),
                            _ => name,
                        };
                        let Some(Fate::Spooled(spool, target)) = fates.insert(at, Fate::Moved(first.clone().into_bytes()))
                        else {
                            unreachable!("resolved to a spooled member")
                        };
                        self.copy_spooled(&spool, &target, &first)?;
                        if let (Some(renamed), true) = (&renamed, keep) {
                            self.copy_spooled(&spool, &target, renamed)?;
                        }
                        let _ = fs::remove_file(&spool);
                        continue;
                    }
                    _ if gone(&target) && links.present.contains(&target) => {
                        let target = String::from_utf8_lossy(&member.link_name().unwrap_or_default()).into_owned();
                        return Err(io::Error::other(format!(
                            "{name} in the archive is a hard link to {target}, which comes after it and would be lost: not changed"
                        )));
                    }
                    _ => {}
                }
            }
            let relink = relink.as_deref();
            let Some(renamed) = renamed else {
                self.copy_member(&mut reader, &member, None, relink)?;
                continue;
            };
            if !keep {
                self.copy_member(&mut reader, &member, Some(&renamed), relink)?;
                continue;
            }
            // Written twice: the data goes through the spool file.
            let mut spool = File::options().read(true).write(true).create(true).truncate(true).open(&self.spool)?;
            reader.copy(member.size, &mut spool)?;
            spool.seek(SeekFrom::Start(0))?;
            self.copy_member(&mut Reader { input: BufReader::new(&spool) }, &member, None, relink)?;
            spool.seek(SeekFrom::Start(0))?;
            self.copy_member(&mut Reader { input: BufReader::new(&spool) }, &member, Some(&renamed), relink)?;
        }
        // To the end, so the gz/bz2 checksum is checked too.
        io::copy(&mut reader.input, &mut io::sink())?;
        Ok(true)
    }

    /// Writes the removed `target`, its data kept in `spool`, as `name`.
    fn copy_spooled(&mut self, spool: &Path, target: &Member, name: &str) -> io::Result<()> {
        let file = File::open(spool)?;
        self.copy_member(&mut Reader { input: BufReader::new(file) }, target, Some(name), None)
    }

    /// Writes `member` (its data read from `reader`), under `rename` if
    /// given: then its long-name header and pax `path` are dropped (a
    /// pax-form sparse file gets its sparse name changed and a made-up
    /// header name, as GNU tar does). `relink` likewise gives a link a new
    /// target.
    fn copy_member<R: Read>(
        &mut self,
        reader: &mut Reader<R>,
        member: &Member,
        rename: Option<&str>,
        relink: Option<&[u8]>,
    ) -> io::Result<()> {
        let out = self.out.as_mut().expect("output is present until commit");
        for (h, data) in &member.exts {
            let kind = h.entry_type();
            let mut h = h.clone();
            match (rename, relink) {
                (Some(_), _) if kind.is_gnu_longname() => {}
                (_, Some(_)) if kind.is_gnu_longlink() => {}
                (Some(_), _) | (_, Some(_)) if kind.is_pax_local_extensions() => {
                    let kept: Vec<u8> = tar::PaxExtensions::new(data)
                        .filter_map(|e| e.ok())
                        .filter_map(|e| match (e.key_bytes(), rename) {
                            (b"path", Some(_)) => None,
                            (b"linkpath", _) if relink.is_some() => None,
                            (b"GNU.sparse.name", Some(name)) => Some(pax_record(e.key_bytes(), name.as_bytes())),
                            (k, _) => Some(pax_record(k, e.value_bytes())),
                        })
                        .flatten()
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
        let mut h = member.header.clone();
        if let Some(target) = relink {
            write_long(out, &mut h, target, true)?;
        }
        if let Some(name) = rename {
            match member.sparse_name() {
                Some(_) => write_long(out, &mut h, &made_up_sparse_name(name), false)?,
                None => write_long(out, &mut h, name.as_bytes(), false)?,
            }
        }
        if rename.is_some() || relink.is_some() {
            h.set_cksum();
        }
        out.write_all(h.as_bytes())?;
        out.write_all(&member.sparse)?;
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
        for spool in &self.link_spools {
            let _ = fs::remove_file(spool);
        }
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

    /// A tar written by hand, member by member.
    #[derive(Default)]
    struct Hand(Vec<u8>);

    impl Hand {
        fn header(name: &str, kind: EntryType, size: u64) -> Header {
            let mut h = Header::new_gnu();
            h.set_entry_type(kind);
            h.set_mode(0o640);
            h.set_uid(1000);
            h.set_gid(100);
            h.set_mtime(1_600_000_000);
            h.set_size(size);
            h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            h
        }

        fn file(&mut self, name: &str, data: &[u8]) {
            write_block(&mut self.0, &mut Hand::header(name, EntryType::Regular, data.len() as u64), data).unwrap();
        }

        fn link(&mut self, name: &str, target: &str) {
            let mut h = Hand::header(name, EntryType::Link, 0);
            h.as_old_mut().linkname[..target.len()].copy_from_slice(target.as_bytes());
            write_block(&mut self.0, &mut h, &[]).unwrap();
        }

        fn pax(&mut self, records: &[(&str, &str)]) {
            let data: Vec<u8> = records.iter().flat_map(|(k, v)| pax_record(k.as_bytes(), v.as_bytes())).collect();
            let mut x = Hand::header("PaxHeader/x", EntryType::XHeader, data.len() as u64);
            write_block(&mut self.0, &mut x, &data).unwrap();
        }

        fn save(mut self, path: &Path) {
            self.0.extend_from_slice(&[0; 2 * BLOCK]);
            fs::write(path, &self.0).unwrap();
        }
    }

    /// Writes `v` as a NUL-ended octal number filling `field`.
    fn octal(field: &mut [u8], v: u64) {
        let s = format!("{:0w$o}", v, w = field.len() - 1);
        field[..s.len()].copy_from_slice(s.as_bytes());
        field[s.len()] = 0;
    }

    /// A sparse file's contents (`size` bytes, data in `segments`, zeros
    /// between), and its data as stored.
    fn sparse_data(size: u64, segments: &[(u64, u64)], seed: u8) -> (Vec<u8>, Vec<u8>) {
        let mut full = vec![0u8; size as usize];
        let mut packed = Vec::new();
        for (n, &(offset, len)) in segments.iter().enumerate() {
            for i in offset..offset + len {
                let b = (i % 251) as u8 ^ seed ^ (n as u8 + 1) | 1;
                full[i as usize] = b;
                packed.push(b);
            }
        }
        (full, packed)
    }

    /// A tar holding sparse files in each GNU form (old 'S' with a map that
    /// goes on in an extension block; pax 0.0, 0.1 and 1.0), a plain file
    /// and a hard link to the old-style sparse file. Returns the sparse
    /// files' names and contents.
    fn make_sparse(path: &Path) -> Vec<(&'static str, Vec<u8>)> {
        let mut tar = Hand::default();
        // Old GNU: four segments in the header, more in an extension
        // block; data 512-aligned but for the last, a hole at the end (an
        // empty segment at the end, as GNU tar writes it).
        let segments = [(0, 512), (1024, 512), (2048, 512), (3072, 512), (4096, 100), (6000, 0)];
        let (old, packed) = sparse_data(6000, &segments, 0x10);
        let mut h = Hand::header("sp/old.bin", EntryType::GNUSparse, packed.len() as u64);
        let b = h.as_mut_bytes();
        for (i, &(offset, len)) in segments[..4].iter().enumerate() {
            octal(&mut b[386 + 24 * i..398 + 24 * i], offset);
            octal(&mut b[398 + 24 * i..410 + 24 * i], len);
        }
        b[482] = 1;
        octal(&mut b[483..495], 6000);
        h.set_cksum();
        let mut ext = [0u8; BLOCK];
        octal(&mut ext[0..12], 4096);
        octal(&mut ext[12..24], 100);
        octal(&mut ext[24..36], 6000);
        octal(&mut ext[36..48], 0);
        tar.0.extend_from_slice(h.as_bytes());
        tar.0.extend_from_slice(&ext);
        tar.0.extend_from_slice(&packed);
        tar.0.resize(tar.0.len().next_multiple_of(BLOCK), 0);
        // Pax 0.0: offset / numbytes records, the real name as the path.
        let (v00, packed) = sparse_data(900, &[(5, 7), (300, 20)], 0x20);
        let size = packed.len().to_string();
        tar.pax(&[
            ("path", "sp/v00.bin"),
            ("GNU.sparse.size", "900"),
            ("GNU.sparse.numblocks", "2"),
            ("GNU.sparse.offset", "5"),
            ("GNU.sparse.numbytes", "7"),
            ("GNU.sparse.offset", "300"),
            ("GNU.sparse.numbytes", "20"),
            ("size", &size),
        ]);
        tar.file("sp/v00.bin", &packed);
        // Pax 0.1: a map record and the real name, a made-up header name.
        let (v01, packed) = sparse_data(2000, &[(0, 3), (1990, 10)], 0x30);
        tar.pax(&[("GNU.sparse.size", "2000"), ("GNU.sparse.numblocks", "2"), ("GNU.sparse.map", "0,3,1990,10"), (
            "GNU.sparse.name",
            "sp/v01.bin",
        )]);
        tar.file("sp/GNUSparseFile.7/v01.bin", &packed);
        // Pax 1.0: the map at the start of the data.
        let (v1, packed) = sparse_data(70_000, &[(3, 10), (700, 5), (65_536, 1000)], 0x40);
        let mut data = b"3\n3\n10\n700\n5\n65536\n1000\n".to_vec();
        data.resize(BLOCK, 0);
        data.extend_from_slice(&packed);
        tar.pax(&[
            ("GNU.sparse.major", "1"),
            ("GNU.sparse.minor", "0"),
            ("GNU.sparse.name", "sp/v1.bin"),
            ("GNU.sparse.realsize", "70000"),
        ]);
        tar.file("sp/GNUSparseFile.7/v1.bin", &data);
        tar.file("sp/plain.txt", b"plain");
        tar.file("keep.txt", b"keep");
        tar.link("hl", "sp/old.bin");
        tar.save(path);
        vec![("old.bin", old), ("v00.bin", v00), ("v01.bin", v01), ("v1.bin", v1)]
    }

    /// The members as stored: name, member, all its bytes (headers before
    /// it, its header, sparse blocks, data).
    fn stored(path: &Path) -> Vec<(String, Member, Vec<u8>)> {
        let mut r = Reader { input: crate::vfs::open_stream(path, Format::Tar).unwrap() };
        let mut all = Vec::new();
        while let Some(m) = r.member().unwrap() {
            let mut bytes = Vec::new();
            for (h, d) in &m.exts {
                bytes.extend_from_slice(h.as_bytes());
                bytes.extend_from_slice(d);
                bytes.resize(bytes.len().next_multiple_of(BLOCK), 0);
            }
            bytes.extend_from_slice(m.header.as_bytes());
            bytes.extend_from_slice(&m.sparse);
            r.copy(m.size, &mut bytes).unwrap();
            all.push((String::from_utf8_lossy(&m.name()).into_owned(), m, bytes));
        }
        all
    }

    fn stored_one<'a>(all: &'a [(String, Member, Vec<u8>)], name: &str) -> &'a (String, Member, Vec<u8>) {
        all.iter().find(|m| m.0 == name).unwrap_or_else(|| panic!("{name} not in {:?}", all.iter().map(|m| &m.0).collect::<Vec<_>>()))
    }

    /// Lists `folder` (inside an archive) and copies it out: the files' names,
    /// sizes and contents must be `want`.
    fn check_out(folder: &Path, out: &Path, want: &[(&str, Vec<u8>)]) {
        let mut listed: Vec<(String, u64)> =
            crate::vfs::list(folder, true).unwrap().into_iter().filter(|e| e.name != "..").map(|e| (e.name, e.size)).collect();
        listed.sort();
        let mut expected: Vec<(String, u64)> = want.iter().map(|(n, d)| (n.to_string(), d.len() as u64)).collect();
        expected.sort();
        assert_eq!(listed, expected);
        fs::create_dir_all(out).unwrap();
        ok(Job::Copy(ops::plan_transfer(&[folder.to_path_buf()], out).unwrap()));
        let name = folder.file_name().unwrap();
        for (file, data) in want {
            assert!(fs::read(out.join(name).join(file)).unwrap() == *data, "{file}");
        }
    }

    /// If a `tar` program is at hand, it extracts `tar` and agrees on the
    /// `files`' contents (the test doesn't need it).
    fn system_tar_agrees(tar: &Path, out: &Path, files: &[(&str, &[u8])]) {
        let tool = || std::process::Command::new("tar");
        if !tool().arg("--version").output().is_ok_and(|o| o.status.success()) {
            return;
        }
        fs::create_dir_all(out).unwrap();
        let o = tool().arg("-xf").arg(tar).arg("-C").arg(out).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        for (name, data) in files {
            assert!(fs::read(out.join(name)).unwrap() == *data, "{name}");
        }
    }

    #[test]
    fn keeps_sparse_files() {
        let tmp = TempDir::new("tar-sparse");
        let tar = tmp.0.join("s.tar");
        let mut want = make_sparse(&tar);
        want.push(("plain.txt", b"plain".to_vec()));
        // Shown with their real names and sizes, read expanded.
        check_out(&tar.join("sp"), &tmp.0.join("out1"), &want);
        let before = stored(&tar);

        // Other members changed: the sparse ones are copied byte for byte.
        ok(Job::Delete { paths: vec![tar.join("keep.txt")], to_trash: false });
        ok(Job::MakeDir(tar.join("new")));
        let after = stored(&tar);
        for name in ["sp/old.bin", "sp/v00.bin", "sp/v01.bin", "sp/v1.bin", "hl"] {
            assert!(stored_one(&after, name).2 == stored_one(&before, name).2, "{name}");
        }

        // Renamed (with their folder): still sparse, the link follows.
        ok(Job::Rename { from: tar.join("sp"), to: tar.join("moved") });
        check_out(&tar.join("moved"), &tmp.0.join("out2"), &want);
        let after = stored(&tar);
        let old = stored_one(&after, "moved/old.bin");
        assert_eq!(old.1.header.entry_type(), EntryType::GNUSparse);
        assert_eq!(old.1.sparse, stored_one(&before, "sp/old.bin").1.sparse);
        let v1 = &stored_one(&after, "moved/v1.bin").1;
        let pax: Vec<(&[u8], &[u8])> = v1.pax().collect();
        assert!(pax.contains(&(&b"GNU.sparse.major"[..], &b"1"[..])), "{pax:?}");
        assert_eq!(v1.header.path_bytes().as_ref(), b"moved/GNUSparseFile.0/v1.bin");
        assert_eq!(stored_one(&after, "hl").1.link_name().unwrap(), b"moved/old.bin");

        // Replaced: a plain file now.
        let entry = tar.join("moved").join("v1.bin");
        let stamp = ops::entry_stamp(&entry).unwrap();
        let copy = tmp.0.join("v1.bin");
        fs::write(&copy, b"not sparse").unwrap();
        ok(Job::Update { src: copy, dst: entry, expect: stamp });
        let after = stored(&tar);
        let v1 = &stored_one(&after, "moved/v1.bin").1;
        assert!(v1.exts.is_empty() && v1.header.entry_type() == EntryType::Regular);
        want[3].1 = b"not sparse".to_vec();
        check_out(&tar.join("moved"), &tmp.0.join("out3"), &want);

        // The link's sparse target removed: the link becomes a sparse file.
        ok(Job::Delete { paths: vec![tar.join("moved").join("old.bin")], to_trash: false });
        let after = stored(&tar);
        assert_eq!(stored_one(&after, "hl").1.header.entry_type(), EntryType::GNUSparse);
        fs::create_dir_all(tmp.0.join("out4")).unwrap();
        ok(Job::Copy(ops::plan_transfer(&[tar.join("hl")], &tmp.0.join("out4")).unwrap()));
        assert!(fs::read(tmp.0.join("out4").join("hl")).unwrap() == want[0].1);
        let files: Vec<(String, &[u8])> = want[1..].iter().map(|(n, d)| (format!("moved/{n}"), d.as_slice())).collect();
        let mut files: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (n.as_str(), *d)).collect();
        files.push(("hl", &want[0].1));
        system_tar_agrees(&tar, &tmp.0.join("system"), &files);
        assert!(leftovers(&tmp.0).is_empty());
    }

    /// Kinds and link targets as the `tar` crate reads them, and the
    /// files it unpacks.
    fn links_of(tar: &Path) -> Vec<(String, EntryType, Option<String>)> {
        let mut archive = tar::Archive::new(File::open(tar).unwrap());
        let entries = archive.entries().unwrap().map(|e| e.unwrap());
        let link = |e: &tar::Entry<File>| e.link_name().unwrap().map(|l| l.to_string_lossy().into_owned());
        entries.map(|e| (e.path().unwrap().to_string_lossy().into_owned(), e.header().entry_type(), link(&e))).collect()
    }

    fn unpacks(tar: &Path, out: &Path, files: &[(&str, &[u8])]) {
        tar::Archive::new(File::open(tar).unwrap()).unpack(out).unwrap();
        for (name, data) in files {
            assert!(fs::read(out.join(name)).unwrap() == *data, "{name}");
        }
    }

    #[test]
    fn keeps_hard_links_working() {
        let tmp = TempDir::new("tar-hard");
        let tar = tmp.0.join("a.tar");
        let mut hand = Hand::default();
        hand.file("t.txt", b"target data");
        hand.link("a", "t.txt");
        hand.link("b", "t.txt");
        hand.link("c", "t.txt");
        hand.file("other.txt", b"other");
        hand.save(&tar);
        use EntryType::{Link, Regular};
        let some = |s: &str| Some(s.to_owned());

        // The target removed: the first link takes its place, the others
        // point at it.
        ok(Job::Delete { paths: vec![tar.join("t.txt")], to_trash: false });
        let all = links_of(&tar);
        assert_eq!(all, [
            ("a".into(), Regular, None),
            ("b".into(), Link, some("a")),
            ("c".into(), Link, some("a")),
            ("other.txt".into(), Regular, None)
        ]);
        let a = &stored(&tar)[0].1;
        assert_eq!((a.header.mode().unwrap(), a.header.uid().unwrap()), (0o640, 1000));
        unpacks(&tar, &tmp.0.join("x1"), &[("a", b"target data"), ("b", b"target data"), ("c", b"target data")]);

        // The target renamed (to a long name, too): the links follow.
        ok(Job::Rename { from: tar.join("a"), to: tar.join("z") });
        assert_eq!(links_of(&tar)[1], ("b".into(), Link, some("z")));
        let long = format!("dir/{}", "l".repeat(150));
        ok(Job::Rename { from: tar.join("z"), to: tar.join(&long) });
        let all = links_of(&tar);
        assert_eq!((&all[1], &all[2]), (&("b".into(), Link, Some(long.clone())), &("c".into(), Link, Some(long.clone()))));
        unpacks(&tar, &tmp.0.join("x2"), &[(&long, b"target data"), ("b", b"target data"), ("c", b"target data")]);

        // The target replaced: the links keep the old data.
        let entry = tar.join(&long);
        let stamp = ops::entry_stamp(&entry).unwrap();
        let copy = tmp.0.join("new.txt");
        fs::write(&copy, b"new data").unwrap();
        ok(Job::Update { src: copy, dst: entry, expect: stamp });
        let all = links_of(&tar);
        let all: Vec<_> = all.into_iter().filter(|m| m.1 != EntryType::Directory).collect();
        let names: Vec<(&str, EntryType)> = all.iter().map(|m| (m.0.as_str(), m.1)).collect();
        assert_eq!(names, [("b", Regular), ("c", Link), ("other.txt", Regular), (&long, Regular)]);
        assert_eq!(all[1].2, some("b"));
        unpacks(&tar, &tmp.0.join("x3"), &[(&long, b"new data"), ("b", b"target data"), ("c", b"target data")]);

        // Target and link removed together, the last link is moved.
        ok(Job::Rename { from: tar.join("c"), to: tar.join("dir").join("c") });
        ok(Job::Delete { paths: vec![tar.join("b")], to_trash: false });
        assert_eq!(links_of(&tar)[0], ("dir/c".into(), Regular, None));
        unpacks(&tar, &tmp.0.join("x4"), &[("dir/c", b"target data")]);
        system_tar_agrees(&tar, &tmp.0.join("system"), &[("dir/c", b"target data"), (&long, b"new data")]);
        assert!(leftovers(&tmp.0).is_empty());
    }

    #[test]
    fn hard_links_through_folders_chains_and_order() {
        let tmp = TempDir::new("tar-hard-more");
        let some = |s: &str| Some(s.to_owned());
        // A folder with the target renamed, then removed.
        let tar = tmp.0.join("a.tar");
        make(&tar, Format::Tar);
        ok(Job::Rename { from: tar.join("docs"), to: tar.join("papers") });
        assert_eq!(links_of(&tar)[4], ("hard".into(), EntryType::Link, some("papers/one.txt")));
        ok(Job::Copy(ops::plan_transfer(&[tar.join("papers")], &tar.join("copy")).unwrap()));
        ok(Job::Delete { paths: vec![tar.join("papers")], to_trash: false });
        let all = links_of(&tar);
        let hard = all.iter().find(|m| m.0 == "hard").unwrap();
        assert_eq!(hard, &("hard".into(), EntryType::Regular, None));
        // (Not unpacked: it has a symlink, which needs rights on Windows.)
        assert_eq!(find(&members(&tar, Format::Tar), "hard").3, b"first");

        // A link to a link: both removed with the target, the last one
        // gets the data.
        let tar = tmp.0.join("chain.tar");
        let mut hand = Hand::default();
        hand.file("t", b"data");
        hand.link("l1", "t");
        hand.link("l2", "l1");
        hand.save(&tar);
        ok(Job::Delete { paths: vec![tar.join("t"), tar.join("l1")], to_trash: false });
        assert_eq!(links_of(&tar), [("l2".into(), EntryType::Regular, None)]);
        unpacks(&tar, &tmp.0.join("x2"), &[("l2", b"data")]);

        // A link before its target: refused, nothing changed.
        let tar = tmp.0.join("order.tar");
        let mut hand = Hand::default();
        hand.link("l", "t");
        hand.file("t", b"data");
        hand.save(&tar);
        let before = fs::read(&tar).unwrap();
        let o = run(Job::Delete { paths: vec![tar.join("t")], to_trash: false });
        assert!(o.errors.len() == 1 && o.errors[0].1.contains("hard link"), "{:?}", o.errors);
        assert_eq!(fs::read(&tar).unwrap(), before);
        // Removed with it, or a link to nothing: fine.
        ok(Job::Delete { paths: vec![tar.join("t"), tar.join("l")], to_trash: false });
        let tar = tmp.0.join("dangling.tar");
        let mut hand = Hand::default();
        hand.link("l", "nothing");
        hand.file("t", b"data");
        hand.save(&tar);
        ok(Job::Delete { paths: vec![tar.join("t")], to_trash: false });
        assert_eq!(links_of(&tar), [("l".into(), EntryType::Link, some("nothing"))]);
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

        // A sparse member whose map goes on in a block that isn't there.
        let mut h = Header::new_old();
        h.as_mut_bytes().copy_from_slice(&bytes[..BLOCK]);
        h.set_entry_type(EntryType::GNUSparse);
        h.as_mut_bytes()[482] = 1;
        h.set_cksum();
        let sp = tmp.0.join("sparse.tar");
        fs::write(&sp, h.as_bytes()).unwrap();
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
