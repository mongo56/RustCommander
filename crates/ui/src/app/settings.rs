//! Settings view (F10, TC's `cm_Configuration`): fills the window with three
//! pages. General: the F4 editor, hidden files, the compare-folders options.
//! Key bindings: every command with its effective keys; Enter waits for a
//! key combination and binds it (asking first when another command has
//! it), Ins adds one, Del unbinds, R resets. Colours: the theme presets
//! and every colour as an editable `#rrggbb` with a swatch. Each change is
//! written to `config.toml` at once (`config::write`, comments kept) and
//! applied live, the compare options (`[compare]`) included.

use super::*;
use crate::config::Edit;
use crate::theme::filled;
use iced::widget::scrollable::RelativeOffset;
use iced::widget::text::Wrapping;
use iced::widget::{button, checkbox, column, container, mouse_area, row, scrollable, text, Column, Row, Space};
use iced::{Alignment, Element, Length};

const ROW_H: f32 = 26.0;
const LABEL_W: f32 = 220.0;
const KEYS_W: f32 = 240.0;

fn list_id() -> scrollable::Id {
    scrollable::Id::new("settings-list")
}

fn terminal_id() -> text_input::Id {
    text_input::Id::new("settings-terminal")
}

fn shell_id() -> text_input::Id {
    text_input::Id::new("settings-shell")
}

fn editor_id() -> text_input::Id {
    text_input::Id::new("settings-editor")
}

fn filter_id() -> text_input::Id {
    text_input::Id::new("settings-filter")
}

fn hex_id(i: usize) -> text_input::Id {
    text_input::Id::new(format!("settings-hex-{i}"))
}

/// The commands the key bindings page lists: those matching the words typed in
/// the filter box (in the description, the id or the keys, like the command
/// palette; every word must match), in the usual order. Empty on other pages.
fn shown_commands(keymap: &keymap::Keymap, s: &Settings) -> Vec<Command> {
    if s.page != Page::Keys {
        return Vec::new();
    }
    filter_commands(&s.filter, &keymap.bindings())
}

/// `Command::ALL` narrowed to the commands matching `query` (all when it has no words).
fn filter_commands(query: &str, bindings: &[(String, Command)]) -> Vec<Command> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    Command::ALL
        .iter()
        .copied()
        .filter(|&cmd| {
            let keys = keymap::Keymap::keys_for(bindings, cmd).join(", ");
            words.iter().all(|w| palette::word_score(w, cmd, &keys).is_some())
        })
        .collect()
}

