//! File operations (copy, move, delete) as blocking jobs meant to run on a
//! worker thread. The UI talks to a running job through [`JobHandle`] and
//! receives [`JobEvent`]s through a callback.
//!
//! Zip archives can be a destination or source too: every change to one
//! (copy/move in, delete, new folder, rename, move out) is a single rewrite
//! of the archive (`zip_edit`), done after all conflicts are answered, so a
//! cancelled or failed job leaves the archive as it was.
//!
//! Servers (`remote`) are written through their [`Vfs`] directly: uploads
//! file by file (conflicts asked as they come), deletes deepest first;
//! copies out of them are extracts like from an archive. Between servers
//! (or folders of one) each file is downloaded to a temp file, uploaded and
//! deleted; a move on one server is a rename.

use crate::entry::Entry;
use crate::remote;
use crate::vfs::{self, LocalFs, Vfs};
use crate::zip_edit::{Edits, Rewrite};
use chrono::{DateTime, Local};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

const BUFFER_SIZE: usize = 1024 * 1024;
const REPORT_INTERVAL: Duration = Duration::from_millis(50);
const READONLY: &str = "Only zip, 7z, tar and gz archives can be changed (not rar or ones inside those)";

/// One top-level source and where it should end up (full path, not folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub src: PathBuf,
    pub dst: PathBuf,
}

#[derive(Debug, Clone)]
pub enum Job {
    Copy(Vec<Transfer>),
    Move(Vec<Transfer>),
    /// Paths inside an archive are always deleted permanently.
    Delete { paths: Vec<PathBuf>, to_trash: bool },
    /// Creates a folder (with its parents). Local folders are made by the
    /// UI directly; this is for folders inside archives.
    MakeDir(PathBuf),
    /// Renames inside an archive (`to` is the full new path).
    Rename { from: PathBuf, to: PathBuf },
    /// Packs `sources` into the archive `archive` (created if missing, else
    /// added to). With a `password` the archive must be a new zip or 7z: its
    /// files are encrypted with it (AES-256; a 7z's names too).
    Pack { sources: Vec<PathBuf>, archive: PathBuf, password: Option<Secret> },
    /// Unpacks each archive's contents into the folder `target`.
    Unpack { archives: Vec<PathBuf>, target: PathBuf },
    /// Puts the edited temp copy `src` back into its archive as the file `dst`
    /// (F4 inside an archive), replacing it without asking, but only if
    /// that file is still as `expect` says (see [`entry_stamp`]); otherwise
    /// it changed in the archive meanwhile and nothing is overwritten.
    Update { src: PathBuf, dst: PathBuf, expect: Stamp },
    /// Cuts the local file `src` into parts of `part_size` bytes in the folder
    /// `target` (`name.001`, ... and `name.crc`, see [`crate::split`]).
    Split { src: PathBuf, target: PathBuf, part_size: u64 },
    /// Joins the parts `x.001`, `x.002`, ... (`first` is the first) into the file `x`
    /// in the folder `target`, checking `x.crc` if it exists.
    Combine { first: PathBuf, target: PathBuf },
    /// Changes attributes and dates of local `paths` (and, with `recursive`,
    /// of everything inside the folders; links are not followed).
    SetAttrib { paths: Vec<PathBuf>, change: crate::attrib::Change, recursive: bool },
}

/// A password, shown as `***` in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// Why [`Job::Pack`] can't encrypt `archive` with `password`: the password
/// is empty, the archive can't be encrypted (only zip and 7z can), or it
/// exists already (only a new archive gets a password; files added to an
/// existing one would sit next to unencrypted ones).
pub fn pack_password_problem(archive: &Path, password: &Secret) -> Option<&'static str> {
    match archive.file_name().and_then(vfs::pack_encryptable) {
        _ if password.0.is_empty() => Some("The password is empty"),
        Some(true) if archive_exists(archive) => {
            Some("Files can't be added to an existing archive with a password: pack into a new one")
        }
        Some(true) => None,
        _ => Some(vfs::NO_ENCRYPTION),
    }
}

/// Size and date of a file inside an archive (`None`: there's no such
/// file), to tell whether it changed.
pub type Stamp = Option<(u64, Option<DateTime<Local>>)>;

/// The [`Stamp`] of `path` inside an archive, as it is now.
pub fn entry_stamp(path: &Path) -> io::Result<Stamp> {
    Ok(stamp_of(vfs::open(path)?.stat(path)?))
}

fn stamp_of(entry: Option<Entry>) -> Stamp {
    entry.filter(|e| !e.is_dir()).map(|e| (e.size, e.modified))
}

impl Job {
    /// Past-tense verb for summaries: "Copied", "Moved", ...
    pub fn done_verb(&self) -> &'static str {
        match self {
            Job::Copy(_) => "Copied",
            Job::Move(_) => "Moved",
            Job::Delete { to_trash: true, .. } => "Recycled",
            Job::Delete { .. } => "Deleted",
            Job::MakeDir(_) => "Created",
            Job::Rename { .. } => "Renamed",
            Job::Pack { .. } => "Packed",
            Job::Unpack { .. } => "Unpacked",
            Job::Update { .. } => "Updated",
            Job::Split { .. } => "Split",
            Job::Combine { .. } => "Combined",
            Job::SetAttrib { .. } => "Changed",
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Job::Copy(_) => "Copying",
            Job::Move(_) => "Moving",
            Job::Delete { to_trash: true, .. } => "Recycling",
            Job::Delete { .. } => "Deleting",
            Job::MakeDir(_) => "Creating",
            Job::Rename { .. } => "Renaming",
            Job::Pack { .. } => "Packing",
            Job::Unpack { .. } => "Unpacking",
            Job::Update { .. } => "Updating",
            Job::Split { .. } => "Splitting",
            Job::Combine { .. } => "Combining",
            Job::SetAttrib { .. } => "Changing",
        }
    }
}

/// Shared pause/cancel flags, checked between file chunks.
#[derive(Debug, Default)]
pub struct Control {
    cancel: AtomicBool,
    pause: AtomicBool,
}

impl Control {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }

    /// Blocks while paused. Returns false if the job should stop.
    fn checkpoint(&self) -> bool {
        while self.is_paused() && !self.is_cancelled() {
            std::thread::sleep(Duration::from_millis(50));
        }
        !self.is_cancelled()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_items: u64,
    pub done_items: u64,
    pub current: Option<PathBuf>,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.total_bytes > 0 {
            (self.done_bytes as f64 / self.total_bytes as f64).min(1.0) as f32
        } else if self.total_items > 0 {
            (self.done_items as f64 / self.total_items as f64).min(1.0) as f32
        } else {
            0.0
        }
    }
}

/// A file already exists at the destination.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub src_size: u64,
    pub dst_size: u64,
    pub src_modified: Option<DateTime<Local>>,
    pub dst_modified: Option<DateTime<Local>>,
    /// The existing file can be finished instead of replaced
    /// ([`Resolution::Resume`]): a download from or an upload to a server,
    /// and the existing file is smaller.
    pub resumable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Overwrite,
    OverwriteAll,
    /// Overwrite this and all later conflicts where the target is older.
    OverwriteOlder,
    Skip,
    SkipAll,
    /// Keep the existing (smaller) file and transfer only the rest of the
    /// new one onto its end. Only when the conflict is `resumable`; the
    /// result must end up as long as the new file, else it's an error.
    Resume,
    /// Resume this and all later resumable conflicts (the others are asked).
    ResumeAll,
    Cancel,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub done_items: u64,
    pub done_bytes: u64,
    pub skipped: u64,
    pub errors: Vec<(PathBuf, String)>,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
pub enum JobEvent {
    Progress(Progress),
    Conflict(Conflict),
    Finished(Outcome),
}

pub trait Reporter {
    fn progress(&mut self, progress: &Progress);
    /// Called on the worker thread; may block waiting for the user.
    fn conflict(&mut self, conflict: Conflict) -> Resolution;
}

// ---- planning -------------------------------------------------------------

/// Works out where each source goes, Total Commander style:
/// - target is an existing folder, ends with a separator, or there are
///   several sources: copy *into* it, keeping names;
/// - otherwise (single source): `target` is the new full path (copy+rename).
pub fn plan_transfer(sources: &[PathBuf], target: &Path) -> Result<Vec<Transfer>, String> {
    if sources.is_empty() {
        return Err("Nothing selected".into());
    }
    let ends_with_sep = target
        .as_os_str()
        .to_string_lossy()
        .ends_with(['/', '\\']);
    let into_dir = sources.len() > 1 || ends_with_sep || target.is_dir() || is_archive_dir(target);

    let mut transfers = Vec::with_capacity(sources.len());
    for src in sources {
        let name = src
            .file_name()
            .ok_or_else(|| format!("Invalid source: {}", src.display()))?;
        let dst = if into_dir { target.join(name) } else { target.to_path_buf() };

        let (src_n, dst_n) = (normalize_entry(src), normalize_entry(&dst));
        if src_n == dst_n {
            return Err(format!("Cannot copy '{}' onto itself", name.to_string_lossy()));
        }
        let into_itself = match (remote::split(src), remote::split(&dst)) {
            // On a server only a folder can have something inside it.
            (Some((a, from)), Some((b, to))) => {
                a.eq_ignore_ascii_case(&b) && to.strip_prefix(from.as_str()).is_some_and(|rest| rest.starts_with('/'))
            }
            _ => !is_link(src) && src.is_dir() && dst_n.starts_with(&src_n),
        };
        if into_itself {
            return Err(format!(
                "Cannot copy folder '{}' into itself",
                name.to_string_lossy()
            ));
        }
        transfers.push(Transfer { src: src.clone(), dst });
    }
    Ok(transfers)
}

/// An archive (its root) or a folder inside one.
fn is_archive_dir(path: &Path) -> bool {
    vfs::in_archive(path) && vfs::open(path).and_then(|v| v.stat(path)).is_ok_and(|e| e.is_some_and(|e| e.is_dir()))
}

