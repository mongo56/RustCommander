//! F4 on a file inside an archive. The file is extracted to its temp copy
//! (`vfs::extract_temp`) and opened in the editor. In a zip or tar the copy is
//! then watched (polled, since many editors return at once): each save is
//! put back into the archive by a `Job::Update`, which refuses to overwrite the file
//! if it changed in the archive meanwhile. Watching stops once the editor
//! closes (when it ran as its own process), when no tab shows the archive
//! any more, and on quit (after a last check). Copies from read-only
//! archives (7z, gz, archives inside archives) open with a note that
//! changes aren't saved.
//!
//! Shift+F4 in a zip or tar adds the new file empty (also a `Job::Update`, of an
//! empty temp file) and then edits it like F4.

use super::*;
use rc_core::ops::Stamp;
use std::process::Child;
use std::time::SystemTime;

/// How often the edited copies are checked.
const POLL: Duration = Duration::from_millis(700);
/// An editor process that ends sooner than this only handed the file to an
/// already running editor (or a launcher): keep watching after it.
const LAUNCHER_TIME: Duration = Duration::from_secs(5);
/// How often the temp copies are marked as used, so the startup cleanup of
/// other instances keeps them.
const KEEP_EVERY: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub enum EditMsg {
    /// The file `entry` was extracted (`Ok`: its temp copy and the stamp
    /// it had in the archive); `bool`: the archive can be changed.
    Extracted(PathBuf, bool, Result<(PathBuf, Stamp), String>),
    Tick,
}

/// Size and modification time of a temp copy.
type FileState = (u64, Option<SystemTime>);

fn file_state(path: &Path) -> Option<FileState> {
    std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()))
}

/// A file from a zip or tar being edited.
pub(super) struct ArchiveEdit {
    /// The file inside the archive.
    entry: PathBuf,
    archive: PathBuf,
    copy: PathBuf,
    /// The entry as last extracted or put back; an update needs it unchanged.
    stamp: Stamp,
    /// The copy as last extracted or put back.
    synced: Option<FileState>,
    /// The copy at the previous check: a change is put back once it's
    /// stable for one check (not while the editor is still writing).
    seen: Option<FileState>,
    /// The update job running, with the copy's state it's putting back.
    job: Option<(u64, Option<FileState>)>,
    /// The editor process (when started directly) and when.
    editor: Option<(Child, Instant)>,
    /// The editor process was closed: stop once the copy is in sync.
    closed: bool,
    /// The last save couldn't be put back: keep the copy (F4 reopens it
    /// rather than extracting again) while the archive is open.
    failed: bool,
    kept: Instant,
}

#[derive(Default)]
pub(super) struct ArchiveEdits {
    edits: Vec<ArchiveEdit>,
    /// A `Tick` is pending.
    ticking: bool,
    /// Shift+F4 in an archive: jobs adding a new empty file (job id, the
    /// file in the archive, the temp folder of the empty source), edited
    /// once added.
    creating: Vec<(u64, PathBuf, PathBuf)>,
}

