//! SHA-256 checksums (`cm_CalcChecksum`, `cm_VerifyChecksum`): a dialog that
//! hashes the selected files (or checks the files a `.sha256` file lists) on
//! a worker thread with progress. Esc cancels. The result of a calculation
//! can be copied or saved as `<name>.sha256` next to the files; a check
//! shows OK / MISMATCH / missing per file. The core is `rc_core::checksum`.

use super::*;
use iced::widget::text::Wrapping;
use iced::widget::{column, container, progress_bar, row, scrollable, text, Space};
use iced::{Element, Length};
use rc_core::checksum as sum;

#[derive(Debug, Clone)]
pub enum ChecksumMsg {
    /// Run `seq`: now in file `file` (0-based, named `name`) with `bytes` of all files done.
    Progress { seq: u64, file: usize, name: String, bytes: u64 },
    /// Run `seq` finished (`None`: cancelled).
    Done(u64, Option<Vec<Outcome>>),
    /// Button `n` of the dialog (see `buttons`).
    Button(usize),
}

/// What hashing one file gave.
#[derive(Debug, Clone)]
pub enum Hashed {
    Hash(String),
    Missing,
    Error(String),
}

/// One file of a run: its name, the digest the `.sha256` file expects (a check) and what was found.
#[derive(Debug, Clone)]
pub struct Outcome {
    name: String,
    expected: Option<String>,
    found: Hashed,
}