/// True if both paths refer to the same file (case-insensitive on Windows).
pub fn same_path(a: &Path, b: &Path) -> bool {
    normalize(a) == normalize(b)
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Like `normalize`, but a link names itself, not its target (links are
/// copied as links, and an existing one is replaced, not written through).
fn normalize_entry(path: &Path) -> PathBuf {
    match (is_link(path), path.parent(), path.file_name()) {
        (true, Some(parent), Some(name)) if cfg!(windows) => {
            normalize(parent).join(name.to_string_lossy().to_lowercase())
        }
        (true, Some(parent), Some(name)) => normalize(parent).join(name),
        _ => normalize(path),
    }
}

/// Canonicalises the deepest existing ancestor and re-appends the rest, so
/// paths that don't exist yet can still be compared.
fn normalize(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(canon) = fs::canonicalize(cur) {
            let mut out = canon;
            for c in rest.iter().rev() {
                out.push(c);
            }
            return out;
        }
        match (cur.parent(), cur.components().next_back()) {
            (Some(parent), Some(Component::Normal(name))) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

// ---- running --------------------------------------------------------------

/// Handle to a job running on its own thread.
pub struct JobHandle {
    pub control: Arc<Control>,
    answers: mpsc::Sender<Resolution>,
}

impl JobHandle {
    pub fn answer(&self, resolution: Resolution) {
        let _ = self.answers.send(resolution);
    }

    pub fn cancel(&self) {
        self.control.cancel();
        // Unblock a pending conflict question, if any.
        let _ = self.answers.send(Resolution::Cancel);
    }
}

impl std::fmt::Debug for JobHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle").finish_non_exhaustive()
    }
}

/// Runs `job` on a new thread. `on_event` receives throttled progress,
/// conflicts (answer them with [`JobHandle::answer`]) and finally
/// [`JobEvent::Finished`].
pub fn spawn(job: Job, on_event: impl FnMut(JobEvent) + Send + 'static) -> JobHandle {
    struct ChannelReporter<F> {
        on_event: F,
        answers: mpsc::Receiver<Resolution>,
    }
    impl<F: FnMut(JobEvent)> Reporter for ChannelReporter<F> {
        fn progress(&mut self, progress: &Progress) {
            (self.on_event)(JobEvent::Progress(progress.clone()));
        }
        fn conflict(&mut self, conflict: Conflict) -> Resolution {
            (self.on_event)(JobEvent::Conflict(conflict));
            self.answers.recv().unwrap_or(Resolution::Cancel)
        }
    }

    let control = Arc::new(Control::default());
    let (tx, rx) = mpsc::channel();
    let thread_control = control.clone();
    std::thread::spawn(move || {
        let mut reporter = ChannelReporter { on_event, answers: rx };
        let outcome = run(&job, &thread_control, &mut reporter);
        (reporter.on_event)(JobEvent::Finished(outcome));
    });
    JobHandle { control, answers: tx }
}

/// Runs a job to completion on the current thread.
pub fn run(job: &Job, control: &Control, reporter: &mut dyn Reporter) -> Outcome {
    let mut runner = Runner {
        control,
        reporter,
        progress: Progress::default(),
        outcome: Outcome::default(),
        sticky: None,
        buffer: vec![0; BUFFER_SIZE],
        last_report: Instant::now(),
        staged: Vec::new(),
        relayed: 0,
        password: None,
    };

    match job {
        Job::Copy(transfers) | Job::Move(transfers) => {
            let is_move = matches!(job, Job::Move(_));
            // A move on one server is a rename: not worth walking the tree.
            let renamed = is_move && same_server(transfers);
            // From one server to another (or to another folder of the same
            // one) every byte goes down to a temp file and up again: both
            // halves count (`Runner::relayed` takes the first out again).
            let relayed = |t: &Transfer| remote::is_remote(&t.src) && remote::is_remote(&t.dst);
            let sizes: Vec<(u64, u64)> = transfers
                .iter()
                .map(|t| match (renamed, relayed(t)) {
                    (true, _) => (0, 1),
                    (false, true) => {
                        let (bytes, items) = measure(&t.src);
                        (bytes * 2, items)
                    }
                    (false, false) => measure(&t.src),
                })
                .collect();
            for (bytes, items) in &sizes {
                runner.progress.total_bytes += bytes;
                runner.progress.total_items += items;
            }
            runner.report(true);
            let from_server = transfers.iter().find(|t| remote::is_remote(&t.src));
            if transfers.iter().any(|t| remote::is_remote(&t.dst)) {
                match renamed {
                    true => runner.move_on_server(transfers),
                    false => runner.upload(transfers, is_move),
                }
            } else if let (Some(t), Some(_)) = (from_server, dst_archive(transfers)) {
                runner.fail_msg(&t.src, "Can't copy from a server into an archive: copy to a local folder first");
            } else if let Some(archive) = dst_archive(transfers) {
                let inside = |t: &Transfer| {
                    vfs::inner_path(&t.src).is_some_and(|(file, inner)| !inner.is_empty() && same_path(&file, &archive))
                };
                let (within, other): (Vec<Transfer>, Vec<Transfer>) = transfers.iter().cloned().partition(inside);
                let (from_archives, local): (Vec<Transfer>, Vec<Transfer>) =
                    other.into_iter().partition(|t| vfs::is_virtual(&t.src));
                if !within.is_empty() {
                    runner.relocate(&archive, &within, !is_move);
                }
                if !from_archives.is_empty() && runner.alive() {
                    runner.copy_between_archives(&archive, &from_archives, is_move);
                }
                if !local.is_empty() && runner.alive() {
                    runner.pack(&archive, &local, is_move);
                }
            } else {
                // Moved out of archives: deleted there once extracted.
                let mut extracted = Vec::new();
                for (t, size) in transfers.iter().zip(sizes) {
                    if !runner.alive() {
                        break;
                    }
                    if let Some(parent) = t.dst.parent() {
                        if let Err(e) = fs::create_dir_all(parent) {
                            runner.fail(parent, e);
                            continue;
                        }
                    }
                    if vfs::is_virtual(&t.src) {
                        let before = (runner.outcome.errors.len(), runner.outcome.skipped);
                        runner.extract(t);
                        if is_move && before == (runner.outcome.errors.len(), runner.outcome.skipped) {
                            extracted.push(t.src.clone());
                        }
                    } else if is_move {
                        runner.move_item(t, size);
                    } else {
                        runner.copy(&t.src, &t.dst, false);
                    }
                }
                if !extracted.is_empty() && runner.alive() {
                    runner.delete_in_archives(&extracted);
                }
            }
        }
        Job::Delete { paths, .. } if paths.iter().any(|p| vfs::is_virtual(p)) => {
            runner.progress.total_items = paths.iter().map(|p| measure(p).1).sum();
            runner.report(true);
            if runner.delete_in_archives(paths) {
                runner.progress.done_items = runner.progress.total_items;
            }
        }
        Job::MakeDir(path) => {
            runner.progress.total_items = 1;
            runner.report(true);
            runner.make_dir(path);
        }
        Job::Rename { from, to } => {
            runner.progress.total_items = 1;
            runner.report(true);
            match vfs::split_archive(from) {
                None if remote::is_remote(from) => runner.move_on_server(&[Transfer { src: from.clone(), dst: to.clone() }]),
                Some((archive, _)) => {
                    runner.relocate(&archive, &[Transfer { src: from.clone(), dst: to.clone() }], false);
                }
                None => match fs::rename(from, to) {
                    Ok(()) => runner.item_done(0),
                    Err(e) => {
                        runner.fail(from, e);
                    }
                },
            }
        }
        Job::Pack { sources, archive, password } => match password.as_ref().and_then(|p| pack_password_problem(archive, p)) {
            Some(problem) => {
                runner.fail_msg(archive, problem);
            }
            None => {
                runner.password = password.clone();
                let mut transfers = Vec::new();
                for src in sources {
                    match src.file_name() {
                        Some(name) => transfers.push(Transfer { src: src.clone(), dst: archive.join(name) }),
                        None => {
                            runner.fail_msg(src, "Can't pack a drive root");
                        }
                    }
                }
                for (bytes, items) in transfers.iter().map(|t| measure(&t.src)) {
                    runner.progress.total_bytes += bytes;
                    runner.progress.total_items += items;
                }
                runner.report(true);
                runner.pack(archive, &transfers, false);
            }
        },
        Job::Unpack { archives, target } => {
            let all: Vec<_> = archives.iter().map(|a| vfs::open(a).and_then(|v| v.walk(a))).collect();
            for items in all.iter().flatten() {
                runner.progress.total_bytes += items.iter().map(|e| e.size).sum::<u64>();
                runner.progress.total_items += items.len() as u64;
            }
            runner.report(true);
            for archive in archives {
                if !runner.alive() {
                    break;
                }
                if let Err(e) = fs::create_dir_all(target) {
                    runner.fail(target, e);
                    break;
                }
                runner.extract(&Transfer { src: archive.clone(), dst: target.clone() });
            }
        }
        Job::Update { src, dst, expect } => {
            let (bytes, items) = measure(src);
            runner.progress.total_bytes = bytes;
            runner.progress.total_items = items;
            runner.report(true);
            runner.update_entry(src, dst, expect);
        }
        Job::Split { src, target, part_size } => runner.split(src, target, *part_size),
        Job::Combine { first, target } => runner.combine(first, target),
        Job::SetAttrib { paths, change, recursive } => {
            for path in paths {
                runner.progress.total_items += if *recursive { measure(path).1 } else { 1 };
            }
            runner.report(true);
            for path in paths {
                if !runner.alive() {
                    break;
                }
                runner.set_attrib(path, change, *recursive);
            }
        }
        Job::Delete { paths, to_trash: true } => {
            runner.progress.total_items = paths.len() as u64;
            runner.report(true);
            for path in paths {
                if !runner.alive() {
                    break;
                }
                runner.progress.current = Some(path.clone());
                match trash::delete(path) {
                    Ok(()) => runner.item_done(0),
                    Err(e) => {
                        runner.fail_msg(path, e.to_string());
                    }
                }
            }
        }
        Job::Delete { paths, to_trash: false } => {
            for path in paths {
                let (bytes, items) = measure(path);
                runner.progress.total_bytes += bytes;
                runner.progress.total_items += items;
            }
            runner.report(true);
            for path in paths {
                if !runner.alive() {
                    break;
                }
                runner.remove(path);
            }
        }
    }

    runner.progress.current = None;
    runner.report(true);
    runner.outcome.cancelled = control.is_cancelled();
    runner.outcome.done_items = runner.progress.done_items - runner.outcome.skipped.min(runner.progress.done_items);
    runner.outcome.done_bytes = runner.progress.done_bytes - runner.relayed.min(runner.progress.done_bytes);
    runner.outcome
}

struct Runner<'a> {
    control: &'a Control,
    reporter: &'a mut dyn Reporter,
    progress: Progress,
    outcome: Outcome,
    /// An "... all" answer that applies to every later conflict.
    sticky: Option<Resolution>,
    buffer: Vec<u8>,
    last_report: Instant,
    /// Temp copies of files from another archive, being packed into an archive:
    /// (temp path, the original), to show the original in errors and
    /// questions.
    staged: Vec<(PathBuf, PathBuf)>,
    /// Bytes of the download half of copies between servers (counted in
    /// the progress, not in the outcome).
    relayed: u64,
    /// [`Job::Pack`]'s password, taken by the one archive write it does.
    password: Option<Secret>,
}

impl Runner<'_> {
    fn alive(&self) -> bool {
        self.control.checkpoint()
    }

    fn report(&mut self, force: bool) {
        if force || self.last_report.elapsed() >= REPORT_INTERVAL {
            self.last_report = Instant::now();
            self.reporter.progress(&self.progress);
        }
    }

    fn item_done(&mut self, bytes: u64) {
        self.progress.done_items += 1;
        self.progress.done_bytes += bytes;
        self.report(false);
    }

    fn fail(&mut self, path: &Path, err: io::Error) -> bool {
        self.fail_msg(path, err.to_string())
    }

    fn fail_msg(&mut self, path: &Path, msg: impl Into<String>) -> bool {
        let path = self.origin(path);
        self.outcome.errors.push((path, msg.into()));
        false
    }

    /// `path`, or the file in another archive it's a temp copy of.
    fn origin(&self, path: &Path) -> PathBuf {
        for (temp, original) in &self.staged {
            if let Ok(rest) = path.strip_prefix(temp) {
                return match rest.as_os_str().is_empty() {
                    true => original.clone(),
                    false => original.join(rest),
                };
            }
        }
        path.to_path_buf()
    }

    /// Copies `src` to `dst` (recursively). With `is_move`, each source is
    /// removed once its copy succeeded. Returns true if everything under
    /// `src` was copied (so the source folder may be removed).
    fn copy(&mut self, src: &Path, dst: &Path, is_move: bool) -> bool {
        if !self.alive() {
            return false;
        }
        self.progress.current = Some(src.to_path_buf());

        let meta = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => return self.fail(src, e),
        };

        if meta.file_type().is_symlink() {
            match self.copy_link(src, &meta, dst) {
                LinkCopy::Done => {
                    self.item_done(link_bytes(src));
                    if is_move {
                        if let Err(e) = remove_link(src, &meta) {
                            return self.fail(src, e);
                        }
                    }
                    return true;
                }
                LinkCopy::Contents => {} // a file link without privilege: copy the file
                LinkCopy::Stop => return false,
            }
        }

        if meta.is_dir() {
            match fs::symlink_metadata(dst) {
                Ok(m) if !m.is_dir() => {
                    return self.fail_msg(dst, "A file with this name already exists")
                }
                Ok(_) => {}
                Err(_) => {
                    if let Err(e) = fs::create_dir(dst) {
                        return self.fail(dst, e);
                    }
                }
            }
            self.item_done(0);

            let children = match fs::read_dir(src) {
                Ok(rd) => rd,
                Err(e) => return self.fail(src, e),
            };
            let mut complete = true;
            for child in children {
                match child {
                    Ok(child) => {
                        complete &= self.copy(&child.path(), &dst.join(child.file_name()), is_move)
                    }
                    Err(e) => complete = self.fail(src, e),
                }
            }
            if complete && is_move {
                if let Err(e) = fs::remove_dir(src) {
                    return self.fail(src, e);
                }
            }
            return complete;
        }

        // Regular file (or a link to one: its contents are copied).
        let size = fs::metadata(src).map(|m| m.len()).unwrap_or(meta.len());
        if let Ok(dst_meta) = fs::symlink_metadata(dst) {
            if dst_meta.is_dir() {
                return self.fail_msg(dst, "A folder with this name already exists");
            }
            match self.resolve(src, (meta.len(), meta.modified().ok()), dst, stat_of(&dst_meta)) {
                // An existing link is replaced, not written through.
                Resolution::Overwrite if dst_meta.file_type().is_symlink() => {
                    if let Err(e) = remove_link(dst, &dst_meta) {
                        return self.fail(dst, e);
                    }
                }
                Resolution::Overwrite => {}
                Resolution::Cancel => {
                    self.control.cancel();
                    return false;
                }
                _ => {
                    self.outcome.skipped += 1;
                    self.item_done(size);
                    return false;
                }
            }
        }

        match self.copy_file(src, dst) {
            Ok(true) => {
                self.item_done(0);
                if is_move {
                    if let Err(e) = remove_file_force(src) {
                        return self.fail(src, e);
                    }
                }
                true
            }
            Ok(false) => false, // cancelled
            Err(e) => self.fail(src, e),
        }
    }

    /// Recreates the link `src` at `dst`, pointing where `src` points. An
    /// existing file or link at `dst` is asked about and replaced (the old
    /// link only, never its target); a real folder there is an error.
    fn copy_link(&mut self, src: &Path, meta: &fs::Metadata, dst: &Path) -> LinkCopy {
        let link = match Link::read(src, meta) {
            Ok(link) => link,
            Err(e) => {
                self.fail(src, e);
                return LinkCopy::Stop;
            }
        };
        if let Ok(dst_meta) = fs::symlink_metadata(dst) {
            if dst_meta.is_dir() {
                self.fail_msg(dst, "A folder with this name already exists");
                return LinkCopy::Stop;
            }
            match self.resolve(src, (0, meta.modified().ok()), dst, stat_of(&dst_meta)) {
                Resolution::Overwrite => {
                    let removed = match dst_meta.file_type().is_symlink() {
                        true => remove_link(dst, &dst_meta),
                        false => remove_file_force(dst),
                    };
                    if let Err(e) = removed {
                        self.fail(dst, e);
                        return LinkCopy::Stop;
                    }
                }
                Resolution::Cancel => {
                    self.control.cancel();
                    return LinkCopy::Stop;
                }
                _ => {
                    self.outcome.skipped += 1;
                    self.item_done(link_bytes(src));
                    return LinkCopy::Stop;
                }
            }
        }
        match link.create(src, dst) {
            Ok(()) => LinkCopy::Done,
            Err(e) if link.is_file() && is_privilege_error(&e) => LinkCopy::Contents,
            Err(e) => {
                self.fail_msg(dst, format!("Can't create the link: {e}"));
                LinkCopy::Stop
            }
        }
    }

    /// Asks (or applies a sticky answer). Returns Overwrite, Skip or Cancel.
    fn resolve(
        &mut self,
        src: &Path,
        src_stat: (u64, Option<SystemTime>),
        dst: &Path,
        dst_stat: (u64, Option<SystemTime>),
    ) -> Resolution {
        self.resolve_resumable(src, src_stat, dst, dst_stat, false)
    }

    /// [`Runner::resolve`] for a transfer that can resume (to or from a
    /// server): Resume is offered when `resumable` and the existing file is
    /// smaller. Returns Overwrite, Resume, Skip or Cancel.
    fn resolve_resumable(
        &mut self,
        src: &Path,
        (src_size, src_modified): (u64, Option<SystemTime>),
        dst: &Path,
        (dst_size, dst_modified): (u64, Option<SystemTime>),
        resumable: bool,
    ) -> Resolution {
        let target_older = matches!((src_modified, dst_modified), (Some(s), Some(d)) if d < s);
        let resumable = resumable && dst_size < src_size;

        let answer = match self.sticky {
            // Not resumable: asked after all.
            Some(Resolution::ResumeAll) if !resumable => None,
            other => other,
        };
        let answer = match answer {
            Some(sticky) => sticky,
            None => {
                let conflict = Conflict {
                    src: self.origin(src),
                    dst: dst.to_path_buf(),
                    src_size,
                    dst_size,
                    src_modified: src_modified.map(DateTime::from),
                    dst_modified: dst_modified.map(DateTime::from),
                    resumable,
                };
                self.reporter.conflict(conflict)
            }
        };

        match answer {
            Resolution::OverwriteAll | Resolution::SkipAll | Resolution::OverwriteOlder | Resolution::ResumeAll => {
                self.sticky = Some(answer)
            }
            _ => {}
        }
        match answer {
            Resolution::Overwrite | Resolution::OverwriteAll => Resolution::Overwrite,
            Resolution::OverwriteOlder if target_older => Resolution::Overwrite,
            Resolution::Resume | Resolution::ResumeAll if resumable => Resolution::Resume,
            Resolution::Cancel => Resolution::Cancel,
            _ => Resolution::Skip,
        }
    }

    /// Copies file contents with progress. Ok(false) means cancelled. Any
    /// partially written target is removed.
    fn copy_file(&mut self, src: &Path, dst: &Path) -> io::Result<bool> {
        let mut input = File::open(src)?;
        let src_meta = input.metadata()?;
        let mut output = match File::create(dst) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                clear_readonly(dst)?;
                File::create(dst)?
            }
            other => other?,
        };

        let result = self.pump(&mut input, &mut output);
        let result = result.and_then(|complete| {
            if complete {
                if let Ok(modified) = src_meta.modified() {
                    output.set_modified(modified)?;
                }
            }
            Ok(complete)
        });
        drop(output);

        match result {
            Ok(true) => {
                // Carries over the read-only flag.
                let _ = fs::set_permissions(dst, src_meta.permissions());
                Ok(true)
            }
            other => {
                let _ = fs::remove_file(dst);
                other
            }
        }
    }

    /// Copies up to `limit` bytes from `input` to `output` (if any), feeding
    /// `hasher`, with progress. Returns the bytes copied (less than `limit`
    /// at the end of the input), or `None` when cancelled.
    fn pump_hashed(
        &mut self,
        input: &mut dyn Read,
        mut output: Option<&mut dyn Write>,
        limit: u64,
        hasher: &mut crc32fast::Hasher,
    ) -> io::Result<Option<u64>> {
        let mut copied = 0u64;
        while copied < limit {
            if !self.alive() {
                return Ok(None);
            }
            let want = (limit - copied).min(self.buffer.len() as u64) as usize;
            let n = match input.read(&mut self.buffer[..want]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            hasher.update(&self.buffer[..n]);
            if let Some(out) = output.as_mut() {
                out.write_all(&self.buffer[..n])?;
            }
            copied += n as u64;
            self.progress.done_bytes += n as u64;
            self.report(false);
        }
        Ok(Some(copied))
    }

    /// Asks about an existing `dst` (a file written by Split / Combine).
    /// Returns None to write it, Some(true) to skip it, Some(false) when the
    /// job can't go on (a folder is in the way, or cancelled).
    fn split_conflict(&mut self, src: &Path, src_size: u64, dst: &Path) -> Option<bool> {
        let Ok(meta) = fs::symlink_metadata(dst) else { return None };
        if meta.is_dir() {
            self.fail_msg(dst, "A folder with this name already exists");
            return Some(false);
        }
        match self.resolve(src, (src_size, None), dst, stat_of(&meta)) {
            Resolution::Overwrite => None,
            Resolution::Cancel => {
                self.control.cancel();
                Some(false)
            }
            _ => Some(true),
        }
    }

    /// `Job::Split`: parts, then the crc file. What this run wrote is
    /// removed again if it fails or is cancelled.
    fn split(&mut self, src: &Path, target: &Path, part_size: u64) {
        use crate::split as sp;
        let Some(name) = src.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            self.fail_msg(src, "Invalid file");
            return;
        };
        let size = match fs::metadata(src) {
            Ok(m) if m.is_file() => m.len(),
            Ok(_) => {
                self.fail_msg(src, "Only files can be split");
                return;
            }
            Err(e) => {
                self.fail(src, e);
                return;
            }
        };
        let count = sp::part_count(size, part_size);
        self.progress.total_bytes = size;
        self.progress.total_items = count + 1;
        self.report(true);
        let mut input = match File::open(src) {
            Ok(f) => f,
            Err(e) => {
                self.fail(src, e);
                return;
            }
        };
        if let Err(e) = fs::create_dir_all(target) {
            self.fail(target, e);
            return;
        }
        let mut created: Vec<PathBuf> = Vec::new();
        let mut hasher = crc32fast::Hasher::new();
        let mut ok = true;
        let mut remaining = size;
        for i in 1..=count {
            if !self.alive() {
                ok = false;
                break;
            }
            let dst = target.join(sp::part_name(&name, i, 0));
            let len = remaining.min(part_size);
            self.progress.current = Some(dst.clone());
            let skip = match self.split_conflict(src, len, &dst) {
                None => false,
                Some(true) => true,
                Some(false) => {
                    ok = false;
                    break;
                }
            };
            // A skipped part is still read, for the crc of the whole file.
            let mut out = None;
            if !skip {
                match File::create(&dst) {
                    Ok(f) => {
                        created.push(dst.clone());
                        out = Some(f);
                    }
                    Err(e) => {
                        self.fail(&dst, e);
                        ok = false;
                        break;
                    }
                }
            }
            let copied = self.pump_hashed(&mut input, out.as_mut().map(|f| f as &mut dyn Write), len, &mut hasher);
            match copied {
                Ok(Some(n)) if n == len => {}
                Ok(Some(_)) => {
                    self.fail_msg(src, "The file got shorter while it was split");
                    ok = false;
                    break;
                }
                Ok(None) => {
                    ok = false;
                    break;
                }
                Err(e) => {
                    self.fail(&dst, e);
                    ok = false;
                    break;
                }
            }
            remaining -= len;
            if skip {
                self.outcome.skipped += 1;
            }
            self.item_done(0);
        }
        if ok {
            let crc = sp::CrcFile { filename: name.clone(), size, crc32: hasher.finalize() };
            let text = crc.format();
            let dst = target.join(sp::crc_name(&name));
            self.progress.current = Some(dst.clone());
            match self.split_conflict(src, text.len() as u64, &dst) {
                None => match fs::write(&dst, text) {
                    Ok(()) => {
                        created.push(dst);
                        self.item_done(0);
                    }
                    Err(e) => {
                        self.fail(&dst, e);
                        ok = false;
                    }
                },
                Some(true) => {
                    self.outcome.skipped += 1;
                    self.item_done(0);
                }
                Some(false) => ok = false,
            }
        }
        if !ok {
            drop(input);
            for path in created {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// `Job::Combine`: joins the parts into `target/<name>` and checks the
    /// crc file. A cancelled or failed join removes the output; one whose
    /// crc doesn't match is kept, with an error.
    fn combine(&mut self, first: &Path, target: &Path) {
        use crate::split as sp;
        let parts = match sp::find_parts(first) {
            Ok(p) => p,
            Err(e) => {
                self.fail_msg(first, e);
                return;
            }
        };
        let first_name = first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Some((base, _, _)) = sp::parse_part(&first_name) else { return };
        let mut total = 0u64;
        for part in &parts {
            match fs::metadata(part) {
                Ok(m) => total += m.len(),
                Err(e) => {
                    self.fail(part, e);
                    return;
                }
            }
        }
        self.progress.total_bytes = total;
        self.progress.total_items = parts.len() as u64;
        self.report(true);
        let out_path = target.join(base);
        if parts.iter().any(|p| same_path(p, &out_path)) {
            self.fail_msg(&out_path, "The joined file would replace a part");
            return;
        }
        if let Err(e) = fs::create_dir_all(target) {
            self.fail(target, e);
            return;
        }
        self.progress.current = Some(out_path.clone());
        match self.split_conflict(first, total, &out_path) {
            None => {}
            Some(true) => {
                self.outcome.skipped += parts.len() as u64;
                self.progress.done_items += parts.len() as u64;
                return;
            }
            Some(false) => return,
        }
        let mut out = match File::create(&out_path) {
            Ok(f) => f,
            Err(e) => {
                self.fail(&out_path, e);
                return;
            }
        };
        let mut hasher = crc32fast::Hasher::new();
        let mut written = 0u64;
        let mut ok = true;
        for part in &parts {
            self.progress.current = Some(part.clone());
            let result = File::open(part)
                .and_then(|mut input| self.pump_hashed(&mut input, Some(&mut out as &mut dyn Write), u64::MAX, &mut hasher));
            match result {
                Ok(Some(n)) => written += n,
                Ok(None) => {
                    ok = false;
                    break;
                }
                Err(e) => {
                    self.fail(part, e);
                    ok = false;
                    break;
                }
            }
            self.item_done(0);
        }
        if let (true, Err(e)) = (ok, out.flush()) {
            self.fail(&out_path, e);
            ok = false;
        }
        drop(out);
        if !ok {
            let _ = fs::remove_file(&out_path);
            return;
        }
        let crc_path = first.with_file_name(sp::crc_name(base));
        if !crc_path.is_file() {
            return;
        }
        let crc = fs::read(&crc_path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| sp::CrcFile::parse(&String::from_utf8_lossy(&bytes)));
        match crc {
            Err(e) => {
                self.fail_msg(&crc_path, format!("Can't check the result: {e}"));
            }
            Ok(c) if c.size != written => {
                let msg = format!("Joined file is damaged: {written} bytes instead of {}", c.size);
                self.fail_msg(&out_path, msg);
            }
            Ok(c) if c.crc32 != hasher.finalize() => {
                self.fail_msg(&out_path, "Joined file is damaged: the crc32 doesn't match the .crc file");
            }
            Ok(_) => {}
        }
    }

    fn pump(&mut self, input: &mut dyn Read, output: &mut dyn Write) -> io::Result<bool> {
        loop {
            if !self.alive() {
                return Ok(false);
            }
            let n = match input.read(&mut self.buffer) {
                Ok(0) => return Ok(true),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            output.write_all(&self.buffer[..n])?;
            self.progress.done_bytes += n as u64;
            self.report(false);
        }
    }

    /// Copies a file or folder out of an archive (`t.src` is a path inside
    /// one) to the local `t.dst`. Folders are created and conflicts asked
    /// first, then the files are read in the archive's order.
    fn extract(&mut self, t: &Transfer) {
        self.progress.current = Some(t.src.clone());
        let items = match crate::vfs::open(&t.src).and_then(|vfs| Ok((vfs.walk(&t.src)?, vfs))) {
            Ok(found) => found,
            Err(e) => {
                self.fail(&t.src, e);
                return;
            }
        };
        let (items, vfs) = items;
        // From a server a download can be resumed (and a partial one kept).
        let from_server = remote::is_remote(&t.src);
        let mut files = Vec::new();
        let mut offsets = Vec::new();
        let mut targets = Vec::new();
        // Folders made here, to get their dates once their files are in.
        let mut made = Vec::new();
        for item in items {
            if !self.alive() {
                return;
            }
            let dst = match item.path.strip_prefix(&t.src) {
                Ok(rel) if !rel.as_os_str().is_empty() => t.dst.join(rel),
                _ => t.dst.clone(),
            };
            self.progress.current = Some(item.path.clone());
            let existing = fs::symlink_metadata(&dst);
            if item.is_dir() {
                match existing {
                    Ok(m) if !m.is_dir() => {
                        self.fail_msg(&dst, "A file with this name already exists");
                    }
                    Ok(_) => self.item_done(0),
                    Err(_) => match fs::create_dir_all(&dst) {
                        Ok(()) => {
                            if let Some(m) = item.modified {
                                made.push((dst, SystemTime::from(m)));
                            }
                            self.item_done(0)
                        }
                        Err(e) => {
                            self.fail(&dst, e);
                        }
                    },
                }
                continue;
            }
            let mut offset = 0;
            if let Ok(dst_meta) = existing {
                if dst_meta.is_dir() {
                    self.fail_msg(&dst, "A folder with this name already exists");
                    continue;
                }
                let src_stat = (item.size, item.modified.map(Into::into));
                let resumable = from_server && dst_meta.is_file();
                match self.resolve_resumable(&item.path, src_stat, &dst, stat_of(&dst_meta), resumable) {
                    Resolution::Overwrite => {}
                    Resolution::Resume => offset = dst_meta.len(),
                    Resolution::Cancel => {
                        self.control.cancel();
                        return;
                    }
                    _ => {
                        self.outcome.skipped += 1;
                        self.item_done(item.size);
                        continue;
                    }
                }
            }
            files.push(item.path.clone());
            offsets.push(offset);
            targets.push((dst, item));
        }
        let mut failures = Vec::new();
        let result = vfs.read_files_from(&files, &offsets, &mut |i, input| {
            let (dst, item) = &targets[i];
            self.progress.current = Some(item.path.clone());
            let resume = from_server.then_some(offsets[i]);
            let written = self.write_out(input, dst, item.modified.map(Into::into), resume);
            match written.and_then(|done| match done && offsets[i] > 0 {
                true => check_resumed(fs::metadata(dst)?.len(), item.size).map(|()| true),
                false => Ok(done),
            }) {
                Ok(true) => {
                    self.item_done(0);
                    Ok(self.alive())
                }
                Ok(false) => Ok(false),
                Err(e) => {
                    failures.push((item.path.clone(), e));
                    Ok(true)
                }
            }
        });
        for (path, e) in failures {
            self.fail(&path, e);
        }
        if let Err(e) = result {
            self.fail(&t.src, e);
        }
        // Deepest first: setting a folder's date doesn't touch its parent's.
        for (dir, modified) in made.iter().rev() {
            let _ = set_dir_modified(dir, *modified);
        }
    }

    /// Writes `input` to the new file `dst` with progress. Ok(false) means
    /// cancelled; a partial file is removed. A download from a server is
    /// resumable (`resume` `Some(offset)`): from an offset above 0 `input`
    /// is the rest of the file, written after the first `offset` bytes of
    /// `dst`, and a partial file is kept as [`remote::keeps_partial`] says
    /// (the error then says so).
    fn write_out(
        &mut self,
        input: &mut dyn Read,
        dst: &Path,
        modified: Option<SystemTime>,
        resume: Option<u64>,
    ) -> io::Result<bool> {
        let offset = resume.unwrap_or(0);
        let open = || match offset {
            0 => File::create(dst),
            _ => {
                let mut f = File::options().write(true).open(dst)?;
                // Anything past the offset (if the file grew meanwhile) goes.
                f.set_len(offset)?;
                io::Seek::seek(&mut f, io::SeekFrom::Start(offset))?;
                Ok(f)
            }
        };
        let mut output = match open() {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                clear_readonly(dst)?;
                open()?
            }
            other => other?,
        };
        self.progress.done_bytes += offset;
        let before = self.progress.done_bytes;
        let result = self.pump(input, &mut output).and_then(|complete| {
            if let Some(m) = modified.filter(|_| complete) {
                output.set_modified(m)?;
            }
            Ok(complete)
        });
        drop(output);
        let written = self.progress.done_bytes - before;
        match &result {
            Ok(true) => {}
            Ok(false) if resume.is_some_and(|o| remote::keeps_partial(o, written, true)) => {}
            Err(_) if resume.is_some_and(|o| remote::keeps_partial(o, written, false)) => {
                return result.map_err(|e| kept_partial(e, offset + written));
            }
            _ => {
                let _ = fs::remove_file(dst);
            }
        }
        result
    }

    /// Tries a cheap rename first; falls back to copy + delete (e.g. across
    /// drives, or when merging into an existing folder).
    fn move_item(&mut self, t: &Transfer, (bytes, items): (u64, u64)) {
        self.progress.current = Some(t.src.clone());
        if fs::symlink_metadata(&t.dst).is_err() && fs::rename(&t.src, &t.dst).is_ok() {
            self.progress.done_items += items;
            self.progress.done_bytes += bytes;
            self.report(false);
            return;
        }
        self.copy(&t.src, &t.dst, true);
    }

    // ---- archives ---------------------------------------------------------

    /// Opens `archive` for changing: an error is recorded (and `None`
    /// returned) if it can't be read or can't be changed.
    fn open_writable(&mut self, archive: &Path) -> Option<std::sync::Arc<dyn Vfs>> {
        match vfs::open(archive) {
            Ok(v) if v.writable() => Some(v),
            Ok(_) => {
                self.fail_msg(archive, "Only zip, 7z, tar and gz archives can be changed (rar is read-only)");
                None
            }
            Err(e) => {
                self.fail(archive, e);
                None
            }
        }
    }

    /// Copies or moves local files and folders into the archive `archive` (the
    /// transfers' destinations are inside it; it's created if missing).
    /// Conflicts are asked first, then the archive is rewritten once.
    /// Returns the transfers that were packed completely.
    fn pack(&mut self, archive: &Path, transfers: &[Transfer], is_move: bool) -> Vec<usize> {
        // Not 7z or rar, nor archives inside those.
        if vfs::archive_writable(archive) == Some(false) {
            self.fail_msg(archive, READONLY);
            return Vec::new();
        }
        let index = match archive_exists(archive) {
            true => match self.open_writable(archive) {
                Some(v) => Some(v),
                None => return Vec::new(),
            },
            false => None,
        };
        let saved = self.snapshot();
        let mut edits = Edits::default();
        let mut adds = Vec::new();
        let mut complete = vec![true; transfers.len()];
        // A plain .gz takes only a file named like its one file (replacing it).
        let gz = vfs::gz_member(archive);
        for (ti, t) in transfers.iter().enumerate() {
            let Some(base) = inner_of(archive, &t.dst).filter(|b| !b.is_empty()) else {
                self.fail_msg(&t.dst, "Not a path inside the archive");
                complete[ti] = false;
                continue;
            };
            if let Some(member) = &gz {
                let what = if t.src.is_dir() { "folders" } else { "other files" };
                if t.src.is_dir() || crate::zip_edit::key(&base) != crate::zip_edit::key(member) {
                    let msg = format!("A .gz holds just one file, named like the archive ({member}): it can't take {what}");
                    complete[ti] = self.fail_msg(&t.src, msg);
                    continue;
                }
            }
            let items = match LocalFs.walk(&t.src) {
                Ok(items) => items,
                Err(e) => {
                    complete[ti] = self.fail(&t.src, e);
                    continue;
                }
            };
            for item in items {
                if !self.alive() {
                    self.restore(saved);
                    return Vec::new();
                }
                // Never the archive itself (packing its own folder).
                if same_path(&item.path, archive) {
                    continue;
                }
                let rel = inner_of(&t.src, &item.path).unwrap_or_default();
                let (dst, inner) = match rel.is_empty() {
                    true => (t.dst.clone(), base.clone()),
                    false => (t.dst.join(&rel), format!("{base}/{rel}")),
                };
                self.progress.current = Some(item.path.clone());
                let existing = index.as_ref().and_then(|v| v.stat(&dst).ok().flatten());
                let modified = item.modified.map(SystemTime::from);
                if item.is_dir() {
                    match existing {
                        Some(e) if !e.is_dir() => {
                            complete[ti] = self.fail_msg(&dst, "A file with this name already exists");
                        }
                        Some(_) => self.item_done(0),
                        None => adds.push(Add::Dir(inner, modified)),
                    }
                    continue;
                }
                if let Some(e) = existing {
                    if e.is_dir() {
                        complete[ti] = self.fail_msg(&dst, "A folder with this name already exists");
                        continue;
                    }
                    match self.resolve(&item.path, (item.size, modified), &dst, (e.size, e.modified.map(Into::into))) {
                        Resolution::Overwrite => edits.remove.push(inner.clone()),
                        Resolution::Cancel => {
                            self.control.cancel();
                            self.restore(saved);
                            return Vec::new();
                        }
                        _ => {
                            self.outcome.skipped += 1;
                            self.item_done(item.size);
                            complete[ti] = false;
                            continue;
                        }
                    }
                }
                adds.push(Add::File { src: item.path, inner, modified, size: item.size, transfer: ti });
            }
        }
        let Some(failed) = self.rewrite(archive, &edits, &adds, &[], saved) else { return Vec::new() };
        let done: Vec<usize> = (0..transfers.len()).filter(|ti| complete[*ti] && !failed.contains(ti)).collect();
        if is_move {
            for &ti in &done {
                self.remove_source(&transfers[ti].src);
            }
        }
        done
    }

    /// [`Job::Update`]: replaces the file `dst` in its archive with `src`, if
    /// it's still as `expect` says.
    fn update_entry(&mut self, src: &Path, dst: &Path, expect: &Stamp) {
        let Some((archive, _)) = vfs::split_archive(dst) else {
            self.fail_msg(dst, "Not a path inside an archive");
            return;
        };
        if vfs::archive_writable(&archive) == Some(false) {
            self.fail_msg(&archive, READONLY);
            return;
        }
        let Some(index) = self.open_writable(&archive) else { return };
        let now = match index.stat(dst) {
            Ok(entry) => stamp_of(entry),
            Err(e) => {
                self.fail(dst, e);
                return;
            }
        };
        if now != *expect {
            let msg = match now {
                _ if expect.is_none() => "A file with this name was added to the archive meanwhile: not overwritten",
                None => "It was removed from the archive since it was opened for editing: not put back",
                Some(_) => "It changed in the archive since it was opened for editing: not overwritten",
            };
            self.fail_msg(dst, msg);
            return;
        }
        self.sticky = Some(Resolution::OverwriteAll);
        self.pack(&archive, &[Transfer { src: src.to_path_buf(), dst: dst.to_path_buf() }], false);
    }

    /// Copies or moves files and folders from other archives (any kind that
    /// can be read, also ones inside archives) into the archive `archive`:
    /// they're extracted to a temp folder, packed like local files, and the
    /// temp folder is removed. A move then deletes the sources, so it needs
    /// them in an archive that can be changed too.
    fn copy_between_archives(&mut self, archive: &Path, transfers: &[Transfer], is_move: bool) {
        if vfs::archive_writable(archive) == Some(false) {
            self.fail_msg(archive, READONLY);
            return;
        }
        if archive_exists(archive) && self.open_writable(archive).is_none() {
            return;
        }
        let stage = vfs::temp_root().join(format!("copy-{}-{}", std::process::id(), unique_id()));
        // Left by a crashed run whose process id was the same.
        let _ = fs::remove_dir_all(&stage);
        let mut staged = Vec::new();
        // Per staged transfer: its source, and whether all of it was extracted.
        let mut sources = Vec::new();
        let before = (self.progress.done_bytes, self.progress.done_items);
        for (i, t) in transfers.iter().enumerate() {
            if !self.alive() {
                break;
            }
            if is_move && vfs::archive_writable(&t.src) == Some(false) {
                self.fail_msg(&t.src, "Can't move out of a read-only archive (only zip, 7z, tar and gz archives can be changed): copy with F5 instead");
                continue;
            }
            let Some(name) = t.src.file_name() else {
                self.fail_msg(&t.src, "Not a path inside an archive");
                continue;
            };
            let folder = stage.join(i.to_string());
            if let Err(e) = fs::create_dir_all(&folder) {
                self.fail(&folder, e);
                continue;
            }
            let temp = folder.join(name);
            let problems = (self.outcome.errors.len(), self.outcome.skipped);
            self.extract(&Transfer { src: t.src.clone(), dst: temp.clone() });
            if fs::symlink_metadata(&temp).is_ok() {
                sources.push((t.src.clone(), problems == (self.outcome.errors.len(), self.outcome.skipped)));
                self.staged.push((temp.clone(), t.src.clone()));
                staged.push(Transfer { src: temp, dst: t.dst.clone() });
            }
        }
        // Packing counts it all once more (taken off again at the end, so
        // the outcome counts each item once).
        let extracted = (self.progress.done_bytes - before.0, self.progress.done_items - before.1);
        self.progress.total_bytes += extracted.0;
        self.progress.total_items += extracted.1;
        if !staged.is_empty() && self.alive() {
            let done = self.pack(archive, &staged, false);
            let moved: Vec<PathBuf> =
                done.into_iter().filter(|&i| is_move && sources[i].1).map(|i| sources[i].0.clone()).collect();
            if !moved.is_empty() && self.alive() {
                let saved = self.snapshot();
                self.delete_in_archives(&moved);
                self.restore(saved);
            }
        }
        self.staged.clear();
        let _ = fs::remove_dir_all(&stage);
        let p = &mut self.progress;
        (p.total_bytes, p.total_items) = (p.total_bytes - extracted.0, p.total_items - extracted.1);
        (p.done_bytes, p.done_items) = (p.done_bytes.saturating_sub(extracted.0), p.done_items.saturating_sub(extracted.1));
        self.report(true);
    }

    /// Moves (or with `keep`, copies) items within the archive `archive`: the
    /// transfers' sources and destinations are both inside it. An existing
    /// destination is an error (no merging).
    fn relocate(&mut self, archive: &Path, transfers: &[Transfer], keep: bool) {
        let Some(index) = self.open_writable(archive) else { return };
        // A plain .gz lists its one file under the archive's name.
        if let Some(member) = vfs::gz_member(archive) {
            let msg = match keep {
                true => format!("A .gz holds just one file ({member}): it can't be copied inside it"),
                false => format!("The file in a .gz is named like the archive ({member}): rename the .gz itself instead"),
            };
            for t in transfers {
                self.fail_msg(&t.src, msg.clone());
            }
            return;
        }
        let saved = self.snapshot();
        let mut edits = Edits::default();
        let mut parents = Vec::new();
        let mut items = 0;
        for t in transfers {
            let (Some((src_file, from)), Some(to)) = (vfs::inner_path(&t.src), inner_of(archive, &t.dst)) else {
                self.fail_msg(&t.src, "Not a path inside the archive");
                continue;
            };
            if !same_path(&src_file, archive) {
                self.fail_msg(&t.src, "Can't copy or move between archives: copy out first");
                continue;
            }
            let renamed_case = crate::zip_edit::key(&from) == crate::zip_edit::key(&to);
            match index.stat(&t.dst) {
                Ok(Some(_)) if !renamed_case => {
                    self.fail_msg(&t.dst, "Already exists in the archive");
                    continue;
                }
                Err(e) => {
                    self.fail(&t.dst, e);
                    continue;
                }
                _ => {}
            }
            items += index.walk(&t.src).map_or(1, |w| w.len() as u64);
            parents.push(inner_parent(&from).to_owned());
            edits.relocate.push((from, to, keep));
        }
        if edits.relocate.is_empty() {
            return;
        }
        let keep_dirs = if keep { Vec::new() } else { parents };
        if self.rewrite(archive, &edits, &[], &keep_dirs, saved).is_some() {
            self.progress.done_items += items;
            self.report(false);
        }
    }

    /// Deletes paths inside archives (one rewrite per archive). True if
    /// all of them were deleted.
    fn delete_in_archives(&mut self, paths: &[PathBuf]) -> bool {
        let mut groups: Vec<(PathBuf, Vec<String>)> = Vec::new();
        let mut ok = true;
        for path in paths.iter().filter(|p| remote::is_remote(p)) {
            ok &= self.delete_on_server(path);
        }
        for path in paths.iter().filter(|p| !remote::is_remote(p)) {
            match vfs::inner_path(path).filter(|(_, inner)| !inner.is_empty()) {
                Some((file, inner)) => match groups.iter_mut().find(|(f, _)| *f == file) {
                    Some((_, inners)) => inners.push(inner),
                    None => groups.push((file, vec![inner])),
                },
                None => ok = self.fail_msg(path, "Not a path inside an archive"),
            }
        }
        for (archive, inners) in groups {
            if !self.alive() {
                return false;
            }
            if self.open_writable(&archive).is_none() {
                ok = false;
                continue;
            }
            if let Some(member) = vfs::gz_member(&archive) {
                ok &= self.delete_gz(&archive, &member, &inners);
                continue;
            }
            let mut keep_dirs: Vec<String> = inners.iter().map(|i| inner_parent(i).to_owned()).collect();
            keep_dirs.dedup();
            let edits = Edits { remove: inners, ..Edits::default() };
            let saved = self.snapshot();
            ok &= self.rewrite(&archive, &edits, &[], &keep_dirs, saved).is_some();
        }
        ok
    }

    /// Deletes the one file of the plain .gz `archive`: that deletes the .gz
    /// itself (it can't be empty), from the archive holding it if nested.
    fn delete_gz(&mut self, archive: &Path, member: &str, inners: &[String]) -> bool {
        let key = crate::zip_edit::key;
        if let Some(other) = inners.iter().find(|i| key(i) != key(member)) {
            return self.fail_msg(&archive.join(other), "Not found in the archive");
        }
        self.progress.current = Some(archive.to_path_buf());
        let done = match archive.is_file() {
            true => match fs::remove_file(archive) {
                Ok(()) => true,
                Err(e) => self.fail(archive, e),
            },
            false => self.delete_nested(archive),
        };
        if done {
            vfs::forget(archive);
            vfs::unmount(archive);
        }
        done
    }

    /// Deletes the nested archive `archive` (a logical path) from the archive
    /// holding it.
    fn delete_nested(&mut self, archive: &Path) -> bool {
        let found = archive.parent().and_then(vfs::split_archive).map(|(outer, _)| outer);
        let Some((outer, inner)) = found.and_then(|o| inner_of(&o, archive).filter(|i| !i.is_empty()).map(|i| (o, i))) else {
            return self.fail_msg(archive, "Not an archive inside another archive");
        };
        if self.open_writable(&outer).is_none() {
            return false;
        }
        if let Some(member) = vfs::gz_member(&outer) {
            return self.delete_gz(&outer, &member, &[inner]);
        }
        let saved = self.snapshot();
        let keep_dirs = [inner_parent(&inner).to_owned()];
        let edits = Edits { remove: vec![inner], ..Edits::default() };
        self.rewrite(&outer, &edits, &[], &keep_dirs, saved).is_some()
    }

    /// Creates a folder: inside an archive by rewriting it, else on disk.
    fn make_dir(&mut self, path: &Path) {
        if remote::is_remote(path) {
            match vfs::open(path).and_then(|v| make_dirs_on_server(v.as_ref(), path)) {
                Ok(()) => self.item_done(0),
                Err(e) => {
                    self.fail(path, e);
                }
            }
            return;
        }
        let Some((archive, inner)) = vfs::inner_path(path).filter(|(_, inner)| !inner.is_empty()) else {
            match fs::create_dir_all(path) {
                Ok(()) => self.item_done(0),
                Err(e) => {
                    self.fail(path, e);
                }
            }
            return;
        };
        let Some(index) = self.open_writable(&archive) else { return };
        if let Ok(Some(e)) = index.stat(path) {
            let what = if e.is_dir() { "folder" } else { "file" };
            self.fail_msg(path, format!("A {what} with this name already exists"));
            return;
        }
        let saved = self.snapshot();
        self.rewrite(&archive, &Edits::default(), &[Add::Dir(inner, Some(SystemTime::now()))], &[], saved);
    }

    // ---- servers ----------------------------------------------------------

    /// Uploads files and folders to a server (the transfers' destinations
    /// are on it). Conflicts are asked file by file; a moved source is
    /// removed once everything in it was uploaded.
    ///
    /// A source on a server (another one, or another folder of the same
    /// one) is copied file by file through a temp file in a `copy-relay-*`
    /// folder: downloaded, uploaded, deleted. The folder goes when the job
    /// ends, however it ends.
    fn upload(&mut self, transfers: &[Transfer], is_move: bool) {
        let Some(first) = transfers.first() else { return };
        let server = match vfs::open(&first.dst) {
            Ok(v) => v,
            Err(e) => {
                self.fail(&first.dst, e);
                return;
            }
        };
        let relay = match transfers.iter().find(|t| remote::is_remote(&t.src)) {
            Some(t) => match Stage::relay() {
                Ok(stage) => Some(stage),
                Err(e) => {
                    self.fail_msg(&t.src, format!("Can't create a temp folder: {e}"));
                    return;
                }
            },
            None => None,
        };
        // Listings of the target folders, to find conflicts without asking
        // the server about every file.
        let mut listed: HashMap<String, Vec<Entry>> = HashMap::new();
        for t in transfers {
            if !self.alive() {
                return;
            }
            self.progress.current = Some(t.src.clone());
            let from_server = remote::is_remote(&t.src);
            let source: io::Result<Arc<dyn Vfs>> = match from_server {
                true => vfs::open(&t.src),
                false => Ok(Arc::new(LocalFs)),
            };
            let (items, source) = match source.and_then(|v| Ok((v.walk(&t.src)?, v))) {
                Ok(found) => found,
                Err(e) => {
                    self.fail(&t.src, e);
                    continue;
                }
            };
            // Each byte of a relayed file is counted twice (down and up).
            let weight = if from_server { 2 } else { 1 };
            let mut complete = true;
            for item in items {
                if !self.alive() {
                    return;
                }
                let dst = match item.path.strip_prefix(&t.src) {
                    Ok(rel) if !rel.as_os_str().is_empty() => t.dst.join(rel),
                    _ => t.dst.clone(),
                };
                self.progress.current = Some(item.path.clone());
                let existing = match existing_on_server(server.as_ref(), &mut listed, &dst) {
                    Ok(e) => e,
                    Err(e) => {
                        complete = self.fail(&dst, e);
                        continue;
                    }
                };
                if item.is_dir() {
                    match existing {
                        Some(e) if !e.is_dir() => complete = self.fail_msg(&dst, "A file with this name already exists"),
                        Some(_) => self.item_done(0),
                        None => match server.make_dir(&dst) {
                            Ok(()) => {
                                listed.insert(remote::display(&dst), Vec::new());
                                self.item_done(0);
                            }
                            Err(e) => complete = self.fail(&dst, e),
                        },
                    }
                    continue;
                }
                // An upload from here can be resumed (and a partial one kept);
                // a copy between servers starts over.
                let mut resume = (!from_server).then_some(0);
                if let Some(e) = existing {
                    if e.is_dir() {
                        complete = self.fail_msg(&dst, "A folder with this name already exists");
                        continue;
                    }
                    let src_stat = (item.size, item.modified.map(Into::into));
                    let dst_stat = (e.size, e.modified.map(Into::into));
                    match self.resolve_resumable(&item.path, src_stat, &dst, dst_stat, !from_server) {
                        Resolution::Overwrite => {}
                        Resolution::Resume => resume = Some(e.size),
                        Resolution::Cancel => {
                            self.control.cancel();
                            return;
                        }
                        _ => {
                            self.outcome.skipped += 1;
                            self.item_done(item.size * weight);
                            if from_server {
                                self.relayed += item.size;
                            }
                            complete = false;
                            continue;
                        }
                    }
                }
                let offset = resume.unwrap_or(0);
                let opened = match (&relay, from_server) {
                    (Some(relay), true) => self.download(source.as_ref(), &item.path, &relay.file),
                    _ => File::open(&item.path).and_then(|mut f| {
                        io::Seek::seek(&mut f, io::SeekFrom::Start(offset))?;
                        Ok(Some(f))
                    }),
                };
                let mut input = match opened {
                    Ok(Some(f)) => f,
                    Ok(None) => return, // cancelled
                    Err(e) => {
                        complete = self.fail(&item.path, e);
                        continue;
                    }
                };
                let modified = item.modified.map(Into::into);
                self.progress.done_bytes += offset;
                let before = self.progress.done_bytes;
                let result = server.write_file(&dst, resume, modified, &mut |out| self.pump(&mut input, out));
                drop(input);
                if let (Some(relay), true) = (&relay, from_server) {
                    let _ = fs::remove_file(&relay.file);
                }
                let written = self.progress.done_bytes - before;
                let result = match result {
                    // What arrived must be as long as the source.
                    Ok(true) if offset > 0 => server
                        .stat(&dst)
                        .and_then(|e| check_resumed(e.map_or(0, |e| e.size), item.size))
                        .map(|()| true),
                    Err(e) if resume.is_some_and(|o| remote::keeps_partial(o, written, false)) => {
                        Err(kept_partial(e, offset + written))
                    }
                    other => other,
                };
                match result {
                    Ok(true) => self.item_done(0),
                    Ok(false) => return,
                    Err(e) => complete = self.fail(&dst, e),
                }
            }
            if is_move && complete && self.alive() {
                match from_server {
                    true => {
                        let saved = self.snapshot();
                        self.delete_on_server(&t.src);
                        self.restore(saved);
                    }
                    false => self.remove_source(&t.src),
                }
            }
        }
    }

    /// Downloads the server file `src` to the local `temp` (replaced) and
    /// opens it for reading. `None`: cancelled (nothing left at `temp`).
    fn download(&mut self, source: &dyn Vfs, src: &Path, temp: &Path) -> io::Result<Option<File>> {
        self.progress.current = Some(src.to_path_buf());
        let before = self.progress.done_bytes;
        let mut written = None;
        let result = source.read_files(&[src.to_path_buf()], &mut |_, input| {
            let r = self.write_out(input, temp, None, None);
            let go_on = matches!(r, Ok(true));
            written = Some(r);
            Ok(go_on)
        });
        self.relayed += self.progress.done_bytes - before;
        let outcome = match (result, written) {
            (Err(e), _) | (Ok(()), Some(Err(e))) => Err(e),
            (Ok(()), Some(Ok(true))) => match File::open(temp) {
                Ok(f) => return Ok(Some(f)),
                Err(e) => Err(e),
            },
            (Ok(()), Some(Ok(false))) => Ok(None),
            (Ok(()), None) => Err(io::Error::other("The server sent no data")),
        };
        let _ = fs::remove_file(temp);
        outcome
    }

    /// Moves or renames on one server (the transfers' sources and
    /// destinations are on the same one).
    fn move_on_server(&mut self, transfers: &[Transfer]) {
        let mut moved = 0;
        for t in transfers {
            if !self.alive() {
                return;
            }
            self.progress.current = Some(t.src.clone());
            let server = match vfs::open(&t.src) {
                Ok(v) => v,
                Err(e) => {
                    self.fail(&t.src, e);
                    continue;
                }
            };
            let result = match server.stat(&t.dst) {
                Ok(Some(e)) => {
                    let what = if e.is_dir() { "folder" } else { "file" };
                    Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("A {what} with this name already exists")))
                }
                Ok(None) => server.rename(&t.src, &t.dst),
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => moved += 1,
                Err(e) => {
                    self.fail(&t.dst, e);
                }
            }
        }
        if moved == transfers.len() {
            self.progress.done_items = self.progress.total_items;
            self.progress.done_bytes = self.progress.total_bytes;
        }
    }

    /// Deletes a file or folder (with everything in it) on a server.
    fn delete_on_server(&mut self, path: &Path) -> bool {
        self.progress.current = Some(path.to_path_buf());
        let found = vfs::open(path).and_then(|v| Ok((v.walk(path)?, v)));
        let (items, server) = match found {
            Ok(found) => found,
            Err(e) => return self.fail(path, e),
        };
        // A folder comes before its contents: delete from the end.
        let mut ok = true;
        let mut failed: Vec<PathBuf> = Vec::new();
        for item in items.iter().rev() {
            if !self.alive() {
                return false;
            }
            // A folder whose contents couldn't all be deleted stays.
            if item.is_dir() && failed.iter().any(|f| f.starts_with(&item.path)) {
                continue;
            }
            self.progress.current = Some(item.path.clone());
            match server.remove(&item.path, item.is_dir()) {
                Ok(()) => self.item_done(item.size),
                Err(e) => {
                    failed.push(item.path.clone());
                    ok = self.fail(&item.path, e);
                }
            }
        }
        ok
    }

    /// Progress and skip counts, to put back when an archive rewrite doesn't
    /// happen (nothing was done after all).
    fn snapshot(&self) -> (u64, u64, u64) {
        (self.progress.done_items, self.progress.done_bytes, self.outcome.skipped)
    }

    fn restore(&mut self, (items, bytes, skipped): (u64, u64, u64)) {
        self.progress.done_items = items;
        self.progress.done_bytes = bytes;
        self.outcome.skipped = skipped;
        self.report(true);
    }

    /// Rewrites `archive` with `edits`, then `adds`, then folder entries for
    /// `keep_dirs` (the folders of removed items, so a folder that only
    /// existed implicitly doesn't vanish when emptied). Returns the
    /// transfers with a file that couldn't be added, or `None` if the archive
    /// wasn't changed (cancelled or failed; recorded, progress reset to
    /// `saved`).
    fn rewrite(&mut self, archive: &Path, edits: &Edits, adds: &[Add], keep_dirs: &[String], saved: (u64, u64, u64))
        -> Option<HashSet<usize>> {
        let mut failed = HashSet::new();
        if edits.remove.is_empty() && edits.relocate.is_empty() && adds.is_empty() {
            return Some(failed);
        }
        let result = self.write_archive(archive, edits, adds, keep_dirs, &mut failed);
        match result {
            Ok(true) => Some(failed),
            Ok(false) => {
                self.restore(saved);
                None
            }
            Err(e) => {
                self.fail(archive, e);
                self.restore(saved);
                None
            }
        }
    }

    fn write_archive(
        &mut self,
        archive: &Path,
        edits: &Edits,
        adds: &[Add],
        keep_dirs: &[String],
        failed: &mut HashSet<usize>,
    ) -> io::Result<bool> {
        self.progress.current = Some(archive.to_path_buf());
        self.report(true);
        // An archive inside another one (mounted as a temp copy): a copy of
        // that is changed, then put back into the outer archive.
        let nested = !archive.exists() && archive_exists(archive);
        let stage = match nested {
            true => Some(Stage::copy_of(archive)?),
            false => None,
        };
        let target = stage.as_ref().map_or_else(|| archive.to_path_buf(), |s| s.file.clone());
        let password = self.password.take();
        let mut edit = Rewrite::begin(&target, password.as_ref().map(|p| p.0.as_str()))?;
        let control = self.control;
        if !edit.copy_existing(edits, &mut || control.checkpoint())? {
            return Ok(false);
        }
        for add in adds {
            if !self.alive() {
                return Ok(false);
            }
            match add {
                Add::Dir(inner, modified) => {
                    edit.add_dir(inner, *modified)?;
                    self.item_done(0);
                }
                Add::File { src, inner, modified, size, transfer } => {
                    self.progress.current = Some(src.clone());
                    let mut input = match File::open(src) {
                        Ok(f) => f,
                        Err(e) => {
                            self.fail(src, e);
                            failed.insert(*transfer);
                            continue;
                        }
                    };
                    edit.start_file(inner, *modified, *size)?;
                    match self.pump(&mut input, edit.output()) {
                        Ok(true) => self.item_done(0),
                        Ok(false) => return Ok(false),
                        Err(e) => {
                            edit.abort_file()?;
                            self.fail(src, e);
                            failed.insert(*transfer);
                        }
                    }
                }
            }
        }
        for dir in keep_dirs.iter().filter(|d| !d.is_empty()) {
            edit.add_dir(dir, None)?;
        }
        if !self.alive() {
            return Ok(false);
        }
        self.progress.current = Some(archive.to_path_buf());
        self.report(true);
        edit.commit()?;
        match stage {
            Some(stage) => Ok(self.put_back(archive, &stage.file)),
            None => Ok(true),
        }
    }

    /// Replaces the nested archive `archive` (a logical path) in the archive
    /// holding it with the changed copy `changed`. False if that failed
    /// (recorded; nothing changed).
    fn put_back(&mut self, archive: &Path, changed: &Path) -> bool {
        let found = archive.parent().and_then(vfs::split_archive).map(|(outer, _)| outer);
        let Some((outer, inner)) = found.and_then(|o| inner_of(&o, archive).filter(|i| !i.is_empty()).map(|i| (o, i))) else {
            return self.fail_msg(archive, "Not an archive inside another archive");
        };
        if self.open_writable(&outer).is_none() {
            return false;
        }
        let meta = match fs::metadata(changed) {
            Ok(m) => m,
            Err(e) => return self.fail(changed, e),
        };
        // Not through `pack`: its checks would see the mounted archive, not
        // the entry holding it.
        let saved = self.snapshot();
        let edits = Edits { remove: vec![inner.clone()], ..Edits::default() };
        let add = Add::File { src: changed.to_path_buf(), inner, modified: Some(SystemTime::now()), size: meta.len(), transfer: 0 };
        let done = self.rewrite(&outer, &edits, &[add], &[], saved).is_some_and(|failed| failed.is_empty());
        // Counted as one change, not as one more file.
        (self.progress.done_items, self.progress.done_bytes, self.outcome.skipped) = saved;
        vfs::unmount(archive);
        done
    }

    /// Removes a moved source after it was copied, without counting it as
    /// progress again.
    fn remove_source(&mut self, path: &Path) {
        let saved = self.snapshot();
        self.remove(path);
        self.restore(saved);
    }

    /// Permanently deletes `path` (recursively). Links are removed, never
    /// followed.
    /// `Job::SetAttrib` on `path` and, with `recursive`, what is inside it
    /// (a failing item is recorded and the rest goes on).
    fn set_attrib(&mut self, path: &Path, change: &crate::attrib::Change, recursive: bool) {
        if !self.alive() {
            return;
        }
        self.progress.current = Some(path.to_path_buf());
        match crate::attrib::apply(path, change) {
            Ok(()) => self.item_done(0),
            Err(e) => {
                self.fail(path, e);
            }
        }
        let is_dir = fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
        if !recursive || !is_dir || crate::attrib::is_link(path) {
            return;
        }
        match fs::read_dir(path) {
            Ok(children) => {
                for child in children.flatten() {
                    self.set_attrib(&child.path(), change, true);
                }
            }
            Err(e) => {
                self.fail(path, e);
            }
        }
    }

    fn remove(&mut self, path: &Path) -> bool {
        if !self.alive() {
            return false;
        }
        self.progress.current = Some(path.to_path_buf());
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => return self.fail(path, e),
        };

        let result = if meta.file_type().is_symlink() {
            remove_link(path, &meta)
        } else if meta.is_dir() {
            let children = match fs::read_dir(path) {
                Ok(rd) => rd,
                Err(e) => return self.fail(path, e),
            };
            let mut complete = true;
            for child in children {
                match child {
                    Ok(child) => complete &= self.remove(&child.path()),
                    Err(e) => complete = self.fail(path, e),
                }
            }
            if !complete {
                return false;
            }
            fs::remove_dir(path)
        } else {
            remove_file_force(path)
        };

        match result {
            Ok(()) => {
                let bytes = if meta.is_file() { meta.len() } else { 0 };
                self.item_done(bytes);
                true
            }
            Err(e) => self.fail(path, e),
        }
    }
}

