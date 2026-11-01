//! Connect to a server (Ctrl+F, TC's `cm_FtpConnect`): fills the window
//! while open. Lists the saved connections (`crate::sites`); N/Ins adds
//! one, E/F4 edits, Del twice deletes, Enter connects in the panel the tool
//! was opened from. Logging in runs on a worker thread
//! (`rc_core::remote::connect`); Esc abandons it. A password is asked for
//! if none is saved in the Credential Manager (and then kept in memory for
//! the session); an SFTP server's unknown host key is shown with its
//! fingerprint and trusted only on Y. Ctrl+Shift+F (`cm_FtpDisconnect`)
//! closes the connection of the panel's server.

use super::*;
use crate::sites::{self, Site};
use crate::theme::filled;
use iced::widget::text::Wrapping;
use iced::widget::{button, checkbox, column, container, mouse_area, row, text, text_input, Column, Row, Space};
use iced::{Alignment, Element, Length};
use rc_core::remote::{self, Login, Protocol};
use std::sync::Mutex;

const ROW_H: f32 = 22.0;
const LABEL_W: f32 = 120.0;

/// Passwords typed this session (by server address); never written out.
static TYPED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn typed_password(address: &str) -> Option<String> {
    TYPED.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(a, _)| a == address).map(|(_, p)| p.clone())
}

fn remember_password(address: &str, password: Option<String>) {
    let mut typed = TYPED.lock().unwrap_or_else(|e| e.into_inner());
    typed.retain(|(a, _)| a != address);
    if let Some(p) = password {
        typed.push((address.to_owned(), p));
    }
}

fn input_id(field: Field) -> text_input::Id {
    text_input::Id::new(match field {
        Field::Name => "connect-name",
        Field::Host => "connect-host",
        Field::Port => "connect-port",
        Field::User => "connect-user",
        Field::Password => "connect-password",
        Field::KeyFile => "connect-key",
        Field::Folder => "connect-folder",
    })
}

fn password_id() -> text_input::Id {
    text_input::Id::new("connect-ask")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Host,
    Port,
    User,
    Password,
    KeyFile,
    Folder,
}

#[derive(Debug, Clone)]
pub enum ConnectMsg {
    Click(usize),
    Connect,
    New,
    Edit,
    Delete,
    Close,
    Input(Field, String),
    Protocol(Protocol),
    SavePassword(bool),
    SaveForm,
    /// Back to the list (from the form, a password question, a host key
    /// question or a connection attempt).
    Back,
    AskedInput(String),
    AskedSubmit,
    Trust,
    Done(u64, Result<PathBuf, String>),
}

/// A connection being added or edited.
pub(super) struct Form {
    /// Index of the edited site; `None` for a new one.
    index: Option<usize>,
    name: String,
    protocol: Protocol,
    host: String,
    port: String,
    user: String,
    /// Typed password; empty keeps a saved one.
    password: String,
    save_password: bool,
    key_file: String,
    folder: String,
}

pub(super) enum Mode {
    List,
    Edit(Form),
    /// Asking the password (or key passphrase) of `site`.
    Password { site: usize, input: String },
    Connecting { seq: u64, site: Site, login: Login, typed: bool },
    /// Trust the host key (`text`: the server's identity) and connect?
    Trust { site: Site, login: Login, typed: bool, text: String },
}

pub(super) struct ConnectTool {
    /// Panel to connect in.
    side: usize,
    sites: Vec<Site>,
    selected: usize,
    mode: Mode,
    /// Site index after a first Del (a second one deletes).
    deleting: Option<usize>,
    note: Option<(String, Tone)>,
    last_click: Option<(usize, Instant)>,
}

impl ConnectTool {
    fn new_form(&self, index: Option<usize>) -> Form {
        match index.and_then(|i| self.sites.get(i)) {
            Some(s) => Form {
                index,
                name: s.name.clone(),
                protocol: s.protocol(),
                host: s.host.clone(),
                // Empty: the protocol's default (shown as placeholder).
                port: if s.port == s.protocol().default_port() { String::new() } else { s.port.to_string() },
                user: s.user.clone(),
                password: String::new(),
                save_password: s.save_password,
                key_file: s.key_file.clone(),
                folder: s.folder.clone(),
            },
            None => Form {
                index: None,
                name: String::new(),
                protocol: Protocol::Sftp,
                host: String::new(),
                port: String::new(),
                user: String::new(),
                password: String::new(),
                save_password: false,
                key_file: String::new(),
                folder: String::new(),
            },
        }
    }
}

