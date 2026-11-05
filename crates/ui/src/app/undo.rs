//! Undo of the last file operation (Ctrl+Z, `cm_Undo`). The steps and their
//! safety checks live in `rc_core::undo`; here they are recorded, asked
//! about ("Undo: rename a.txt → b.txt?") and run on a worker thread.

use super::{App, Dialog, Message};
use iced::Task;
use rc_core::undo::{UndoStep, Undone, MAX_STEPS};

impl App {
    /// Remembers a finished operation as the newest step (the oldest one
    /// goes past [`MAX_STEPS`]).
    pub(super) fn push_undo(&mut self, step: Option<UndoStep>) {
        let Some(step) = step else { return };
        self.undo.push(step);
        if self.undo.len() > MAX_STEPS {
            self.undo.remove(0);
        }
    }

    /// Ctrl+Z: asks before undoing the newest step (it stays on the stack
    /// until Enter).
    pub(super) fn ask_undo(&mut self) -> Task<Message> {
        if self.undo_running {
            self.info("An undo is still running");
            return Task::none();
        }
        let Some(step) = self.undo.last() else {
            self.info("Nothing to undo");
            return Task::none();
        };
        let what = step.describe();
        self.open_dialog(Dialog::Undo { what })
    }

    /// Enter in the question: takes the newest step off the stack and
    /// undoes it in the background (verifying first). It's gone either way:
    /// a step that failed can't be undone any more.
    pub(super) fn run_undo(&mut self) -> Task<Message> {
        let Some(step) = self.undo.pop() else { return Task::none() };
        self.undo_running = true;
        self.info(format!("Undoing: {}", step.describe()));
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(step.undo());
                });
                rx.await.unwrap_or_else(|_| Err("The undo stopped unexpectedly".into()))
            },
            Message::UndoDone,
        )
    }

    pub(super) fn undo_done(&mut self, result: Result<Undone, String>) {
        self.undo_running = false;
        self.disk.borrow_mut().clear();
        self.reload_panels();
        match result {
            Ok(done) => {
                if let Some(path) = &done.focus {
                    let (parent, name) = (path.parent(), path.file_name().map(|n| n.to_string_lossy().into_owned()));
                    for side in 0..2 {
                        if let (Some(parent), Some(name)) = (parent, &name) {
                            if self.panel(side).path.as_path() == parent {
                                self.panel_mut(side).focus(name);
                            }
                        }
                    }
                }
                self.after_change();
                self.success(done.message);
            }
            Err(why) => self.error(format!("Can't undo: {why}")),
        }
    }
}