/// Whether all transfers stay on one server.
fn same_server(transfers: &[Transfer]) -> bool {
    let key = |p: &Path| remote::split(p).map(|(k, _)| k.to_lowercase());
    transfers.iter().all(|t| key(&t.src).is_some() && key(&t.src) == key(&t.dst))
}

/// What exists at `path` on `server`, from the listing of its folder
/// (cached in `listed`; a missing folder lists as empty).
fn existing_on_server(server: &dyn Vfs, listed: &mut HashMap<String, Vec<Entry>>, path: &Path)
    -> io::Result<Option<Entry>> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return Ok(None) };
    let dir = remote::display(parent);
    if !listed.contains_key(&dir) {
        let entries = match server.list(parent) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        listed.insert(dir.clone(), entries);
    }
    let name = name.to_string_lossy();
    Ok(listed[&dir].iter().find(|e| !e.is_parent() && e.name == name).cloned())
}

/// Creates the folder `path` on a server, with any missing parents.
fn make_dirs_on_server(server: &dyn Vfs, path: &Path) -> io::Result<()> {
    if let Some(e) = server.stat(path)? {
        let what = if e.is_dir() { "folder" } else { "file" };
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("A {what} with this name already exists")));
    }
    let mut missing = vec![path.to_path_buf()];
    let mut cur = path.parent();
    while let Some(dir) = cur.filter(|d| remote::split(d).is_some_and(|(_, r)| r != "/")) {
        if server.stat(dir)?.is_some() {
            break;
        }
        missing.push(dir.to_path_buf());
        cur = dir.parent();
    }
    for dir in missing.iter().rev() {
        server.make_dir(dir)?;
    }
    Ok(())
}

