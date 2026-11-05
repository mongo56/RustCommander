//! Changing plain `.gz` files (one gzipped file, not a tar), like
//! [`crate::zip_edit`] does for zips. A `.gz` holds exactly one file, named
//! like the archive without `.gz` (that's how it's listed), so the only
//! change is replacing that file: its new contents are compressed into a
//! temp file next to the archive, which then replaces it in one rename
//! (the header gets the member's name and date, the archive file its date,
//! like gzip does). Until [`GzEdit::commit`] the archive is untouched;
//! dropping an uncommitted edit removes the temp file.
//!
//! Refused (an error, nothing changed): folders, a file with another name
//! (a second file), renaming or copying the member within the archive (its
//! name is the archive's: rename the `.gz` instead), and leaving the archive
//! empty. Removing the member removes the `.gz` itself; that's done by
//! `ops`, not here.

use crate::zip_edit::{key, matches, Edits};
use flate2::write::GzEncoder;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct GzEdit {
    archive: PathBuf,
    temp: PathBuf,
    /// The one file's name: the archive's without `.gz`.
    member: String,
    /// The current file stays as it is (nothing to write).
    kept: bool,
    /// The new file being (or written) compressed.
    out: Option<GzEncoder<BufWriter<File>>>,
    modified: Option<SystemTime>,
    none: NoFile,
}

/// The output when no file was started: refuses writes.
struct NoFile;

