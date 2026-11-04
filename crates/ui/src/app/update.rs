//! Check for updates (`cm_CheckForUpdates`): a dialog that asks GitHub for
//! the latest release on a worker (`crate::update`), offers a newer one,
//! downloads its installer with progress, then starts it and quits so the
//! files can be replaced. Portable copies (no uninstaller next to the exe)
//! get the release page instead. With `check_updates_at_startup` a quiet
//! check runs at startup and a newer, not skipped version is offered in the
//! status line (a click opens the dialog).

use super::*;
use crate::update::{self as up, Check, Release};
use iced::widget::{column, container, mouse_area, progress_bar, row, text, Space};
use iced::{Element, Length};

#[derive(Debug, Clone)]
pub enum UpdateMsg {
    /// A check finished; `manual`: run from the command (else at startup).
    Checked { manual: bool, result: Result<Check, String> },
    /// Button `n` of the dialog (see `buttons`).
    Button(usize),
    /// Download `seq` has this many bytes.
    Progress(u64, u64),
    /// Download `seq` finished (`None`: cancelled).
    Downloaded(u64, Result<Option<PathBuf>, String>),
    /// The status-line notice was clicked.
    OpenNotice,
}

/// Sets the flag when dropped, so a download stops when its dialog goes.
#[derive(Debug)]
pub(super) struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(super) enum Stage {
    Checking,
    NoReleases,
    UpToDate(Release),
    Failed(String),
    /// A newer release; `installer`: this copy was installed by the setup
    /// program and the release has one (else: open the release page).
    Available { release: Release, installer: bool },
    Downloading { release: Release, seq: u64, received: u64, _cancel: CancelOnDrop },
}

#[derive(Debug)]
pub(super) struct UpdateDialog {
    pub stage: Stage,
    /// Focused button (`None` = the last, default one).
    pub focus: Option<usize>,
}

/// What a button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Close,
    Skip,
    Install,
    OpenPage,
    CancelDownload,
}

impl UpdateDialog {
    fn new(stage: Stage) -> Self {
        Self { stage, focus: None }
    }

    /// The buttons, left to right; the last is the default.
    fn buttons(&self) -> Vec<(&'static str, Action)> {
        match &self.stage {
            Stage::Checking => vec![("Cancel", Action::Close)],
            Stage::NoReleases | Stage::UpToDate(_) | Stage::Failed(_) => vec![("OK", Action::Close)],
            Stage::Available { installer: true, .. } => {
                vec![("Skip this version", Action::Skip), ("Later", Action::Close), ("Download & install", Action::Install)]
            }
            Stage::Available { installer: false, .. } => {
                vec![("Skip this version", Action::Skip), ("Later", Action::Close), ("Open release page", Action::OpenPage)]
            }
            Stage::Downloading { .. } => vec![("Cancel", Action::CancelDownload)],
        }
    }
}

/// Runs a check on a worker thread.
fn check_task(manual: bool) -> Task<Message> {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(up::check());
    });
    Task::perform(rx, move |r| {
        let result = r.unwrap_or_else(|_| Err("The update check stopped".into()));
        Message::Update(UpdateMsg::Checked { manual, result })
    })
}

impl App {
    /// `cm_CheckForUpdates`: opens the dialog and checks.
    pub(super) fn check_for_updates(&mut self) -> Task<Message> {
        self.leave_edits();
        self.dialog = Some(Dialog::Update(UpdateDialog::new(Stage::Checking)));
        check_task(true)
    }

    /// The quiet check at startup (setting `check_updates_at_startup`).
    pub(super) fn startup_update_check(&self) -> Task<Message> {
        if self.config.check_updates_at_startup {
            check_task(false)
        } else {
            Task::none()
        }
    }

    fn update_dialog(&mut self) -> Option<&mut UpdateDialog> {
        match &mut self.dialog {
            Some(Dialog::Update(d)) => Some(d),
            _ => None,
        }
    }

    fn offer_stage(release: Release) -> Stage {
        let installer = release.setup.is_some() && up::is_installed();
        Stage::Available { release, installer }
    }

