//! Login for a network share (`\\server\share`): when opening a folder on a
//! share fails with access denied / logon failure, this dialog asks for a user
//! and password, connects with `rc_platform::connect_share` on a worker thread
//! and retries the navigation. The password only lives in the dialog while
//! it is typed; it is moved into the worker on OK and never stored or logged.

use super::*;
use iced::widget::{button, column, container, row, text, text_input, Space};
use iced::{Alignment, Element, Length};
use rc_platform::ShareError;

#[derive(Debug, Clone)]
pub enum ShareMsg {
    User(String),
    Password(String),
    Remember,
    Ok,
    Cancel,
    /// The worker finished connecting `share` for the navigation of `side` to `target`.
    Done { share: String, side: usize, target: PathBuf, result: Result<(), ShareError> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    User,
    Password,
    Remember,
    Cancel,
    Ok,
}

const CONTROLS: [Control; 5] = [Control::User, Control::Password, Control::Remember, Control::Cancel, Control::Ok];

#[derive(Debug)]
pub(super) struct ShareLogin {
    share: String,
    side: usize,
    target: PathBuf,
    user: String,
    password: String,
    remember: bool,
    focus: Control,
    busy: bool,
}

/// True when a listing error text (an `io::Error` display, which ends in
/// `(os error N)`) means the server wants a login.
pub(super) fn is_logon_error(error: &str) -> bool {
    let Some(rest) = error.rsplit_once("(os error ").map(|(_, r)| r) else { return false };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<i32>().is_ok_and(rc_platform::is_logon_code)
}

fn focus_task(c: Control) -> Task<Message> {
    match c {
        Control::User => Task::batch([text_input::focus(dialog_input_id()), text_input::select_all(dialog_input_id())]),
        Control::Password => {
            Task::batch([text_input::focus(dialog_comment_id()), text_input::select_all(dialog_comment_id())])
        }
        _ => attrib::unfocus_boxes(),
    }
}

impl App {
    fn share_dialog(&mut self) -> Option<&mut ShareLogin> {
        match &mut self.dialog {
            Some(Dialog::ShareLogin(d)) => Some(d),
            _ => None,
        }
    }

    /// Navigation of `side` to `target` failed with a logon error: asks for
    /// credentials for its share. None when the target isn't on a share, a
    /// dialog is open, or we just connected this share (no loop).
    pub(super) fn ask_share_login(&mut self, side: usize, target: &Path) -> Option<Task<Message>> {
        let share = rc_platform::share_root(target)?;
        let just_connected = self.share_retry.take().is_some_and(|s| s.eq_ignore_ascii_case(&share));
        if self.dialog.is_some() || just_connected {
            return None;
        }
        self.leave_edits();
        let d = ShareLogin {
            share,
            side,
            target: target.to_path_buf(),
            user: String::new(),
            password: String::new(),
            remember: false,
            focus: Control::User,
            busy: false,
        };
        self.dialog = Some(Dialog::ShareLogin(Box::new(d)));
        self.dialog_focus = None;
        Some(focus_task(Control::User))
    }

    pub(super) fn share_msg(&mut self, msg: ShareMsg) -> Task<Message> {
        match msg {
            ShareMsg::User(s) => {
                if let Some(d) = self.share_dialog().filter(|d| !d.busy) {
                    d.user = s;
                    d.focus = Control::User;
                }
                Task::none()
            }
            ShareMsg::Password(s) => {
                if let Some(d) = self.share_dialog().filter(|d| !d.busy) {
                    d.password = s;
                    d.focus = Control::Password;
                }
                Task::none()
            }
            ShareMsg::Remember => {
                if let Some(d) = self.share_dialog().filter(|d| !d.busy) {
                    d.remember = !d.remember;
                    d.focus = Control::Remember;
                }
                attrib::unfocus_boxes()
            }
            ShareMsg::Cancel => {
                self.dialog = None;
                Task::none()
            }
            ShareMsg::Ok => self.share_ok(),
            ShareMsg::Done { share, side, target, result } => {
                // Cancelled meanwhile: nothing more to do.
                if !matches!(&self.dialog, Some(Dialog::ShareLogin(d)) if d.busy && d.share == share) {
                    return Task::none();
                }
                let Some(Dialog::ShareLogin(mut d)) = self.dialog.take() else { return Task::none() };
                match result {
                    Ok(()) => {
                        self.share_retry = Some(share);
                        self.start_nav(side, Nav::to(target))
                    }
                    Err(ShareError::CredentialConflict) => {
                        self.error(ShareError::CredentialConflict.to_string());
                        Task::none()
                    }
                    Err(e) => {
                        self.error(e.to_string());
                        d.busy = false;
                        d.password.clear();
                        d.focus = Control::Password;
                        self.dialog = Some(Dialog::ShareLogin(d));
                        focus_task(Control::Password)
                    }
                }
            }
        }
    }