fn name_of(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

impl App {
    /// F4 on `entry`, a file inside an archive.
    pub(super) fn edit_in_archive(&mut self, entry: PathBuf) -> Task<Message> {
        let writable = vfs::archive_writable(&entry) == Some(true);
        if let Some(i) = self.archive_edits.edits.iter().position(|e| e.entry == entry && e.copy.is_file()) {
            // Already being edited: open the copy again.
            let copy = self.archive_edits.edits[i].copy.clone();
            let child = self.launch_editor(&copy, &format!("Editing {}", name_of(&entry)));
            let edit = &mut self.archive_edits.edits[i];
            if let Ok(Some(child)) = child {
                edit.editor = Some((child, Instant::now()));
                edit.closed = false;
            }
            return Task::none();
        }
        self.status = Some((format!("Extracting {}…", name_of(&entry)), Tone::Info));
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                let path = entry.clone();
                std::thread::spawn(move || {
                    // The stamp first: if the file changes in between, the
                    // first save is refused rather than overwriting it.
                    let stamp = match writable {
                        true => ops::entry_stamp(&path).map_err(|e| e.to_string()),
                        false => Ok(None),
                    };
                    let result = stamp.and_then(|stamp| {
                        vfs::extract_temp(&path).map(|copy| (copy, stamp)).map_err(|e| e.to_string())
                    });
                    let _ = tx.send(result.map_err(|e| format!("Can't extract {}: {e}", path.display())));
                });
                let result = rx.await.unwrap_or_else(|_| Err("extract thread failed".into()));
                (entry, result)
            },
            move |(entry, result)| Message::ArchiveEdit(EditMsg::Extracted(entry, writable, result)),
        )
    }

    /// Shift+F4 inside an archive: `entry` (typed as `name`) is opened if it
    /// exists, else added as an empty file by a `Job::Update` (which refuses
    /// if one appears meanwhile) and opened once that's done.
    pub(super) fn new_in_archive(&mut self, entry: PathBuf, name: &str) -> Task<Message> {
        if vfs::archive_writable(&entry) != Some(true) {
            let msg = "Read-only archive (only zips and tars, not inside archives, can be changed): can't create files here";
            self.status = Some((msg.into(), Tone::Info));
            return Task::none();
        }
        match vfs::open(&entry).and_then(|v| v.stat(&entry)) {
            Ok(Some(e)) if e.is_dir() => {
                self.status = Some((format!("Can't create {name}: a folder with this name exists"), Tone::Error));
                return Task::none();
            }
            Ok(Some(_)) => return self.edit_in_archive(entry),
            Ok(None) => {}
            Err(e) => {
                self.status = Some((format!("Can't create {name}: {e}"), Tone::Error));
                return Task::none();
            }
        }
        // The empty source, in a `copy-*` folder (the startup cleanup
        // removes ones a crash left behind).
        let stage = vfs::temp_root().join(format!("copy-{}-new{}", std::process::id(), self.next_job_id));
        let src = stage.join(entry.file_name().unwrap_or_default());
        let made = std::fs::create_dir_all(&stage).and_then(|_| std::fs::File::create(&src).map(drop));
        if let Err(e) = made {
            let _ = std::fs::remove_dir_all(&stage);
            self.status = Some((format!("Can't create {name}: {e}"), Tone::Error));
            return Task::none();
        }
        let first = Path::new(name).components().next().map(|c| c.as_os_str().to_string_lossy().into_owned());
        let job = Job::Update { src, dst: entry.clone(), expect: None };
        let task = self.start_job(job, format!("Creating {name}"));
        let job = self.jobs.last_mut().expect("job just started");
        job.source_side = None;
        job.focus = first.map(|f| (self.active, f));
        self.archive_edits.creating.push((job.id, entry, stage));
        task
    }

    pub(super) fn archive_edit_update(&mut self, msg: EditMsg) -> Task<Message> {
        match msg {
            EditMsg::Extracted(entry, writable, result) => {
                let (copy, stamp) = match result {
                    Ok(r) => r,
                    Err(e) => {
                        self.status = Some((e, Tone::Error));
                        return Task::none();
                    }
                };
                let name = name_of(&entry);
                let Some((archive, _)) = vfs::split_archive(&entry) else { return Task::none() };
                if !writable {
                    let note = format!("Editing a copy of {name}: this archive is read-only, so changes aren't saved into it");
                    let _ = self.launch_editor(&copy, &note);
                    return Task::none();
                }
                let note = format!("Editing {name}: each save is put back into {}", name_of(&archive));
                let Ok(child) = self.launch_editor(&copy, &note) else { return Task::none() };
                let state = file_state(&copy);
                self.archive_edits.edits.push(ArchiveEdit {
                    entry,
                    archive,
                    copy,
                    stamp,
                    synced: state,
                    seen: state,
                    job: None,
                    editor: child.map(|c| (c, Instant::now())),
                    closed: false,
                    failed: false,
                    kept: Instant::now(),
                });
                self.archive_edit_tick_later()
            }
            EditMsg::Tick => {
                self.archive_edits.ticking = false;
                let tasks = self.archive_edit_check();
                Task::batch([tasks, self.archive_edit_tick_later()])
            }
        }
    }

    /// Starts the editor on `copy`, showing `note` (or the error). Returns
    /// its process when it was started directly (not through the default
    /// app); `Err` if it couldn't be started.
    fn launch_editor(&mut self, copy: &Path, note: &str) -> Result<Option<Child>, ()> {
        self.refresh_config();
        match crate::editor::launch(&self.config.editor, copy) {
            Ok(child) => {
                self.status = Some((note.to_owned(), Tone::Success));
                Ok(child)
            }
            Err(e) => {
                self.status = Some((format!("Can't start the editor: {e}"), Tone::Error));
                Err(())
            }
        }
    }

    fn archive_edit_tick_later(&mut self) -> Task<Message> {
        if self.archive_edits.ticking || self.archive_edits.edits.is_empty() {
            return Task::none();
        }
        self.archive_edits.ticking = true;
        delayed(POLL, Message::ArchiveEdit(EditMsg::Tick))
    }

    /// Checks every edited copy: puts back saved changes, stops watching
    /// the finished ones.
    fn archive_edit_check(&mut self) -> Task<Message> {
        let open: Vec<PathBuf> = self
            .sides
            .iter()
            .flat_map(|s| s.tabs.iter())
            .filter_map(|t| vfs::split_archive(&t.path).map(|(file, _)| file))
            .collect();
        let mut tasks = Vec::new();
        let mut i = 0;
        while i < self.archive_edits.edits.len() {
            let edit = &mut self.archive_edits.edits[i];
            if edit.kept.elapsed() >= KEEP_EVERY {
                edit.kept = Instant::now();
                let _ = vfs::keep_temp(&edit.copy);
            }
            if let Some((child, started)) = &mut edit.editor {
                if !matches!(child.try_wait(), Ok(None)) {
                    edit.closed = started.elapsed() >= LAUNCHER_TIME;
                    edit.editor = None;
                }
            }
            if edit.job.is_some() {
                i += 1;
                continue;
            }
            let state = file_state(&edit.copy);
            if state.is_some() && state != edit.synced {
                if state == edit.seen {
                    let job = Job::Update { src: edit.copy.clone(), dst: edit.entry.clone(), expect: edit.stamp };
                    let title = format!("Saving {} into {}", name_of(&edit.entry), name_of(&edit.archive));
                    tasks.push(self.start_job(job, title));
                    let job = self.jobs.last_mut().expect("job just started");
                    job.source_side = None;
                    self.archive_edits.edits[i].job = Some((job.id, state));
                } else {
                    edit.seen = state;
                }
                i += 1;
                continue;
            }
            let gone = state.is_none() || !open.iter().any(|a| ops::same_path(a, &edit.archive));
            if (edit.closed && !edit.failed) || gone {
                let edit = self.archive_edits.edits.remove(i);
                if self.status.is_none() {
                    let why = match edit.closed {
                        true => "the editor was closed",
                        false => "the archive isn't open any more",
                    };
                    self.status = Some((format!("Stopped watching {} ({why})", name_of(&edit.entry)), Tone::Info));
                }
                continue;
            }
            i += 1;
        }
        Task::batch(tasks)
    }

    /// A job finished: if it put back an edited copy, notes the new state;
    /// if it added a new file (Shift+F4), opens that.
    pub(super) fn archive_edit_job_done(&mut self, id: u64, outcome: &Outcome) -> Task<Message> {
        if let Some(i) = self.archive_edits.creating.iter().position(|(j, ..)| *j == id) {
            let (_, entry, stage) = self.archive_edits.creating.remove(i);
            let _ = std::fs::remove_dir_all(&stage);
            if outcome.errors.is_empty() && !outcome.cancelled {
                return self.edit_in_archive(entry);
            }
            return Task::none();
        }
        let Some(edit) = self.archive_edits.edits.iter_mut().find(|e| e.job.is_some_and(|(j, _)| j == id)) else {
            return Task::none();
        };
        let (_, state) = edit.job.take().expect("matched a job");
        // Not retried until the copy changes again.
        edit.synced = state;
        let (name, archive) = (name_of(&edit.entry), name_of(&edit.archive));
        edit.failed = !outcome.errors.is_empty() || outcome.cancelled;
        if let Some((_, err)) = outcome.errors.first() {
            let msg = format!("{name} not saved into {archive}: {err}. Your edited copy: {}", edit.copy.display());
            self.status = Some((msg, Tone::Error));
        } else if outcome.cancelled {
            self.status = Some((format!("{name} not saved into {archive}: cancelled"), Tone::Info));
        } else {
            match ops::entry_stamp(&edit.entry) {
                Ok(stamp) => edit.stamp = stamp,
                Err(e) => eprintln!("can't read {}: {e}", edit.entry.display()),
            }
            self.status = Some((format!("Saved {name} into {archive}"), Tone::Success));
        }
        Task::none()
    }

    /// On quit: puts back copies saved since the last check (waiting for
    /// it), so a save just before quitting isn't lost.
    pub(super) fn archive_edits_flush(&mut self) {
        struct NoAsk;
        impl ops::Reporter for NoAsk {
            fn progress(&mut self, _: &Progress) {}
            fn conflict(&mut self, _: Conflict) -> Resolution {
                Resolution::Cancel
            }
        }
        for edit in std::mem::take(&mut self.archive_edits.edits) {
            let state = file_state(&edit.copy);
            if edit.job.is_some() || state.is_none() || state == edit.synced {
                continue;
            }
            let job = Job::Update { src: edit.copy.clone(), dst: edit.entry.clone(), expect: edit.stamp };
            let outcome = ops::run(&job, &ops::Control::default(), &mut NoAsk);
            if let Some((path, err)) = outcome.errors.first() {
                eprintln!("{}: {err}", path.display());
            }
        }
    }
}