impl Write for NoFile {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("No file started in the .gz"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl GzEdit {
    /// Starts rewriting the `.gz` `archive` (it may not exist yet).
    pub fn begin(archive: &Path) -> io::Result<GzEdit> {
        let name = archive.file_name().ok_or_else(|| io::Error::other("Invalid archive path"))?.to_string_lossy();
        let temp = archive.with_file_name(format!(".{name}.{}.rctmp", std::process::id()));
        let member = archive.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let archive = archive.to_path_buf();
        Ok(GzEdit { archive, temp, member, kept: false, out: None, modified: None, none: NoFile })
    }

    fn one_file(&self) -> String {
        format!("A .gz holds just one file, named like the archive ({})", self.member)
    }

    /// Takes in what `edits` say about the current file (if the archive
    /// exists): removed (to be replaced) or kept.
    pub fn copy_existing(&mut self, edits: &Edits, alive: &mut dyn FnMut() -> bool) -> io::Result<bool> {
        if !self.archive.exists() {
            return Ok(true);
        }
        if !alive() {
            return Ok(false);
        }
        let member = key(&self.member);
        if edits.relocate.iter().any(|(from, _, _)| matches(&member, &key(from)).is_some()) {
            let one = self.one_file();
            return Err(io::Error::other(format!("{one}: it can't be renamed or copied inside it (rename the .gz instead)")));
        }
        self.kept = !edits.remove.iter().any(|r| matches(&member, &key(r)).is_some());
        Ok(true)
    }

    pub fn add_dir(&mut self, _inner: &str, _modified: Option<SystemTime>) -> io::Result<()> {
        Err(io::Error::other(format!("{}: it can't hold folders", self.one_file())))
    }

    /// Starts the file (it must be named like the archive, and the current
    /// one removed); write its contents with [`Self::output`].
    pub fn start_file(&mut self, inner: &str, modified: Option<SystemTime>) -> io::Result<()> {
        if key(inner) != key(&self.member) {
            return Err(io::Error::other(format!("{}: can't add {inner}", self.one_file())));
        }
        if self.kept || self.out.is_some() {
            return Err(io::Error::other(format!("The .gz already holds {}", self.member)));
        }
        let secs = modified.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let file = BufWriter::new(File::create(&self.temp)?);
        let builder = flate2::GzBuilder::new().filename(self.member.as_bytes()).mtime(u32::try_from(secs).unwrap_or(0));
        self.out = Some(builder.write(file, flate2::Compression::default()));
        self.modified = modified;
        Ok(())
    }

    pub fn output(&mut self) -> &mut dyn Write {
        match &mut self.out {
            Some(out) => out,
            // Only after a refused `start_file` (whose error stops the rewrite).
            None => &mut self.none,
        }
    }

    /// Drops the file being written.
    pub fn abort_file(&mut self) -> io::Result<()> {
        drop(self.out.take());
        let _ = fs::remove_file(&self.temp);
        Ok(())
    }

    /// Finishes the new archive and puts it in place of the old one.
    pub fn commit(mut self) -> io::Result<()> {
        let Some(out) = self.out.take() else {
            return match self.kept {
                true => Ok(()),
                false => Err(io::Error::other(format!("{}: it can't be left empty", self.one_file()))),
            };
        };
        let file = out.finish()?.into_inner().map_err(|e| e.into_error())?;
        if let Some(t) = self.modified {
            let _ = file.set_modified(t);
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&self.temp, &self.archive)?;
        crate::vfs::forget(&self.archive);
        Ok(())
    }
}

impl Drop for GzEdit {
    fn drop(&mut self) {
        drop(self.out.take());
        let _ = fs::remove_file(&self.temp);
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::{self, entry_stamp, Conflict, Control, Job, Outcome, Progress, Reporter, Resolution, Transfer};
    use crate::vfs::{self, tests::TempDir};
    use std::fs::{self, File};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// Answers every conflict the same way, counting them.
    struct Answer(Resolution, usize);
    impl Reporter for Answer {
        fn progress(&mut self, _: &Progress) {}
        fn conflict(&mut self, _: Conflict) -> Resolution {
            self.1 += 1;
            self.0
        }
    }

    fn run(job: Job, answer: Resolution) -> (Outcome, usize) {
        let mut reporter = Answer(answer, 0);
        let outcome = ops::run(&job, &Control::default(), &mut reporter);
        (outcome, reporter.1)
    }

    fn ok(job: Job) {
        let (o, _) = run(job, Resolution::Cancel);
        assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
    }

    /// One error, mentioning `text`.
    fn refused(job: Job, text: &str) {
        let (o, _) = run(job, Resolution::Overwrite);
        assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
        assert!(format!("{:?}", o.errors[0]).contains(text), "{:?}", o.errors);
    }

    fn make(path: &Path, data: &[u8]) {
        let mut w = flate2::GzBuilder::new().filename("original").write(File::create(path).unwrap(), Default::default());
        w.write_all(data).unwrap();
        w.finish().unwrap();
    }

    /// The contents and the header's file name.
    fn read(path: &Path) -> (Vec<u8>, Option<String>) {
        let mut d = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut data = Vec::new();
        d.read_to_end(&mut data).unwrap();
        let name = d.header().and_then(|h| h.filename()).map(|n| String::from_utf8_lossy(n).into_owned());
        (data, name)
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let names = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned());
        names.filter(|n| n.contains(".rctmp")).collect()
    }

    fn copy(src: &Path, dst: PathBuf) -> Job {
        Job::Copy(vec![Transfer { src: src.to_path_buf(), dst }])
    }

    #[test]
    fn replaces_the_one_file() {
        let tmp = TempDir::new("gz-edit");
        let gz = tmp.0.join("notes.txt.gz");
        make(&gz, b"old text");
        assert_eq!(vfs::archive_writable(&gz), Some(true));
        let entry = gz.join("notes.txt");

        // F4 save: put back without asking; recompressed with its name.
        let edited = tmp.0.join("edited");
        fs::create_dir(&edited).unwrap();
        let copy_of = edited.join("notes.txt");
        fs::write(&copy_of, b"new text, longer than before").unwrap();
        let date = SystemTime::now() - Duration::from_secs(3600);
        File::options().write(true).open(&copy_of).unwrap().set_modified(date).unwrap();
        ok(Job::Update { src: copy_of.clone(), dst: entry.clone(), expect: entry_stamp(&entry).unwrap() });
        assert_eq!(read(&gz), (b"new text, longer than before".to_vec(), Some("notes.txt".into())));
        let listed = vfs::list(&gz, true).unwrap();
        assert_eq!((listed[1].name.as_str(), listed[1].size), ("notes.txt", 28));
        // The archive gets the file's date (shown as the member's).
        let stamp = fs::metadata(&gz).unwrap().modified().unwrap();
        assert!(stamp.duration_since(date).unwrap_or_default() < Duration::from_secs(1));
        // A stale stamp isn't overwritten.
        let stale = Some((8, None));
        let (o, _) = run(Job::Update { src: copy_of.clone(), dst: entry.clone(), expect: stale }, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);

        // Copying a file of that name in asks, then replaces it.
        let other = tmp.0.join("other");
        fs::create_dir(&other).unwrap();
        let same = other.join("NOTES.txt");
        fs::write(&same, b"copied in").unwrap();
        let (o, asked) = run(copy(&same, entry.clone()), Resolution::Skip);
        assert_eq!((asked, o.skipped, o.errors.len()), (1, 1, 0));
        assert_eq!(read(&gz).0, b"new text, longer than before");
        let (o, asked) = run(copy(&same, entry.clone()), Resolution::Overwrite);
        assert_eq!((asked, o.errors.len()), (1, 0), "{:?}", o.errors);
        assert_eq!(read(&gz), (b"copied in".to_vec(), Some("notes.txt".into())));

        // A move in removes the source once it's in.
        let (o, _) = run(Job::Move(vec![Transfer { src: same.clone(), dst: entry.clone() }]), Resolution::Overwrite);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(!same.exists());
        assert_eq!(leftovers(&tmp.0), Vec::<String>::new());
    }

    #[test]
    fn refuses_what_a_gz_cant_hold() {
        let tmp = TempDir::new("gz-refuse");
        let gz = tmp.0.join("notes.txt.gz");
        make(&gz, b"keep me");
        let before = fs::read(&gz).unwrap();
        let other = tmp.0.join("other.txt");
        fs::write(&other, b"x").unwrap();
        let folder = tmp.0.join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("notes.txt"), b"x").unwrap();

        refused(copy(&other, gz.join("other.txt")), "just one file");
        refused(copy(&folder, gz.join("folder")), "just one file");
        refused(Job::MakeDir(gz.join("sub")), "just one file");
        refused(Job::Rename { from: gz.join("notes.txt"), to: gz.join("renamed.txt") }, "rename the .gz itself");
        refused(copy(&gz.join("notes.txt"), gz.join("twice.txt")), "just one file");
        refused(Job::Delete { paths: vec![gz.join("missing.txt")], to_trash: false }, "Not found");
        assert_eq!(fs::read(&gz).unwrap(), before);
        assert_eq!(leftovers(&tmp.0), Vec::<String>::new());
    }

    #[test]
    fn removing_the_file_removes_the_gz() {
        let tmp = TempDir::new("gz-remove");
        let gz = tmp.0.join("notes.txt.gz");
        make(&gz, b"moved out");
        let out = tmp.0.join("out");
        fs::create_dir(&out).unwrap();

        // Moved out: extracted, then the (now empty) .gz is deleted.
        ok(Job::Move(vec![Transfer { src: gz.join("notes.txt"), dst: out.join("notes.txt") }]));
        assert_eq!(fs::read(out.join("notes.txt")).unwrap(), b"moved out");
        assert!(!gz.exists());

        // Deleted inside: the .gz goes.
        make(&gz, b"deleted");
        ok(Job::Delete { paths: vec![gz.join("notes.txt")], to_trash: false });
        assert!(!gz.exists());

        // A new .gz by packing a file named like it.
        let src = out.join("notes.txt");
        ok(Job::Pack { sources: vec![src.clone()], archive: gz.clone(), password: None });
        assert_eq!(read(&gz), (b"moved out".to_vec(), Some("notes.txt".into())));
        let wrong = tmp.0.join("wrong.gz");
        refused(Job::Pack { sources: vec![src], archive: wrong.clone(), password: None }, "just one file");
        assert!(!wrong.exists());
        assert_eq!(leftovers(&tmp.0), Vec::<String>::new());
    }

    #[test]
    fn changes_a_gz_inside_a_zip() {
        let tmp = TempDir::new("gz-nested");
        let gz = tmp.0.join("inner.txt.gz");
        make(&gz, b"inner");
        let zip = tmp.0.join("outer.zip");
        ok(Job::Pack { sources: vec![gz.clone()], archive: zip.clone(), password: None });
        fs::remove_file(&gz).unwrap();
        let nested = zip.join("inner.txt.gz");
        let entry = nested.join("inner.txt");
        assert_eq!(vfs::archive_writable(&entry), Some(true));

        let copy_of = tmp.0.join("inner.txt");
        fs::write(&copy_of, b"changed inside").unwrap();
        ok(Job::Update { src: copy_of.clone(), dst: entry.clone(), expect: entry_stamp(&entry).unwrap() });
        let index = vfs::open(&entry).unwrap();
        let mut got = Vec::new();
        index
            .read_files(std::slice::from_ref(&entry), &mut |_, r| {
                r.read_to_end(&mut got)?;
                Ok(true)
            })
            .unwrap();
        assert_eq!(got, b"changed inside");

        // Deleting its file takes the .gz out of the zip.
        ok(Job::Delete { paths: vec![entry], to_trash: false });
        let names: Vec<String> = vfs::list(&zip, true).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, [".."]);
    }
}