    pub(super) fn update_msg(&mut self, msg: UpdateMsg) -> Task<Message> {
        match msg {
            UpdateMsg::Checked { manual: false, result } => {
                // Startup: only a newer, not skipped release is worth a word.
                if let Ok(Check::Latest(release)) = result {
                    let version = release.version.to_string();
                    if release.is_newer() && self.skipped_update.as_deref() != Some(version.as_str()) {
                        // Shown whenever the status line has nothing else to say.
                        let notice = format!("RustCommander {version} is available \u{2013} click here to update");
                        self.update_notice = Some((notice, release));
                    }
                }
            }
            UpdateMsg::Checked { manual: true, result } => {
                let Some(d) = self.update_dialog() else { return Task::none() };
                if !matches!(d.stage, Stage::Checking) {
                    return Task::none();
                }
                d.focus = None;
                d.stage = match result {
                    Err(e) => Stage::Failed(e),
                    Ok(Check::NoReleases) => Stage::NoReleases,
                    Ok(Check::Latest(release)) if release.is_newer() => Self::offer_stage(release),
                    Ok(Check::Latest(release)) => Stage::UpToDate(release),
                };
            }
            UpdateMsg::OpenNotice => {
                if let Some((_, release)) = self.update_notice.clone() {
                    self.leave_edits();
                    self.dialog = Some(Dialog::Update(UpdateDialog::new(Self::offer_stage(release))));
                }
            }
            UpdateMsg::Button(i) => {
                let Some(d) = self.update_dialog() else { return Task::none() };
                if let Some(&(_, action)) = d.buttons().get(i) {
                    return self.update_action(action);
                }
            }
            UpdateMsg::Progress(seq, bytes) => {
                if let Some(UpdateDialog { stage: Stage::Downloading { seq: s, received, .. }, .. }) = self.update_dialog() {
                    if *s == seq {
                        *received = bytes;
                    }
                }
            }
            UpdateMsg::Downloaded(seq, result) => return self.update_downloaded(seq, result),
        }
        Task::none()
    }

    fn update_action(&mut self, action: Action) -> Task<Message> {
        let jobs_running = !self.jobs.is_empty();
        let Some(d) = self.update_dialog() else { return Task::none() };
        d.focus = None;
        match action {
            Action::Close | Action::CancelDownload => {
                // Dropping a download's stage cancels it.
                self.dialog = None;
                if action == Action::CancelDownload {
                    self.info("Update download cancelled");
                }
            }
            Action::Skip => {
                if let Stage::Available { release, .. } = &d.stage {
                    let version = release.version.to_string();
                    self.info(format!("Version {version} skipped; Check for updates still offers it"));
                    self.skipped_update = Some(version);
                    self.update_notice = None;
                    self.dialog = None;
                    self.save_session();
                }
            }
            Action::OpenPage => {
                if let Stage::Available { release, .. } = &d.stage {
                    let page = release.page.clone();
                    self.dialog = None;
                    self.status = Some(match rc_platform::open_default(Path::new(&page)) {
                        Ok(()) => (format!("Opened {page}"), Tone::Success),
                        Err(e) => (format!("Can't open {page}: {e}"), Tone::Error),
                    });
                }
            }
            Action::Install => {
                let Stage::Available { release, .. } = &d.stage else { return Task::none() };
                let release = release.clone();
                let Some(asset) = release.setup.clone() else { return Task::none() };
                if jobs_running {
                    d.stage = Stage::Failed("File jobs are still running. Try again when they are done.".into());
                    return Task::none();
                }
                let seq = self.next_job_id;
                self.next_job_id += 1;
                let cancel = Arc::new(AtomicBool::new(false));
                let flag = cancel.clone();
                let (tx, rx) = iced::futures::channel::mpsc::unbounded();
                std::thread::spawn(move || {
                    let mut last = 0u64;
                    let progress_tx = tx.clone();
                    let result = up::download(&asset, &up::download_dir(), &flag, |n| {
                        if n - last >= 128 * 1024 || n == asset.size {
                            last = n;
                            let _ = progress_tx.unbounded_send(UpdateMsg::Progress(seq, n));
                        }
                    });
                    let _ = tx.unbounded_send(UpdateMsg::Downloaded(seq, result));
                });
                if let Some(d) = self.update_dialog() {
                    d.stage = Stage::Downloading { release, seq, received: 0, _cancel: CancelOnDrop(cancel) };
                }
                return Task::run(rx, Message::Update);
            }
        }
        Task::none()
    }

    /// A download ended: start the installer and quit, or report.
    fn update_downloaded(&mut self, seq: u64, result: Result<Option<PathBuf>, String>) -> Task<Message> {
        let jobs_running = !self.jobs.is_empty();
        let current = matches!(
            self.update_dialog(),
            Some(UpdateDialog { stage: Stage::Downloading { seq: s, .. }, .. }) if *s == seq
        );
        if !current {
            // Cancelled meanwhile: don't leave the file behind.
            if let Ok(Some(path)) = result {
                let _ = std::fs::remove_file(path);
            }
            return Task::none();
        }
        let Some(d) = self.update_dialog() else { return Task::none() };
        d.focus = None;
        match result {
            Ok(None) => self.dialog = None,
            Err(e) => d.stage = Stage::Failed(e),
            Ok(Some(_)) if jobs_running => {
                d.stage = Stage::Failed("File jobs are still running. Try again when they are done.".into());
            }
            Ok(Some(path)) => match rc_platform::run_program(&path, up::INSTALLER_ARGS) {
                Ok(()) => {
                    self.dialog = None;
                    self.success("Installing the update\u{2026}");
                    return self.quit();
                }
                Err(e) => d.stage = Stage::Failed(format!("Can't start the installer: {e}")),
            },
        }
        Task::none()
    }