/// Size and modification time, as conflicts show them.
fn stat_of(meta: &fs::Metadata) -> (u64, Option<SystemTime>) {
    (meta.len(), meta.modified().ok())
}

/// The archive the transfers go into (they share a target folder), if any.
/// A destination that is the archive file itself is a local copy over it.
fn dst_archive(transfers: &[Transfer]) -> Option<PathBuf> {
    let t = transfers.first()?;
    vfs::split_archive(&t.dst).filter(|(_, inner)| !inner.as_os_str().is_empty()).map(|(file, _)| file)
}

/// `path` inside `archive` as a `/`-separated inner path (`""` for the
/// archive itself). Works for archives that don't exist yet.
/// Whether the archive `archive` exists: on disk, or mounted inside
/// another archive (see [`vfs::split_archive`]).
fn archive_exists(archive: &Path) -> bool {
    archive.exists() || vfs::on_disk(archive).is_ok()
}

fn inner_of(archive: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(archive).ok()?;
    let parts: Vec<String> = rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Some(parts.join("/"))
}

/// A temp folder (`copy-*` in [`vfs::temp_root`]) holding a copy of a
/// nested archive while it's changed, or files copied between servers;
/// removed when dropped.
struct Stage {
    dir: PathBuf,
    file: PathBuf,
}

