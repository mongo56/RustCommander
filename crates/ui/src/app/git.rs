//! Git status in the panels: a status letter per entry and the branch name
//! in the path bar (setting `git_status`). `git status` runs on a worker
//! thread, at most one per panel at a time; see `rc_core::git_status`.

use super::{delayed, App, Message};
use iced::Task;
use rc_core::git_status::{self, GitStatus, QueryError};
use rc_core::vfs;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a folder change waits before `git status` runs again.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// `git status` is killed after this long.
const LIMIT: Duration = Duration::from_secs(5);

/// What a panel shows for its folder.
#[derive(Debug, Clone, PartialEq)]
pub struct GitView {
    /// The panel folder this is about.
    pub folder: PathBuf,
    pub branch: Option<String>,
    /// Status letter per entry name of `folder` (folders: strongest inside).
    pub marks: HashMap<String, char>,
}

#[derive(Debug, Clone)]
pub enum GitMsg {
    /// The debounce of request `u64` for the side's folder is over.
    Start(usize, u64),
    /// A query finished: `Ok(None)` = the folder is not in a work tree,
    /// `Err` = git missing or failed.
    Done(usize, u64, Result<Option<GitView>, bool>),
}

/// Per panel side.
#[derive(Debug, Default)]
pub struct GitSide {
    pub view: Option<GitView>,
    /// Latest wanted folder and its request number.
    want: Option<(PathBuf, u64)>,
    running: bool,
    /// A newer request came while one was running.
    queued: bool,
}

impl GitSide {
    /// The marks of `folder`, if this side has them.
    pub fn marks_for(&self, folder: &Path) -> Option<&GitView> {
        self.view.as_ref().filter(|v| v.folder == folder)
    }
}

/// Runs git for `folder` (blocking).
fn load(folder: &Path) -> Result<Option<GitView>, bool> {
    let Some(root) = git_status::find_root(folder) else { return Ok(None) };
    let status: GitStatus = git_status::query(&root, LIMIT).map_err(|e| e == QueryError::NoGit)?;
    let rel = folder.strip_prefix(&root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    Ok(Some(GitView { folder: folder.to_path_buf(), branch: status.branch.clone(), marks: status.marks_in(&rel) }))
}

impl App {
    /// After every message: makes sure each panel has (or is getting) the git
    /// status of its folder. `changed` is the folder a watcher just reported.
    pub(super) fn git_check(&mut self, changed: Option<&Path>) -> Task<Message> {
        let mut tasks = Vec::new();
        for side in 0..2 {
            let path = &self.sides[side].tabs[self.sides[side].current].path;
            let g = &self.git[side];
            if !self.config.git_status || self.git_missing || vfs::is_virtual(path) || vfs::unc_server_only(path).is_some()
            {
                if g.view.is_some() || g.want.is_some() {
                    self.git[side] = GitSide::default();
                }
                continue;
            }
            let covered = g.view.as_ref().is_some_and(|v| v.folder == *path) || g.want.as_ref().is_some_and(|(f, _)| f == path);
            let refresh = changed == Some(path.as_path());
            if covered && !refresh {
                continue;
            }
            let path = path.clone();
            self.git_gen += 1;
            let gen = self.git_gen;
            self.git[side].want = Some((path, gen));
            if covered {
                tasks.push(delayed(DEBOUNCE, Message::Git(GitMsg::Start(side, gen))));
            } else {
                tasks.push(self.git_start(side));
            }
        }
        Task::batch(tasks)
    }

    /// Starts the query for the wanted folder, or queues it behind the one
    /// that is running.
    fn git_start(&mut self, side: usize) -> Task<Message> {
        let g = &mut self.git[side];
        let Some((folder, gen)) = g.want.clone() else { return Task::none() };
        if g.running {
            g.queued = true;
            return Task::none();
        }
        g.running = true;
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(load(&folder));
                });
                rx.await.unwrap_or(Err(false))
            },
            move |result| Message::Git(GitMsg::Done(side, gen, result)),
        )
    }

    pub(super) fn git_update(&mut self, msg: GitMsg) -> Task<Message> {
        match msg {
            GitMsg::Start(side, gen) => {
                // Only the newest request starts.
                if self.git[side].want.as_ref().is_some_and(|(_, g)| *g == gen) {
                    return self.git_start(side);
                }
                Task::none()
            }
            GitMsg::Done(side, gen, result) => {
                let g = &mut self.git[side];
                g.running = false;
                let current = g.want.as_ref().is_some_and(|(_, w)| *w == gen);
                if current {
                    match result {
                        Ok(Some(view)) => g.view = Some(view),
                        Ok(None) => {
                            // Not in a work tree: remember that for this folder.
                            let folder = g.want.as_ref().map(|(f, _)| f.clone()).unwrap_or_default();
                            g.view = Some(GitView { folder, branch: None, marks: HashMap::new() });
                        }
                        Err(true) => {
                            self.git_missing = true;
                            self.git = Default::default();
                            return Task::none();
                        }
                        Err(false) => {
                            let folder = g.want.as_ref().map(|(f, _)| f.clone()).unwrap_or_default();
                            g.view = Some(GitView { folder, branch: None, marks: HashMap::new() });
                        }
                    }
                }
                if std::mem::take(&mut self.git[side].queued) {
                    return self.git_start(side);
                }
                Task::none()
            }
        }
    }
}
