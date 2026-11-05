//! Branch view (Ctrl+B): every file of the panel's folder and its subfolders
//! in one list. The folder is walked on a worker (`rc_core::search::list_branch`,
//! Esc stops it), then the panel switches to the same "results" mode Find
//! files feeds (`Panel::show_results`), with names relative to the folder.
//! Ctrl+B again, `..` or any navigation leaves it.

use super::*;
use rc_core::panel::BRANCH_LABEL;
use rc_core::search::{self, Batch, Summary};

#[derive(Debug, Clone)]
pub enum BranchMsg {
    Batch(Batch),
    Done(Summary),
}

/// A listing in progress.
pub(super) struct Branching {
    side: usize,
    root: PathBuf,
    seq: u64,
    cancel: Arc<AtomicBool>,
    hits: Vec<Entry>,
}

impl Branching {
    pub(super) fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn count_text(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

impl App {
    /// Ctrl+B: starts the listing, or leaves the branch view.
    pub(super) fn branch_view(&mut self) -> Task<Message> {
        let side = self.active;
        if let Some(b) = self.branching.take() {
            b.stop();
            self.info("Listing cancelled");
            return Task::none();
        }
        let panel = self.panel(side);
        match panel.results() {
            Some(BRANCH_LABEL) => {
                // Back to the folder, the cursor on the subfolder holding the file it was on.
                let focus = panel.current().filter(|e| !e.is_parent()).map(|e| {
                    e.name.split(['\\', '/']).next().unwrap_or(&e.name).to_owned()
                });
                let nav = Nav::To { path: panel.path.clone(), focus };
                return self.start_nav(side, nav);
            }
            Some(_) => {
                self.info("Leave the search results first (Backspace)");
                return Task::none();
            }
            None => {}
        }
        if remote::is_remote(&panel.path) {
            self.info("Branch view doesn't work on servers yet");
            return Task::none();
        }
        let (root, hidden) = (panel.path.clone(), panel.show_hidden);
        self.next_listing += 1;
        let seq = self.next_listing;
        let cancel = Arc::new(AtomicBool::new(false));
        self.branching = Some(Branching { side, root: root.clone(), seq, cancel: cancel.clone(), hits: Vec::new() });
        self.info("Listing\u{2026} (Esc cancels)");
        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let summary = search::list_branch(&root, hidden, search::BRANCH_MAX, &cancel, |b| {
                tx.unbounded_send(BranchMsg::Batch(b)).is_ok()
            });
            let _ = tx.unbounded_send(BranchMsg::Done(summary));
        });
        Task::run(rx, move |m| Message::Branch(seq, m))
    }

    pub(super) fn branch_update(&mut self, seq: u64, msg: BranchMsg) -> Task<Message> {
        let Some(b) = self.branching.as_mut().filter(|b| b.seq == seq) else { return Task::none() };
        match msg {
            BranchMsg::Batch(batch) => {
                b.hits.extend(batch.hits);
                let text = format!("Listing\u{2026} {} files (Esc cancels)", count_text(b.hits.len()));
                self.info(text);
            }
            BranchMsg::Done(summary) => {
                let b = self.branching.take().unwrap();
                if summary.cancelled {
                    self.info("Listing cancelled");
                    return Task::none();
                }
                let (side, root) = (b.side, b.root);
                let panel = self.panel(side);
                // The panel moved on meanwhile: drop the result.
                if panel.path != root || panel.results().is_some() {
                    return Task::none();
                }
                let rows = self.page_rows();
                self.nav[side] = None;
                let count = b.hits.len();
                let panel = self.panel_mut(side);
                panel.show_results(root, BRANCH_LABEL.into(), b.hits);
                panel.ensure_visible(rows);
                let mut text = format!("Branch view: {} files", count_text(count));
                if summary.errors > 0 {
                    text.push_str(&format!(" \u{b7} {} folders could not be read", summary.errors));
                }
                if summary.truncated {
                    let warning = format!("{text} \u{b7} INCOMPLETE: stopped at {} files", count_text(search::BRANCH_MAX));
                    self.error(warning);
                } else if summary.errors > 0 {
                    self.info(text);
                } else {
                    self.success(text);
                }
            }
        }
        Task::none()
    }
}