impl Stage {
    /// Copies the temp copy of the nested archive `archive`.
    fn copy_of(archive: &Path) -> io::Result<Stage> {
        let name = archive.file_name().ok_or_else(|| io::Error::other("Invalid archive path"))?;
        let dir = vfs::temp_root().join(format!("copy-nested-{}-{}", std::process::id(), unique_id()));
        let stage = Stage { file: dir.join(name), dir };
        fs::create_dir_all(&stage.dir)?;
        fs::copy(vfs::on_disk(archive)?, &stage.file)?;
        Ok(stage)
    }

    /// An empty temp folder for files copied between servers, which pass
    /// through `file` one at a time.
    fn relay() -> io::Result<Stage> {
        let dir = vfs::temp_root().join(format!("copy-relay-{}-{}", std::process::id(), unique_id()));
        fs::create_dir_all(&dir)?;
        Ok(Stage { file: dir.join("file"), dir })
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A number not returned before in this process (for temp folder names).
fn unique_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The folder holding the inner path `inner` (`""` = the archive root).
fn inner_parent(inner: &str) -> &str {
    inner.rfind('/').map_or("", |i| &inner[..i])
}

/// Something to add to an archive.
enum Add {
    Dir(String, Option<SystemTime>),
    /// A local file; `transfer` indexes the job's transfers.
    File { src: PathBuf, inner: String, modified: Option<SystemTime>, size: u64, transfer: usize },
}

/// (bytes, items) under `path`. Links count as one item and aren't followed.
fn measure(path: &Path) -> (u64, u64) {
    if crate::vfs::is_virtual(path) {
        let items = crate::vfs::open(path).and_then(|vfs| vfs.walk(path)).unwrap_or_default();
        return (items.iter().map(|e| e.size).sum(), items.len().max(1) as u64);
    }
    let Ok(meta) = fs::symlink_metadata(path) else { return (0, 1) };
    if meta.file_type().is_symlink() {
        return (link_bytes(path), 1);
    }
    if !meta.is_dir() {
        return (meta.len(), 1);
    }
    let mut total = (0, 1);
    if let Ok(children) = fs::read_dir(path) {
        for child in children.flatten() {
            let (b, n) = measure(&child.path());
            total.0 += b;
            total.1 += n;
        }
    }
    total
}

/// Sets a folder's modification time (opening a folder needs
/// `FILE_FLAG_BACKUP_SEMANTICS` on Windows).
fn set_dir_modified(dir: &Path, modified: SystemTime) -> io::Result<()> {
    #[cfg(windows)]
    let handle = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir)?
    };
    #[cfg(not(windows))]
    let handle = File::open(dir)?;
    handle.set_modified(modified)
}

fn clear_readonly(path: &Path) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    if perms.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// The check after a resumed transfer: the file must now be `want` bytes
/// long. A server that ignored the offset (sent or took the whole file
/// again) leaves a different size; the bytes themselves aren't compared.
fn check_resumed(got: u64, want: u64) -> io::Result<()> {
    match got == want {
        true => Ok(()),
        false => Err(io::Error::other(format!(
            "After resuming, the file has {got} bytes instead of {want}: it is damaged (the server may not \
             support resuming). Copy it again and choose Overwrite"
        ))),
    }
}

/// `e` of a transfer whose partial target (now `bytes` long) was kept.
fn kept_partial(e: io::Error, bytes: u64) -> io::Error {
    io::Error::new(
        e.kind(),
        format!("{e} (the partial file, {bytes} bytes, was kept: copy again and choose Resume to finish it)"),
    )
}

fn remove_file_force(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            clear_readonly(path)?;
            fs::remove_file(path)
        }
        other => other,
    }
}

#[cfg(windows)]
fn is_dir_link(_path: &Path, meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::FileTypeExt;
    meta.file_type().is_symlink_dir()
}

#[cfg(not(windows))]
fn is_dir_link(path: &Path, _meta: &fs::Metadata) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// The size a link counts as in progress: its target's, for a file.
fn link_bytes(path: &Path) -> u64 {
    fs::metadata(path).map(|m| if m.is_file() { m.len() } else { 0 }).unwrap_or(0)
}

/// What `Runner::copy_link` did.
enum LinkCopy {
    Done,
    /// Copy the link's contents instead (no privilege for file symlinks).
    Contents,
    /// Failed, skipped or cancelled (already recorded).
    Stop,
}

/// A symlink or junction, as read from disk, so it can be recreated.
enum Link {
    #[cfg(windows)]
    Junction(PathBuf),
    /// The target as stored (may be relative to the link's folder).
    Symlink { target: PathBuf, dir: bool },
}

impl Link {
    fn read(path: &Path, meta: &fs::Metadata) -> io::Result<Link> {
        let dir = is_dir_link(path, meta);
        #[cfg(windows)]
        if dir && junction::exists(path)? {
            return junction::get_target(path).map(Link::Junction);
        }
        Ok(Link::Symlink { target: fs::read_link(path)?, dir })
    }

    fn is_file(&self) -> bool {
        matches!(self, Link::Symlink { dir: false, .. })
    }

    /// Creates the same link at `dst` (`src` is the original, for resolving
    /// relative targets). On Windows a folder symlink falls back to a
    /// junction to the same folder when symlinks need a privilege we lack.
    fn create(&self, src: &Path, dst: &Path) -> io::Result<()> {
        match self {
            #[cfg(windows)]
            Link::Junction(target) => make_junction(target, dst),
            #[cfg(windows)]
            Link::Symlink { target, dir: true } => match std::os::windows::fs::symlink_dir(target, dst) {
                Err(e) if is_privilege_error(&e) => {
                    let absolute = src.parent().map_or_else(|| target.clone(), |p| p.join(target));
                    make_junction(&absolute, dst)
                }
                other => other,
            },
            #[cfg(windows)]
            Link::Symlink { target, dir: false } => std::os::windows::fs::symlink_file(target, dst),
            #[cfg(not(windows))]
            Link::Symlink { target, .. } => {
                let _ = src;
                std::os::unix::fs::symlink(target, dst)
            }
        }
    }
}

/// `junction::create` makes the folder first; it's removed again on failure.
#[cfg(windows)]
fn make_junction(target: &Path, dst: &Path) -> io::Result<()> {
    let existed = fs::symlink_metadata(dst).is_ok();
    let made = junction::create(target, dst);
    if made.is_err() && !existed {
        let _ = fs::remove_dir(dst);
    }
    made
}

/// ERROR_PRIVILEGE_NOT_HELD: symlinks need admin rights or developer mode.
fn is_privilege_error(e: &io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(1314)
}

