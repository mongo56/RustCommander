//! The Pack dialog (Alt+F5): the archive to pack the selection into (zip, 7z
//! or tar by its extension; any other name gets `.zip`) and an optional
//! password with its confirmation, which encrypts a new zip or 7z (AES-256;
//! a 7z's names too). The password only lives in the dialog while it is
//! typed: on OK it moves into the job (`Job::Pack`) and the dialog is gone.
//! It is never shown, stored or logged.

use super::*;
use iced::widget::{column, container, row, text, text_input, Space};
use iced::{Alignment, Element, Length};
use rc_core::ops::Secret;

#[derive(Debug, Clone)]
pub enum PackMsg {
    Name(String),
    Password(String),
    Confirm(String),
    Ok,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Name,
    Password,
    Confirm,
    Cancel,
    Ok,
}

const CONTROLS: [Control; 5] = [Control::Name, Control::Password, Control::Confirm, Control::Cancel, Control::Ok];

pub(super) struct PackDialog {
    sources: Vec<PathBuf>,
    /// The folder a relative name is in.
    base: PathBuf,
    input: String,
    password: String,
    confirm: String,
    focus: Control,
}

impl std::fmt::Debug for PackDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackDialog")
            .field("sources", &self.sources)
            .field("input", &self.input)
            .field("password", &"***")
            .finish_non_exhaustive()
    }
}

fn password_id() -> text_input::Id {
    text_input::Id::new("pack-password")
}

fn confirm_id() -> text_input::Id {
    text_input::Id::new("pack-confirm")
}

fn focus_task(c: Control) -> Task<Message> {
    let id = match c {
        Control::Name => dialog_input_id(),
        Control::Password => password_id(),
        Control::Confirm => confirm_id(),
        Control::Cancel | Control::Ok => return attrib::unfocus_boxes(),
    };
    Task::batch([text_input::focus(id.clone()), text_input::select_all(id)])
}

/// The archive the typed name stands for: relative to `base`, with `.zip`
/// added unless it names a zip, 7z or tar archive.
fn pack_target(input: &str, base: &Path) -> PathBuf {
    let mut archive = resolve_typed_path(input.trim().trim_matches('"'), base);
    if archive.file_name().and_then(vfs::pack_encryptable).is_none() {
        let mut name = archive.file_name().unwrap_or_default().to_os_string();
        name.push(".zip");
        archive.set_file_name(name);
    }
    archive
}

/// Why the typed password can't be used, shown under the boxes while typing
/// (None: no password typed, or it's fine so far).
fn password_note(d: &PackDialog) -> Option<&'static str> {
    if d.password.is_empty() && d.confirm.is_empty() {
        return None;
    }
    let archive = pack_target(&d.input, &d.base);
    match archive.file_name().and_then(vfs::pack_encryptable) {
        Some(false) => Some("tar archives can't be encrypted: only zip and 7z can"),
        _ if archive.exists() => Some("This archive exists: files are added to it, which can't be done with a password"),
        _ if !d.confirm.is_empty() && d.confirm != d.password => Some("The passwords don't match"),
        _ => None,
    }
}

impl App {
    fn pack_dialog(&mut self) -> Option<&mut PackDialog> {
        match &mut self.dialog {
            Some(Dialog::Pack(d)) => Some(d),
            _ => None,
        }
    }