    /// Keys while the dialog is open: Esc closes (cancels a download),
    /// Left/Right/Tab move the button focus, Enter presses it.
    pub(super) fn update_key(&mut self, named: Option<Named>, shift: bool) -> Task<Message> {
        let Some(d) = self.update_dialog() else { return Task::none() };
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
            Some(Named::Escape) => {
                let cancel = if matches!(d.stage, Stage::Downloading { .. }) { Action::CancelDownload } else { Action::Close };
                self.update_action(cancel)
            }
            Some(Named::Enter) => self.update_msg(UpdateMsg::Button(focused)),
            _ => Task::none(),
        }
    }

    pub(super) fn update_view<'a>(&'a self, d: &'a UpdateDialog) -> Element<'a, Message> {
        let current = up::Version::current();
        let muted = theme::c().muted;
        let (title, body): (String, Element<'a, Message>) = match &d.stage {
            Stage::Checking => ("Check for updates".into(), text("Asking GitHub for the latest release\u{2026}").size(13).color(muted).into()),
            Stage::NoReleases => (
                "Check for updates".into(),
                text(format!("No release has been published yet. This is version {current}.")).size(13).color(muted).into(),
            ),
            Stage::UpToDate(release) => (
                "Up to date".into(),
                text(if release.version == current {
                    format!("RustCommander {current} is the latest release.")
                } else {
                    format!("This is RustCommander {current}; the latest release is {}.", release.version)
                })
                .size(13)
                .color(muted)
                .into(),
            ),
            Stage::Failed(e) => ("Check for updates".into(), text(e.as_str()).size(13).color(theme::c().error).into()),
            Stage::Available { release, installer } => {
                let how = if *installer {
                    "Download & install fetches the installer, closes RustCommander and updates it; it starts again when done.".to_owned()
                } else if release.setup.is_none() {
                    "This release has no installer; get it from the release page.".to_owned()
                } else {
                    "This copy wasn't installed by the setup program (portable or development build): get the new version from the release page.".to_owned()
                };
                let mut col = column![text(format!("RustCommander {} is available (you have {current}).", release.version)).size(13)]
                    .spacing(10);
                if !release.notes.is_empty() {
                    col = col.push(
                        container(text(release.notes.as_str()).size(12))
                            .padding([8, 10])
                            .width(Length::Fill)
                            .style(crate::theme::filled(theme::c().header)),
                    );
                }
                ("Update available".into(), col.push(text(how).size(12).color(muted)).into())
            }
            Stage::Downloading { release, received, .. } => {
                let total = release.setup.as_ref().map_or(0, |a| a.size);
                let fraction = if total == 0 { 0.0 } else { *received as f32 / total as f32 };
                (
                    format!("Downloading RustCommander {}", release.version),
                    column![
                        progress_bar(0.0..=1.0, fraction).height(Length::Fixed(6.0)).style(theme::progress),
                        text(format!(
                            "{} of {}",
                            rc_core::format::size(*received),
                            rc_core::format::size(total)
                        ))
                        .size(12)
                        .color(muted),
                    ]
                    .spacing(8)
                    .into(),
                )
            }
        };
        let buttons = d.buttons();
        let focus = d.focus.unwrap_or(buttons.len() - 1);
        let last = buttons.len() - 1;
        let mut bar = row![Space::with_width(Length::Fill)].spacing(8);
        for (i, (label, _)) in buttons.into_iter().enumerate() {
            bar = bar.push(view::dialog_button(label, Message::Update(UpdateMsg::Button(i)), i == last, i == focus));
        }
        let content = column![text(title).size(16), body, bar].spacing(14);
        container(content).padding(20).width(Length::Fixed(560.0)).style(theme::dialog).into()
    }

    /// The status line's message: the status, else the update notice.
    pub(super) fn status_line(&self) -> Option<(String, Tone)> {
        self.status.clone().or_else(|| self.update_notice.as_ref().map(|(n, _)| (n.clone(), Tone::Success)))
    }

    /// The status line, clickable while it shows the update notice.
    pub(super) fn status_clickable<'a>(&self, status: Element<'a, Message>) -> Element<'a, Message> {
        let notice = self.update_notice.as_ref().map(|(n, _)| n);
        if notice.is_some() && self.status.as_ref().is_none_or(|(s, _)| Some(s) == notice) {
            mouse_area(status).on_press(Message::Update(UpdateMsg::OpenNotice)).interaction(iced::mouse::Interaction::Pointer).into()
        } else {
            status
        }
    }
}