/// Focusing an id no widget has takes the focus from every text box.
fn blur() -> Task<Message> {
    text_input::focus(text_input::Id::new("settings-none"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    General,
    Keys,
    Colors,
}

impl Page {
    const ALL: [Page; 3] = [Page::General, Page::Keys, Page::Colors];

    fn title(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Keys => "Key bindings",
            Page::Colors => "Colours",
        }
    }

    fn index(self) -> usize {
        Page::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

/// Rows of the General page.
const GENERAL_ROWS: usize = 15;
const ROW_EDITOR: usize = 0;
const ROW_TERMINAL: usize = 1;
const ROW_SHELL: usize = 2;
const ROW_MARKDOWN: usize = 3;
const ROW_SCRIPTS: usize = 4;
const ROW_HIDDEN: usize = 5;
const ROW_GIT: usize = 6;
const ROW_TREE: usize = 7;
const ROW_FKEYS: usize = 8;
const ROW_UPDATES: usize = 9;
const ROW_RENDER: usize = 10;
/// Compare options 0..3 (`toggle_compare_option`) are rows 11..14.
const ROW_COMPARE: usize = 11;
const ROW_CONFIG: usize = 14;

/// General rows that are text boxes (Enter there saves instead of toggling).
fn text_row(row: usize) -> bool {
    matches!(row, ROW_EDITOR | ROW_TERMINAL | ROW_SHELL)
}

#[derive(Debug, Clone)]
pub enum SettingsMsg {
    Page(Page),
    /// A row of the current page was clicked.
    Click(usize),
    Editor(String),
    SaveEditor,
    /// Open the file dialog for the editor program.
    Browse,
    /// A background job failed; nothing to do.
    Nothing,
    Picked(Option<std::path::PathBuf>),
    /// Installed editors found in the background.
    Detected(Vec<rc_platform::FoundEditor>),
    /// Use the detected editor with this index.
    UseEditor(usize),
    /// Installed terminals found in the background.
    DetectedTerminals(Vec<rc_platform::FoundTerminal>),
    /// Use the detected terminal with this index.
    UseTerminal(usize),
    /// Open the file dialog for the terminal program.
    BrowseTerminal,
    PickedTerminal(Option<std::path::PathBuf>),
    Terminal(String),
    /// The built-in terminal's shell command being typed.
    Shell(String),
    /// The key bindings filter being typed.
    Filter(String),
    ShowHidden(bool),
    MarkdownViewer(bool),
    ScriptsInTerminal(bool),
    GitStatus(bool),
    ShowFkeys(bool),
    HotlistTree(bool),
    CheckUpdates(bool),
    SoftwareRendering(bool),
    /// Toggle compare option 0..3.
    Compare(u8),
    EditConfig,
    /// Bind a key to the selected command: `add` keeps its other keys.
    Capture { add: bool },
    Unbind,
    ResetKey,
    ResetAllKeys,
    /// Bind the key that was found taken anyway.
    Confirm,
    CancelCapture,
    Theme(&'static str),
    Hex(usize, String),
    ResetColor(usize),
    /// Open the colour picker for colour `i`.
    PickOpen(usize),
    PickHue(f32),
    PickSat(f32),
    PickVal(f32),
    PickHex(String),
    PickOk,
    PickCancel,
    Close,
}

/// The colour picker: hue / saturation / value sliders with a live preview
/// (the colour is only applied, not saved, until OK).
#[derive(Debug)]
struct Picker {
    /// Colour index in `theme::NAMES`.
    i: usize,
    /// The colours in use when it opened, put back on cancel.
    saved: theme::Colors,
    /// Hue 0..360, saturation and value 0..100.
    h: f32,
    s: f32,
    v: f32,
    hex: String,
}

impl Picker {
    fn color(&self) -> iced::Color {
        theme::from_hsv(self.h, self.s / 100.0, self.v / 100.0)
    }
}

/// Waiting for the key combination to bind to `cmd`.
#[derive(Debug)]
struct Capture {
    cmd: Command,
    add: bool,
    /// The pressed key (canonical) and the command that has it now, while
    /// asking whether to take it.
    taken: Option<(String, Command)>,
}

#[derive(Debug)]
pub(super) struct Settings {
    page: Page,
    /// Selected row per page.
    row: [usize; 3],
    /// The editor command being typed.
    editor: String,
    /// Installed editors found on this machine (filled in the background).
    editors: Vec<rc_platform::FoundEditor>,
    /// The terminal command being typed.
    terminal: String,
    /// Installed terminals found on this machine (filled in the background).
    terminals: Vec<rc_platform::FoundTerminal>,
    /// The built-in terminal's shell command being typed.
    shell: String,
    /// The colours being typed, by `theme::NAMES` index.
    hex: Vec<String>,
    /// The key bindings filter (not saved).
    filter: String,
    capture: Option<Capture>,
    picker: Option<Picker>,
    /// Ctrl+R pressed once on the keys page (twice resets every key).
    reset_all: bool,
    note: Option<(String, Tone)>,
}

fn current_hex() -> Vec<String> {
    let colors = theme::c();
    theme::NAMES.iter().map(|n| colors.get(n).map(theme::to_hex).unwrap_or_default()).collect()
}

impl App {
    pub(super) fn open_settings(&mut self) -> Task<Message> {
        self.leave_edits();
        self.refresh_config();
        self.screen = Some(Screen::Settings(Box::new(Settings {
            page: Page::General,
            row: [0; 3],
            editor: self.config.editor.clone(),
            editors: Vec::new(),
            terminal: self.config.terminal.clone(),
            terminals: Vec::new(),
            shell: self.config.builtin_shell.clone(),
            hex: current_hex(),
            filter: String::new(),
            capture: None,
            picker: None,
            reset_all: false,
            note: None,
        })));
        Task::batch([
            scrollable::snap_to(list_id(), RelativeOffset::START),
            off_thread(rc_platform::detect_editors, |found| Message::Settings(SettingsMsg::Detected(found))),
            off_thread(rc_platform::detect_terminals, |found| {
                Message::Settings(SettingsMsg::DetectedTerminals(found))
            }),
        ])
    }

    /// Rows of `page`; `shown` is the number of key bindings the filter lets through.
    fn rows_of(page: Page, shown: usize) -> usize {
        match page {
            Page::General => GENERAL_ROWS,
            Page::Keys => shown,
            // The theme row, then the colours.
            Page::Colors => 1 + theme::NAMES.len(),
        }
    }

    /// Writes `edits` to `config.toml`, applies the result and reports
    /// `done` (or the error) in the footer.
    fn settings_write(&mut self, edits: &[Edit], done: String) {
        let result = crate::config::write(edits).and_then(|()| self.reload_config());
        if let Some(s) = self.screen.settings_mut() {
            s.note = Some(match result {
                Ok(()) => (done, Tone::Success),
                Err(e) => (format!("Not saved: {e}"), Tone::Error),
            });
        }
    }

    /// Makes `path` the editor program (quoted when it has spaces) and saves.
    fn settings_use_editor(&mut self, path: &std::path::Path) -> Task<Message> {
        let text = path.display().to_string();
        let text = if text.contains(' ') { format!("\"{text}\"") } else { text };
        if let Some(s) = self.screen.settings_mut() {
            s.row[0] = ROW_EDITOR;
            s.editor = text;
        }
        self.settings_save_editor();
        Task::none()
    }

    /// Makes `command` the terminal command line and saves.
    fn settings_use_terminal(&mut self, command: String) -> Task<Message> {
        if let Some(s) = self.screen.settings_mut() {
            s.row[0] = ROW_TERMINAL;
            s.terminal = command;
        }
        self.settings_save_editor();
        Task::none()
    }

    /// Saves changed editor / terminal / shell commands (on Enter, leaving the page, closing).
    fn settings_save_editor(&mut self) {
        let Some(s) = self.screen.settings() else { return };
        if s.editor != self.config.editor {
            let editor = s.editor.trim().to_owned();
            self.settings_write(&[Edit::Text("editor", editor)], "Editor saved".into());
        }
        let Some(s) = self.screen.settings() else { return };
        if s.terminal != self.config.terminal {
            let terminal = s.terminal.trim().to_owned();
            self.settings_write(&[Edit::Text("terminal", terminal)], "Terminal saved".into());
        }
        let Some(s) = self.screen.settings() else { return };
        if s.shell != self.config.builtin_shell {
            let shell = s.shell.trim().to_owned();
            self.settings_write(&[Edit::Text("builtin_shell", shell)], "Built-in terminal shell saved".into());
        }
    }

    /// Closes the colour picker; `keep` false puts the old colours back.
    fn picker_close(&mut self, keep: bool) {
        if let Some(p) = self.screen.settings_mut().and_then(|s| s.picker.take()) {
            if !keep {
                theme::set(p.saved);
            }
        }
    }

    /// Applies the picker's colour to the running app (not saved).
    fn picker_preview(&mut self) {
        let Some(p) = self.screen.settings_mut().and_then(|s| s.picker.as_mut()) else { return };
        let color = p.color();
        p.hex = theme::to_hex(color);
        theme::set(p.saved.with_color(theme::NAMES[p.i], color));
    }

    fn settings_close(&mut self) {
        self.picker_close(false);
        self.settings_save_editor();
        if matches!(self.screen, Some(Screen::Settings(_))) {
            self.screen = None;
        }
    }

    /// Keeps the selected row in view.
    fn settings_snap(&self) -> Task<Message> {
        let Some(s) = self.screen.settings() else { return Task::none() };
        let rows = Self::rows_of(s.page, shown_commands(&self.keymap, s).len());
        let y = if rows > 1 { s.row[s.page.index()] as f32 / (rows - 1) as f32 } else { 0.0 };
        scrollable::snap_to(list_id(), RelativeOffset { x: 0.0, y })
    }

    fn settings_page(&mut self, page: Page) -> Task<Message> {
        self.picker_close(false);
        self.settings_save_editor();
        let Some(s) = self.screen.settings_mut() else { return Task::none() };
        s.page = page;
        s.capture = None;
        s.reset_all = false;
        s.hex = current_hex();
        Task::batch([blur(), self.settings_snap()])
    }

    /// Enter / Space / a click on the selected row.
    fn settings_activate(&mut self) -> Task<Message> {
        let Some(s) = self.screen.settings() else { return Task::none() };
        let row = s.row[s.page.index()];
        match s.page {
            Page::General => match row {
                ROW_EDITOR => Task::batch([text_input::focus(editor_id()), text_input::move_cursor_to_end(editor_id())]),
                ROW_TERMINAL => {
                    Task::batch([text_input::focus(terminal_id()), text_input::move_cursor_to_end(terminal_id())])
                }
                ROW_SHELL => Task::batch([text_input::focus(shell_id()), text_input::move_cursor_to_end(shell_id())]),
                ROW_MARKDOWN => self.settings_update(SettingsMsg::MarkdownViewer(!self.config.markdown_in_viewer)),
                ROW_SCRIPTS => {
                    self.settings_update(SettingsMsg::ScriptsInTerminal(!self.config.run_scripts_in_terminal))
                }
                ROW_GIT => self.settings_update(SettingsMsg::GitStatus(!self.config.git_status)),
                ROW_HIDDEN => self.settings_update(SettingsMsg::ShowHidden(!self.config.show_hidden)),
                ROW_FKEYS => self.settings_update(SettingsMsg::ShowFkeys(!self.config.show_fkeys)),
                ROW_TREE => self.settings_update(SettingsMsg::HotlistTree(!self.config.hotlist_tree)),
                ROW_UPDATES => {
                    self.settings_update(SettingsMsg::CheckUpdates(!self.config.check_updates_at_startup))
                }
                ROW_RENDER => {
                    self.settings_update(SettingsMsg::SoftwareRendering(!self.config.software_rendering))
                }
                ROW_CONFIG => self.settings_update(SettingsMsg::EditConfig),
                _ => self.settings_update(SettingsMsg::Compare((row - ROW_COMPARE) as u8)),
            },
            Page::Keys => self.settings_update(SettingsMsg::Capture { add: false }),
            Page::Colors if row == 0 => {
                let i = theme::PRESETS.iter().position(|p| theme::preset(&self.config.theme).is_some_and(|q| q.id == p.id));
                let next = theme::PRESETS[i.map_or(0, |i| (i + 1) % theme::PRESETS.len())].id;
                self.settings_update(SettingsMsg::Theme(next))
            }
            Page::Colors => Task::batch([text_input::focus(hex_id(row - 1)), text_input::select_all(hex_id(row - 1))]),
        }
    }

    /// Binds `name` to the command being captured.
    fn settings_bind(&mut self, name: String) {
        let Some(c) = self.screen.settings_mut().and_then(|s| s.capture.take()) else { return };
        let mut edits = Vec::new();
        if !c.add {
            edits.extend(self.keymap.unbind(c.cmd).into_iter().filter(|(n, _)| *n != name));
        }
        edits.push(self.keymap.bind(&name, Some(c.cmd)));
        let mut done = format!("{} now runs: {}", keymap::display_key(&name), c.cmd.description());
        if let Some((_, other)) = c.taken {
            done.push_str(&format!(" (was: {})", other.description()));
        }
        let edits: Vec<Edit> = edits.into_iter().map(Edit::Key).collect();
        self.settings_write(&edits, done);
    }

    /// A key pressed while waiting for a key combination to bind.
    fn settings_capture_key(&mut self, named: Option<Named>, key: &keyboard::Key, mods: keyboard::Modifiers) {
        let Some(s) = self.screen.settings_mut() else { return };
        let Some(c) = &mut s.capture else { return };
        match named {
            Some(Named::Escape) => {
                s.capture = None;
                s.note = Some(("Cancelled".into(), Tone::Info));
                return;
            }
            Some(Named::Enter) if c.taken.is_some() => {
                let name = c.taken.as_ref().map(|(n, _)| n.clone()).unwrap_or_default();
                return self.settings_bind(name);
            }
            _ => {}
        }
        let Some(name) = keymap::key_name(key, mods).filter(|n| keymap::parse_key(n).as_deref() == Some(n.as_str())) else {
            s.note = Some(("That key can't be bound: try another".into(), Tone::Error));
            return;
        };
        if matches!(key, keyboard::Key::Character(_)) && !mods.control() && !mods.alt() && !mods.logo() {
            s.note = Some((
                format!("{} types in a quick search: use it with Ctrl or Alt", keymap::display_key(&name)),
                Tone::Error,
            ));
            return;
        }
        match self.keymap.command_of(&name) {
            Some(cmd) if cmd == c.cmd => {
                s.capture = None;
                s.note = Some((format!("{} already runs this", keymap::display_key(&name)), Tone::Info));
            }
            Some(other) => {
                s.note = Some((
                    format!(
                        "{} runs \"{}\": Enter takes it over, Esc cancels, or press another key",
                        keymap::display_key(&name),
                        other.description()
                    ),
                    Tone::Error,
                ));
                c.taken = Some((name, other));
            }
            None => self.settings_bind(name),
        }
    }

    pub(super) fn settings_update(&mut self, msg: SettingsMsg) -> Task<Message> {
        if matches!(msg, SettingsMsg::Click(_) | SettingsMsg::Theme(_) | SettingsMsg::ResetColor(_)) {
            self.picker_close(false);
        }
        let Some(s) = self.screen.settings_mut() else { return Task::none() };
        if !matches!(msg, SettingsMsg::ResetAllKeys) {
            s.reset_all = false;
        }
        // The commands the keys page lists and the selected one.
        let shown = shown_commands(&self.keymap, s);
        let selected = shown.get(s.row[Page::Keys.index()]).copied();
        match msg {
            SettingsMsg::Page(page) => return self.settings_page(page),
            SettingsMsg::Click(i) => {
                let i = i.min(Self::rows_of(s.page, shown.len()).saturating_sub(1));
                let again = s.row[s.page.index()] == i;
                s.row[s.page.index()] = i;
                if s.capture.is_some() {
                    s.capture = None;
                    s.note = None;
                }
                // A second click on a row acts like Enter (toggles a switch,
                // waits for a key, picks the next theme).
                if again && !(s.page == Page::General && text_row(i)) && !(s.page == Page::Colors && i > 0) {
                    return self.settings_activate();
                }
                if s.page == Page::General && !text_row(i) {
                    self.settings_save_editor();
                }
            }
            SettingsMsg::Editor(value) => s.editor = value,
            SettingsMsg::Terminal(value) => s.terminal = value,
            SettingsMsg::Shell(value) => s.shell = value,
            SettingsMsg::Filter(value) => {
                s.filter = value;
                s.row[Page::Keys.index()] = 0;
                s.note = None;
                return scrollable::snap_to(list_id(), RelativeOffset::START);
            }
            SettingsMsg::SaveEditor => {
                self.settings_save_editor();
                return blur();
            }
            SettingsMsg::ShowHidden(on) => {
                s.row[0] = ROW_HIDDEN;
                let state = if on { "shown" } else { "hidden" };
                self.settings_write(&[Edit::Flag("show_hidden", on)], format!("Hidden files {state} in every tab"));
                // Every open tab follows; Ctrl+H still toggles one tab.
                for side in &mut self.sides {
                    for tab in &mut side.tabs {
                        tab.show_hidden = on;
                    }
                }
                let paths: Vec<PathBuf> = (0..2).map(|side| self.panel(side).path.clone()).collect();
                return Task::batch(paths.into_iter().map(|p| self.list_in_background(p)).collect::<Vec<_>>());
            }
            SettingsMsg::MarkdownViewer(on) => {
                s.row[0] = ROW_MARKDOWN;
                let state = if on { "open in the viewer" } else { "open in the default app" };
                self.settings_write(&[Edit::Flag("markdown_in_viewer", on)], format!("Markdown files {state}"));
            }
            SettingsMsg::ScriptsInTerminal(on) => {
                s.row[0] = ROW_SCRIPTS;
                let state = if on { "run in the built-in terminal" } else { "open with the default app" };
                self.settings_write(&[Edit::Flag("run_scripts_in_terminal", on)], format!("Scripts {state}"));
            }
            SettingsMsg::GitStatus(on) => {
                s.row[0] = ROW_GIT;
                let state = if on { "shown in git work trees" } else { "off" };
                self.settings_write(&[Edit::Flag("git_status", on)], format!("Git status {state}"));
            }
            SettingsMsg::ShowFkeys(on) => {
                s.row[0] = ROW_FKEYS;
                let state = if on { "shown" } else { "hidden" };
                self.settings_write(&[Edit::Flag("show_fkeys", on)], format!("Function-key bar {state}"));
            }
            SettingsMsg::HotlistTree(on) => {
                s.row[0] = ROW_TREE;
                let state = if on { "on: the button left of the left panel's drives shows it" } else { "off" };
                self.settings_write(&[Edit::Flag("hotlist_tree", on)], format!("Hotlist tree {state}"));
            }
            SettingsMsg::CheckUpdates(on) => {
                s.row[0] = ROW_UPDATES;
                let state = if on { "on" } else { "off" };
                self.settings_write(&[Edit::Flag("check_updates_at_startup", on)], format!("Update check at startup {state}"));
            }
            SettingsMsg::SoftwareRendering(on) => {
                s.row[0] = ROW_RENDER;
                let state = if on { "on (CPU)" } else { "off (GPU)" };
                self.settings_write(
                    &[Edit::Flag("software_rendering", on)],
                    format!("Software rendering {state} from the next start"),
                );
            }
            SettingsMsg::Compare(i) => {
                s.row[0] = ROW_COMPARE + i as usize;
                toggle_compare_option(&mut self.compare_opts, i);
                let note = match self.save_compare() {
                    Ok(()) => ("Compare options saved".into(), Tone::Success),
                    Err(e) => (format!("Not saved: {e}"), Tone::Error),
                };
                if let Some(s) = self.screen.settings_mut() {
                    s.note = Some(note);
                }
            }
            SettingsMsg::Browse => {
                s.row[0] = ROW_EDITOR;
                return off_thread(rc_platform::pick_program, |p| Message::Settings(SettingsMsg::Picked(p)));
            }
            SettingsMsg::Picked(path) => {
                if let Some(path) = path {
                    return self.settings_use_editor(&path);
                }
            }
            SettingsMsg::Detected(found) => s.editors = found,
            SettingsMsg::Nothing => {}
            SettingsMsg::UseEditor(i) => {
                if let Some(found) = s.editors.get(i) {
                    let path = found.path.clone();
                    return self.settings_use_editor(&path);
                }
            }
            SettingsMsg::DetectedTerminals(found) => s.terminals = found,
            SettingsMsg::UseTerminal(i) => {
                if let Some(found) = s.terminals.get(i) {
                    let command = found.command();
                    return self.settings_use_terminal(command);
                }
            }
            SettingsMsg::BrowseTerminal => {
                s.row[0] = ROW_TERMINAL;
                return off_thread(rc_platform::pick_program, |p| Message::Settings(SettingsMsg::PickedTerminal(p)));
            }
            SettingsMsg::PickedTerminal(path) => {
                if let Some(path) = path {
                    let text = path.display().to_string();
                    let text = if text.contains(' ') { format!("\"{text}\"") } else { text };
                    return self.settings_use_terminal(text);
                }
            }
            SettingsMsg::EditConfig => {
                s.row[0] = ROW_CONFIG;
                self.settings_save_editor();
                let result = crate::config::create_if_missing("", Some(self.compare_opts.into())).map_err(|e| e.to_string());
                if let (Ok(()), Some(path)) = (&result, crate::config::file()) {
                    self.edit_file(&path, "config.toml");
                }
                let note = match result {
                    Ok(()) => self.status.clone().unwrap_or(("Editing config.toml".into(), Tone::Success)),
                    Err(e) => (format!("Can't create config.toml: {e}"), Tone::Error),
                };
                if let Some(s) = self.screen.settings_mut() {
                    s.note = Some(note);
                }
            }
            SettingsMsg::Capture { add } => {
                let Some(cmd) = selected else { return Task::none() };
                s.capture = Some(Capture { cmd, add, taken: None });
                let what = if add { "to add" } else { "for" };
                s.note = Some((format!("Press the key combination {what} \"{}\" (Esc cancels)", cmd.description()), Tone::Info));
                // The filter box may have the focus: it must not take the key.
                return blur();
            }
            SettingsMsg::Confirm => {
                if let Some((name, _)) = s.capture.as_ref().and_then(|c| c.taken.clone()) {
                    self.settings_bind(name);
                }
            }
            SettingsMsg::CancelCapture => {
                s.capture = None;
                s.note = None;
            }
            SettingsMsg::Unbind => {
                let Some(cmd) = selected else { return Task::none() };
                let edits: Vec<Edit> = self.keymap.unbind(cmd).into_iter().map(Edit::Key).collect();
                if edits.is_empty() {
                    s.note = Some(("No keys to remove".into(), Tone::Info));
                } else {
                    self.settings_write(&edits, format!("\"{}\" has no key now", cmd.description()));
                }
            }
            SettingsMsg::ResetKey => {
                let Some(cmd) = selected else { return Task::none() };
                let edits: Vec<Edit> = self.keymap.reset(cmd).into_iter().map(Edit::Key).collect();
                if edits.is_empty() {
                    s.note = Some(("Already the default keys".into(), Tone::Info));
                } else {
                    self.settings_write(&edits, format!("\"{}\" has its default keys again", cmd.description()));
                }
            }
            SettingsMsg::ResetAllKeys => {
                if !s.reset_all {
                    s.reset_all = true;
                    s.note = Some(("Press Ctrl+R (or click) again to reset every key to the default".into(), Tone::Error));
                    return Task::none();
                }
                s.reset_all = false;
                let edits: Vec<Edit> = Command::ALL.iter().flat_map(|&c| self.keymap.reset(c)).map(Edit::Key).collect();
                self.settings_write(&edits, "Every key has its default again".into());
            }
            SettingsMsg::Theme(id) => {
                s.row[2] = 0;
                let name = theme::preset(id).map_or(id, |p| p.name);
                self.settings_write(&[Edit::Text("theme", id.to_owned()), Edit::ClearColors], format!("Theme: {name}"));
                if let Some(s) = self.screen.settings_mut() {
                    s.hex = current_hex();
                }
            }
            SettingsMsg::Hex(i, value) => {
                s.row[2] = i + 1;
                let Some(color) = theme::parse_hex(&value) else {
                    s.hex[i] = value;
                    return Task::none();
                };
                s.hex[i] = value;
                let name = theme::NAMES[i];
                let preset = theme::preset(&self.config.theme).map(|p| p.colors);
                let hex = theme::to_hex(color);
                let edit = if preset.and_then(|p| p.get(name)) == Some(color) { None } else { Some(hex.clone()) };
                self.settings_write(&[Edit::Color(name.to_owned(), edit)], format!("{name} = {hex}"));
            }
            SettingsMsg::ResetColor(i) => {
                s.row[2] = i + 1;
                let name = theme::NAMES[i];
                self.settings_write(&[Edit::Color(name.to_owned(), None)], format!("{name}: the theme's colour"));
                if let Some(s) = self.screen.settings_mut() {
                    s.hex = current_hex();
                }
                return blur();
            }
            SettingsMsg::PickOpen(i) => {
                s.row[2] = i + 1;
                // Another picker still open: its preview is dropped first.
                if let Some(old) = s.picker.take() {
                    theme::set(old.saved);
                }
                let saved = theme::c();
                let (h, sat, v) = theme::to_hsv(saved.get(theme::NAMES[i]).unwrap_or(saved.text));
                let hex = s.hex[i].clone();
                s.picker = Some(Picker { i, saved, h, s: sat * 100.0, v: v * 100.0, hex });
                return blur();
            }
            SettingsMsg::PickHue(x) | SettingsMsg::PickSat(x) | SettingsMsg::PickVal(x) => {
                if let Some(p) = &mut s.picker {
                    match msg {
                        SettingsMsg::PickHue(_) => p.h = x,
                        SettingsMsg::PickSat(_) => p.s = x,
                        _ => p.v = x,
                    }
                    self.picker_preview();
                }
            }
            SettingsMsg::PickHex(value) => {
                if let Some(p) = &mut s.picker {
                    match theme::parse_hex(&value) {
                        Some(color) => {
                            let (h, sat, v) = theme::to_hsv(color);
                            // Grey has no hue: keep the slider where it was.
                            if sat > 0.0 {
                                p.h = h;
                            }
                            p.s = sat * 100.0;
                            p.v = v * 100.0;
                            self.picker_preview();
                            if let Some(p) = self.screen.settings_mut().and_then(|s| s.picker.as_mut()) {
                                p.hex = value;
                            }
                        }
                        None => p.hex = value,
                    }
                }
            }
            SettingsMsg::PickOk => {
                let Some(p) = &s.picker else { return Task::none() };
                if theme::parse_hex(&p.hex).is_none() {
                    s.note = Some(("Type the colour as #rrggbb".into(), Tone::Error));
                    return Task::none();
                }
                let (i, hex) = (p.i, theme::to_hex(p.color()));
                // The saved path decides whether the theme's own colour needs writing.
                self.picker_close(false);
                return self.settings_update(SettingsMsg::Hex(i, hex));
            }
            SettingsMsg::PickCancel => self.picker_close(false),
            SettingsMsg::Close => self.settings_close(),
        }
        Task::none()
    }

    /// Keys of `settings_key`, for the F1 help. Keep in sync.
    pub(super) const SETTINGS_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Tab, Shift+Tab, Ctrl+Tab", "Next / previous page: General, Key bindings, Colours"),
        ("Up, Down, PgUp, PgDn, Home, End", "Pick a row"),
        ("Enter, Space, click twice", "Change the row: edit the text, flip the switch, next theme"),
        ("Ctrl+F, / (key bindings)", "Type in the filter box: words of the description, id or key (F7); Esc clears it"),
        ("Enter (key bindings)", "Press a key combination for the command (replaces its keys)"),
        ("Ins, A (key bindings)", "Add one more key combination"),
        ("Enter (a key that's taken)", "Take the key from the other command"),
        ("Del (key bindings)", "Remove every key of the command"),
        ("R (key bindings)", "Default keys for the command"),
        ("Ctrl+R, Ctrl+R (key bindings)", "Default keys for every command"),
        ("Left, Right (theme row)", "Previous / next theme"),
        ("Enter (a colour)", "Type the colour as #rrggbb: it applies as soon as it's valid"),
        ("Click a swatch (a colour)", "Colour picker: Enter takes the colour, Esc keeps the old one"),
        ("Del (a colour)", "Back to the theme's colour"),
        ("Esc", "Leave a text box / stop waiting for a key / close"),
    ];

    pub(super) fn settings_key(
        &mut self,
        named: Option<Named>,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
        text: Option<&str>,
        captured: bool,
    ) -> Task<Message> {
        let Some(s) = self.screen.settings_mut() else { return Task::none() };
        // The colour picker takes Enter (OK) and Esc (cancel) and nothing else.
        if s.picker.is_some() {
            return match named {
                Some(Named::Enter) => self.settings_update(SettingsMsg::PickOk),
                Some(Named::Escape) => self.settings_update(SettingsMsg::PickCancel),
                _ => Task::none(),
            };
        }
        if s.capture.is_some() {
            self.settings_capture_key(named, key, mods);
            return Task::none();
        }
        // Keys a text box used (typing, Enter, Esc leaving it). On the keys
        // page the filter box is the only one: its Esc clears the text first,
        // and Enter (the box has no submit) binds the selected command's key.
        if captured {
            if s.page == Page::Keys {
                match named {
                    Some(Named::Escape) if !s.filter.is_empty() => {
                        s.filter.clear();
                        s.row[Page::Keys.index()] = 0;
                        // The box lost the focus with the Esc: it keeps it while it has text to clear.
                        return Task::batch([text_input::focus(filter_id()), scrollable::snap_to(list_id(), RelativeOffset::START)]);
                    }
                    Some(Named::Enter) => return self.settings_activate(),
                    _ => {}
                }
            }
            return Task::none();
        }
        let page = s.page;
        let rows = Self::rows_of(page, shown_commands(&self.keymap, s).len());
        let row = s.row[page.index()];
        // `key` is the key without Shift; "/" is Shift+7 on many layouts, so it comes from the typed text.
        let ch = match key {
            keyboard::Key::Character(_) if text == Some("/") && !mods.control() => Some("/".to_owned()),
            keyboard::Key::Character(c) if !mods.alt() && !mods.logo() => Some(c.to_ascii_lowercase()),
            _ => None,
        };
        let go = |s: &mut Settings, to: isize| {
            s.row[page.index()] = to.clamp(0, (rows as isize - 1).max(0)) as usize;
        };
        let page_rows = ((self.window_height - 160.0) / ROW_H).max(1.0) as isize;
        match named {
            Some(Named::Escape) => {
                self.settings_close();
                return Task::none();
            }
            Some(Named::Tab) => {
                let n = Page::ALL.len();
                let next = if mods.shift() { (page.index() + n - 1) % n } else { (page.index() + 1) % n };
                return self.settings_page(Page::ALL[next]);
            }
            Some(Named::ArrowDown) => go(s, row as isize + 1),
            Some(Named::ArrowUp) => go(s, row as isize - 1),
            Some(Named::PageDown) => go(s, row as isize + page_rows),
            Some(Named::PageUp) => go(s, row as isize - page_rows),
            Some(Named::Home) => go(s, 0),
            Some(Named::End) => go(s, rows as isize),
            Some(Named::Enter | Named::Space) if !mods.control() && !(page == Page::Keys && rows == 0) => {
                return self.settings_activate();
            }
            Some(Named::ArrowLeft | Named::ArrowRight) if page == Page::Colors && row == 0 => {
                let n = theme::PRESETS.len();
                let i = theme::PRESETS.iter().position(|p| theme::preset(&self.config.theme).is_some_and(|q| q.id == p.id));
                let i = i.unwrap_or(0);
                let next = if named == Some(Named::ArrowLeft) { (i + n - 1) % n } else { (i + 1) % n };
                return self.settings_update(SettingsMsg::Theme(theme::PRESETS[next].id));
            }
            Some(Named::Insert) if page == Page::Keys => return self.settings_update(SettingsMsg::Capture { add: true }),
            Some(Named::Delete) if page == Page::Keys => return self.settings_update(SettingsMsg::Unbind),
            Some(Named::Delete) if page == Page::Colors && row > 0 => {
                return self.settings_update(SettingsMsg::ResetColor(row - 1));
            }
            _ => match (page, ch.as_deref(), mods.control()) {
                (Page::Keys, Some("f"), true) | (Page::Keys, Some("/"), false) => {
                    return Task::batch([text_input::focus(filter_id()), text_input::select_all(filter_id())]);
                }
                (Page::Keys, Some("a"), false) => return self.settings_update(SettingsMsg::Capture { add: true }),
                (Page::Keys, Some("r"), false) => return self.settings_update(SettingsMsg::ResetKey),
                (Page::Keys, Some("r"), true) => return self.settings_update(SettingsMsg::ResetAllKeys),
                _ => return Task::none(),
            },
        }
        s.reset_all = false;
        if page == Page::General && text_row(row) {
            self.settings_save_editor();
        }
        // The filter box keeps the focus while the list is moved.
        let leave = if page == Page::Keys { Task::none() } else { blur() };
        Task::batch([leave, self.settings_snap()])
    }

    pub(super) fn settings_view<'a>(&'a self, s: &'a Settings) -> Element<'a, Message> {
        let colors = theme::c();
        let msg = Message::Settings;
        let action = |label: &'a str, m: SettingsMsg, primary: bool| {
            button(text(label).size(13)).on_press(msg(m)).padding([6, 14]).style(if primary { theme::primary } else { theme::fkey })
        };
        let tabs = Page::ALL.iter().fold(Row::new().spacing(4), |r, &p| {
            r.push(button(text(p.title()).size(13)).on_press(msg(SettingsMsg::Page(p))).padding([5, 14]).style(theme::tab(p == s.page)))
        });
        let path = crate::config::file().map(|p| p.display().to_string()).unwrap_or_default();
        let header = container(
            row![
                text("Settings").size(16),
                tabs,
                Space::with_width(Length::Fill),
                text(path).size(12).color(colors.muted).wrapping(Wrapping::None),
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(filled(colors.header));

        let selected = s.row[s.page.index()];
        let shown = shown_commands(&self.keymap, s);
        // One clickable row; the selected one is highlighted.
        let line = move |i: usize, content: Element<'a, Message>| -> Element<'a, Message> {
            let bg = if i == selected {
                colors.cursor
            } else if i % 2 == 1 {
                colors.zebra
            } else {
                colors.panel
            };
            mouse_area(container(content).width(Length::Fill).padding([3, 14]).style(filled(bg)))
                .on_press(msg(SettingsMsg::Click(i)))
                .into()
        };
        let heading = |t: &'a str| container(text(t).size(14).color(colors.accent)).padding([10, 14]);
        let label = |t: &'a str| text(t).size(13).width(Length::Fixed(LABEL_W));
        let hint = |t: String| text(t).size(12).color(colors.muted);
        let switch = |on: bool, m: SettingsMsg| checkbox("", on).on_toggle(move |_| msg(m.clone())).size(14);

        let body: Element<'a, Message> = match s.page {
            Page::General => {
                let editor = text_input("Empty: Notepad", &s.editor)
                    .id(editor_id())
                    .on_input(move |v| msg(SettingsMsg::Editor(v)))
                    .on_submit(msg(SettingsMsg::SaveEditor))
                    .padding(5)
                    .size(13)
                    .style(theme::input);
                let terminal = text_input("Empty: Windows Terminal / PowerShell", &s.terminal)
                    .id(terminal_id())
                    .on_input(move |v| msg(SettingsMsg::Terminal(v)))
                    .on_submit(msg(SettingsMsg::SaveEditor))
                    .padding(5)
                    .size(13)
                    .style(theme::input);
                let shell = text_input("Empty: PowerShell 7 if installed, else Windows PowerShell", &s.shell)
                    .id(shell_id())
                    .on_input(move |v| msg(SettingsMsg::Shell(v)))
                    .on_submit(msg(SettingsMsg::SaveEditor))
                    .padding(5)
                    .size(13)
                    .style(theme::input);
                let browse = button(text("Browse...").size(13))
                    .on_press(msg(SettingsMsg::Browse))
                    .padding([3, 12])
                    .style(theme::fkey);
                let browse_terminal = button(text("Browse...").size(13))
                    .on_press(msg(SettingsMsg::BrowseTerminal))
                    .padding([3, 12])
                    .style(theme::fkey);
                let detected_terminals: Element<'a, Message> = if s.terminals.is_empty() {
                    hint("Looking for installed terminals...".into()).into()
                } else {
                    let current = s.terminal.trim();
                    let buttons = s.terminals.iter().enumerate().map(|(i, t)| {
                        let on = current == t.command();
                        Element::from(
                            button(text(t.name.as_str()).size(13))
                                .on_press(msg(SettingsMsg::UseTerminal(i)))
                                .padding([3, 12])
                                .style(theme::drive(on)),
                        )
                    });
                    Row::with_children(buttons).spacing(6).wrap().into()
                };
                let detected: Element<'a, Message> = if s.editors.is_empty() {
                    hint("Looking for installed editors...".into()).into()
                } else {
                    let buttons = s.editors.iter().enumerate().map(|(i, e)| {
                        let on = s.editor.trim_matches('"').starts_with(&e.path.display().to_string());
                        Element::from(
                            button(text(e.name.as_str()).size(13))
                                .on_press(msg(SettingsMsg::UseEditor(i)))
                                .padding([3, 12])
                                .style(theme::drive(on)),
                        )
                    });
                    Row::with_children(buttons).spacing(6).wrap().into()
                };
                let opts = self.compare_opts;
                let compare = |i: u8, title: &'a str, on: bool, about: &'a str| {
                    line(
                        ROW_COMPARE + i as usize,
                        row![label(title), switch(on, SettingsMsg::Compare(i)), hint(about.into())]
                            .spacing(8)
                            .align_y(Alignment::Center)
                            .into(),
                    )
                };
                column![
                    heading("Programs"),
                    line(
                        ROW_EDITOR,
                        row![label("Editor for F4"), editor, browse].spacing(8).align_y(Alignment::Center).into()
                    ),
                    container(hint("A program and arguments; %1 is the file (added when missing). Enter saves.".into()))
                        .padding([0, 14 + LABEL_W as u16 + 8]),
                    row![label("Installed editors"), detected].spacing(8).padding([3, 14]),
                    line(
                        ROW_TERMINAL,
                        row![label("External terminal (Ctrl+F9)"), terminal, browse_terminal]
                            .spacing(8)
                            .align_y(Alignment::Center)
                            .into()
                    ),
                    container(hint("Starts in the panel's folder; %1 is that folder (optional). Enter saves.".into()))
                        .padding([0, 14 + LABEL_W as u16 + 8]),
                    row![label("Installed terminals"), detected_terminals].spacing(8).padding([3, 14]),
                    line(
                        ROW_SHELL,
                        row![label("Built-in terminal shell (F9)"), shell].spacing(8).align_y(Alignment::Center).into()
                    ),
                    container(hint("A program and arguments, started in the panel's folder; for new terminals. Enter saves.".into()))
                        .padding([0, 14 + LABEL_W as u16 + 8]),
                    heading("Opening files"),
                    line(
                        ROW_MARKDOWN,
                        row![
                            label("Markdown in the viewer"),
                            switch(
                                self.config.markdown_in_viewer,
                                SettingsMsg::MarkdownViewer(!self.config.markdown_in_viewer)
                            ),
                            hint("Open Markdown files (.md) in the internal viewer, shown rendered; off = the default app".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                    line(
                        ROW_SCRIPTS,
                        row![
                            label("Run scripts in terminal"),
                            switch(
                                self.config.run_scripts_in_terminal,
                                SettingsMsg::ScriptsInTerminal(!self.config.run_scripts_in_terminal)
                            ),
                            hint("Enter on a .ps1, .cmd or .bat file runs it in a new built-in terminal (F9) in its folder; off = the default app".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                    heading("Panels"),
                    line(
                        ROW_HIDDEN,
                        row![
                            label("Show hidden and system files"),
                            switch(self.config.show_hidden, SettingsMsg::ShowHidden(!self.config.show_hidden)),
                            hint("Files with the Hidden or System attribute, every tab now and at the first start; Ctrl+H toggles one tab".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                    line(
                        ROW_GIT,
                        row![
                            label("Git status"),
                            switch(self.config.git_status, SettingsMsg::GitStatus(!self.config.git_status)),
                            hint("In a git work tree (git on PATH): a status letter after each name and the branch in the path bar".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                    line(
                        ROW_TREE,
                        row![
                            label("Hotlist tree"),
                            switch(self.config.hotlist_tree, SettingsMsg::HotlistTree(!self.config.hotlist_tree)),
                            hint("A button left of the left panel's drives (Ctrl+Shift+D) shows the hotlist as a tree left of the panels".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                    line(
                        ROW_FKEYS,
                        row![
                            label("Function-key bar"),
                            switch(self.config.show_fkeys, SettingsMsg::ShowFkeys(!self.config.show_fkeys)),
                            hint("Buttons F1-F10 under the panels; off = a one-line hint".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                    heading("Startup and display"),
                    line(
                        ROW_UPDATES,
                        row![
                            label("Check for updates"),
                            switch(
                                self.config.check_updates_at_startup,
                                SettingsMsg::CheckUpdates(!self.config.check_updates_at_startup)
                            ),
                            hint("At startup, ask GitHub for a newer release (the command palette's Check for updates looks any time)".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                    line(
                        ROW_RENDER,
                        row![
                            label("Software rendering"),
                            switch(
                                self.config.software_rendering,
                                SettingsMsg::SoftwareRendering(!self.config.software_rendering)
                            ),
                            hint("Draw with the CPU instead of the graphics card: starts sooner, but scrolling and big folders are slower. Applies at the next start".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                    heading("Compare folders (Shift+F2; saved with the session)"),
                    compare(0, "Compare contents", opts.by_content, "Same-size files are read; equal contents are never marked"),
                    compare(1, "Ignore dates", opts.ignore_date, "Differing files are marked on both sides"),
                    compare(2, "Include subfolders", opts.subfolders, "Folders on both sides are marked if anything inside differs"),
                    heading("Configuration file"),
                    line(
                        ROW_CONFIG,
                        row![
                            label("config.toml"),
                            button(text("Open in the editor (Ctrl+,)").size(13))
                                .on_press(msg(SettingsMsg::EditConfig))
                                .padding([3, 12])
                                .style(theme::fkey),
                            hint("Changes made here keep the file's comments".into()),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                    ),
                ]
                .push(heading("User commands (read-only: edit config.toml to change)"))
                .extend(self.user_cmds.iter().map(|c| {
                    let keys: Vec<String> = self
                        .user_keys
                        .iter()
                        .filter(|(_, &i)| self.user_cmds.get(i).is_some_and(|u| u.name == c.name))
                        .map(|(k, _)| keymap::display_key(k))
                        .collect();
                    let tail = if keys.is_empty() { String::new() } else { format!("   [{}]", keys.join(", ")) };
                    let term = if c.terminal { " (terminal)" } else { "" };
                    container(text(format!("{}: {}{term}{tail}", c.name, c.run)).size(13)).padding([2, 14]).into()
                }))
                .push_maybe(self.user_cmds.is_empty().then(|| {
                    container(hint("None. Add [[user_commands]] entries (name, run, key) to config.toml".into()))
                        .padding([2, 14])
                }))
                .into()
            }
            Page::Keys => {
                let bindings = self.keymap.bindings();
                let capturing = s.capture.as_ref().map(|c| c.cmd);
                let rows = shown.iter().enumerate().map(|(i, &cmd)| {
                    let keys = keymap::Keymap::keys_for(&bindings, cmd).join(", ");
                    let (keys, color) = match capturing {
                        Some(c) if c == cmd => ("Press a key...".to_owned(), colors.accent),
                        _ if keys.is_empty() => ("(no key)".to_owned(), colors.muted),
                        _ => (keys, colors.dir),
                    };
                    let changed = if self.keymap.is_changed(cmd) { "changed" } else { "" };
                    line(
                        i,
                        row![
                            text(keys).size(13).color(color).width(Length::Fixed(KEYS_W)).wrapping(Wrapping::None),
                            text(cmd.description()).size(13).width(Length::Fill).wrapping(Wrapping::None),
                            text(changed).size(11).color(colors.selected).width(Length::Fixed(60.0)),
                            text(cmd.id()).size(12).color(colors.muted).width(Length::Fixed(200.0)).wrapping(Wrapping::None),
                        ]
                        .spacing(10)
                        .height(Length::Fixed(ROW_H - 6.0))
                        .align_y(Alignment::Center)
                        .into(),
                    )
                });
                if shown.is_empty() {
                    container(text("No command matches").size(13).color(colors.muted)).padding([8, 14]).into()
                } else {
                    Column::with_children(rows).into()
                }
            }
            Page::Colors => {
                let current = theme::preset(&self.config.theme);
                let presets = theme::PRESETS.iter().fold(Row::new().spacing(6), |r, p| {
                    let on = current.is_some_and(|c| c.id == p.id);
                    let swatch = |c: iced::Color| container(Space::new(10, 10)).style(filled(c));
                    let content = row![
                        row![swatch(p.colors.bg), swatch(p.colors.panel), swatch(p.colors.text), swatch(p.colors.accent)],
                        text(p.name).size(13)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center);
                    r.push(button(content).on_press(msg(SettingsMsg::Theme(p.id))).padding([4, 12]).style(theme::drive(on)))
                });
                let custom = if self.config.colors.is_empty() {
                    String::new()
                } else {
                    format!("{} colour(s) changed", self.config.colors.len())
                };
                let mut list = Column::new().push(heading("Theme")).push(line(
                    0,
                    row![label("Theme (Left / Right)"), presets, hint(custom)].spacing(8).align_y(Alignment::Center).into(),
                ));
                list = list.push(heading("Colours (#rrggbb; applied as you type)"));
                let preset = current.map_or(theme::DARK, |p| p.colors);
                for (i, name) in theme::NAMES.iter().enumerate() {
                    let value = theme::c().get(name).unwrap_or(colors.text);
                    let swatch = button(Space::new(36, 16))
                        .on_press(msg(SettingsMsg::PickOpen(i)))
                        .padding(0)
                        .style(move |_: &Theme, status| button::Style {
                            background: Some(value.into()),
                            border: iced::Border {
                                color: if status == button::Status::Hovered { colors.accent } else { colors.border },
                                width: 1.0,
                                radius: 3.0.into(),
                            },
                            ..button::Style::default()
                        });
                    let input = text_input("#rrggbb", &s.hex[i])
                        .id(hex_id(i))
                        .on_input(move |v| msg(SettingsMsg::Hex(i, v)))
                        .padding(3)
                        .size(13)
                        .width(Length::Fixed(90.0))
                        .style(theme::input);
                    let changed: Element<'a, Message> = if preset.get(name) == Some(value) {
                        Space::with_width(Length::Fixed(70.0)).into()
                    } else {
                        button(text("Reset").size(11))
                            .on_press(msg(SettingsMsg::ResetColor(i)))
                            .padding([1, 8])
                            .width(Length::Fixed(70.0))
                            .style(theme::fkey)
                            .into()
                    };
                    list = list.push(line(
                        i + 1,
                        row![
                            text(*name).size(13).width(Length::Fixed(140.0)),
                            swatch,
                            input,
                            changed,
                            hint(theme::PURPOSES[i].into()),
                        ]
                        .spacing(10)
                        .align_y(Alignment::Center)
                        .into(),
                    ));
                }
                list.into()
            }
        };
        let list: Element<'a, Message> = scrollbar::thin(body).id(list_id()).height(Length::Fill).into();
        let body: Element<'a, Message> = if s.page == Page::Keys {
            let filter = text_input("Filter: words of the description, id or key (Ctrl+F or /)", &s.filter)
                .id(filter_id())
                .on_input(move |v| msg(SettingsMsg::Filter(v)))
                .padding(5)
                .size(13)
                .style(theme::input);
            let count = format!("{} of {}", shown.len(), Command::ALL.len());
            let bar = row![filter, text(count).size(12).color(colors.muted)].spacing(10).align_y(Alignment::Center);
            column![container(bar).padding([6, 14]).style(filled(colors.header)), list].into()
        } else {
            list
        };
        let body = container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.panel));

        let (note, color) = match &s.note {
            Some((t, tone)) => (
                t.clone(),
                match tone {
                    Tone::Error => colors.error,
                    Tone::Success => colors.success,
                    Tone::Info => colors.text,
                },
            ),
            None => (
                match s.page {
                    Page::General => "Tab next page \u{b7} Up/Down pick \u{b7} Enter/Space change \u{b7} Esc close",
                    Page::Keys => "Ctrl+F or / filter \u{b7} Enter set key \u{b7} Ins add key \u{b7} Del remove keys \u{b7} R default \u{b7} Ctrl+R all defaults \u{b7} Esc close",
                    Page::Colors => "Left/Right theme \u{b7} Enter edit colour \u{b7} Del theme's colour \u{b7} Tab next page \u{b7} Esc close",
                }
                .to_owned(),
                colors.muted,
            ),
        };
        let mut buttons = Row::new().spacing(8);
        match (s.page, &s.capture) {
            (Page::Keys, Some(c)) => {
                if c.taken.is_some() {
                    buttons = buttons.push(action("Take it over", SettingsMsg::Confirm, true));
                }
                buttons = buttons.push(action("Cancel", SettingsMsg::CancelCapture, false));
            }
            (Page::Keys, None) => {
                buttons = buttons
                    .push(action("Set key", SettingsMsg::Capture { add: false }, false))
                    .push(action("Add key", SettingsMsg::Capture { add: true }, false))
                    .push(action("Remove keys", SettingsMsg::Unbind, false))
                    .push(action("Default", SettingsMsg::ResetKey, false))
                    .push(action("All defaults", SettingsMsg::ResetAllKeys, false));
            }
            _ => {}
        }
        buttons = buttons.push(action("Close", SettingsMsg::Close, s.capture.is_none()));
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

        let mut page = Column::new().push(header).push(body);
        if let Some(p) = &s.picker {
            let value = p.color();
            let slider_row = |name: &'static str, max: f32, v: f32, make: fn(f32) -> SettingsMsg| {
                row![
                    text(name).size(12).width(Length::Fixed(70.0)),
                    iced::widget::slider(0.0..=max, v, move |x| msg(make(x))).step(1.0_f32).width(Length::Fixed(260.0)),
                    text(format!("{}", v.round() as i32)).size(12).width(Length::Fixed(36.0)),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            };
            let preview = container(Space::new(90, 54)).style(move |_: &Theme| container::Style {
                background: Some(value.into()),
                border: iced::Border { color: colors.border, width: 1.0, radius: 4.0.into() },
                ..container::Style::default()
            });
            let input = text_input("#rrggbb", &p.hex)
                .on_input(move |v| msg(SettingsMsg::PickHex(v)))
                .padding(3)
                .size(13)
                .width(Length::Fixed(90.0))
                .style(theme::input);
            let panel = row![
                column![
                    text(format!("Colour: {}", theme::NAMES[p.i])).size(13),
                    slider_row("Hue", 360.0, p.h, SettingsMsg::PickHue),
                    slider_row("Saturation", 100.0, p.s, SettingsMsg::PickSat),
                    slider_row("Value", 100.0, p.v, SettingsMsg::PickVal),
                ]
                .spacing(6),
                column![preview, input].spacing(8),
                column![
                    action("OK (Enter)", SettingsMsg::PickOk, true),
                    action("Cancel (Esc)", SettingsMsg::PickCancel, false),
                ]
                .spacing(8),
            ]
            .spacing(24)
            .align_y(Alignment::Center);
            page = page.push(container(panel).width(Length::Fill).padding([8, 14]).style(filled(colors.header)));
        }
        container(page.push(footer)).width(Length::Fill).height(Length::Fill).style(filled(colors.bg)).into()
    }
}

/// Runs `work` on its own thread and maps the result to a message.
fn off_thread<T: Send + 'static>(work: fn() -> T, done: fn(T) -> Message) -> Task<Message> {
    Task::perform(
        async move {
            let (tx, rx) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(work());
            });
            rx.await.ok()
        },
        move |r| r.map_or(Message::Settings(SettingsMsg::Nothing), done),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(query: &str) -> Vec<Command> {
        filter_commands(query, &keymap::Keymap::default().bindings())
    }

    #[test]
    fn empty_filter_lists_every_command_in_order() {
        assert_eq!(found(""), Command::ALL.to_vec());
        assert_eq!(found("  "), Command::ALL.to_vec());
    }

    #[test]
    fn filter_matches_description_id_and_keys() {
        let terminal = found("terminal");
        assert!(terminal.len() >= 2, "{terminal:?}");
        let f7 = found("f7");
        assert!(f7.contains(&Command::MakeDir));
        assert!(f7.len() < Command::ALL.len());
        assert!(found("MKDIR").contains(&Command::MakeDir));
        assert!(found("new tab").contains(&Command::NewTab));
        assert!(found("zzzz").is_empty());
        // The usual order is kept.
        let at = |c: Command| Command::ALL.iter().position(|&x| x == c);
        assert!(found("copy").windows(2).all(|w| at(w[0]) < at(w[1])));
    }
}