fn protocol_label(p: Protocol) -> &'static str {
    match p {
        Protocol::Sftp => "SFTP",
        Protocol::Ftp => "FTP",
        Protocol::Ftps => "FTPS (TLS)",
    }
}

impl App {
    pub(super) fn open_connect(&mut self) -> Task<Message> {
        self.leave_edits();
        let sites = sites::load();
        // Start on the site of the panel's server, if any.
        let here = remote::split(&self.panel(self.active).path).map(|(key, _)| key);
        let selected = here.and_then(|k| sites.iter().position(|s| s.address().eq_ignore_ascii_case(&k))).unwrap_or(0);
        self.connect = Some(ConnectTool {
            side: self.active,
            sites,
            selected,
            mode: Mode::List,
            deleting: None,
            note: None,
            last_click: None,
        });
        Task::none()
    }

    /// Ctrl+Shift+F: forgets the connection of the active panel's server;
    /// panels showing it go back to the last local folder they showed.
    pub(super) fn disconnect(&mut self) -> Task<Message> {
        let path = self.panel(self.active).path.clone();
        let Some((key, _)) = remote::split(&path) else {
            self.status = Some(("The panel doesn't show a server (Ctrl+F connects)".into(), Tone::Info));
            return Task::none();
        };
        remote::disconnect(&path);
        let mut tasks = Vec::new();
        for side in 0..2 {
            let panel = self.panel(side);
            if remote::split(&panel.path).is_none_or(|(k, _)| !k.eq_ignore_ascii_case(&key)) {
                continue;
            }
            let (history, _) = panel.history();
            let local = history.iter().find(|p| !remote::is_remote(p)).map(|p| p.to_path_buf());
            let target = local.or_else(rc_platform::home_dir).unwrap_or_else(|| PathBuf::from("C:\\"));
            tasks.push(self.start_nav(side, Nav::to(target)));
        }
        self.status = Some((format!("Disconnected from {key}"), Tone::Info));
        Task::batch(tasks)
    }

    fn connect_close(&mut self) {
        self.connect = None;
    }

    fn connect_move(&mut self, delta: isize) {
        let Some(c) = &mut self.connect else { return };
        c.selected = c.selected.saturating_add_signed(delta).min(c.sites.len().saturating_sub(1));
        c.deleting = None;
    }

    /// Enter on a site: connects with its saved or typed password, or asks
    /// for one first.
    fn connect_selected(&mut self, asked: Option<String>) -> Task<Message> {
        let Some(c) = &mut self.connect else { return Task::none() };
        let Some(site) = c.sites.get(c.selected).cloned() else { return Task::none() };
        let address = site.address();
        let password = match asked.clone() {
            Some(p) => Some(p),
            None if site.save_password => rc_platform::credentials::load(&site.credential_target()),
            None => typed_password(&address),
        };
        let typed = !site.save_password || asked.is_some();
        if password.is_none() && (site.asks_password() || site.save_password) {
            c.mode = Mode::Password { site: c.selected, input: String::new() };
            c.note = site.save_password.then(|| ("No saved password found: type it".to_owned(), Tone::Info));
            return text_input::focus(password_id());
        }
        let login = site.login(password);
        self.start_connect(site, login, typed)
    }