fn remove_link(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    #[cfg(windows)]
    if is_dir_link(path, meta) {
        return fs::remove_dir(path);
    }
    let _ = meta;
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Auto {
        answer: Resolution,
        asked: usize,
    }
    impl Reporter for Auto {
        fn progress(&mut self, _: &Progress) {}
        fn conflict(&mut self, _: Conflict) -> Resolution {
            self.asked += 1;
            self.answer
        }
    }

    fn run_job(job: Job, answer: Resolution) -> (Outcome, usize) {
        let mut r = Auto { answer, asked: 0 };
        let outcome = run(&job, &Control::default(), &mut r);
        (outcome, r.asked)
    }

    fn split_round_trip(len: usize, part: u64) {
        use crate::split::CrcFile;
        let tmp = TempDir::new(&format!("split-{len}-{part}"));
        let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
        let src = tmp.0.join("f.bin");
        fs::write(&src, &data).unwrap();
        let parts = tmp.0.join("parts");
        let job = Job::Split { src, target: parts.clone(), part_size: part };
        let (o, _) = run_job(job, Resolution::Cancel);
        assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
        let count = len.div_ceil(part as usize);
        for i in 1..=count {
            let size = fs::metadata(parts.join(format!("f.bin.{i:03}"))).unwrap().len() as usize;
            assert_eq!(size, (len - (i - 1) * part as usize).min(part as usize), "part {i}");
        }
        assert!(!parts.join(format!("f.bin.{:03}", count + 1)).exists());
        let crc = CrcFile::parse(&fs::read_to_string(parts.join("f.bin.crc")).unwrap()).unwrap();
        assert_eq!((crc.filename.as_str(), crc.size, crc.crc32), ("f.bin", len as u64, crc32fast::hash(&data)));

        let out = tmp.0.join("out");
        let job = Job::Combine { first: parts.join("f.bin.001"), target: out.clone() };
        let (o, _) = run_job(job, Resolution::Cancel);
        assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
        assert_eq!(fs::read(out.join("f.bin")).unwrap(), data);
    }

    #[test]
    fn split_and_combine_round_trip() {
        split_round_trip(100, 25); // exact multiple
        split_round_trip(100, 30); // not a multiple
        split_round_trip(20, 1); // one byte per part
        split_round_trip(5, 100); // one part
    }

    #[test]
    fn combine_reports_a_missing_part_and_a_damaged_result() {
        let tmp = TempDir::new("combine-bad");
        fs::write(tmp.0.join("f.bin"), vec![7u8; 90]).unwrap();
        let parts = tmp.0.join("parts");
        let job = Job::Split { src: tmp.0.join("f.bin"), target: parts.clone(), part_size: 30 };
        assert!(run_job(job, Resolution::Cancel).0.errors.is_empty());
        // Damage the middle part (same size): the join is kept, with an error.
        fs::write(parts.join("f.bin.002"), vec![8u8; 30]).unwrap();
        let job = Job::Combine { first: parts.join("f.bin.001"), target: tmp.0.join("out") };
        let (o, _) = run_job(job, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
        assert!(o.errors[0].1.contains("damaged"), "{:?}", o.errors);
        assert_eq!(fs::metadata(tmp.0.join("out/f.bin")).unwrap().len(), 90);
        // Without the crc file there is no check.
        fs::remove_file(parts.join("f.bin.crc")).unwrap();
        let job = Job::Combine { first: parts.join("f.bin.001"), target: tmp.0.join("out2") };
        assert!(run_job(job, Resolution::Cancel).0.errors.is_empty());
        // A missing middle part is named, and nothing is written.
        fs::remove_file(parts.join("f.bin.002")).unwrap();
        let job = Job::Combine { first: parts.join("f.bin.001"), target: tmp.0.join("out3") };
        let (o, _) = run_job(job, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
        assert!(o.errors[0].1.contains("f.bin.002"), "{:?}", o.errors);
        assert!(!tmp.0.join("out3/f.bin").exists());
    }

    #[test]
    fn split_asks_about_existing_parts_and_cancel_removes_the_output() {
        let tmp = TempDir::new("split-exists");
        fs::write(tmp.0.join("f.bin"), vec![1u8; 60]).unwrap();
        let parts = tmp.0.join("parts");
        fs::create_dir_all(&parts).unwrap();
        fs::write(parts.join("f.bin.002"), b"old").unwrap();
        let job = Job::Split { src: tmp.0.join("f.bin"), target: parts.clone(), part_size: 20 };
        let (o, asked) = run_job(job.clone(), Resolution::Cancel);
        assert!(o.cancelled && asked == 1);
        // The part written before the question is gone; the old one stays.
        assert!(!parts.join("f.bin.001").exists() && !parts.join("f.bin.003").exists());
        assert_eq!(fs::read(parts.join("f.bin.002")).unwrap(), b"old");
        let (o, asked) = run_job(job, Resolution::SkipAll);
        assert!(o.errors.is_empty() && asked == 1 && o.skipped == 1);
        assert_eq!(fs::read(parts.join("f.bin.002")).unwrap(), b"old");
        assert_eq!(fs::metadata(parts.join("f.bin.003")).unwrap().len(), 20);
        let crc = crate::split::CrcFile::parse(&fs::read_to_string(parts.join("f.bin.crc")).unwrap()).unwrap();
        assert_eq!(crc.crc32, crc32fast::hash(&[1u8; 60]));
    }

    #[test]
    fn resumed_size_must_match_the_source() {
        assert!(check_resumed(10, 10).is_ok());
        for got in [9, 11, 20] {
            let e = check_resumed(got, 10).unwrap_err().to_string();
            assert!(e.contains("After resuming") && e.contains(&format!("{got} bytes instead of 10")), "{e}");
        }
    }

    #[test]
    fn local_copies_never_resume() {
        let tmp = TempDir::new("no-resume");
        fs::write(tmp.0.join("a.txt"), b"abcdef").unwrap();
        fs::create_dir_all(tmp.0.join("out")).unwrap();
        fs::write(tmp.0.join("out/a.txt"), b"ab").unwrap();
        let ts = plan_transfer(&[tmp.0.join("a.txt")], &tmp.0.join("out")).unwrap();
        let mut r = Auto { answer: Resolution::ResumeAll, asked: 0 };
        let outcome = run(&Job::Copy(ts), &Control::default(), &mut r);
        // Not offered, so the answer counts as Skip.
        assert_eq!((r.asked, outcome.skipped), (1, 1));
        assert_eq!(fs::read(tmp.0.join("out/a.txt")).unwrap(), b"ab");
    }

    #[cfg(windows)]
    #[test]
    fn set_attrib_job_goes_into_subfolders_only_when_asked() {
        use crate::attrib::{self, Change, Tri};
        let tmp = TempDir::new("setattrib");
        let inner = tmp.0.join("d").join("in.txt");
        fs::create_dir_all(inner.parent().unwrap()).unwrap();
        fs::write(&inner, b"x").unwrap();
        let change = Change { flags: [Tri::Leave, Tri::Set, Tri::Leave, Tri::Leave], ..Default::default() };
        let hidden = |p: &Path| attrib::get_attributes(p).unwrap() & attrib::HIDDEN != 0;
        let job = Job::SetAttrib { paths: vec![tmp.0.join("d")], change: change.clone(), recursive: false };
        let (outcome, _) = run_job(job, Resolution::Skip);
        assert!(outcome.errors.is_empty());
        assert!(hidden(&tmp.0.join("d")) && !hidden(&inner));
        let job = Job::SetAttrib { paths: vec![tmp.0.join("d")], change, recursive: true };
        let (outcome, _) = run_job(job, Resolution::Skip);
        assert!(outcome.errors.is_empty() && outcome.done_items == 2);
        assert!(hidden(&inner));
        // A missing path is reported and the rest still done.
        let change = Change { flags: [Tri::Leave, Tri::Clear, Tri::Leave, Tri::Leave], ..Default::default() };
        let job = Job::SetAttrib { paths: vec![tmp.0.join("nope"), inner.clone()], change, recursive: false };
        let (outcome, _) = run_job(job, Resolution::Skip);
        assert_eq!(outcome.errors.len(), 1);
        assert!(!hidden(&inner));
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("rc-ops-{name}-{}", std::process::id()));
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

    fn tree(root: &Path) -> PathBuf {
        let src = root.join("src");
        fs::create_dir_all(src.join("sub/deep")).unwrap();
        fs::write(src.join("a.txt"), b"aaa").unwrap();
        fs::write(src.join("sub/b.bin"), vec![7u8; BUFFER_SIZE * 2 + 5]).unwrap();
        fs::write(src.join("sub/deep/c.txt"), b"c").unwrap();
        src
    }

    #[test]
    fn copies_tree() {
        let tmp = TempDir::new("copy");
        let src = tree(&tmp.0);
        let dest = tmp.0.join("dest");
        fs::create_dir(&dest).unwrap();

        let ts = plan_transfer(std::slice::from_ref(&src), &dest).unwrap();
        assert_eq!(ts[0].dst, dest.join("src"));
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(fs::read(dest.join("src/sub/b.bin")).unwrap().len(), BUFFER_SIZE * 2 + 5);
        assert_eq!(fs::read(dest.join("src/sub/deep/c.txt")).unwrap(), b"c");
        assert!(src.join("a.txt").exists());
    }

    #[test]
    fn copy_with_rename_and_conflicts() {
        let tmp = TempDir::new("conflict");
        let a = tmp.0.join("a.txt");
        let b = tmp.0.join("b.txt");
        fs::write(&a, b"new").unwrap();
        fs::write(&b, b"old").unwrap();

        // Single source onto a non-folder target = copy under the new name.
        let ts = plan_transfer(std::slice::from_ref(&a), &b).unwrap();
        let (o, asked) = run_job(Job::Copy(ts.clone()), Resolution::Skip);
        assert_eq!((asked, o.skipped), (1, 1));
        assert_eq!(fs::read(&b).unwrap(), b"old");

        let (o, _) = run_job(Job::Copy(ts), Resolution::Overwrite);
        assert!(o.errors.is_empty());
        assert_eq!(fs::read(&b).unwrap(), b"new");
    }

    #[test]
    fn extracts_from_zip() {
        let tmp = TempDir::new("extract");
        let zip = tmp.0.join("a.zip");
        crate::vfs::tests::make_zip(&zip);
        let out = tmp.0.join("out");
        fs::create_dir(&out).unwrap();
        let sources = [zip.join("sub"), zip.join("Readme.md")];
        let ts = plan_transfer(&sources, &out).unwrap();
        assert_eq!(ts[0].dst, out.join("sub"));
        let (o, asked) = run_job(Job::Copy(ts.clone()), Resolution::Skip);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!((asked, o.done_items, o.done_bytes), (0, 4, 5009));
        assert_eq!(fs::read(out.join("sub/deep/x.txt")).unwrap(), b"deep file");
        assert_eq!(fs::read(out.join("Readme.md")).unwrap(), vec![b'r'; 5000]);

        // Existing targets: asked once per file, skipped files untouched.
        fs::write(out.join("Readme.md"), b"mine").unwrap();
        let (o, asked) = run_job(Job::Copy(ts.clone()), Resolution::Skip);
        assert_eq!((asked, o.skipped), (2, 2));
        assert_eq!(fs::read(out.join("Readme.md")).unwrap(), b"mine");
        let (o, _) = run_job(Job::Copy(ts), Resolution::OverwriteAll);
        assert!(o.errors.is_empty());
        assert_eq!(fs::read(out.join("Readme.md")).unwrap(), vec![b'r'; 5000]);

        // Single file under a new name; moving out deletes it in the zip.
        let ts = plan_transfer(&[zip.join("a.txt")], &out.join("renamed.txt")).unwrap();
        let (o, _) = run_job(Job::Copy(ts.clone()), Resolution::Skip);
        assert!(o.errors.is_empty());
        assert_eq!(fs::read(out.join("renamed.txt")).unwrap(), b"hello zip");
        let ts = plan_transfer(&[zip.join("a.txt"), zip.join("sub")], &out.join("moved")).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Skip);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(fs::read(out.join("moved/a.txt")).unwrap(), b"hello zip");
        assert_eq!(fs::read(out.join("moved/sub/deep/x.txt")).unwrap(), b"deep file");
        assert_eq!(zip_names(&zip), ["Readme.md"]);
    }

    /// Every file and folder in a zip (inner paths, sorted), as listed.
    fn zip_names(zip: &Path) -> Vec<String> {
        let vfs = vfs::open(zip).unwrap();
        let mut names: Vec<String> =
            vfs.walk(zip).unwrap().iter().map(|e| inner_of(zip, &e.path).unwrap()).collect();
        names.sort();
        names
    }

    fn zip_read(path: &Path) -> Vec<u8> {
        let mut out = Vec::new();
        vfs::open(path).unwrap().read_files(&[path.to_path_buf()], &mut |_, r| r.read_to_end(&mut out).map(|_| true)).unwrap();
        out
    }

    #[test]
    fn copies_and_moves_into_zip() {
        let tmp = TempDir::new("zip-in");
        let src = tree(&tmp.0);
        let zip = tmp.0.join("a.zip");
        crate::vfs::tests::make_zip(&zip);

        // The archive itself is a folder to copy into.
        let ts = plan_transfer(std::slice::from_ref(&src), &zip).unwrap();
        assert_eq!(ts[0].dst, zip.join("src"));
        let (o, asked) = run_job(Job::Copy(ts.clone()), Resolution::Skip);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!((asked, o.done_items), (0, 6));
        let names = zip_names(&zip);
        let expected = ["Readme.md", "a.txt", "src", "src/a.txt", "src/sub", "src/sub/b.bin", "src/sub/deep"];
        assert_eq!(names[..7], expected);
        assert_eq!(zip_read(&zip.join("src/sub/b.bin")), vec![7u8; BUFFER_SIZE * 2 + 5]);
        // Entries that were there are copied over unchanged.
        assert_eq!(zip_read(&zip.join("Readme.md")), vec![b'r'; 5000]);
        assert_eq!(zip_read(&zip.join("sub/deep/x.txt")), b"deep file");
        // Dates are kept (zip: 2 s steps).
        let local = fs::metadata(src.join("a.txt")).unwrap().modified().unwrap();
        let stored: SystemTime = vfs::open(&zip).unwrap().stat(&zip.join("src/a.txt")).unwrap().unwrap().modified.unwrap().into();
        assert!(local.duration_since(stored).unwrap_or_default() <= Duration::from_secs(2));

        // Again: every file is a conflict.
        let (o, asked) = run_job(Job::Copy(ts.clone()), Resolution::SkipAll);
        assert_eq!((asked, o.skipped), (1, 3));
        fs::write(src.join("a.txt"), b"changed").unwrap();
        let (o, _) = run_job(Job::Copy(ts.clone()), Resolution::OverwriteAll);
        assert!(o.errors.is_empty());
        assert_eq!(zip_read(&zip.join("src/a.txt")), b"changed");
        assert_eq!(zip_names(&zip).len(), names.len());

        // Into a folder inside, under a new name; then a move.
        let ts = plan_transfer(&[src.join("a.txt")], &zip.join("sub").join("new.txt")).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Skip);
        assert!(o.errors.is_empty());
        assert_eq!(zip_read(&zip.join("sub/new.txt")), b"changed");
        let ts = plan_transfer(std::slice::from_ref(&src), &zip.join("sub")).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Skip);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(!src.exists());
        assert_eq!(zip_read(&zip.join("sub/src/sub/deep/c.txt")), b"c");
    }

    #[test]
    fn updates_edited_entry() {
        let tmp = TempDir::new("zip-update");
        let zip = tmp.0.join("a.zip");
        crate::vfs::tests::make_zip(&zip);
        let entry = zip.join("sub").join("deep").join("x.txt");
        let opened = entry_stamp(&entry).unwrap();
        assert_eq!(opened.map(|s| s.0), Some(9));
        let copy = tmp.0.join("x.txt");

        // Saved in the editor: replaced without asking, the rest kept.
        fs::write(&copy, b"edited").unwrap();
        let (o, asked) = run_job(Job::Update { src: copy.clone(), dst: entry.clone(), expect: opened }, Resolution::Cancel);
        assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
        assert_eq!((asked, o.done_items), (0, 1));
        assert_eq!(zip_read(&entry), b"edited");
        assert_eq!(zip_names(&zip), ["Readme.md", "a.txt", "sub", "sub/deep", "sub/deep/x.txt"]);
        assert_eq!(zip_read(&zip.join("Readme.md")), vec![b'r'; 5000]);
        let saved = entry_stamp(&entry).unwrap();
        assert_eq!(saved.map(|s| s.0), Some(6));

        // Saved again, with the stamp from after the last save.
        fs::write(&copy, b"edited twice").unwrap();
        let (o, _) = run_job(Job::Update { src: copy.clone(), dst: entry.clone(), expect: saved }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&entry), b"edited twice");

        // Changed in the archive by someone else meanwhile: not overwritten.
        let other = tmp.0.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("x.txt"), b"someone else's change").unwrap();
        let theirs = entry_stamp(&entry).unwrap();
        let ts = plan_transfer(&[other.join("x.txt")], &zip.join("sub").join("deep")).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::OverwriteAll);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        fs::write(&copy, b"mine").unwrap();
        let (o, _) = run_job(Job::Update { src: copy.clone(), dst: entry.clone(), expect: theirs }, Resolution::Overwrite);
        assert_eq!(o.errors.len(), 1);
        assert!(o.errors[0].1.contains("changed in the archive"), "{:?}", o.errors);
        assert_eq!(zip_read(&entry), b"someone else's change");

        // Removed meanwhile: not put back.
        let now = entry_stamp(&entry).unwrap();
        let (o, _) = run_job(Job::Delete { paths: vec![entry.clone()], to_trash: false }, Resolution::Cancel);
        assert!(o.errors.is_empty());
        let (o, _) = run_job(Job::Update { src: copy.clone(), dst: entry.clone(), expect: now }, Resolution::Overwrite);
        assert!(o.errors.len() == 1 && o.errors[0].1.contains("removed"), "{:?}", o.errors);
        assert_eq!(entry_stamp(&entry).unwrap(), None);

        // In a 7z it works the same way: the file is put back into the 7z.
        let seven = tmp.0.join("s.7z");
        let mut w = sevenz_rust2::ArchiveWriter::create(&seven).unwrap();
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file("x.txt"), Some(&b"old"[..])).unwrap();
        w.finish().unwrap();
        let in_seven = seven.join("x.txt");
        let expect = entry_stamp(&in_seven).unwrap();
        let (o, _) = run_job(Job::Update { src: copy.clone(), dst: in_seven.clone(), expect }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&in_seven), b"mine");
    }

    /// Shift+F4 in a zip: an empty file is added with `expect: None`, which
    /// refuses to replace a file or folder that exists by then.
    #[test]
    fn adds_new_empty_entry() {
        let tmp = TempDir::new("zip-new");
        let zip = tmp.0.join("a.zip");
        crate::vfs::tests::make_zip(&zip);
        let empty = tmp.0.join("new.txt");
        fs::write(&empty, b"").unwrap();
        let entry = zip.join("sub").join("new.txt");
        let (o, _) = run_job(Job::Update { src: empty.clone(), dst: entry.clone(), expect: None }, Resolution::Cancel);
        assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
        assert_eq!(zip_read(&entry), b"");
        assert_eq!(entry_stamp(&entry).unwrap().map(|s| s.0), Some(0));
        assert_eq!(zip_names(&zip).len(), 6);

        // It exists by now (e.g. added by another instance): not replaced.
        let before = fs::read(&zip).unwrap();
        fs::write(&empty, b"x").unwrap();
        let (o, _) = run_job(Job::Update { src: empty.clone(), dst: entry.clone(), expect: None }, Resolution::Overwrite);
        assert!(o.errors.len() == 1 && o.errors[0].1.contains("added to the archive meanwhile"), "{:?}", o.errors);
        // A folder of that name: refused, the zip untouched.
        let (o, _) = run_job(Job::Update { src: empty, dst: zip.join("sub").join("deep"), expect: None }, Resolution::Overwrite);
        assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
        assert_eq!(fs::read(&zip).unwrap(), before);
    }

    #[test]
    fn edits_inside_zip() {
        let tmp = TempDir::new("zip-edit");
        let zip = tmp.0.join("a.zip");
        crate::vfs::tests::make_zip(&zip);
        let ok = |job| {
            let (o, _) = run_job(job, Resolution::Cancel);
            assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
            o
        };

        // Deleting the only file of an implicit folder keeps the folder.
        let o = ok(Job::Delete { paths: vec![zip.join("sub/deep/x.txt")], to_trash: true });
        assert_eq!(o.done_items, 1);
        assert_eq!(zip_names(&zip), ["Readme.md", "a.txt", "sub", "sub/deep"]);

        ok(Job::MakeDir(zip.join("sub/new/inner")));
        assert_eq!(zip_names(&zip), ["Readme.md", "a.txt", "sub", "sub/deep", "sub/new", "sub/new/inner"]);
        let (o, _) = run_job(Job::MakeDir(zip.join("SUB")), Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);

        // Rename a file (also case only) and a folder with its contents.
        ok(Job::Rename { from: zip.join("a.txt"), to: zip.join("b.txt") });
        ok(Job::Rename { from: zip.join("b.txt"), to: zip.join("B.txt") });
        ok(Job::Rename { from: zip.join("sub"), to: zip.join("folder") });
        assert_eq!(zip_names(&zip), ["B.txt", "Readme.md", "folder", "folder/deep", "folder/new", "folder/new/inner"]);
        assert_eq!(zip_read(&zip.join("B.txt")), b"hello zip");
        let (o, _) = run_job(Job::Rename { from: zip.join("B.txt"), to: zip.join("readme.md") }, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);

        // Copy and move within the archive.
        let ts = plan_transfer(&[zip.join("B.txt")], &zip.join("folder")).unwrap();
        ok(Job::Copy(ts));
        let ts = plan_transfer(&[zip.join("Readme.md")], &zip.join("folder").join("new")).unwrap();
        ok(Job::Move(ts));
        assert_eq!(zip_read(&zip.join("folder/B.txt")), b"hello zip");
        assert_eq!(zip_read(&zip.join("folder/new/Readme.md")), vec![b'r'; 5000]);
        assert_eq!(
            zip_names(&zip),
            ["B.txt", "folder", "folder/B.txt", "folder/deep", "folder/new", "folder/new/Readme.md", "folder/new/inner"]
        );

        ok(Job::Delete { paths: vec![zip.join("folder"), zip.join("B.txt")], to_trash: false });
        assert!(zip_names(&zip).is_empty());
    }

    /// A 7z with a run of two files (one block) and one file of its own.
    fn make_7z(at: &Path) {
        let mut w = sevenz_rust2::ArchiveWriter::create(at).unwrap();
        let entries: Vec<sevenz_rust2::ArchiveEntry> =
            ["sub/deep/x.txt", "sub/Readme.md"].map(sevenz_rust2::ArchiveEntry::new_file).to_vec();
        let data: [&[u8]; 2] = [b"hello 7z", &[b'r'; 5000]];
        w.push_archive_entries(entries, data.into_iter().map(Into::into).collect()).unwrap();
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file("a.txt"), Some(&b"hello 7z"[..])).unwrap();
        w.finish().unwrap();
    }

    #[test]
    fn edits_inside_7z() {
        let tmp = TempDir::new("7z-edit");
        let seven = tmp.0.join("a.7z");
        make_7z(&seven);
        assert_eq!(zip_names(&seven), ["a.txt", "sub", "sub/Readme.md", "sub/deep", "sub/deep/x.txt"]);
        let ok = |job| {
            let (o, _) = run_job(job, Resolution::Cancel);
            assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
            o
        };

        // Deleting the only file of an implicit folder keeps the folder.
        let o = ok(Job::Delete { paths: vec![seven.join("sub/deep/x.txt")], to_trash: true });
        assert_eq!(o.done_items, 1);
        assert_eq!(zip_names(&seven), ["a.txt", "sub", "sub/Readme.md", "sub/deep"]);

        ok(Job::MakeDir(seven.join("sub/new/inner")));
        assert_eq!(
            zip_names(&seven),
            ["a.txt", "sub", "sub/Readme.md", "sub/deep", "sub/new", "sub/new/inner"]
        );
        let (o, _) = run_job(Job::MakeDir(seven.join("SUB")), Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);

        // Rename a file (also case only) and a folder with its contents.
        ok(Job::Rename { from: seven.join("a.txt"), to: seven.join("b.txt") });
        ok(Job::Rename { from: seven.join("b.txt"), to: seven.join("B.txt") });
        ok(Job::Rename { from: seven.join("sub"), to: seven.join("folder") });
        assert_eq!(zip_names(&seven), ["B.txt", "folder", "folder/Readme.md", "folder/deep", "folder/new", "folder/new/inner"]);
        assert_eq!(zip_read(&seven.join("B.txt")), b"hello 7z");
        assert_eq!(zip_read(&seven.join("folder/Readme.md")), &[b'r'; 5000]);
        let (o, _) = run_job(Job::Rename { from: seven.join("B.txt"), to: seven.join("folder") }, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);

        // Copy and move within the archive.
        let ts = plan_transfer(&[seven.join("B.txt")], &seven.join("folder")).unwrap();
        ok(Job::Copy(ts));
        let ts = plan_transfer(&[seven.join("folder/Readme.md")], &seven.join("folder").join("new")).unwrap();
        ok(Job::Move(ts));
        assert_eq!(zip_read(&seven.join("folder/B.txt")), b"hello 7z");
        assert_eq!(zip_read(&seven.join("folder/new/Readme.md")), &[b'r'; 5000]);
        assert_eq!(
            zip_names(&seven),
            [
                "B.txt", "folder", "folder/B.txt", "folder/deep", "folder/new",
                "folder/new/Readme.md", "folder/new/inner"
            ]
        );

        // Pack a folder in, then drop everything.
        let local = tmp.0.join("in");
        fs::create_dir_all(local.join("deep")).unwrap();
        fs::write(local.join("deep/note.txt"), b"note").unwrap();
        ok(Job::Pack { sources: vec![local], archive: seven.clone(), password: None });
        assert_eq!(zip_read(&seven.join("in/deep/note.txt")), b"note");
        ok(Job::Delete { paths: vec![seven.join("in"), seven.join("folder"), seven.join("B.txt")], to_trash: false });
        assert!(zip_names(&seven).is_empty());
    }

    #[test]
    fn files_move_out_of_7z() {
        // A file moved out of a 7z into a folder, and copied back in.
        let tmp = TempDir::new("7z-out");
        let seven = tmp.0.join("a.7z");
        make_7z(&seven);
        let out = tmp.0.join("out");
        fs::create_dir(&out).unwrap();
        let (o, _) = run_job(Job::Move(plan_transfer(&[seven.join("sub/Readme.md")], &out).unwrap()), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&out.join("Readme.md")), &[b'r'; 5000]);
        assert_eq!(zip_names(&seven), ["a.txt", "sub", "sub/deep", "sub/deep/x.txt"]);
        let (o, _) = run_job(Job::Copy(plan_transfer(&[out.join("Readme.md")], &seven).unwrap()), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&seven.join("Readme.md")), &[b'r'; 5000]);
    }

    #[test]
    fn encrypted_7z_is_not_changed() {
        // Encrypted files (names plain) and encrypted names (password given):
        // both refused, the archive untouched.
        use sevenz_rust2::{encoder_options::AesEncoderOptions, ArchiveEntry, ArchiveWriter, EncoderMethod};
        let tmp = TempDir::new("7z-aes");
        let local = tmp.0.join("note.txt");
        fs::write(&local, b"note").unwrap();
        for hide_names in [false, true] {
            let seven = tmp.0.join(format!("s{hide_names}.7z"));
            let mut w = ArchiveWriter::create(&seven).unwrap();
            w.set_content_methods(vec![AesEncoderOptions::new("pw".into()).into(), EncoderMethod::LZMA2.into()]);
            w.set_encrypt_header(hide_names);
            w.push_archive_entry(ArchiveEntry::new_file("x.txt"), Some(&b"secret"[..])).unwrap();
            w.finish().unwrap();
            if hide_names {
                assert!(vfs::try_password(&seven, "pw").unwrap());
            }
            let before = fs::read(&seven).unwrap();
            let (o, _) = run_job(Job::Copy(plan_transfer(std::slice::from_ref(&local), &seven).unwrap()), Resolution::Cancel);
            assert!(o.errors.len() == 1 && o.errors[0].1.contains("encrypted"), "{:?}", o.errors);
            assert_eq!(fs::read(&seven).unwrap(), before);
        }
    }

    /// Every file under `root` (relative `/` path, contents), sorted.
    fn files_under(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<(String, Vec<u8>)> = LocalFs
            .walk(root)
            .unwrap()
            .into_iter()
            .filter(|e| !e.is_dir())
            .map(|e| (inner_of(root, &e.path).unwrap(), fs::read(&e.path).unwrap()))
            .collect();
        out.sort();
        out
    }

    fn secret(p: &str) -> Option<Secret> {
        Some(Secret(p.into()))
    }

    #[test]
    fn packs_encrypted_zip_and_7z() {
        let tmp = TempDir::new("pack-pw");
        let src = tree(&tmp.0);
        fs::create_dir_all(src.join("empty")).unwrap();
        fs::write(src.join("sub/nothing.txt"), b"").unwrap();
        for name in ["enc.zip", "enc.7z"] {
            let archive = tmp.0.join(name);
            let job = Job::Pack { sources: vec![src.clone()], archive: archive.clone(), password: secret("s3cret pw") };
            assert!(!format!("{job:?}").contains("s3cret"), "{job:?}");
            let (o, _) = run_job(job, Resolution::Cancel);
            assert!(o.errors.is_empty() && !o.cancelled, "{name}: {:?}", o.errors);
            let left: Vec<_> = fs::read_dir(&tmp.0).unwrap().flatten().map(|e| e.file_name()).collect();
            assert!(!left.iter().any(|n| n.to_string_lossy().contains(".rc")), "{name}: temp files left: {left:?}");

            if name.ends_with(".zip") {
                // Names show, every file is AES-encrypted, folders are plain.
                let mut zip = zip::ZipArchive::new(File::open(&archive).unwrap()).unwrap();
                for i in 0..zip.len() {
                    let f = zip.by_index_raw(i).unwrap();
                    assert_eq!(f.encrypted(), !f.is_dir(), "{name}: {:?}", f.name());
                }
                assert!(vfs::needs_password(&archive));
                let listed = vfs::list(&archive.join("src"), true).unwrap();
                assert!(listed.iter().any(|e| e.name == "a.txt"));
            } else {
                // The names are hidden too.
                let err = vfs::list(&archive, true).unwrap_err();
                assert!(err.to_string().contains(vfs::PASSWORD_REQUIRED), "{err}");
            }
            let err = vfs::open(&archive)
                .and_then(|v| v.read_files(&[archive.join("src/a.txt")], &mut |_, r| io::copy(r, &mut io::sink()).map(|_| true)));
            assert!(err.is_err(), "{name}: read without the password");

            assert!(!vfs::try_password(&archive, "wrong").unwrap(), "{name}");
            assert!(vfs::try_password(&archive, "s3cret pw").unwrap(), "{name}");
            let out = tmp.0.join(format!("out-{name}"));
            let (o, _) = run_job(Job::Unpack { archives: vec![archive.clone()], target: out.clone() }, Resolution::Cancel);
            assert!(o.errors.is_empty(), "{name}: {:?}", o.errors);
            assert_eq!(files_under(&out.join("src")), files_under(&src), "{name}");
            assert!(out.join("src/empty").is_dir(), "{name}");

            // An existing archive takes no password: refused, left as it was.
            let before = fs::read(&archive).unwrap();
            let job = Job::Pack { sources: vec![src.clone()], archive: archive.clone(), password: secret("pw") };
            let (o, _) = run_job(job, Resolution::Cancel);
            assert!(o.errors.len() == 1 && o.errors[0].1.contains("existing archive"), "{name}: {:?}", o.errors);
            assert_eq!(fs::read(&archive).unwrap(), before, "{name}");
        }
    }

    #[test]
    fn password_refused_for_tar_and_empty() {
        let tmp = TempDir::new("pack-pw-tar");
        let src = tree(&tmp.0);
        for name in ["a.tar", "a.tar.gz", "a.tgz", "a.tar.bz2", "a.txt.gz"] {
            let archive = tmp.0.join(name);
            let job = Job::Pack { sources: vec![src.clone()], archive: archive.clone(), password: secret("pw") };
            let (o, _) = run_job(job, Resolution::Cancel);
            assert!(o.errors.len() == 1 && o.errors[0].1 == vfs::NO_ENCRYPTION, "{name}: {:?}", o.errors);
            assert!(!archive.exists(), "{name}");
        }
        assert!(Rewrite::begin(&tmp.0.join("b.tar"), Some("pw")).is_err());
        assert!(!tmp.0.join("b.tar").exists());
        let archive = tmp.0.join("e.zip");
        let (o, _) = run_job(Job::Pack { sources: vec![src], archive: archive.clone(), password: secret("") }, Resolution::Cancel);
        assert!(o.errors.len() == 1 && o.errors[0].1.contains("empty"), "{:?}", o.errors);
        assert!(!archive.exists());
    }

    #[test]
    fn cancelled_encrypted_pack_leaves_nothing() {
        struct CancelAtArchive(Arc<Control>, PathBuf);
        impl Reporter for CancelAtArchive {
            fn progress(&mut self, p: &Progress) {
                if p.current.as_deref() == Some(self.1.as_path()) {
                    self.0.cancel();
                }
            }
            fn conflict(&mut self, _: Conflict) -> Resolution {
                Resolution::Cancel
            }
        }
        let tmp = TempDir::new("pack-pw-cancel");
        let src = tree(&tmp.0);
        for name in ["c.zip", "c.7z"] {
            let archive = tmp.0.join(name);
            let control = Arc::new(Control::default());
            let job = Job::Pack { sources: vec![src.clone()], archive: archive.clone(), password: secret("pw") };
            let o = run(&job, &control, &mut CancelAtArchive(control.clone(), archive.clone()));
            assert!(o.cancelled, "{name}");
            let left: Vec<_> = fs::read_dir(&tmp.0).unwrap().flatten().map(|e| e.file_name()).collect();
            assert_eq!(left, ["src"], "{name}");
            assert_eq!(files_under(&src).len(), 3, "{name}: sources kept");
        }
    }

    #[test]
    fn cancelled_7z_write_leaves_archive_intact() {
        let tmp = TempDir::new("7z-fail");
        let seven = tmp.0.join("a.7z");
        make_7z(&seven);
        let original = fs::read(&seven).unwrap();
        let no_temp = || {
            let left: Vec<_> =
                fs::read_dir(&tmp.0).unwrap().flatten().filter(|e| {
                    let entry = e.file_name();
                    let n = entry.to_string_lossy();
                    n.ends_with(".rctmp") || n.ends_with(".rcspool")
                }).collect();
            assert!(left.is_empty(), "temp files left behind: {:?}", left);
        };

        // Cancelled while writing: the temp file exists by the second report.
        struct CancelOnSecondProgress(Arc<Control>, usize);
        impl Reporter for CancelOnSecondProgress {
            fn progress(&mut self, p: &Progress) {
                self.1 += 1;
                if self.1 >= 2 && p.current.is_some() {
                    self.0.cancel();
                }
            }
            fn conflict(&mut self, _: Conflict) -> Resolution {
                Resolution::Cancel
            }
        }
        let control = Arc::new(Control::default());
        let ts = plan_transfer(&[seven.join("a.txt")], &seven.join("sub")).unwrap();
        let o = run(&Job::Move(ts), &control, &mut CancelOnSecondProgress(control.clone(), 0));
        assert!(o.cancelled && o.errors.is_empty());
        assert_eq!(fs::read(&seven).unwrap(), original);
        no_temp();

        // Cancelled at a conflict: a disk file copied into the archive twice.
        let local = tmp.0.join("note.txt");
        fs::write(&local, b"note").unwrap();
        let (o, _) = run_job(Job::Copy(plan_transfer(std::slice::from_ref(&local), &seven).unwrap()), Resolution::Cancel);
        assert_eq!(o.done_items, 1);
        let with_copy = fs::read(&seven).unwrap();
        // Cancelled at the conflict of the same copy again.
        let (o, asked) = run_job(Job::Copy(plan_transfer(&[local], &seven).unwrap()), Resolution::Cancel);
        assert!(o.cancelled && asked == 1);
        assert_eq!(fs::read(&seven).unwrap(), with_copy);
        no_temp();

        // The temp file can't be made: an error, the archive is untouched.
        let blocker = tmp.0.join(format!(".a.7z.{}.rctmp", std::process::id()));
        fs::create_dir(&blocker).unwrap();
        let (o, _) = run_job(Job::Delete { paths: vec![seven.join("sub/deep/x.txt")], to_trash: false }, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
        assert_eq!(fs::read(&seven).unwrap(), with_copy);
        fs::remove_dir(&blocker).unwrap();
        no_temp();
    }

    #[test]
    fn failed_or_cancelled_zip_write_leaves_archive_intact() {
        let tmp = TempDir::new("zip-fail");
        let src = tree(&tmp.0);
        let zip = tmp.0.join("a.zip");
        crate::vfs::tests::make_zip(&zip);
        let original = fs::read(&zip).unwrap();
        let ts = plan_transfer(std::slice::from_ref(&src), &zip).unwrap();

        // Cancelled while writing (when it starts rewriting the archive).
        struct CancelOnProgress(Arc<Control>, PathBuf);
        impl Reporter for CancelOnProgress {
            fn progress(&mut self, p: &Progress) {
                if p.current.as_ref() == Some(&self.1) {
                    self.0.cancel();
                }
            }
            fn conflict(&mut self, _: Conflict) -> Resolution {
                Resolution::Cancel
            }
        }
        let control = Arc::new(Control::default());
        let o = run(&Job::Move(ts.clone()), &control, &mut CancelOnProgress(control.clone(), zip.clone()));
        assert!(o.cancelled && o.errors.is_empty());
        assert_eq!(o.done_items, 0);
        assert_eq!(fs::read(&zip).unwrap(), original);
        assert!(src.join("sub/b.bin").exists());

        // Cancelled at a conflict.
        let (o, _) = run_job(Job::Copy(plan_transfer(&[src.join("a.txt")], &zip).unwrap()), Resolution::Skip);
        assert!(o.errors.is_empty());
        let with_a = fs::read(&zip).unwrap();
        let (o, asked) = run_job(Job::Copy(plan_transfer(&[src.join("a.txt")], &zip).unwrap()), Resolution::Cancel);
        assert!(o.cancelled && asked == 1);
        assert_eq!(fs::read(&zip).unwrap(), with_a);

        // The temp file can't be made: an error, the archive is untouched.
        let blocker = tmp.0.join(format!(".a.zip.{}.rctmp", std::process::id()));
        fs::create_dir(&blocker).unwrap();
        let before = fs::read(&zip).unwrap();
        let (o, _) = run_job(Job::Delete { paths: vec![zip.join("Readme.md")], to_trash: false }, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
        assert_eq!(fs::read(&zip).unwrap(), before);
        fs::remove_dir(&blocker).unwrap();
        // No temp files left behind.
        let left: Vec<_> = fs::read_dir(&tmp.0).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".rctmp")).collect();
        assert!(left.is_empty());
    }

    #[test]
    fn rewriting_keeps_encrypted_entries() {
        use std::io::Write as _;
        let tmp = TempDir::new("zip-aes");
        let zip = tmp.0.join("s.zip");
        let mut w = zip::ZipWriter::new(File::create(&zip).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("plain.txt", opts).unwrap();
        w.write_all(b"open").unwrap();
        w.start_file("aes.txt", opts.with_aes_encryption(zip::AesMode::Aes256, "pw")).unwrap();
        w.write_all(b"classified").unwrap();
        w.finish().unwrap();
        let (o, _) = run_job(Job::Delete { paths: vec![zip.join("plain.txt")], to_trash: false }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        let (o, _) = run_job(Job::Rename { from: zip.join("aes.txt"), to: zip.join("renamed.txt") }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_names(&zip), ["renamed.txt"]);
        assert!(vfs::try_password(&zip, "pw").unwrap());
        assert_eq!(zip_read(&zip.join("renamed.txt")), b"classified");
    }

    /// A zip made by 7-Zip with ZipCrypto (password "abc"): `keep.txt`
    /// ("kept secret") and `drop.txt`.
    const ZIPCRYPTO: &[u8] = &[
        0x50, 0x4b, 0x03, 0x04, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x50, 0x83, 0x16, 0xdc, 0x8c, 0x0d, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x64, 0x72, 0x6f, 0x70, 0x2e, 0x74, 0x78, 0x74, 0x4e, 0x74,
        0xbf, 0x62, 0x69, 0xf4, 0x90, 0x7a, 0x21, 0x96, 0x48, 0xb2, 0xd2, 0x50, 0x4b, 0x03, 0x04, 0x14, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x21, 0x50, 0xc1, 0x43, 0xd8, 0xd5, 0x17, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00,
        0x00, 0x6b, 0x65, 0x65, 0x70, 0x2e, 0x74, 0x78, 0x74, 0x84, 0x60, 0x33, 0x13, 0x22, 0x38, 0x14, 0x98, 0xc5, 0x7e, 0xb4,
        0xc6, 0x0b, 0xe7, 0xa6, 0xdd, 0xc8, 0xf9, 0x08, 0x14, 0x02, 0xe5, 0x76, 0x50, 0x4b, 0x01, 0x02, 0x3f, 0x00, 0x14, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x50, 0x83, 0x16, 0xdc, 0x8c, 0x0d, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x08, 0x00, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x72,
        0x6f, 0x70, 0x2e, 0x74, 0x78, 0x74, 0x0a, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x18, 0x00, 0x00, 0x98,
        0x40, 0x07, 0x2e, 0xc0, 0xd5, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x50, 0x4b, 0x01, 0x02, 0x3f, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x50, 0xc1, 0x43,
        0xd8, 0xd5, 0x17, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x08, 0x00, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x20, 0x00, 0x00, 0x00, 0x33, 0x00, 0x00, 0x00, 0x6b, 0x65, 0x65, 0x70, 0x2e, 0x74, 0x78, 0x74, 0x0a, 0x00, 0x20, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x18, 0x00, 0x00, 0x98, 0x40, 0x07, 0x2e, 0xc0, 0xd5, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00,
        0x02, 0x00, 0x02, 0x00, 0xb4, 0x00, 0x00, 0x00, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn zipcrypto_entries_survive_a_rewrite() {
        let tmp = TempDir::new("zipcrypto");
        let zip = tmp.0.join("zc.zip");
        fs::write(&zip, ZIPCRYPTO).unwrap();
        assert!(vfs::needs_password(&zip));
        assert!(!vfs::try_password(&zip, "abd").unwrap());
        assert!(vfs::try_password(&zip, "abc").unwrap());
        assert_eq!(zip_read(&zip.join("keep.txt")), b"kept secret");
        // Deleting an entry keeps the encrypted one encrypted and readable.
        let (o, _) = run_job(Job::Delete { paths: vec![zip.join("drop.txt")], to_trash: false }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_ne!(fs::read(&zip).unwrap(), ZIPCRYPTO);
        assert_eq!(zip_names(&zip), ["keep.txt"]);
        assert_eq!(zip_read(&zip.join("keep.txt")), b"kept secret");
        // Renaming it keeps it encrypted too.
        let (o, _) = run_job(Job::Rename { from: zip.join("keep.txt"), to: zip.join("moved.txt") }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_names(&zip), ["moved.txt"]);
        assert_eq!(zip_read(&zip.join("moved.txt")), b"kept secret");
        // A plain file can be added next to it.
        let local = tmp.0.join("note.txt");
        fs::write(&local, b"note").unwrap();
        let (o, _) = run_job(Job::Copy(plan_transfer(std::slice::from_ref(&local), &zip).unwrap()), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&zip.join("note.txt")), b"note");
        assert_eq!(zip_read(&zip.join("moved.txt")), b"kept secret");
    }

    #[test]
    fn copies_and_moves_between_archives() {
        struct Asked(Vec<PathBuf>);
        impl Reporter for Asked {
            fn progress(&mut self, _: &Progress) {}
            fn conflict(&mut self, c: Conflict) -> Resolution {
                self.0.push(c.src);
                Resolution::Skip
            }
        }
        let staging_left = || {
            let prefix = format!("copy-{}-", std::process::id());
            fs::read_dir(vfs::temp_root()).into_iter().flatten().flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        };
        let tmp = TempDir::new("zip-zip");
        let from = tmp.0.join("from.zip");
        let to = tmp.0.join("to.zip");
        crate::vfs::tests::make_zip(&from);
        crate::vfs::tests::make_zip(&to);
        let from_names = zip_names(&from);

        // A folder and a file into a folder of the other zip.
        let ts = plan_transfer(&[from.join("sub"), from.join("Readme.md")], &to.join("sub")).unwrap();
        assert_eq!(ts[0].dst, to.join("sub").join("sub"));
        let (o, asked) = run_job(Job::Copy(ts.clone()), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(asked, 0);
        // Counted once, though extracted and then packed.
        assert_eq!((o.done_items, o.done_bytes), (4, 5009));
        assert_eq!(zip_read(&to.join("sub/sub/deep/x.txt")), b"deep file");
        assert_eq!(zip_read(&to.join("sub/Readme.md")), vec![b'r'; 5000]);
        assert!(zip_names(&to).contains(&"sub/sub/deep".to_string()));
        // Entries that were there are kept.
        assert_eq!(zip_read(&to.join("sub/deep/x.txt")), b"deep file");
        assert_eq!(zip_read(&to.join("a.txt")), b"hello zip");
        assert_eq!(zip_names(&from), from_names);
        assert!(!staging_left());

        // Again: questions name the original files, skipped ones stay.
        let mut r = Asked(Vec::new());
        let o = run(&Job::Copy(ts), &Control::default(), &mut r);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        r.0.sort();
        assert_eq!(r.0, [from.join("Readme.md"), from.join("sub").join("deep").join("x.txt")]);
        assert_eq!(o.skipped, 2);

        // A move deletes the source once it's in.
        let ts = plan_transfer(&[from.join("a.txt")], &to.join("sub")).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&to.join("sub/a.txt")), b"hello zip");
        assert!(!zip_names(&from).contains(&"a.txt".to_string()));
        assert!(!staging_left());

        // From a tar: copying and moving work (tars can be changed too).
        let tar = tmp.0.join("t.tar");
        crate::vfs::tests::make_tar(File::create(&tar).unwrap());
        let ts = plan_transfer(&[tar.join("docs")], &to).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&to.join("docs/one.txt")), b"first");
        let ts = plan_transfer(&[tar.join("top.txt")], &to).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&to.join("top.txt")), vec![b't'; 3000]);
        assert!(!zip_names(&tar).contains(&"top.txt".to_string()));

        // From a 7z: moving works, and the file is dropped from the 7z.
        let seven = tmp.0.join("s.7z");
        let mut w = sevenz_rust2::ArchiveWriter::create(&seven).unwrap();
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file("x.txt"), Some(&b"seven"[..])).unwrap();
        w.finish().unwrap();
        let ts = plan_transfer(&[seven.join("x.txt")], &to).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&to.join("x.txt")), b"seven");
        assert_eq!(entry_stamp(&seven.join("x.txt")).unwrap(), None);

        // Into a tar and into a 7z: added.
        let ts = plan_transfer(&[from.join("Readme.md")], &tar).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&tar.join("Readme.md")), vec![b'r'; 5000]);
        let ts = plan_transfer(&[from.join("Readme.md")], &seven).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&seven.join("Readme.md")), vec![b'r'; 5000]);
        assert!(!staging_left());
    }

    #[test]
    fn writes_into_nested_archives() {
        let tmp = TempDir::new("nested");
        let src = tree(&tmp.0);
        let inner = tmp.0.join("inner.zip");
        crate::vfs::tests::make_zip(&inner);
        let (o, _) = run_job(Job::Pack { sources: vec![inner.clone()], archive: tmp.0.join("outer.zip"), password: None }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        let outer = tmp.0.join("outer.zip");
        let nested = outer.join("inner.zip");
        assert_eq!(vfs::archive_writable(&nested), Some(true));
        let ok = |job: Job| {
            let what = format!("{job:?}");
            let (o, _) = run_job(job, Resolution::OverwriteAll);
            assert!(o.errors.is_empty() && !o.cancelled, "{what}: {:?}", o.errors);
            o
        };

        // Copy in, rename, new folder, delete: the inner zip is rewritten
        // and put back into the outer one.
        let ts = plan_transfer(&[src.join("a.txt")], &nested).unwrap();
        assert_eq!(ts[0].dst, nested.join("a.txt"));
        let o = ok(Job::Copy(ts));
        assert_eq!(o.done_items, 1);
        assert_eq!(zip_read(&nested.join("a.txt")), b"aaa");
        ok(Job::Rename { from: nested.join("a.txt"), to: nested.join("b.txt") });
        ok(Job::MakeDir(nested.join("new")));
        ok(Job::Delete { paths: vec![nested.join("Readme.md")], to_trash: false });
        assert_eq!(zip_names(&nested), ["b.txt", "new", "sub", "sub/deep", "sub/deep/x.txt"]);
        assert_eq!(zip_names(&outer), ["inner.zip"]);
        // The outer zip itself holds the change (read without the mount).
        let mut z = zip::ZipArchive::new(File::open(&outer).unwrap()).unwrap();
        let mut bytes = Vec::new();
        z.by_name("inner.zip").unwrap().read_to_end(&mut bytes).unwrap();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut b = String::new();
        z.by_name("b.txt").unwrap().read_to_string(&mut b).unwrap();
        assert_eq!(b, "aaa");

        // F4 save-back into it.
        let entry = nested.join("b.txt");
        let stamp = entry_stamp(&entry).unwrap();
        let copy = tmp.0.join("edited.txt");
        fs::write(&copy, b"edited").unwrap();
        ok(Job::Update { src: copy, dst: entry.clone(), expect: stamp });
        assert_eq!(zip_read(&entry), b"edited");

        // Refused inside: both untouched.
        let before = fs::read(&outer).unwrap();
        let (o, _) = run_job(Job::MakeDir(nested.join("NEW")), Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
        assert_eq!(fs::read(&outer).unwrap(), before);
        // The outer zip can't be replaced (read-only file): an error, both
        // zips as they were, no temp files left.
        let writable = fs::metadata(&outer).unwrap().permissions();
        let mut perms = writable.clone();
        perms.set_readonly(true);
        fs::set_permissions(&outer, perms).unwrap();
        let (o, _) = run_job(Job::MakeDir(nested.join("other")), Resolution::Cancel);
        fs::set_permissions(&outer, writable).unwrap();
        assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
        assert_eq!(fs::read(&outer).unwrap(), before);
        assert_eq!(zip_names(&nested), ["b.txt", "new", "sub", "sub/deep", "sub/deep/x.txt"]);
        let names = fs::read_dir(&tmp.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned());
        assert!(!names.into_iter().any(|n| n.contains(".rctmp")));
        let staged = fs::read_dir(vfs::temp_root()).into_iter().flatten().flatten();
        let prefix = format!("copy-nested-{}-", std::process::id());
        assert!(!staged.into_iter().any(|e| e.file_name().to_string_lossy().starts_with(&prefix)));

        // Reading out of it works: copy out and unpack.
        let out = tmp.0.join("out");
        let (o, _) = run_job(Job::Copy(plan_transfer(&[nested.join("sub")], &out.join("x")).unwrap()), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(fs::read(out.join("x/deep/x.txt")).unwrap(), b"deep file");
        let (o, _) = run_job(Job::Unpack { archives: vec![nested.clone()], target: out.join("all") }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(fs::read(out.join("all/b.txt")).unwrap(), b"edited");
        let _ = fs::remove_dir_all(vfs::temp_path(&nested).parent().unwrap());
    }

    #[test]
    fn packs_and_unpacks() {
        let tmp = TempDir::new("pack");
        let src = tree(&tmp.0);
        let archive = tmp.0.join("out").join("new.zip");
        fs::create_dir(tmp.0.join("out")).unwrap();
        let sources = vec![src.join("sub"), src.join("a.txt")];
        let (o, _) = run_job(Job::Pack { sources: sources.clone(), archive: archive.clone(), password: None }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_names(&archive), ["a.txt", "sub", "sub/b.bin", "sub/deep", "sub/deep/c.txt"]);
        // Packing again into it asks about each file.
        let (o, asked) = run_job(Job::Pack { sources, archive: archive.clone(), password: None }, Resolution::SkipAll);
        assert_eq!((asked, o.skipped), (1, 3));

        // A folder's date survives the round trip.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        set_dir_modified(&src.join("sub").join("deep"), old).unwrap();
        let (o, _) = run_job(Job::Pack { sources: vec![src.join("sub")], archive: tmp.0.join("d.zip"), password: None }, Resolution::OverwriteAll);
        assert!(o.errors.is_empty());

        let target = tmp.0.join("unpacked");
        let job = Job::Unpack { archives: vec![archive], target: target.clone() };
        let (o, _) = run_job(job, Resolution::OverwriteAll);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"aaa");
        assert_eq!(fs::read(target.join("sub/b.bin")).unwrap(), vec![7u8; BUFFER_SIZE * 2 + 5]);
        let dated = tmp.0.join("dated");
        let (o, _) = run_job(Job::Unpack { archives: vec![tmp.0.join("d.zip")], target: dated.clone() }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        let got = fs::metadata(dated.join("sub").join("deep")).unwrap().modified().unwrap();
        assert!(got.duration_since(old).unwrap_or_default() <= Duration::from_secs(2), "{got:?}");

        // A 7z can be changed too: packing adds a file, and its old entries
        // are kept; deleting rewrites it without one.
        let seven = tmp.0.join("b.7z");
        let mut w = sevenz_rust2::ArchiveWriter::create(&seven).unwrap();
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file("x.txt"), Some(&b"x"[..])).unwrap();
        w.finish().unwrap();
        let ts = plan_transfer(&[target.join("a.txt")], &seven).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&seven.join("a.txt")), b"aaa");
        assert_eq!(zip_read(&seven.join("x.txt")), b"x");
        let (o, _) = run_job(Job::Delete { paths: vec![seven.join("x.txt")], to_trash: false }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(entry_stamp(&seven.join("x.txt")).unwrap(), None);
        assert_eq!(zip_read(&seven.join("a.txt")), b"aaa");
    }

    #[test]
    fn moves_and_merges() {
        let tmp = TempDir::new("move");
        let src = tree(&tmp.0);
        let dest = tmp.0.join("dest");
        fs::create_dir(&dest).unwrap();

        // Pre-existing folder with one clashing file forces the copy path.
        fs::create_dir_all(dest.join("src/sub")).unwrap();
        fs::write(dest.join("src/sub/b.bin"), b"x").unwrap();

        let ts = plan_transfer(std::slice::from_ref(&src), &dest).unwrap();
        let (o, asked) = run_job(Job::Move(ts), Resolution::SkipAll);
        assert_eq!(asked, 1);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(fs::read(dest.join("src/a.txt")).unwrap(), b"aaa");
        // Skipped file stays in the source; everything else moved.
        assert!(src.join("sub/b.bin").exists());
        assert!(!src.join("a.txt").exists());
        assert!(!src.join("sub/deep").exists());

        // Plain rename path.
        let ts = plan_transfer(&[dest.join("src/a.txt")], &tmp.0.join("renamed.txt")).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Cancel);
        assert!(o.errors.is_empty());
        assert!(tmp.0.join("renamed.txt").exists());
    }

    #[test]
    fn deletes_permanently_including_readonly() {
        let tmp = TempDir::new("delete");
        let src = tree(&tmp.0);
        let ro = src.join("sub/deep/c.txt");
        let mut perms = fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&ro, perms).unwrap();

        let (o, _) = run_job(Job::Delete { paths: vec![src.clone()], to_trash: false }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(!src.exists());
        assert_eq!(o.done_items, 6);
    }

    #[test]
    fn rejects_copy_into_itself() {
        let tmp = TempDir::new("self");
        let src = tree(&tmp.0);
        assert!(plan_transfer(std::slice::from_ref(&src), &src.join("sub")).is_err());
        assert!(plan_transfer(&[src.join("a.txt")], &src).is_err());
        assert!(plan_transfer(&[src.join("a.txt")], &src.join("A.TXT")).is_err() == cfg!(windows));
    }

    /// A sandbox with `target/inside.txt` and `src/` holding a junction `j`
    /// to `target`.
    #[cfg(windows)]
    fn junction_tree(root: &Path) -> (PathBuf, PathBuf) {
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("inside.txt"), b"keep me").unwrap();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        junction::create(&target, src.join("j")).unwrap();
        (src, target)
    }

    #[cfg(windows)]
    fn assert_target_intact(target: &Path) {
        assert_eq!(fs::read(target.join("inside.txt")).unwrap(), b"keep me");
    }

    #[cfg(windows)]
    #[test]
    fn copies_junction_as_junction() {
        let tmp = TempDir::new("junction-copy");
        let (src, target) = junction_tree(&tmp.0);
        let dest = tmp.0.join("dest");
        fs::create_dir(&dest).unwrap();

        let ts = plan_transfer(std::slice::from_ref(&src), &dest).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        let copy = dest.join("src/j");
        assert!(junction::exists(&copy).unwrap());
        assert_eq!(normalize(&junction::get_target(&copy).unwrap()), normalize(&target));
        assert_eq!(fs::read(copy.join("inside.txt")).unwrap(), b"keep me");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);

        // The link alone, too; copying it into its own target is fine.
        let ts = plan_transfer(&[src.join("j")], &target).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(junction::exists(target.join("j")).unwrap());
        fs::remove_dir(target.join("j")).unwrap();
        assert_target_intact(&target);
    }

    #[cfg(windows)]
    #[test]
    fn overwrites_link_not_its_target() {
        let tmp = TempDir::new("junction-over");
        let (src, target) = junction_tree(&tmp.0);
        let other = tmp.0.join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("o.txt"), b"o").unwrap();
        let dest = tmp.0.join("dest");
        fs::create_dir(&dest).unwrap();
        junction::create(&other, dest.join("j")).unwrap();

        let ts = plan_transfer(&[src.join("j")], &dest).unwrap();
        let (o, asked) = run_job(Job::Copy(ts.clone()), Resolution::Skip);
        assert_eq!((asked, o.skipped), (1, 1));
        let (o, _) = run_job(Job::Copy(ts), Resolution::Overwrite);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(normalize(&junction::get_target(dest.join("j")).unwrap()), normalize(&target));
        assert_eq!(fs::read(other.join("o.txt")).unwrap(), b"o");
        assert_target_intact(&target);

        // A real folder in the way is an error, and left alone.
        fs::create_dir(dest.join("k")).unwrap();
        let ts = plan_transfer(&[src.join("j")], &dest.join("k")).unwrap();
        assert_eq!(ts[0].dst, dest.join("k/j"));
        fs::create_dir(dest.join("k/j")).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Overwrite);
        assert_eq!(o.errors.len(), 1);
        assert!(dest.join("k/j").is_dir() && !is_link(&dest.join("k/j")));
    }

    #[cfg(windows)]
    #[test]
    fn moves_junction_without_touching_target() {
        let tmp = TempDir::new("junction-move");
        let (src, target) = junction_tree(&tmp.0);
        let dest = tmp.0.join("dest");

        // Rename path.
        let ts = plan_transfer(&[src.join("j")], &tmp.0.join("renamed")).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(junction::exists(tmp.0.join("renamed")).unwrap());
        assert!(fs::symlink_metadata(src.join("j")).is_err());
        assert_target_intact(&target);

        // Copy + delete path (merging into an existing folder, as across drives).
        fs::rename(tmp.0.join("renamed"), src.join("j")).unwrap();
        fs::create_dir_all(dest.join("src")).unwrap();
        let ts = plan_transfer(std::slice::from_ref(&src), &dest).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(!src.exists());
        assert!(junction::exists(dest.join("src/j")).unwrap());
        assert_target_intact(&target);
    }

    #[cfg(windows)]
    #[test]
    fn deletes_junction_only() {
        let tmp = TempDir::new("junction-delete");
        let (src, target) = junction_tree(&tmp.0);
        let (o, _) = run_job(Job::Delete { paths: vec![src.clone()], to_trash: false }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(!src.exists());
        assert_target_intact(&target);
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
    }

    /// Symlinks need developer mode or admin rights on Windows; skipped
    /// (with a note) without them.
    #[test]
    fn copies_symlinks_as_links() {
        let tmp = TempDir::new("symlink");
        let target = tmp.0.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inside.txt"), b"keep me").unwrap();
        let src = tmp.0.join("src");
        fs::create_dir(&src).unwrap();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(r"..\target", src.join("d"))
            .and_then(|_| std::os::windows::fs::symlink_file(r"..\target\inside.txt", src.join("f")));
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink("../target", src.join("d"))
            .and_then(|_| std::os::unix::fs::symlink("../target/inside.txt", src.join("f")));
        if let Err(e) = made {
            eprintln!("skipping symlink test: {e}");
            return;
        }
        let dest = tmp.0.join("dest");
        fs::create_dir(&dest).unwrap();
        let ts = plan_transfer(std::slice::from_ref(&src), &dest).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        for name in ["d", "f"] {
            let copy = dest.join("src").join(name);
            assert!(is_link(&copy), "{name}");
            assert_eq!(fs::read_link(&copy).unwrap(), fs::read_link(src.join(name)).unwrap());
        }

        let (o, _) = run_job(Job::Delete { paths: vec![dest.clone(), src.clone()], to_trash: false }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(!dest.exists() && !src.exists());
        assert_eq!(fs::read(target.join("inside.txt")).unwrap(), b"keep me");
    }
}