    /// Alt+F5: asks for the name of an archive to pack the selection into,
    /// in the other panel's folder.
    pub(super) fn open_pack(&mut self) -> Task<Message> {
        let sources = self.target_paths(self.active);
        if sources.is_empty() {
            return Task::none();
        }
        if sources.iter().any(|p| vfs::is_virtual(p)) {
            self.info("Can't pack files inside archives or on servers: copy them out first");
            return Task::none();
        }
        let target = self.panel(1 - self.active).path.clone();
        let folder = if vfs::in_archive(&target) { self.panel(self.active).path.clone() } else { target };
        let stem = match &sources[..] {
            [one] if one.is_dir() => one.file_name().map(|n| n.to_string_lossy().into_owned()),
            [one] => one.file_stem().map(|n| n.to_string_lossy().into_owned()),
            _ => self.panel(self.active).path.file_name().map(|n| n.to_string_lossy().into_owned()),
        };
        let input = folder.join(format!("{}.zip", stem.unwrap_or_else(|| "archive".into()))).display().to_string();
        let base = self.panel(self.active).path.clone();
        let d = PackDialog { sources, base, input, password: String::new(), confirm: String::new(), focus: Control::Name };
        self.open_dialog(Dialog::Pack(Box::new(d)))
    }

    pub(super) fn pack_msg(&mut self, msg: PackMsg) -> Task<Message> {
        let Some(d) = self.pack_dialog() else { return Task::none() };
        match msg {
            PackMsg::Name(s) => (d.input, d.focus) = (s, Control::Name),
            PackMsg::Password(s) => (d.password, d.focus) = (s, Control::Password),
            PackMsg::Confirm(s) => (d.confirm, d.focus) = (s, Control::Confirm),
            PackMsg::Cancel => {
                self.dialog = None;
                self.dialog_focus = None;
            }
            PackMsg::Ok => return self.pack_ok(),
        }
        Task::none()
    }

    /// OK: starts the job, or says what's wrong and keeps the dialog open.
    fn pack_ok(&mut self) -> Task<Message> {
        let Some(d) = self.pack_dialog() else { return Task::none() };
        let archive = pack_target(&d.input, &d.base);
        let problem = if archive.is_dir() || vfs::in_archive(archive.parent().unwrap_or(&archive)) {
            Some((format!("Can't pack into {}", archive.display()), Control::Name))
        } else if d.password.is_empty() && d.confirm.is_empty() {
            None
        } else if d.password != d.confirm {
            d.confirm.clear();
            Some(("The passwords don't match: type the password again to confirm it".into(), Control::Confirm))
        } else {
            ops::pack_password_problem(&archive, &Secret(d.password.clone())).map(|p| (p.to_owned(), Control::Password))
        };
        if let Some((msg, at)) = problem {
            d.focus = at;
            self.error(msg);
            return focus_task(at);
        }
        let Some(Dialog::Pack(d)) = self.dialog.take() else { return Task::none() };
        self.dialog_focus = None;
        let PackDialog { sources, password, .. } = *d;
        let password = (!password.is_empty()).then_some(Secret(password));
        let title = format!("Packing {} → {}", describe(&sources), archive.display());
        let other = 1 - self.active;
        let focus = (archive.parent() == Some(self.panel(other).path.as_path()))
            .then(|| (other, archive.file_name().unwrap_or_default().to_string_lossy().into_owned()));
        let task = self.start_job(Job::Pack { sources, archive, password }, title);
        if let Some(job) = self.jobs.last_mut() {
            job.focus = focus;
        }
        task
    }

    /// Keys while the dialog is open (`captured`: a box used the key).
    pub(super) fn pack_key(&mut self, named: Option<Named>, shift: bool, captured: bool) -> Task<Message> {
        let Some(d) = self.pack_dialog() else { return Task::none() };
        let focus = d.focus;
        match named {
            Some(Named::Escape) => self.pack_msg(PackMsg::Cancel),
            Some(Named::Tab) => {
                let at = CONTROLS.iter().position(|&c| c == focus).unwrap_or(0);
                let n = CONTROLS.len();
                let next = CONTROLS[if shift { (at + n - 1) % n } else { (at + 1) % n }];
                d.focus = next;
                focus_task(next)
            }
            // A box sends its own Enter (as OK) and keeps its Space.
            _ if captured => Task::none(),
            Some(Named::Enter | Named::Space) => match focus {
                Control::Cancel => self.pack_msg(PackMsg::Cancel),
                Control::Ok => self.pack_msg(PackMsg::Ok),
                _ if named == Some(Named::Enter) => self.pack_msg(PackMsg::Ok),
                _ => Task::none(),
            },
            Some(Named::ArrowLeft | Named::ArrowRight) if matches!(focus, Control::Cancel | Control::Ok) => {
                d.focus = if focus == Control::Ok { Control::Cancel } else { Control::Ok };
                Task::none()
            }
            _ => Task::none(),
        }
    }