    /// OK: connects on a worker thread (it can block for seconds).
    fn share_ok(&mut self) -> Task<Message> {
        let Some(d) = self.share_dialog().filter(|d| !d.busy) else { return Task::none() };
        if d.user.trim().is_empty() {
            d.focus = Control::User;
            self.error("Enter a user name");
            return focus_task(Control::User);
        }
        d.busy = true;
        let (share, side, target) = (d.share.clone(), d.side, d.target.clone());
        let user = d.user.trim().to_owned();
        let password = std::mem::take(&mut d.password);
        let remember = d.remember;
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                let remote = share.clone();
                std::thread::spawn(move || {
                    let mut password = password.into_bytes();
                    let text = String::from_utf8_lossy(&password).into_owned();
                    let r = rc_platform::connect_share(&remote, Some(&user), Some(&text), remember, None);
                    // Overwrite our copies before they are freed.
                    password.fill(0);
                    let mut text = text.into_bytes();
                    text.fill(0);
                    let _ = tx.send(r);
                });
                let result = rx.await.unwrap_or_else(|_| Err(ShareError::Other(0, "connect thread failed".into())));
                (share, side, target, result)
            },
            |(share, side, target, result)| Message::Share(ShareMsg::Done { share, side, target, result }),
        )
    }

    /// Keys while the dialog is open (`captured`: a box used the key).
    pub(super) fn share_key(&mut self, named: Option<Named>, shift: bool, captured: bool) -> Task<Message> {
        let Some(d) = self.share_dialog() else { return Task::none() };
        let focus = d.focus;
        let step = |back: bool| {
            let at = CONTROLS.iter().position(|&c| c == focus).unwrap_or(0);
            let n = CONTROLS.len();
            CONTROLS[if back { (at + n - 1) % n } else { (at + 1) % n }]
        };
        match named {
            Some(Named::Escape) => {
                self.dialog = None;
                Task::none()
            }
            Some(Named::Tab) => {
                let next = step(shift);
                d.focus = next;
                focus_task(next)
            }
            Some(Named::ArrowUp | Named::ArrowDown) if !captured => {
                let next = step(named == Some(Named::ArrowUp));
                d.focus = next;
                focus_task(next)
            }
            // A box sends its own Enter (as OK), and keeps its Space.
            _ if captured => Task::none(),
            Some(Named::Space) => match focus {
                Control::Cancel => self.share_msg(ShareMsg::Cancel),
                Control::Ok => self.share_msg(ShareMsg::Ok),
                Control::Remember => self.share_msg(ShareMsg::Remember),
                _ => Task::none(),
            },
            Some(Named::Enter) => match focus {
                Control::Cancel => self.share_msg(ShareMsg::Cancel),
                _ => self.share_msg(ShareMsg::Ok),
            },
            _ => Task::none(),
        }
    }

    pub(super) fn share_view<'a>(&'a self, d: &'a ShareLogin) -> Element<'a, Message> {
        let c = theme::c();
        let label = |s: &'static str| text(s).size(12).color(c.muted);
        let mark = if d.remember { "[x]" } else { "[ ]" };
        let remember = button(
            row![
                text(mark).size(13).font(iced::Font::MONOSPACE).width(Length::Fixed(26.0)),
                text("Remember (keep the connection and let Windows save the login)").size(13).width(Length::Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .on_press(Message::Share(ShareMsg::Remember))
        .width(Length::Fill)
        .padding([4, 8])
        .style(theme::suggestion(d.focus == Control::Remember));
        let user = text_input("DOMAIN\\user or user@domain", &d.user)
            .id(dialog_input_id())
            .on_input(|s| Message::Share(ShareMsg::User(s)))
            .on_submit(Message::Share(ShareMsg::Ok))
            .padding(8)
            .size(13)
            .style(theme::input);
        let password = text_input("", &d.password)
            .id(dialog_comment_id())
            .secure(true)
            .on_input(|s| Message::Share(ShareMsg::Password(s)))
            .on_submit(Message::Share(ShareMsg::Ok))
            .padding(8)
            .size(13)
            .style(theme::input);
        let status = if d.busy { "Connecting..." } else { "Tab moves between the controls" };
        let content = column![
            text("Network login").size(16),
            text(format!("{} needs a user name and password.", d.share)).size(13).color(c.muted),
            label("User"),
            user,
            label("Password"),
            password,
            remember,
            row![
                text(status).size(11).color(c.muted),
                Space::with_width(Length::Fill),
                view::dialog_button("Cancel", Message::Share(ShareMsg::Cancel), false, d.focus == Control::Cancel),
                view::dialog_button("OK", Message::Share(ShareMsg::Ok), true, d.focus == Control::Ok),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(8);
        container(content).padding(20).width(Length::Fixed(520.0)).style(theme::dialog).into()
    }
}
