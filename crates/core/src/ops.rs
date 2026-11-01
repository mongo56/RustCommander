//! File operations (copy, move, delete) as blocking jobs meant to run on a
//! worker thread. The UI talks to a running job through [`JobHandle`] and
//! receives [`JobEvent`]s through a callback.

use chrono::{DateTime, Local};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

const BUFFER_SIZE: usize = 1024 * 1024;
const REPORT_INTERVAL: Duration = Duration::from_millis(50);

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
    Delete { paths: Vec<PathBuf>, to_trash: bool },
}

impl Job {
    /// Past-tense verb for summaries: "Copied", "Moved", ...
    pub fn done_verb(&self) -> &'static str {
        match self {
            Job::Copy(_) => "Copied",
            Job::Move(_) => "Moved",
            Job::Delete { to_trash: true, .. } => "Recycled",
            Job::Delete { .. } => "Deleted",
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Job::Copy(_) => "Copying",
            Job::Move(_) => "Moving",
            Job::Delete { to_trash: true, .. } => "Recycling",
            Job::Delete { .. } => "Deleting",
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
    let into_dir = sources.len() > 1 || ends_with_sep || target.is_dir();

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
    };

    match job {
        Job::Copy(transfers) | Job::Move(transfers) => {
            let sizes: Vec<(u64, u64)> = transfers.iter().map(|t| measure(&t.src)).collect();
            for (bytes, items) in &sizes {
                runner.progress.total_bytes += bytes;
                runner.progress.total_items += items;
            }
            runner.report(true);
            let is_move = matches!(job, Job::Move(_));
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
                if crate::vfs::is_virtual(&t.src) {
                    if is_move {
                        runner.fail_msg(&t.src, "Archives are read-only: can't move out of them");
                    } else {
                        runner.extract(t);
                    }
                } else if is_move {
                    runner.move_item(t, size);
                } else {
                    runner.copy(&t.src, &t.dst, false);
                }
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
        self.outcome.errors.push((path.to_path_buf(), msg.into()));
        false
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
            match self.resolve(src, (meta.len(), meta.modified().ok()), dst, &dst_meta) {
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
            match self.resolve(src, (0, meta.modified().ok()), dst, &dst_meta) {
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
        dst_meta: &fs::Metadata,
    ) -> Resolution {
        let dst_modified = dst_meta.modified().ok();
        let target_older = matches!((src_modified, dst_modified), (Some(s), Some(d)) if d < s);

        let answer = match self.sticky {
            Some(sticky) => sticky,
            None => {
                let conflict = Conflict {
                    src: src.to_path_buf(),
                    dst: dst.to_path_buf(),
                    src_size,
                    dst_size: dst_meta.len(),
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

    fn pump(&mut self, input: &mut dyn Read, output: &mut File) -> io::Result<bool> {
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
                        Ok(()) => self.item_done(0),
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
                match self.resolve(&item.path, (item.size, item.modified.map(Into::into)), &dst, &dst_meta) {
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

        // Single file under a new name; moving out is refused.
        let ts = plan_transfer(&[zip.join("a.txt")], &out.join("renamed.txt")).unwrap();
        let (o, _) = run_job(Job::Copy(ts.clone()), Resolution::Skip);
        assert!(o.errors.is_empty());
        assert_eq!(fs::read(out.join("renamed.txt")).unwrap(), b"hello zip");
        let (o, _) = run_job(Job::Move(ts), Resolution::Skip);
        assert_eq!(o.errors.len(), 1);
        assert!(zip.is_file());
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