    pub(super) fn pack_view<'a>(&'a self, d: &'a PackDialog) -> Element<'a, Message> {
        let c = theme::c();
        let label = |s: &'static str| text(s).size(12).color(c.muted);
        let field = |value: &'a str, id: text_input::Id, on_input: fn(String) -> PackMsg| {
            text_input("", value)
                .id(id)
                .on_input(move |s| Message::Pack(on_input(s)))
                .on_submit(Message::Pack(PackMsg::Ok))
                .padding(8)
                .size(13)
                .style(theme::input)
        };
        let mut content = column![
            text("Pack").size(16),
            text(format!(
                "Pack {} into the archive (zip, 7z or tar, by its extension; other names get .zip):",
                describe(&d.sources)
            ))
            .size(13)
            .color(c.muted),
            field(&d.input, dialog_input_id(), PackMsg::Name),
            label("Password (optional; encrypts a new zip or 7z with AES-256, a 7z's file names too)"),
            field(&d.password, password_id(), PackMsg::Password).secure(true),
            label("Confirm the password"),
            field(&d.confirm, confirm_id(), PackMsg::Confirm).secure(true),
        ]
        .spacing(8);
        if let Some(note) = password_note(d) {
            content = content.push(text(note).size(12).color(c.muted));
        }
        content = content.push(
            row![
                text("Tab moves between the controls").size(11).color(c.muted),
                Space::with_width(Length::Fill),
                view::dialog_button("Cancel", Message::Pack(PackMsg::Cancel), false, d.focus == Control::Cancel),
                view::dialog_button("Pack", Message::Pack(PackMsg::Ok), true, d.focus == Control::Ok),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        container(content).padding(20).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(input: &str, password: &str, confirm: &str) -> PackDialog {
        PackDialog {
            sources: vec![],
            base: std::env::temp_dir(),
            input: input.into(),
            password: password.into(),
            confirm: confirm.into(),
            focus: Control::Name,
        }
    }

    #[test]
    fn target_keeps_archive_kinds_and_adds_zip() {
        let base = Path::new("C:\\base");
        for (typed, name) in [
            ("a.zip", "a.zip"),
            ("a.7z", "a.7z"),
            ("a.tar", "a.tar"),
            ("a.tar.gz", "a.tar.gz"),
            ("A.JAR", "A.JAR"),
            ("a", "a.zip"),
            ("a.txt", "a.txt.zip"),
            ("a.txt.gz", "a.txt.gz.zip"),
            ("a.rar", "a.rar.zip"),
        ] {
            assert_eq!(pack_target(typed, base), base.join(name), "{typed}");
        }
    }

    #[test]
    fn notes_while_typing() {
        let missing = "rc-pack-note-does-not-exist";
        assert_eq!(password_note(&dialog(&format!("{missing}.zip"), "", "")), None);
        assert_eq!(password_note(&dialog(&format!("{missing}.7z"), "pw", "")), None);
        assert_eq!(password_note(&dialog(&format!("{missing}.7z"), "pw", "pw")), None);
        assert!(password_note(&dialog(&format!("{missing}.7z"), "pw", "px")).is_some_and(|n| n.contains("match")));
        assert!(password_note(&dialog(&format!("{missing}.tar.gz"), "pw", "")).is_some_and(|n| n.contains("tar")));
        // A name without an archive extension is a zip.
        assert_eq!(password_note(&dialog(missing, "pw", "pw")), None);
        assert!(!format!("{:?}", dialog("a.zip", "s3cret", "s3cret")).contains("s3cret"));
    }
}
