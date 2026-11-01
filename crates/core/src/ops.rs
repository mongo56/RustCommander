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
//! copies out of them are extracts like from an archive.

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
const READONLY: &str = "Only zip, tar and gz archives can be changed (not 7z or rar, nor ones inside those)";

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
    /// Packs `sources` into the zip `archive` (created if missing, else
    /// added to).
    Pack { sources: Vec<PathBuf>, archive: PathBuf },
    /// Unpacks each archive's contents into the folder `target`.
    Unpack { archives: Vec<PathBuf>, target: PathBuf },
    /// Puts the edited temp copy `src` back into its zip as the file `dst`
    /// (F4 inside an archive), replacing it without asking, but only if
    /// that file is still as `expect` says (see [`entry_stamp`]); otherwise
    /// it changed in the archive meanwhile and nothing is overwritten.
    Update { src: PathBuf, dst: PathBuf, expect: Stamp },
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Overwrite,
    OverwriteAll,
    /// Overwrite this and all later conflicts where the target is older.
    OverwriteOlder,
    Skip,
    SkipAll,
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
        if !is_link(src) && src.is_dir() && dst_n.starts_with(&src_n) {
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
    };

    match job {
        Job::Copy(transfers) | Job::Move(transfers) => {
            // A move on a server is a rename: not worth walking the tree.
            let renamed = |t: &Transfer| remote::is_remote(&t.src) && remote::is_remote(&t.dst);
            let sizes: Vec<(u64, u64)> =
                transfers.iter().map(|t| if renamed(t) { (0, 1) } else { measure(&t.src) }).collect();
            for (bytes, items) in &sizes {
                runner.progress.total_bytes += bytes;
                runner.progress.total_items += items;
            }
            runner.report(true);
            let is_move = matches!(job, Job::Move(_));
            let from_server = transfers.iter().find(|t| remote::is_remote(&t.src));
            if transfers.iter().any(|t| remote::is_remote(&t.dst)) {
                match from_server {
                    Some(t) if !is_move || !same_server(transfers) => {
                        runner.fail_msg(&t.src, "Copying between or within servers isn't supported: copy to a local folder first");
                    }
                    Some(_) => runner.move_on_server(transfers),
                    None => runner.upload(transfers, is_move),
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
        Job::Pack { sources, archive } => {
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
    runner.outcome.done_bytes = runner.progress.done_bytes;
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
    /// Temp copies of files from another archive, being packed into a zip:
    /// (temp path, the original), to show the original in errors and
    /// questions.
    staged: Vec<(PathBuf, PathBuf)>,
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
        (src_size, src_modified): (u64, Option<SystemTime>),
        dst: &Path,
        (dst_size, dst_modified): (u64, Option<SystemTime>),
    ) -> Resolution {
        let target_older = matches!((src_modified, dst_modified), (Some(s), Some(d)) if d < s);

        let answer = match self.sticky {
            Some(sticky) => sticky,
            None => {
                let conflict = Conflict {
                    src: self.origin(src),
                    dst: dst.to_path_buf(),
                    src_size,
                    dst_size,
                    src_modified: src_modified.map(DateTime::from),
                    dst_modified: dst_modified.map(DateTime::from),
                };
                self.reporter.conflict(conflict)
            }
        };

        match answer {
            Resolution::OverwriteAll | Resolution::SkipAll | Resolution::OverwriteOlder => {
                self.sticky = Some(answer)
            }
            _ => {}
        }
        match answer {
            Resolution::Overwrite | Resolution::OverwriteAll => Resolution::Overwrite,
            Resolution::OverwriteOlder if target_older => Resolution::Overwrite,
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
        let mut files = Vec::new();
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
            if let Ok(dst_meta) = existing {
                if dst_meta.is_dir() {
                    self.fail_msg(&dst, "A folder with this name already exists");
                    continue;
                }
                match self.resolve(&item.path, (item.size, item.modified.map(Into::into)), &dst, stat_of(&dst_meta)) {
                    Resolution::Overwrite => {}
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
            targets.push((dst, item));
        }
        let mut failures = Vec::new();
        let result = vfs.read_files(&files, &mut |i, input| {
            let (dst, item) = &targets[i];
            self.progress.current = Some(item.path.clone());
            match self.write_out(input, dst, item.modified.map(Into::into)) {
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
    /// cancelled; a partial file is removed.
    fn write_out(&mut self, input: &mut dyn Read, dst: &Path, modified: Option<SystemTime>) -> io::Result<bool> {
        let mut output = match File::create(dst) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                clear_readonly(dst)?;
                File::create(dst)?
            }
            other => other?,
        };
        let result = self.pump(input, &mut output).and_then(|complete| {
            if let Some(m) = modified.filter(|_| complete) {
                output.set_modified(m)?;
            }
            Ok(complete)
        });
        drop(output);
        if !matches!(result, Ok(true)) {
            let _ = fs::remove_file(dst);
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
    /// returned) if it can't be read or isn't a zip.
    fn open_writable(&mut self, archive: &Path) -> Option<std::sync::Arc<dyn Vfs>> {
        match vfs::open(archive) {
            Ok(v) if v.writable() => Some(v),
            Ok(_) => {
                self.fail_msg(archive, "Only zip, tar and gz archives can be changed (7z and rar are read-only)");
                None
            }
            Err(e) => {
                self.fail(archive, e);
                None
            }
        }
    }

    /// Copies or moves local files and folders into the zip `archive` (the
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

    /// [`Job::Update`]: replaces the file `dst` in its zip with `src`, if
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
    /// can be read, also ones inside archives) into the zip `archive`:
    /// they're extracted to a temp folder, packed like local files, and the
    /// temp folder is removed. A move then deletes the sources, so it needs
    /// them in a zip too.
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
                self.fail_msg(&t.src, "Can't move out of a read-only archive (only zip, tar and gz archives can be changed): copy with F5 instead");
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

    /// Moves (or with `keep`, copies) items within the zip `archive`: the
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

    /// Deletes paths inside zip archives (one rewrite per archive). True if
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

    /// Creates a folder: inside a zip by rewriting it, else on disk.
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

    /// Uploads local files and folders to a server (the transfers'
    /// destinations are on it). Conflicts are asked file by file; a moved
    /// source is removed once everything in it was uploaded.
    fn upload(&mut self, transfers: &[Transfer], is_move: bool) {
        let Some(first) = transfers.first() else { return };
        let server = match vfs::open(&first.dst) {
            Ok(v) => v,
            Err(e) => {
                self.fail(&first.dst, e);
                return;
            }
        };
        // Listings of the target folders, to find conflicts without asking
        // the server about every file.
        let mut listed: HashMap<String, Vec<Entry>> = HashMap::new();
        for t in transfers {
            if !self.alive() {
                return;
            }
            self.progress.current = Some(t.src.clone());
            let items = match LocalFs.walk(&t.src) {
                Ok(items) => items,
                Err(e) => {
                    self.fail(&t.src, e);
                    continue;
                }
            };
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
                if let Some(e) = existing {
                    if e.is_dir() {
                        complete = self.fail_msg(&dst, "A folder with this name already exists");
                        continue;
                    }
                    let src_stat = (item.size, item.modified.map(Into::into));
                    match self.resolve(&item.path, src_stat, &dst, (e.size, e.modified.map(Into::into))) {
                        Resolution::Overwrite => {}
                        Resolution::Cancel => {
                            self.control.cancel();
                            return;
                        }
                        _ => {
                            self.outcome.skipped += 1;
                            self.item_done(item.size);
                            complete = false;
                            continue;
                        }
                    }
                }
                let mut input = match File::open(&item.path) {
                    Ok(f) => f,
                    Err(e) => {
                        complete = self.fail(&item.path, e);
                        continue;
                    }
                };
                let modified = item.modified.map(Into::into);
                match server.write_file(&dst, modified, &mut |out| self.pump(&mut input, out)) {
                    Ok(true) => self.item_done(0),
                    Ok(false) => return,
                    Err(e) => complete = self.fail(&dst, e),
                }
            }
            if is_move && complete && self.alive() {
                self.remove_source(&t.src);
            }
        }
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
        let result = self.write_zip(archive, edits, adds, keep_dirs, &mut failed);
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

    fn write_zip(
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
        let mut edit = Rewrite::begin(&target)?;
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

/// A number not returned before in this process (for temp folder names).
/// A temp folder (`copy-*` in [`vfs::temp_root`]) holding a copy of a
/// nested archive while it's changed; removed when dropped.
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
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn unique_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The folder holding the inner path `inner` (`""` = the archive root).
fn inner_parent(inner: &str) -> &str {
    inner.rfind('/').map_or("", |i| &inner[..i])
}

/// Something to add to a zip.
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

        // Not in a zip (a 7z, or a zip inside one): refused.
        let (o, _) = run_job(Job::Update { src: copy.clone(), dst: tmp.0.join("a.7z").join("x.txt"), expect: None }, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
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
    fn zipcrypto_is_read_but_not_rewritten() {
        let tmp = TempDir::new("zipcrypto");
        let zip = tmp.0.join("zc.zip");
        fs::write(&zip, ZIPCRYPTO).unwrap();
        assert!(vfs::needs_password(&zip));
        assert!(!vfs::try_password(&zip, "abd").unwrap());
        assert!(vfs::try_password(&zip, "abc").unwrap());
        assert_eq!(zip_read(&zip.join("keep.txt")), b"kept secret");
        // Its entries would lose their encryption flag: refused, unchanged.
        let (o, _) = run_job(Job::Delete { paths: vec![zip.join("drop.txt")], to_trash: false }, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
        assert!(o.errors[0].1.contains("ZipCrypto"), "{:?}", o.errors);
        assert_eq!(fs::read(&zip).unwrap(), ZIPCRYPTO);
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

        // From a 7z: moving is refused (read-only source).
        let seven = tmp.0.join("s.7z");
        let mut w = sevenz_rust2::ArchiveWriter::create(&seven).unwrap();
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file("x.txt"), Some(&b"x"[..])).unwrap();
        w.finish().unwrap();
        let ts = plan_transfer(&[seven.join("x.txt")], &to).unwrap();
        let before = fs::read(&to).unwrap();
        let (o, _) = run_job(Job::Move(ts), Resolution::Cancel);
        assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
        assert_eq!(fs::read(&to).unwrap(), before);

        // Into a tar: added; into a 7z: refused.
        let ts = plan_transfer(&[from.join("Readme.md")], &tar).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_read(&tar.join("Readme.md")), vec![b'r'; 5000]);
        let ts = plan_transfer(&[from.join("Readme.md")], &seven).unwrap();
        let before = fs::read(&seven).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
        assert_eq!(fs::read(&seven).unwrap(), before);
        assert!(!staging_left());
    }

    #[test]
    fn writes_into_nested_archives() {
        let tmp = TempDir::new("nested");
        let src = tree(&tmp.0);
        let inner = tmp.0.join("inner.zip");
        crate::vfs::tests::make_zip(&inner);
        let (o, _) = run_job(Job::Pack { sources: vec![inner.clone()], archive: tmp.0.join("outer.zip") }, Resolution::Cancel);
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
        let (o, _) = run_job(Job::Pack { sources: sources.clone(), archive: archive.clone() }, Resolution::Cancel);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(zip_names(&archive), ["a.txt", "sub", "sub/b.bin", "sub/deep", "sub/deep/c.txt"]);
        // Packing again into it asks about each file.
        let (o, asked) = run_job(Job::Pack { sources, archive: archive.clone() }, Resolution::SkipAll);
        assert_eq!((asked, o.skipped), (1, 3));

        // A folder's date survives the round trip.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        set_dir_modified(&src.join("sub").join("deep"), old).unwrap();
        let (o, _) = run_job(Job::Pack { sources: vec![src.join("sub")], archive: tmp.0.join("d.zip") }, Resolution::OverwriteAll);
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

        // 7z can't be changed.
        let seven = tmp.0.join("b.7z");
        let mut w = sevenz_rust2::ArchiveWriter::create(&seven).unwrap();
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file("x.txt"), Some(&b"x"[..])).unwrap();
        w.finish().unwrap();
        let ts = plan_transfer(&[target.join("a.txt")], &seven).unwrap();
        let (o, _) = run_job(Job::Copy(ts), Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
        let (o, _) = run_job(Job::Delete { paths: vec![seven.join("x.txt")], to_trash: false }, Resolution::Cancel);
        assert_eq!(o.errors.len(), 1);
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