    fn start_connect(&mut self, site: Site, login: Login, typed: bool) -> Task<Message> {
        self.next_listing += 1;
        let seq = self.next_listing;
        let Some(c) = &mut self.connect else { return Task::none() };
        c.note = Some((format!("Connecting to {}\u{2026} (Esc cancels)", site.address()), Tone::Info));
        c.mode = Mode::Connecting { seq, site: site.clone(), login: login.clone(), typed };
        let folder = site.folder.clone();
        Task::perform(
            async move {
                let (tx, rx) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(remote::connect(login, &folder).map_err(|e| e.to_string()));
                });
                rx.await.unwrap_or_else(|_| Err("connection thread failed".into()))
            },
            move |result| Message::Connect(ConnectMsg::Done(seq, result)),
        )
    }

    fn connect_done(&mut self, seq: u64, result: Result<PathBuf, String>) -> Task<Message> {
        let Some(c) = &mut self.connect else { return Task::none() };
        let Mode::Connecting { seq: expected, .. } = &c.mode else { return Task::none() };
        if *expected != seq {
            return Task::none();
        }
        let Mode::Connecting { site, login, typed, .. } = std::mem::replace(&mut c.mode, Mode::List) else {
            return Task::none();
        };
        match result {
            Ok(path) => {
                if typed {
                    remember_password(&site.address(), login.password.clone());
                }
                let side = c.side;
                self.connect_close();
                self.active = side;
                self.status = Some((format!("Connected to {} (Ctrl+Shift+F disconnects)", site.address()), Tone::Success));
                self.start_nav(side, Nav::to(path))
            }
            Err(e) if e.contains(remote::HOST_KEY_UNKNOWN) => {
                c.note = None;
                c.mode = Mode::Trust { site, login, typed, text: e };
                Task::none()
            }
            Err(e) => {
                if e.contains(remote::LOGIN_FAILED) && typed {
                    remember_password(&site.address(), None);
                }
                // An encrypted key needs its passphrase.
                if login.password.is_none() && login.key_file.is_some() && e.contains("private key") {
                    c.mode = Mode::Password { site: c.selected, input: String::new() };
                    c.note = Some(("The key needs its passphrase".into(), Tone::Info));
                    return text_input::focus(password_id());
                }
                c.note = Some((e, Tone::Error));
                Task::none()
            }
        }
    }

    fn connect_trust(&mut self) -> Task<Message> {
        let Some(c) = &mut self.connect else { return Task::none() };
        let Mode::Trust { site, login, typed, .. } = std::mem::replace(&mut c.mode, Mode::List) else {
            return Task::none();
        };
        if let Err(e) = remote::trust_host_key(&login) {
            c.note = Some((format!("Can't save the host key: {e}"), Tone::Error));
            return Task::none();
        }
        self.start_connect(site, login, typed)
    }

    fn save_form(&mut self) -> Task<Message> {
        let Some(c) = &mut self.connect else { return Task::none() };
        let Mode::Edit(f) = &c.mode else { return Task::none() };
        let host = f.host.trim().to_owned();
        if host.is_empty() {
            c.note = Some(("Enter the server's host name or address".into(), Tone::Error));
            return text_input::focus(input_id(Field::Host));
        }
        let port = match f.port.trim() {
            "" => f.protocol.default_port(),
            p => match p.parse::<u16>() {
                Ok(p) if p > 0 => p,
                _ => {
                    c.note = Some(("The port is a number from 1 to 65535".into(), Tone::Error));
                    return text_input::focus(input_id(Field::Port));
                }
            },
        };
        let site = Site {
            name: Some(f.name.trim()).filter(|n| !n.is_empty()).unwrap_or(&host).to_owned(),
            protocol: f.protocol.scheme().to_owned(),
            host,
            port,
            user: f.user.trim().to_owned(),
            save_password: f.save_password,
            key_file: f.key_file.trim().trim_matches('"').to_owned(),
            folder: f.folder.trim().to_owned(),
        };
        // Passwords go to the Credential Manager only.
        let old: Option<Site> = f.index.and_then(|i| c.sites.get(i).cloned());
        let old_saved = old.as_ref().filter(|o| o.save_password).and_then(|o| {
            let target = o.credential_target();
            let secret = rc_platform::credentials::load(&target);
            if target != site.credential_target() || !site.save_password {
                rc_platform::credentials::delete(&target);
            }
            secret
        });
        if site.save_password {
            let secret = Some(f.password.clone()).filter(|p| !p.is_empty()).or(old_saved);
            if let Some(secret) = secret {
                if let Err(e) = rc_platform::credentials::save(&site.credential_target(), &site.user, &secret) {
                    c.note = Some((format!("Can't save the password in the Credential Manager: {e}"), Tone::Error));
                    return Task::none();
                }
            }
        } else if !f.password.is_empty() {
            // Not saved: still good for this session.
            remember_password(&site.address(), Some(f.password.clone()));
        }
        match f.index {
            Some(i) if i < c.sites.len() => c.sites[i] = site,
            _ => {
                c.sites.push(site);
                c.selected = c.sites.len() - 1;
            }
        }
        c.mode = Mode::List;
        c.note = match sites::save(&c.sites) {
            Ok(()) => Some(("Saved (Enter connects)".into(), Tone::Success)),
            Err(e) => Some((format!("Can't save connections.toml: {e}"), Tone::Error)),
        };
        Task::none()
    }

    fn delete_site(&mut self) {
        let Some(c) = &mut self.connect else { return };
        let Some(site) = c.sites.get(c.selected) else { return };
        if c.deleting != Some(c.selected) {
            c.deleting = Some(c.selected);
            c.note = Some((format!("Press Del again to delete \"{}\"", site.name), Tone::Info));
            return;
        }
        let site = c.sites.remove(c.selected);
        if site.save_password && !c.sites.iter().any(|s| s.save_password && s.credential_target() == site.credential_target()) {
            rc_platform::credentials::delete(&site.credential_target());
        }
        c.deleting = None;
        c.selected = c.selected.min(c.sites.len().saturating_sub(1));
        c.note = match sites::save(&c.sites) {
            Ok(()) => Some((format!("Deleted \"{}\"", site.name), Tone::Info)),
            Err(e) => Some((format!("Can't save connections.toml: {e}"), Tone::Error)),
        };
    }

    pub(super) fn connect_update(&mut self, msg: ConnectMsg) -> Task<Message> {
        let Some(c) = &mut self.connect else { return Task::none() };
        match msg {
            ConnectMsg::Click(i) => {
                if !matches!(c.mode, Mode::List) {
                    return Task::none();
                }
                let now = Instant::now();
                let double = matches!(c.last_click, Some((j, t)) if j == i && now - t < DOUBLE_CLICK);
                c.selected = i.min(c.sites.len().saturating_sub(1));
                c.deleting = None;
                c.last_click = Some((i, now));
                if double {
                    c.last_click = None;
                    return self.connect_selected(None);
                }
            }
            ConnectMsg::Connect => {
                if matches!(c.mode, Mode::List) {
                    return self.connect_selected(None);
                }
            }
            ConnectMsg::New | ConnectMsg::Edit => {
                let index = matches!(msg, ConnectMsg::Edit).then_some(c.selected).filter(|&i| i < c.sites.len());
                if matches!(msg, ConnectMsg::Edit) && index.is_none() {
                    return Task::none();
                }
                c.mode = Mode::Edit(c.new_form(index));
                c.note = None;
                c.deleting = None;
                return Task::batch([text_input::focus(input_id(Field::Name)), text_input::select_all(input_id(Field::Name))]);
            }
            ConnectMsg::Delete => {
                if matches!(c.mode, Mode::List) {
                    self.delete_site();
                }
            }
            ConnectMsg::Close => self.connect_close(),
            ConnectMsg::Input(field, value) => {
                if let Mode::Edit(f) = &mut c.mode {
                    match field {
                        Field::Name => f.name = value,
                        Field::Host => f.host = value,
                        Field::Port => f.port = value,
                        Field::User => f.user = value,
                        Field::Password => f.password = value,
                        Field::KeyFile => f.key_file = value,
                        Field::Folder => f.folder = value,
                    }
                }
            }
            ConnectMsg::Protocol(p) => {
                if let Mode::Edit(f) = &mut c.mode {
                    // A typed default port follows the protocol.
                    if f.port.trim() == f.protocol.default_port().to_string() {
                        f.port.clear();
                    }
                    f.protocol = p;
                }
            }
            ConnectMsg::SavePassword(on) => {
                if let Mode::Edit(f) = &mut c.mode {
                    f.save_password = on;
                }
            }
            ConnectMsg::SaveForm => return self.save_form(),
            ConnectMsg::Back => {
                c.mode = Mode::List;
                c.note = None;
            }
            ConnectMsg::AskedInput(value) => {
                if let Mode::Password { input, .. } = &mut c.mode {
                    *input = value;
                }
            }
            ConnectMsg::AskedSubmit => {
                if let Mode::Password { site, input } = std::mem::replace(&mut c.mode, Mode::List) {
                    c.selected = site;
                    return self.connect_selected(Some(input));
                }
            }
            ConnectMsg::Trust => return self.connect_trust(),
            ConnectMsg::Done(seq, result) => return self.connect_done(seq, result),
        }
        Task::none()
    }

    /// Keys of `connect_key`, for the F1 help. Keep in sync.
    pub(super) const CONNECT_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Enter, double-click", "Connect in the panel the tool was opened from"),
        ("Up, Down, Home, End", "Pick a saved connection"),
        ("N, Ins", "New connection"),
        ("E, F4", "Edit the connection"),
        ("Del, Del", "Delete the connection (press twice)"),
        ("Tab, Shift+Tab", "Next / previous field (editing)"),
        ("Ctrl+P", "Next protocol: SFTP, FTP, FTPS (editing)"),
        ("Enter (editing)", "Save the connection"),
        ("Y (host key question)", "Trust the server's host key and connect"),
        ("Esc", "Back to the list / stop connecting / close"),
    ];

    pub(super) fn connect_key(
        &mut self,
        named: Option<Named>,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
        captured: bool,
    ) -> Task<Message> {
        let Some(c) = &mut self.connect else { return Task::none() };
        let ch = match key {
            keyboard::Key::Character(s) if !mods.control() && !mods.alt() => Some(s.to_ascii_lowercase()),
            _ => None,
        };
        match &mut c.mode {
            Mode::List => match named {
                Some(Named::Escape) => self.connect_close(),
                Some(Named::Enter) if !captured => return self.connect_selected(None),
                Some(Named::ArrowDown) => self.connect_move(1),
                Some(Named::ArrowUp) => self.connect_move(-1),
                Some(Named::Home | Named::PageUp) => self.connect_move(isize::MIN / 2),
                Some(Named::End | Named::PageDown) => self.connect_move(isize::MAX / 2),
                Some(Named::Insert) => return self.connect_update(ConnectMsg::New),
                Some(Named::F4) => return self.connect_update(ConnectMsg::Edit),
                Some(Named::Delete) => self.delete_site(),
                _ => match ch.as_deref() {
                    Some("n") => return self.connect_update(ConnectMsg::New),
                    Some("e") => return self.connect_update(ConnectMsg::Edit),
                    _ => {}
                },
            },
            Mode::Edit(f) => match named {
                Some(Named::Escape) => return self.connect_update(ConnectMsg::Back),
                Some(Named::Tab) if mods.shift() => return iced::widget::focus_previous(),
                Some(Named::Tab) => return iced::widget::focus_next(),
                Some(Named::Enter) if !captured => return self.save_form(),
                _ if mods.control() && matches!(key, keyboard::Key::Character(s) if s.eq_ignore_ascii_case("p")) => {
                    let i = Protocol::ALL.iter().position(|p| *p == f.protocol).unwrap_or(0);
                    let next = Protocol::ALL[(i + 1) % Protocol::ALL.len()];
                    return self.connect_update(ConnectMsg::Protocol(next));
                }
                _ => {}
            },
            Mode::Password { .. } => match named {
                Some(Named::Escape) => return self.connect_update(ConnectMsg::Back),
                Some(Named::Enter) if !captured => return self.connect_update(ConnectMsg::AskedSubmit),
                _ => {}
            },
            Mode::Connecting { .. } => {
                if named == Some(Named::Escape) {
                    c.mode = Mode::List;
                    c.note = Some(("Cancelled".into(), Tone::Info));
                }
            }
            Mode::Trust { .. } => match (named, ch.as_deref()) {
                (_, Some("y")) => return self.connect_trust(),
                (Some(Named::Escape), _) | (_, Some("n")) => {
                    c.mode = Mode::List;
                    c.note = Some(("Not connected: the host key wasn't trusted".into(), Tone::Info));
                }
                _ => {}
            },
        }
        Task::none()
    }

    pub(super) fn connect_view<'a>(&'a self, c: &'a ConnectTool) -> Element<'a, Message> {
        let colors = theme::c();
        let msg = Message::Connect;
        let action = |label: &'a str, m: ConnectMsg, primary: bool| {
            button(text(label).size(13)).on_press(msg(m)).padding([6, 14]).style(if primary {
                theme::primary
            } else {
                theme::fkey
            })
        };
        let header = container(
            row![
                text("Connect to server").size(16),
                Space::with_width(Length::Fill),
                text(format!("in the {} panel", if c.side == 0 { "left" } else { "right" })).size(12).color(colors.muted),
            ]
            .align_y(Alignment::Center),
        )
        .padding([10, 14])
        .style(filled(colors.header));

        let body: Element<'a, Message> = match &c.mode {
            Mode::Edit(f) => self.connect_form(f),
            Mode::Password { site, input } => {
                let site = &c.sites[*site];
                let what = if site.key_file.trim().is_empty() || site.protocol() != Protocol::Sftp {
                    "Password"
                } else {
                    "Key passphrase"
                };
                let field = text_input("", input)
                    .id(password_id())
                    .secure(true)
                    .on_input(move |v| msg(ConnectMsg::AskedInput(v)))
                    .on_submit(msg(ConnectMsg::AskedSubmit))
                    .padding(8)
                    .size(13)
                    .style(theme::input);
                let saved = if site.save_password {
                    "It isn't saved: edit the connection to save it."
                } else {
                    "Kept in memory until RustCommander closes; not saved."
                };
                container(
                    column![
                        text(format!("{what} for {}", site.address())).size(14),
                        field,
                        text(saved).size(12).color(colors.muted),
                        row![action("Cancel", ConnectMsg::Back, false), action("Connect", ConnectMsg::AskedSubmit, true)]
                            .spacing(8),
                    ]
                    .spacing(10)
                    .max_width(520),
                )
                .padding(20)
                .into()
            }
            Mode::Trust { text: question, .. } => container(
                column![
                    text("Unknown server").size(15),
                    text(question.clone()).size(13),
                    text(
                        "This is the first connection to this server (or its address changed). Check the fingerprint \
                         with the server's administrator. Trusting saves the key in known_hosts; a different key later \
                         is refused."
                    )
                    .size(12)
                    .color(colors.muted),
                    row![action("Cancel (N)", ConnectMsg::Back, false), action("Trust and connect (Y)", ConnectMsg::Trust, true)]
                        .spacing(8),
                ]
                .spacing(10)
                .max_width(640),
            )
            .padding(20)
            .into(),
            Mode::List | Mode::Connecting { .. } => {
                let rows = c.sites.iter().enumerate().map(|(i, s)| {
                    let bg = if i == c.selected {
                        colors.cursor
                    } else if i % 2 == 1 {
                        colors.zebra
                    } else {
                        colors.panel
                    };
                    let cell = |t: String, w: Length, color| {
                        container(text(t).size(13).color(color).wrapping(Wrapping::None)).width(w).clip(true)
                    };
                    let saved = if s.save_password {
                        "password saved"
                    } else if s.asks_password() {
                        "asks password"
                    } else {
                        "key file"
                    };
                    mouse_area(
                        container(
                            row![
                                cell(s.name.clone(), Length::FillPortion(2), colors.text),
                                cell(s.address(), Length::FillPortion(3), colors.dir),
                                cell(s.folder.clone(), Length::FillPortion(2), colors.muted),
                                cell(saved.into(), Length::Fixed(120.0), colors.muted),
                            ]
                            .spacing(8),
                        )
                        .height(Length::Fixed(ROW_H))
                        .padding([2, 14])
                        .style(filled(bg)),
                    )
                    .on_press(msg(ConnectMsg::Click(i)))
                    .into()
                });
                let list: Element<'a, Message> = if c.sites.is_empty() {
                    container(text("No saved connections yet: press N (or Ins) to add one.").size(13).color(colors.muted))
                        .padding(14)
                        .into()
                } else {
                    Column::with_children(rows).into()
                };
                scrollbar::thin(list).height(Length::Fill).into()
            }
        };
        let body = container(body).width(Length::Fill).height(Length::Fill).style(filled(colors.panel));

        let (note, color) = match &c.note {
            Some((t, tone)) => (t.clone(), match tone {
                Tone::Error => colors.error,
                Tone::Success => colors.success,
                Tone::Info => colors.text,
            }),
            None => (
                match c.mode {
                    Mode::Edit(_) => "Tab next field \u{b7} Ctrl+P protocol \u{b7} Enter save \u{b7} Esc back",
                    Mode::Password { .. } => "Enter connect \u{b7} Esc back",
                    Mode::Trust { .. } => "Y trust and connect \u{b7} N or Esc cancel",
                    _ => "Enter connect \u{b7} N new \u{b7} E edit \u{b7} Del delete \u{b7} Esc close",
                }
                .to_owned(),
                colors.muted,
            ),
        };
        let mut buttons = Row::new().spacing(8);
        match &c.mode {
            Mode::List => {
                buttons = buttons
                    .push(action("New", ConnectMsg::New, false))
                    .push(action("Edit", ConnectMsg::Edit, false))
                    .push(action("Delete", ConnectMsg::Delete, false))
                    .push(action("Close", ConnectMsg::Close, false))
                    .push(action("Connect", ConnectMsg::Connect, true));
            }
            Mode::Edit(_) => {
                buttons = buttons.push(action("Cancel", ConnectMsg::Back, false)).push(action("Save", ConnectMsg::SaveForm, true));
            }
            Mode::Connecting { .. } => buttons = buttons.push(action("Stop", ConnectMsg::Back, false)),
            _ => {}
        }
        let footer = container(
            row![
                container(text(note).size(12).color(color).wrapping(Wrapping::None)).width(Length::Fill).clip(true),
                Space::with_width(Length::Fixed(8.0)),
                buttons
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(filled(colors.header));

        container(column![header, body, footer]).width(Length::Fill).height(Length::Fill).style(filled(colors.bg)).into()
    }

    fn connect_form<'a>(&'a self, f: &'a Form) -> Element<'a, Message> {
        let colors = theme::c();
        let msg = Message::Connect;
        let label = |s: &'a str| text(s).size(13).color(colors.muted).width(Length::Fixed(LABEL_W));
        let input = |field: Field, placeholder: &'a str, value: &'a str| {
            text_input(placeholder, value)
                .id(input_id(field))
                .on_input(move |v| msg(ConnectMsg::Input(field, v)))
                .on_submit(msg(ConnectMsg::SaveForm))
                .padding(6)
                .size(13)
                .style(theme::input)
        };
        let line = |name: &'a str, widget: Element<'a, Message>| row![label(name), widget].spacing(8).align_y(Alignment::Center);
        let protocols = Protocol::ALL.iter().fold(Row::new().spacing(6), |r, &p| {
            r.push(
                button(text(protocol_label(p)).size(13))
                    .on_press(msg(ConnectMsg::Protocol(p)))
                    .padding([4, 12])
                    .style(theme::tab(p == f.protocol)),
            )
        });
        let port_hint = match f.protocol {
            Protocol::Sftp => "22",
            Protocol::Ftp | Protocol::Ftps => "21",
        };
        let password_hint = match (f.index.is_some(), f.save_password) {
            (true, true) => "(saved: type to change)",
            _ => "",
        };
        let mut form = column![
            line("Name", input(Field::Name, "Shown in the list (default: the host)", &f.name).into()),
            line("Protocol", protocols.into()),
            line("Host", input(Field::Host, "server.example.org or 192.168.1.10", &f.host).into()),
            line("Port", input(Field::Port, port_hint, &f.port).width(Length::Fixed(100.0)).into()),
            line("User", input(Field::User, if f.protocol == Protocol::Sftp { "" } else { "empty: anonymous" }, &f.user).into()),
            line("Password", input(Field::Password, password_hint, &f.password).secure(true).into()),
            line(
                "",
                checkbox("Save the password in the Windows Credential Manager (else it's asked when connecting)", f.save_password)
                    .on_toggle(move |on| msg(ConnectMsg::SavePassword(on)))
                    .size(14)
                    .text_size(13)
                    .into()
            ),
        ]
        .spacing(10);
        if f.protocol == Protocol::Sftp {
            form = form.push(line(
                "Private key file",
                input(Field::KeyFile, "Optional: C:\\Users\\me\\.ssh\\id_ed25519 (password = its passphrase)", &f.key_file).into(),
            ));
        }
        form = form.push(line("Remote folder", input(Field::Folder, "Optional: /home/me (empty: the login folder)", &f.folder).into()));
        if f.protocol == Protocol::Ftps {
            form = form.push(text("FTPS uses explicit TLS (AUTH TLS); the server's certificate must be valid for the host.").size(12).color(colors.muted));
        }
        container(form.max_width(760)).padding(20).into()
    }
}