impl Outcome {
    fn verdict(&self) -> Verdict {
        match (&self.found, &self.expected) {
            (Hashed::Missing, _) => Verdict::Missing,
            (Hashed::Error(e), _) => Verdict::Error(e.clone()),
            (Hashed::Hash(h), Some(want)) if h == want => Verdict::Ok,
            (Hashed::Hash(_), Some(_)) => Verdict::Mismatch,
            (Hashed::Hash(_), None) => Verdict::Ok,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Ok,
    Mismatch,
    Missing,
    Error(String),
}

/// One file to hash.
struct Job {
    name: String,
    path: PathBuf,
    expected: Option<String>,
}

/// Sets the flag when dropped, so the worker stops when its dialog goes.
#[derive(Debug)]
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
enum Stage {
    Running { seq: u64, file: usize, name: String, bytes: u64, total: u64, _cancel: CancelOnDrop },
    /// A calculation's result; `confirm`: the `.sha256` file exists and the next Save overwrites it.
    Calculated { results: Vec<Outcome>, confirm: bool },
    Verified(Vec<Outcome>),
}

#[derive(Debug)]
pub(super) struct ChecksumDialog {
    verify: bool,
    /// The side the files are from, and their folder.
    side: usize,
    base: PathBuf,
    /// Number of files of the run.
    count: usize,
    stage: Stage,
    /// Focused button (`None` = the last, default one).
    focus: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Close,
    Copy,
    Save,
}

impl ChecksumDialog {
    fn buttons(&self) -> Vec<(&'static str, Action)> {
        match &self.stage {
            Stage::Running { .. } => vec![("Cancel", Action::Close)],
            Stage::Calculated { confirm, .. } => vec![
                ("Copy", Action::Copy),
                (if *confirm { "Overwrite .sha256" } else { "Save .sha256" }, Action::Save),
                ("Close", Action::Close),
            ],
            Stage::Verified(_) => vec![("Close", Action::Close)],
        }
    }
}

/// Hashes `jobs` on a worker thread, reporting through the returned stream.
fn start(seq: u64, jobs: Vec<Job>, cancel: Arc<AtomicBool>) -> iced::futures::channel::mpsc::UnboundedReceiver<ChecksumMsg> {
    let (tx, rx) = iced::futures::channel::mpsc::unbounded();
    std::thread::spawn(move || {
        let mut out = Vec::with_capacity(jobs.len());
        let mut before = 0u64;
        for (i, job) in jobs.iter().enumerate() {
            let size = std::fs::metadata(&job.path).map_or(0, |m| m.len());
            let _ = tx.unbounded_send(ChecksumMsg::Progress { seq, file: i, name: job.name.clone(), bytes: before });
            let mut last = 0u64;
            let result = sum::hash_file(&job.path, &cancel, |n| {
                if n - last >= 1024 * 1024 {
                    last = n;
                    let _ = tx.unbounded_send(ChecksumMsg::Progress { seq, file: i, name: job.name.clone(), bytes: before + n });
                }
            });
            let found = match result {
                Ok(Some(hash)) => Hashed::Hash(hash),
                Ok(None) => {
                    let _ = tx.unbounded_send(ChecksumMsg::Done(seq, None));
                    return;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Hashed::Missing,
                Err(e) => Hashed::Error(e.to_string()),
            };
            before += size;
            out.push(Outcome { name: job.name.clone(), expected: job.expected.clone(), found });
        }
        let _ = tx.unbounded_send(ChecksumMsg::Done(seq, Some(out)));
    });
    rx
}

impl App {
    fn checksum_dialog(&mut self) -> Option<&mut ChecksumDialog> {
        match &mut self.dialog {
            Some(Dialog::Checksum(d)) => Some(d),
            _ => None,
        }
    }

    /// Opens the dialog and starts hashing `jobs`.
    fn checksum_run(&mut self, verify: bool, side: usize, base: PathBuf, jobs: Vec<Job>) -> Task<Message> {
        self.leave_edits();
        let seq = self.next_job_id;
        self.next_job_id += 1;
        let total = jobs.iter().map(|j| std::fs::metadata(&j.path).map_or(0, |m| m.len())).sum();
        let count = jobs.len();
        let cancel = Arc::new(AtomicBool::new(false));
        let name = jobs.first().map(|j| j.name.clone()).unwrap_or_default();
        let rx = start(seq, jobs, cancel.clone());
        let stage = Stage::Running { seq, file: 0, name, bytes: 0, total, _cancel: CancelOnDrop(cancel) };
        self.dialog = Some(Dialog::Checksum(ChecksumDialog { verify, side, base, count, stage, focus: None }));
        self.dialog_focus = None;
        Task::run(rx, Message::Checksum)
    }

    /// `cm_CalcChecksum`: SHA-256 of the selected files (or the cursor file).
    pub(super) fn calc_checksum(&mut self) -> Task<Message> {
        let side = self.active;
        let panel = self.panel(side);
        if vfs::is_virtual(&panel.path) {
            self.info("Checksums work on local files only, not in an archive or on a server");
            return Task::none();
        }
        let base = panel.path.clone();
        let jobs: Vec<Job> = panel
            .targets()
            .into_iter()
            .filter(|e| !e.is_dir())
            .map(|e| Job { name: e.name.clone(), path: e.path.clone(), expected: None })
            .collect();
        if jobs.is_empty() {
            self.info("No files to checksum (folders are skipped)");
            return Task::none();
        }
        self.checksum_run(false, side, base, jobs)
    }

    /// `cm_VerifyChecksum`: checks the files listed in the `.sha256` file under the cursor.
    pub(super) fn verify_checksum(&mut self) -> Task<Message> {
        let side = self.active;
        let panel = self.panel(side);
        if vfs::is_virtual(&panel.path) {
            self.info("Checksums work on local files only, not in an archive or on a server");
            return Task::none();
        }
        let Some(entry) = panel.current().filter(|e| !e.is_dir() && e.name.to_lowercase().ends_with(".sha256")) else {
            self.info("Put the cursor on a .sha256 file to verify it");
            return Task::none();
        };
        let (file, name, base) = (entry.path.clone(), entry.name.clone(), panel.path.clone());
        let items = match std::fs::read(&file) {
            Ok(bytes) => sum::parse(&String::from_utf8_lossy(&bytes)),
            Err(e) => Err(format!("Can't read it: {e}")),
        };
        match items {
            Err(e) => self.error(format!("{name}: {e}")),
            Ok(items) if items.is_empty() => self.info(format!("{name} lists no files")),
            Ok(items) => {
                let jobs = items
                    .into_iter()
                    .map(|i| Job { path: base.join(&i.name), name: i.name, expected: Some(i.hash) })
                    .collect();
                return self.checksum_run(true, side, base, jobs);
            }
        }
        Task::none()
    }

    pub(super) fn checksum_msg(&mut self, msg: ChecksumMsg) -> Task<Message> {
        match msg {
            ChecksumMsg::Progress { seq, file, name, bytes } => {
                if let Some(ChecksumDialog { stage: Stage::Running { seq: s, file: f, name: n, bytes: b, .. }, .. }) =
                    self.checksum_dialog()
                {
                    if *s == seq {
                        (*f, *n, *b) = (file, name, bytes);
                    }
                }
            }
            ChecksumMsg::Done(seq, results) => {
                let Some(d) = self.checksum_dialog() else { return Task::none() };
                if !matches!(d.stage, Stage::Running { seq: s, .. } if s == seq) {
                    return Task::none();
                }
                d.focus = None;
                match results {
                    None => {
                        self.dialog = None;
                        self.info("Checksum cancelled");
                    }
                    Some(results) if d.verify => d.stage = Stage::Verified(results),
                    Some(results) => d.stage = Stage::Calculated { results, confirm: false },
                }
            }
            ChecksumMsg::Button(i) => {
                let Some(d) = self.checksum_dialog() else { return Task::none() };
                if let Some(&(_, action)) = d.buttons().get(i) {
                    return self.checksum_action(action);
                }
            }
        }
        Task::none()
    }

    fn checksum_action(&mut self, action: Action) -> Task<Message> {
        let Some(d) = self.checksum_dialog() else { return Task::none() };
        match action {
            Action::Close => {
                // Dropping a running stage cancels the worker.
                let running = matches!(d.stage, Stage::Running { .. });
                self.dialog = None;
                if running {
                    self.info("Checksum cancelled");
                }
            }
            Action::Copy => {
                if let Stage::Calculated { results, .. } = &d.stage {
                    let mut lines = lines_of(results);
                    lines.push('\n');
                    let n = results.len();
                    self.success(format!("Copied {n} checksum{} to the clipboard", if n == 1 { "" } else { "s" }));
                    return iced::clipboard::write(lines);
                }
            }
            Action::Save => {
                let Stage::Calculated { results, confirm } = &mut d.stage else { return Task::none() };
                let stem = if results.len() == 1 {
                    Some(results[0].name.clone())
                } else {
                    d.base.file_name().map(|n| n.to_string_lossy().into_owned())
                };
                let name = format!("{}.sha256", stem.unwrap_or_else(|| "checksums".into()));
                let path = d.base.join(&name);
                if path.exists() && !*confirm {
                    *confirm = true;
                    d.focus = None;
                    self.info(format!("{name} exists: press Overwrite to replace it"));
                    return Task::none();
                }
                let mut text = lines_of(results);
                text.push('\n');
                let (side, base) = (d.side, d.base.clone());
                match std::fs::write(&path, text) {
                    Ok(()) => {
                        self.dialog = None;
                        self.success(format!("Saved {name}"));
                        if self.panel(side).path == base {
                            let panel = self.panel_mut(side);
                            if panel.reload().is_ok() {
                                panel.focus(&name);
                            }
                        }
                    }
                    Err(e) => self.error(format!("Can't save {name}: {e}")),
                }
            }
        }
        Task::none()
    }

    /// Keys while the dialog is open: Esc closes (cancels a run),
    /// Left/Right/Tab move the button focus, Enter presses it.
    pub(super) fn checksum_key(&mut self, named: Option<Named>, shift: bool) -> Task<Message> {
        let Some(d) = self.checksum_dialog() else { return Task::none() };
        let count = d.buttons().len();
        let focused = d.focus.unwrap_or(count - 1);
        let back = match named {
            Some(Named::Tab) => Some(shift),
            Some(Named::ArrowLeft) => Some(true),
            Some(Named::ArrowRight) => Some(false),
            _ => None,
        };
        if let Some(back) = back {
            d.focus = Some(button_focus::step(Some(focused), count, back));
            return Task::none();
        }
        match named {
            Some(Named::Escape) => self.checksum_action(Action::Close),
            Some(Named::Enter) => self.checksum_msg(ChecksumMsg::Button(focused)),
            _ => Task::none(),
        }
    }

    pub(super) fn checksum_view<'a>(&'a self, d: &'a ChecksumDialog) -> Element<'a, Message> {
        let c = theme::c();
        let mono = |s: String| text(s).size(12).font(iced::Font::MONOSPACE);
        let list = |rows: Vec<Element<'a, Message>>| {
            container(scrollable(column(rows).spacing(3).padding([0, 12])).height(Length::Shrink))
                .max_height(300.0)
                .width(Length::Fill)
        };
        let (title, body): (String, Element<'a, Message>) = match &d.stage {
            Stage::Running { file, name, bytes, total, .. } => {
                let fraction = if *total == 0 { 0.0 } else { *bytes as f32 / *total as f32 };
                (
                    if d.verify { "Verifying checksums".into() } else { "Calculating SHA-256".into() },
                    column![
                        text(format!("File {} of {}: {name}", file + 1, d.count)).size(13).wrapping(Wrapping::None),
                        progress_bar(0.0..=1.0, fraction).height(Length::Fixed(6.0)).style(theme::progress),
                        text(format!("{} of {}", rc_core::format::size(*bytes), rc_core::format::size(*total)))
                            .size(12)
                            .color(c.muted),
                    ]
                    .spacing(8)
                    .into(),
                )
            }
            Stage::Calculated { results, .. } => {
                let rows = results
                    .iter()
                    .map(|o| match &o.found {
                        Hashed::Hash(h) => mono(format!("{h}  {}", o.name)).into(),
                        Hashed::Missing => mono(format!("missing  {}", o.name)).color(c.error).into(),
                        Hashed::Error(e) => mono(format!("{e}  {}", o.name)).color(c.error).into(),
                    })
                    .collect();
                ("SHA-256".into(), list(rows).into())
            }
            Stage::Verified(results) => {
                let (mut ok, mut bad, mut missing) = (0, 0, 0);
                let rows = results
                    .iter()
                    .map(|o| {
                        let v = o.verdict();
                        let (label, color) = match &v {
                            Verdict::Ok => ("OK", c.success),
                            Verdict::Mismatch => ("MISMATCH", c.error),
                            Verdict::Missing => ("missing", c.error),
                            Verdict::Error(_) => ("error", c.error),
                        };
                        let detail = if let Verdict::Error(e) = &v { format!("  ({e})") } else { String::new() };
                        match v {
                            Verdict::Ok => ok += 1,
                            Verdict::Missing => missing += 1,
                            Verdict::Mismatch | Verdict::Error(_) => bad += 1,
                        }
                        mono(format!("{label:<9} {}{detail}", o.name)).color(color).into()
                    })
                    .collect();
                let summary = format!("{ok} OK, {bad} mismatch, {missing} missing");
                (
                    "Verify checksums".into(),
                    column![list(rows), text(summary).size(13).color(if bad + missing == 0 { c.success } else { c.error })]
                        .spacing(10)
                        .into(),
                )
            }
        };
        let buttons = d.buttons();
        let focus = d.focus.unwrap_or(buttons.len() - 1);
        let last = buttons.len() - 1;
        let mut bar = row![Space::with_width(Length::Fill)].spacing(8);
        for (i, (label, _)) in buttons.into_iter().enumerate() {
            bar = bar.push(view::dialog_button(label, Message::Checksum(ChecksumMsg::Button(i)), i == last, i == focus));
        }
        let content = column![text(title).size(16), body, bar].spacing(14);
        container(content).padding(20).width(Length::Fixed(720.0)).style(theme::dialog).into()
    }
}

/// The lines of a calculation, `<hex> *<name>`, without errors.
fn lines_of(results: &[Outcome]) -> String {
    let lines: Vec<String> = results
        .iter()
        .filter_map(|o| match &o.found {
            Hashed::Hash(h) => Some(sum::format_line(h, &o.name)),
            _ => None,
        })
        .collect();
    lines.join("\n")
}
