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
use iced::widget::{button, checkbox, column, container, mouse_area, responsive, row, scrollable, text, tooltip, Column, Row, Space};
use iced::{Alignment, Element, Length};

const ROW_H: f32 = 26.0;
const LABEL_W: f32 = 220.0;
const LABEL_W_NARROW: f32 = 150.0;
/// Window widths (px) below which setting descriptions go under their control, the key list drops
/// the command id column, and the footer stacks its note above the buttons.
const NARROW_BELOW: f32 = 900.0;
const FOOTER_STACK_BELOW: f32 = 1100.0;
/// Average glyph width as a fraction of the font size, for fitting text into a width.
const CHAR_W: f32 = 0.55;
/// Width a checkbox row spends besides the label column and the description: checkbox, two gaps, scrollbar.
const TOGGLE_FIXED_W: f32 = 14.0 + 16.0 + 16.0 + 16.0;

/// `s` cut to `max_chars` with a trailing "…" when it is longer.
fn elide_right(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max_chars - 1).collect();
    format!("{}…", head.trim_end())
}
/// Width of a theme preview card (name and swatch strip, padding included).
const THEME_CHIP_W: f32 = 88.0;
/// Room kept for the "N colour(s) changed" note next to the theme buttons.
const THEME_NOTE_W: f32 = 130.0;
const KEYS_W: f32 = 240.0;
const KEYS_W_NARROW: f32 = 170.0;

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
    filter_commands(&s.filter[Page::Keys.index()], &keymap.bindings())
}

/// True when every word of `query` is in one of `hay` (case-insensitive; no words: true).
fn matches(query: &str, hay: &[&str]) -> bool {
    let hay = hay.join(" ").to_lowercase();
    query.split_whitespace().all(|w| hay.contains(&w.to_lowercase()))
}

/// Whether General row `row` (`ROW_*`) matches the filter: its label and description.
fn general_matches(query: &str, row: usize) -> bool {
    let (label, about) = GENERAL_TEXT[row];
    matches(query, &[label, about, TEXT_SUMMARY.get(row).copied().unwrap_or("")])
}

/// Whether Colours row `row` (0 the theme, then `theme::NAMES` + 1) matches the filter:
/// the colour's name and purpose, or for the theme row "Theme" and the theme names.
fn colors_matches(query: &str, row: usize) -> bool {
    if row == 0 {
        let mut hay = vec!["Theme (Left / Right)"];
        hay.extend(theme::PRESETS.iter().map(|p| p.name));
        return matches(query, &hay);
    }
    matches(query, &[theme::NAMES[row - 1], theme::PURPOSES.get(row - 1).copied().unwrap_or("")])
}

/// The rows of `page` the filter `query` lets through, in order: `ROW_*` on General, 0 (theme)
/// and colour + 1 on Colours, indices into the `keys_shown` filtered commands on Key bindings.
fn visible_rows(page: Page, query: &str, keys_shown: usize) -> Vec<usize> {
    match page {
        Page::General => (0..GENERAL_ROWS).filter(|&r| general_matches(query, r)).collect(),
        Page::Keys => (0..keys_shown).collect(),
        Page::Colors => (0..1 + theme::NAMES.len()).filter(|&r| colors_matches(query, r)).collect(),
    }
}

/// The visible row `delta` places from `row` (clamped to the first / last one); a hidden `row`
/// counts as the first visible row. None when nothing is visible.
fn step(visible: &[usize], row: usize, delta: isize) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    let at = visible.iter().position(|&r| r == row).unwrap_or(0) as isize;
    let to = (at + delta).clamp(0, visible.len() as isize - 1);
    visible.get(to as usize).copied()
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

/// Label and description of each General row, by `ROW_*` (shown, and searched by the filter).
/// The text rows (editor, terminal, shell) show `TEXT_SUMMARY` with this in a tooltip.
const GENERAL_TEXT: [(&str, &str); GENERAL_ROWS] = [
    ("Editor for F4", "A program and arguments; %1 is the file (added when missing). Enter saves."),
    ("External terminal (Ctrl+F9)", "Starts in the panel's folder; %1 is that folder (optional). Enter saves."),
    (
        "Built-in terminal shell (F9)",
        "A program and arguments, started in the panel's folder; for new terminals. Enter saves.",
    ),
    ("Markdown in the viewer", "Open Markdown files (.md) in the internal viewer, shown rendered; off = the default app"),
    (
        "Run scripts in terminal",
        "Enter on a .ps1, .cmd or .bat file runs it in a new built-in terminal (F9) in its folder; off = the default app",
    ),
    (
        "Show hidden and system files",
        "Files with the Hidden or System attribute, every tab now and at the first start; Ctrl+H toggles one tab",
    ),
    ("Git status", "In a git work tree (git on PATH): a status letter after each name and the branch in the path bar"),
    ("Hotlist tree", "A button left of the left panel's drives (Ctrl+Shift+D) shows the hotlist as a tree left of the panels"),
    ("Function-key bar", "Buttons F1-F10 under the panels; off = a one-line hint"),
    (
        "Check for updates",
        "At startup, ask GitHub for a newer release (the command palette's Check for updates looks any time)",
    ),
    (
        "Software rendering",
        "Draw with the CPU instead of the graphics card: starts sooner, but scrolling and big folders are slower. Applies at the next start",
    ),
    ("Compare contents", "Same-size files are read; equal contents are never marked"),
    ("Ignore dates", "Differing files are marked on both sides"),
    ("Include subfolders", "Folders on both sides are marked if anything inside differs"),
    ("config.toml", "Changes made here keep the file's comments"),
];

/// One-line summaries of the text rows' descriptions (`ROW_EDITOR`, `ROW_TERMINAL`, `ROW_SHELL`).
const TEXT_SUMMARY: [&str; 3] = [
    "Program and arguments; %1 is the file",
    "Starts in the panel's folder; %1 is that folder",
    "Program and arguments for new terminals",
];

/// The General page's headings and their rows (in `ROW_*` order); a heading shows while one of its rows does.
const GENERAL_GROUPS: [(&str, std::ops::Range<usize>); 6] = [
    ("Programs", ROW_EDITOR..ROW_MARKDOWN),
    ("Opening files", ROW_MARKDOWN..ROW_HIDDEN),
    ("Panels", ROW_HIDDEN..ROW_UPDATES),
    ("Startup and display", ROW_UPDATES..ROW_COMPARE),
    ("Compare folders (Shift+F2; saved with the session)", ROW_COMPARE..ROW_CONFIG),
    ("Configuration file", ROW_CONFIG..GENERAL_ROWS),
];

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
    /// The filter box was clicked: typing goes to it.
    StartFilter,
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
    /// The filter of each page (not saved).
    filter: [String; 3],
    /// Typed characters go to the current page's filter (Ctrl+F, /, a click on the box).
    filtering: bool,
    capture: Option<Capture>,
    picker: Option<Picker>,
    /// Ctrl+R pressed once on the keys page (twice resets every key).
    reset_all: bool,
    note: Option<(String, Tone)>,
}

impl Settings {
    /// The cursor is the page and its selected row; a note asking for input (key capture, the second
    /// Ctrl+R) stays while that is pending.
    pub(super) fn note_state(&mut self) -> NoteState<'_> {
        let pending = self.capture.is_some() || self.reset_all;
        NoteState { cursor: (self.page.index(), self.row[self.page.index()]), pending, note: &mut self.note }
    }
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
            filter: Default::default(),
            filtering: false,
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

    /// The rows of the current page the filter lets through (see `visible_rows`).
    fn settings_visible(&self, s: &Settings) -> Vec<usize> {
        visible_rows(s.page, &s.filter[s.page.index()], shown_commands(&self.keymap, s).len())
    }

    /// Keeps the selected row in view (its place among the visible rows).
    fn settings_snap(&self) -> Task<Message> {
        let Some(s) = self.screen.settings() else { return Task::none() };
        let visible = self.settings_visible(s);
        let at = visible.iter().position(|&r| r == s.row[s.page.index()]).unwrap_or(0);
        let y = if visible.len() > 1 { at as f32 / (visible.len() - 1) as f32 } else { 0.0 };
        scrollable::snap_to(list_id(), RelativeOffset { x: 0.0, y })
    }

    /// The current page's filter changed: the selection moves to a row still shown
    /// (the first one on the keys page, whose rows are renumbered), the list follows.
    fn settings_filtered(&mut self) -> Task<Message> {
        let Some(s) = self.screen.settings() else { return Task::none() };
        let visible = self.settings_visible(s);
        let Some(s) = self.screen.settings_mut() else { return Task::none() };
        let row = &mut s.row[s.page.index()];
        if s.page == Page::Keys || !visible.contains(row) {
            *row = visible.first().copied().unwrap_or(0);
        }
        s.note = None;
        self.settings_snap()
    }

    fn settings_page(&mut self, page: Page) -> Task<Message> {
        self.picker_close(false);
        // The old page's note (e.g. "Theme: Nord") doesn't belong on the new page; errors stay.
        if let Some(s) = self.screen.settings_mut() {
            if s.note.as_ref().is_some_and(|(_, tone)| !matches!(tone, Tone::Error)) {
                s.note = None;
            }
        }
        // Saving a pending editor/terminal path may set a fresh note, which is kept.
        self.settings_save_editor();
        let Some(s) = self.screen.settings_mut() else { return Task::none() };
        s.page = page;
        // Each page keeps its filter text; typing stops going to it.
        s.filtering = false;
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
                // Like a text box losing the focus: typing no longer filters.
                s.filtering = false;
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
            SettingsMsg::StartFilter => {
                s.filtering = true;
                if s.page == Page::General {
                    self.settings_save_editor();
                }
                return blur();
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
                // Typed keys go to the capture now, not to the filter.
                s.filtering = false;
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
        (
            "Ctrl+F, /",
            "Filter the page: type words of the name or description (key bindings: also the id or key, F7); Backspace edits",
        ),
        ("Esc (a filter)", "Clear the filter first, then close"),
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
        // Keys a text box used (typing, Enter, Esc leaving it): the editor,
        // terminal, shell and colour boxes. The filter is not a text box (the
        // typing goes to it below), so none of these keys are for it.
        if captured {
            return Task::none();
        }
        let Some(s) = self.screen.settings() else { return Task::none() };
        let visible = self.settings_visible(s);
        let Some(s) = self.screen.settings_mut() else { return Task::none() };
        let page = s.page;
        let row = s.row[page.index()];
        let plain = !mods.control() && !mods.alt() && !mods.logo();
        // While filtering, typed text (Space too) goes to the filter, like in Help (F1).
        let typed = text.filter(|t| plain && !t.is_empty() && !t.chars().any(char::is_control));
        if s.filtering {
            if let (Some(t), keyboard::Key::Character(_) | keyboard::Key::Named(Named::Space)) = (typed, key) {
                s.filter[page.index()].push_str(t);
                return self.settings_filtered();
            }
        }
        // `key` is the key without Shift; "/" is Shift+7 on many layouts, so it comes from the typed text.
        let ch = match key {
            keyboard::Key::Character(_) if text == Some("/") && !mods.control() => Some("/".to_owned()),
            keyboard::Key::Character(c) if !mods.alt() && !mods.logo() => Some(c.to_ascii_lowercase()),
            _ => None,
        };
        let go = |s: &mut Settings, delta: isize| {
            if let Some(to) = step(&visible, row, delta) {
                s.row[page.index()] = to;
            }
        };
        let page_rows = ((self.window_height - 160.0) / ROW_H).max(1.0) as isize;
        match named {
            // Esc clears the filter first, then closes.
            Some(Named::Escape) if s.filtering || !s.filter[page.index()].is_empty() => {
                s.filter[page.index()].clear();
                s.filtering = false;
                return self.settings_filtered();
            }
            Some(Named::Escape) => {
                self.settings_close();
                return Task::none();
            }
            Some(Named::Backspace) if s.filtering || !s.filter[page.index()].is_empty() => {
                s.filtering = true;
                s.filter[page.index()].pop();
                return self.settings_filtered();
            }
            Some(Named::Tab) => {
                let n = Page::ALL.len();
                let next = if mods.shift() { (page.index() + n - 1) % n } else { (page.index() + 1) % n };
                return self.settings_page(Page::ALL[next]);
            }
            Some(Named::ArrowDown) => go(s, 1),
            Some(Named::ArrowUp) => go(s, -1),
            Some(Named::PageDown) => go(s, page_rows),
            Some(Named::PageUp) => go(s, -page_rows),
            Some(Named::Home) => go(s, isize::MIN / 2),
            Some(Named::End) => go(s, isize::MAX / 2),
            // Only a row the filter shows can be changed.
            Some(Named::Enter | Named::Space) if !mods.control() && visible.contains(&row) => {
                s.filtering = false;
                return self.settings_activate();
            }
            Some(Named::ArrowLeft | Named::ArrowRight) if page == Page::Colors && row == 0 && visible.contains(&0) => {
                let n = theme::PRESETS.len();
                let i = theme::PRESETS.iter().position(|p| theme::preset(&self.config.theme).is_some_and(|q| q.id == p.id));
                let i = i.unwrap_or(0);
                let next = if named == Some(Named::ArrowLeft) { (i + n - 1) % n } else { (i + 1) % n };
                return self.settings_update(SettingsMsg::Theme(theme::PRESETS[next].id));
            }
            Some(Named::Insert) if page == Page::Keys => return self.settings_update(SettingsMsg::Capture { add: true }),
            Some(Named::Delete) if page == Page::Keys => return self.settings_update(SettingsMsg::Unbind),
            Some(Named::Delete) if page == Page::Colors && row > 0 && visible.contains(&row) => {
                return self.settings_update(SettingsMsg::ResetColor(row - 1));
            }
            _ => match (page, ch.as_deref(), mods.control()) {
                (_, Some("f"), true) | (_, Some("/"), false) => return self.settings_update(SettingsMsg::StartFilter),
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
        Task::batch([blur(), self.settings_snap()])
    }

    pub(super) fn settings_view<'a>(&'a self, s: &'a Settings) -> Element<'a, Message> {
        responsive(move |size| self.settings_build(s, size.width)).into()
    }

    fn settings_build<'a>(&'a self, s: &'a Settings, width: f32) -> Element<'a, Message> {
        let narrow = width < NARROW_BELOW;
        let lw = if narrow { LABEL_W_NARROW } else { LABEL_W };
        let colors = theme::c();
        let msg = Message::Settings;
        let action = |label: &'a str, m: SettingsMsg, primary: bool| {
            button(text(label).size(theme::SIZE_BODY).wrapping(Wrapping::None)).on_press(msg(m)).padding([6, 14]).style(if primary { theme::primary } else { theme::fkey })
        };
        let tabs = Page::ALL.iter().fold(Row::new().spacing(4), |r, &p| {
            r.push(button(text(p.title()).size(theme::SIZE_BODY).wrapping(Wrapping::None)).on_press(msg(SettingsMsg::Page(p))).padding([5, 14]).style(theme::tab(p == s.page)))
        });
        let shown = shown_commands(&self.keymap, s);
        let query = s.filter[s.page.index()].as_str();
        let visible = visible_rows(s.page, query, shown.len());
        let total = match s.page {
            Page::General => GENERAL_ROWS,
            Page::Keys => Command::ALL.len(),
            Page::Colors => 1 + theme::NAMES.len(),
        };
        // The filter box, as in Help (F1): the typed text with a "|" while typing goes to it.
        let typed: Element<'a, Message> = if s.filtering {
            text(format!("{query}|")).size(theme::SIZE_BODY).wrapping(Wrapping::None).into()
        } else if query.is_empty() {
            text("Ctrl+F or / to filter").size(theme::SIZE_BODY).color(colors.muted).wrapping(Wrapping::None).into()
        } else {
            text(query).size(theme::SIZE_BODY).wrapping(Wrapping::None).into()
        };
        let filtering = s.filtering;
        let search = mouse_area(
            container(typed).padding([5, 8]).width(Length::Fill).max_width(320.0).clip(true).style(move |_: &Theme| {
                container::Style {
                    background: Some(colors.panel.into()),
                    border: iced::Border {
                        color: if filtering { colors.accent } else { colors.border },
                        width: 1.0,
                        radius: 3.0.into(),
                    },
                    ..container::Style::default()
                }
            }),
        )
        .on_press(msg(SettingsMsg::StartFilter));
        let header = container(
            row![
                text("Settings").size(theme::SIZE_TITLE).wrapping(Wrapping::None),
                tabs,
                search,
                Space::with_width(Length::Fill),
                text(format!("{} of {total} shown", visible.len())).size(theme::SIZE_SMALL).color(colors.muted).wrapping(Wrapping::None),
            ]
            .spacing(theme::SPACING_LOOSE)
            .align_y(Alignment::Center),
        )
        .padding(theme::PAD_NORMAL)
        .style(filled(colors.header));

        let selected = s.row[s.page.index()];
        let vis = &visible;
        // One clickable row; the selected one is highlighted, the stripes follow the visible rows.
        let line = move |i: usize, content: Element<'a, Message>| -> Element<'a, Message> {
            let bg = if i == selected {
                colors.cursor
            } else if vis.iter().position(|&r| r == i).unwrap_or(i) % 2 == 1 {
                colors.zebra
            } else {
                colors.panel
            };
            button(content)
                .width(Length::Fill)
                .padding([3, 14])
                .on_press(msg(SettingsMsg::Click(i)))
                .style(theme::list_row(bg, i != selected))
                .into()
        };
        let heading = |t: &'a str| container(text(t).size(theme::SIZE_HEAD).color(colors.accent)).padding(theme::PAD_LOOSE);
        let label = move |t: &'a str| text(t).size(theme::SIZE_BODY).width(Length::Fixed(lw));
        let hint = |t: String| text(t).size(theme::SIZE_SMALL).color(colors.muted).width(Length::Fill);
        // One-line summary under a text field; the full text is in the tooltip of the "(?)" marker.
        let tip = move |short: &'static str, full: &'static str| -> Element<'a, Message> {
            let mark = tooltip(
                text("(?)").size(theme::SIZE_SMALL).color(colors.accent).wrapping(Wrapping::None),
                container(text(full).size(12)).max_width(420).padding([3, 8]).style(theme::drag_label),
                tooltip::Position::Bottom,
            );
            container(
                row![container(text(short).size(theme::SIZE_SMALL).color(colors.muted).wrapping(Wrapping::None)).clip(true), mark]
                    .spacing(theme::SPACING_NORMAL),
            )
            .padding([0, 14 + lw as u16 + 8])
            .into()
        };
        // Label, control and description: the description is a third column, or under the control when narrow.
        let described = move |label: iced::widget::Text<'a>, control: Element<'a, Message>, about: String| -> Element<'a, Message> {
            if narrow {
                column![
                    row![label, control].spacing(theme::SPACING_NORMAL).align_y(Alignment::Center),
                    row![Space::with_width(Length::Fixed(lw + 8.0)), hint(about)],
                ]
                .spacing(theme::SPACING_TIGHT)
                .into()
            } else {
                row![label, control, hint(about)].spacing(theme::SPACING_NORMAL).align_y(Alignment::Center).into()
            }
        };
        // Row with a checkbox: the description stays on the line, cut with "…" at the right edge.
        let toggle_row = move |label: iced::widget::Text<'a>, control: Element<'a, Message>, about: String| -> Element<'a, Message> {
            let avail = width - 2.0 * 14.0 - lw - TOGGLE_FIXED_W;
            let budget = (avail / (12.0 * CHAR_W)).max(0.0) as usize;
            let about = elide_right(&about, budget);
            row![label, control, container(text(about).size(theme::SIZE_SMALL).color(colors.muted).wrapping(Wrapping::None)).width(Length::Fill).clip(true)]
                .spacing(theme::SPACING_NORMAL)
                .align_y(Alignment::Center)
                .into()
        };
        let switch = |on: bool, m: SettingsMsg| checkbox("", on).on_toggle(move |_| msg(m.clone())).size(14);

        let body: Element<'a, Message> = match s.page {
            Page::General => {
                let cfg = &self.config;
                let opts = self.compare_opts;
                let text_box = |placeholder: &'a str, value: &'a str, id: text_input::Id, m: fn(String) -> SettingsMsg| {
                    text_input(placeholder, value)
                        .id(id)
                        .on_input(move |v| msg(m(v)))
                        .on_submit(msg(SettingsMsg::SaveEditor))
                        .padding(5)
                        .size(theme::SIZE_BODY)
                        .style(theme::input)
                };
                let browse = |m: SettingsMsg| {
                    button(text("Browse...").size(theme::SIZE_BODY).wrapping(Wrapping::None))
                        .on_press(msg(m))
                        .padding([3, 12])
                        .style(theme::fkey)
                };
                // Buttons for the programs found on this machine (`on`: the one in use).
                let found = |looking: &'static str, items: Vec<(&'a str, bool, SettingsMsg)>| -> Element<'a, Message> {
                    if items.is_empty() {
                        return hint(looking.into()).into();
                    }
                    let buttons = items.into_iter().map(|(name, on, m)| {
                        Element::from(button(text(name).size(theme::SIZE_BODY)).on_press(msg(m)).padding([3, 12]).style(theme::drive(on)))
                    });
                    Row::with_children(buttons).spacing(6).wrap().into()
                };
                let text_line = |r: usize, control: Element<'a, Message>| {
                    line(r, row![label(GENERAL_TEXT[r].0), control].spacing(theme::SPACING_NORMAL).align_y(Alignment::Center).into())
                };
                let flag = |r: usize, on: bool, m: SettingsMsg| {
                    line(r, toggle_row(label(GENERAL_TEXT[r].0), switch(on, m).into(), GENERAL_TEXT[r].1.into()))
                };
                // The elements of row `r`: the row, and for the text rows the hint and the programs found.
                let general_row = |r: usize| -> Vec<Element<'a, Message>> {
                    match r {
                        ROW_EDITOR => {
                            let current = s.editor.trim_matches('"');
                            let items = s.editors.iter().enumerate().map(|(i, e)| {
                                (e.name.as_str(), current.starts_with(&e.path.display().to_string()), SettingsMsg::UseEditor(i))
                            });
                            vec![
                                text_line(
                                    r,
                                    row![text_box("Empty: Notepad", &s.editor, editor_id(), SettingsMsg::Editor), browse(SettingsMsg::Browse)]
                                        .spacing(theme::SPACING_NORMAL)
                                        .align_y(Alignment::Center)
                                        .into(),
                                ),
                                tip(TEXT_SUMMARY[r], GENERAL_TEXT[r].1),
                                row![label("Installed editors"), found("Looking for installed editors...", items.collect())]
                                    .spacing(theme::SPACING_NORMAL)
                                    .padding([3, 14])
                                    .into(),
                            ]
                        }
                        ROW_TERMINAL => {
                            let current = s.terminal.trim();
                            let items = s.terminals.iter().enumerate().map(|(i, t)| {
                                (t.name.as_str(), current == t.command(), SettingsMsg::UseTerminal(i))
                            });
                            vec![
                                text_line(
                                    r,
                                    row![
                                        text_box("Empty: Windows Terminal / PowerShell", &s.terminal, terminal_id(), SettingsMsg::Terminal),
                                        browse(SettingsMsg::BrowseTerminal)
                                    ]
                                    .spacing(theme::SPACING_NORMAL)
                                    .align_y(Alignment::Center)
                                    .into(),
                                ),
                                tip(TEXT_SUMMARY[r], GENERAL_TEXT[r].1),
                                row![label("Installed terminals"), found("Looking for installed terminals...", items.collect())]
                                    .spacing(theme::SPACING_NORMAL)
                                    .padding([3, 14])
                                    .into(),
                            ]
                        }
                        ROW_SHELL => vec![
                            text_line(
                                r,
                                text_box("Empty: PowerShell 7 if installed, else Windows PowerShell", &s.shell, shell_id(), SettingsMsg::Shell).into(),
                            ),
                            tip(TEXT_SUMMARY[r], GENERAL_TEXT[r].1),
                        ],
                        ROW_MARKDOWN => vec![flag(r, cfg.markdown_in_viewer, SettingsMsg::MarkdownViewer(!cfg.markdown_in_viewer))],
                        ROW_SCRIPTS => vec![flag(
                            r,
                            cfg.run_scripts_in_terminal,
                            SettingsMsg::ScriptsInTerminal(!cfg.run_scripts_in_terminal),
                        )],
                        ROW_HIDDEN => vec![flag(r, cfg.show_hidden, SettingsMsg::ShowHidden(!cfg.show_hidden))],
                        ROW_GIT => vec![flag(r, cfg.git_status, SettingsMsg::GitStatus(!cfg.git_status))],
                        ROW_TREE => vec![flag(r, cfg.hotlist_tree, SettingsMsg::HotlistTree(!cfg.hotlist_tree))],
                        ROW_FKEYS => vec![flag(r, cfg.show_fkeys, SettingsMsg::ShowFkeys(!cfg.show_fkeys))],
                        ROW_UPDATES => vec![flag(
                            r,
                            cfg.check_updates_at_startup,
                            SettingsMsg::CheckUpdates(!cfg.check_updates_at_startup),
                        )],
                        ROW_RENDER => vec![flag(r, cfg.software_rendering, SettingsMsg::SoftwareRendering(!cfg.software_rendering))],
                        ROW_CONFIG => vec![line(
                            r,
                            described(
                                label(GENERAL_TEXT[r].0),
                                button(text("Open in the editor (Ctrl+,)").size(theme::SIZE_BODY).wrapping(Wrapping::None))
                                    .on_press(msg(SettingsMsg::EditConfig))
                                    .padding([3, 12])
                                    .style(theme::fkey)
                                    .into(),
                                GENERAL_TEXT[r].1.into(),
                            ),
                        )],
                        _ => {
                            let i = (r - ROW_COMPARE) as u8;
                            let on = [opts.by_content, opts.ignore_date, opts.subfolders][i as usize];
                            vec![flag(r, on, SettingsMsg::Compare(i))]
                        }
                    }
                };
                let mut list = Column::new();
                for (title, rows) in GENERAL_GROUPS {
                    let rows: Vec<usize> = rows.filter(|r| vis.contains(r)).collect();
                    if !rows.is_empty() {
                        list = list.push(heading(title));
                        for r in rows {
                            list = list.extend(general_row(r));
                        }
                    }
                }
                if vis.is_empty() {
                    list = list.push(
                        container(text(format!("No setting matches \"{query}\"")).size(theme::SIZE_BODY).color(colors.muted))
                            .padding(theme::PAD_NORMAL),
                    );
                }
                // The user commands are not rows: the filter shows those whose name or command line matches.
                let user: Vec<Element<'a, Message>> = self
                    .user_cmds
                    .iter()
                    .filter(|c| matches(query, &[c.name.as_str(), c.run.as_str()]))
                    .map(|c| {
                        let keys: Vec<String> = self
                            .user_keys
                            .iter()
                            .filter(|(_, &i)| self.user_cmds.get(i).is_some_and(|u| u.name == c.name))
                            .map(|(k, _)| keymap::display_key(k))
                            .collect();
                        let tail = if keys.is_empty() { String::new() } else { format!("   [{}]", keys.join(", ")) };
                        let term = if c.terminal { " (terminal)" } else { "" };
                        container(text(format!("{}: {}{term}{tail}", c.name, c.run)).size(theme::SIZE_BODY)).padding([2, 14]).into()
                    })
                    .collect();
                let filtered = !query.trim().is_empty();
                if !filtered || !user.is_empty() {
                    list = list.push(heading("User commands (read-only: edit config.toml to change)")).extend(user);
                }
                list.push_maybe((!filtered && self.user_cmds.is_empty()).then(|| {
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
                            text(keys).size(theme::SIZE_BODY).color(color).width(Length::Fixed(if narrow { KEYS_W_NARROW } else { KEYS_W })).wrapping(Wrapping::None),
                            text(cmd.description()).size(theme::SIZE_BODY).width(Length::Fill).wrapping(Wrapping::None),
                            text(changed).size(11).color(colors.selected).width(Length::Fixed(60.0)),
                        ]
                        .push_maybe((!narrow).then(|| {
                            text(cmd.id()).size(theme::SIZE_SMALL).color(colors.muted).width(Length::Fixed(200.0)).wrapping(Wrapping::None)
                        }))
                        .spacing(10)
                        .height(Length::Fixed(ROW_H - 6.0))
                        .align_y(Alignment::Center)
                        .into(),
                    )
                });
                if shown.is_empty() {
                    container(text(format!("No command matches \"{query}\"")).size(theme::SIZE_BODY).color(colors.muted))
                        .padding(theme::PAD_NORMAL)
                        .into()
                } else {
                    Column::with_children(rows).into()
                }
            }
            Page::Colors => {
                let current = theme::preset(&self.config.theme);
                let custom = if self.config.colors.is_empty() {
                    String::new()
                } else {
                    format!("{} colour(s) changed", self.config.colors.len())
                };
                // Theme buttons flow onto several lines: chunk them by their estimated width.
                let avail = width - 28.0 - lw - 8.0 - if narrow || custom.is_empty() { 0.0 } else { THEME_NOTE_W };
                let mut chunks: Vec<Vec<&theme::Preset>> = vec![Vec::new()];
                let mut used = 0.0;
                for p in theme::PRESETS.iter() {
                    let w = THEME_CHIP_W + theme::SPACING_NORMAL as f32;
                    if used + w > avail && !chunks.last().is_some_and(|c| c.is_empty()) {
                        chunks.push(Vec::new());
                        used = 0.0;
                    }
                    used += w;
                    chunks.last_mut().unwrap().push(p);
                }
                let presets = chunks.into_iter().fold(Column::new().spacing(theme::SPACING_NORMAL), |col, chunk| {
                    col.push(chunk.into_iter().fold(Row::new().spacing(theme::SPACING_NORMAL), |r, p| {
                        let on = current.is_some_and(|c| c.id == p.id);
                        let swatch = |c: iced::Color| container(Space::new(Length::Fill, 6)).style(filled(c));
                        let pc = p.colors;
                        let content = column![
                            text(p.name).size(theme::SIZE_SMALL).color(pc.text).wrapping(Wrapping::None),
                            row![swatch(pc.bg), swatch(pc.panel), swatch(pc.accent), swatch(pc.text)].spacing(theme::SPACING_TIGHT),
                        ]
                        .spacing(3)
                        .width(Length::Fixed(THEME_CHIP_W - 2.0 * 6.0));
                        r.push(button(content).on_press(msg(SettingsMsg::Theme(p.id))).padding([3, 6]).style(
                            move |_: &Theme, status| button::Style {
                                background: Some(pc.panel.into()),
                                text_color: pc.text,
                                border: iced::Border {
                                    color: if on || status == button::Status::Hovered { colors.accent } else { colors.border },
                                    width: if on { 2.0 } else { 1.0 },
                                    radius: 4.0.into(),
                                },
                                ..button::Style::default()
                            },
                        ))
                    }))
                });
                let mut list = Column::new();
                if vis.contains(&0) {
                    list = list.push(heading("Theme")).push(line(
                    0,
                    if narrow {
                        let mut c = Column::new().spacing(theme::SPACING_TIGHT).push(row![label("Theme (Left / Right)"), presets].spacing(theme::SPACING_NORMAL));
                        if !custom.is_empty() {
                            c = c.push(row![Space::with_width(Length::Fixed(lw + 8.0)), hint(custom)]);
                        }
                        c.into()
                    } else {
                        row![label("Theme (Left / Right)"), presets, hint(custom)].spacing(theme::SPACING_NORMAL).align_y(Alignment::Center).into()
                    },
                    ));
                }
                if vis.iter().any(|&r| r > 0) {
                    list = list.push(heading("Colours (#rrggbb; applied as you type)"));
                }
                if vis.is_empty() {
                    list = list.push(
                        container(text(format!("No colour matches \"{query}\"")).size(theme::SIZE_BODY).color(colors.muted))
                            .padding(theme::PAD_NORMAL),
                    );
                }
                let preset = current.map_or(theme::DARK, |p| p.colors);
                for (i, name) in theme::NAMES.iter().enumerate() {
                    if !vis.contains(&(i + 1)) {
                        continue;
                    }
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
                        .size(theme::SIZE_BODY)
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
                    let content: Element<'a, Message> = if narrow {
                        column![
                            row![text(*name).size(theme::SIZE_BODY).width(Length::Fixed(140.0)), swatch, input, changed]
                                .spacing(10)
                                .align_y(Alignment::Center),
                            hint(theme::PURPOSES[i].into()),
                        ]
                        .spacing(theme::SPACING_TIGHT)
                        .into()
                    } else {
                        row![
                            text(*name).size(theme::SIZE_BODY).width(Length::Fixed(140.0)),
                            swatch,
                            input,
                            changed,
                            hint(theme::PURPOSES[i].into()),
                        ]
                        .spacing(10)
                        .align_y(Alignment::Center)
                        .into()
                    };
                    list = list.push(line(i + 1, content));
                }
                list.into()
            }
        };
        let body = container(scrollbar::thin(body).id(list_id()).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.panel));

        let note: Option<(String, iced::Color)> = s.note.as_ref().map(|(t, tone)| {
            (
                t.clone(),
                match tone {
                    Tone::Error => colors.error,
                    Tone::Success => colors.success,
                    Tone::Info => colors.text,
                },
            )
        });
        let hints: &[theme::Hint] = match s.page {
            _ if s.filtering => &[
                (&[], "Type to filter"),
                (&["Backspace"], "edit"),
                (&["\u{2191}\u{2193}"], "pick"),
                (&["Enter"], "change"),
                (&["Esc"], "clear filter"),
            ],
            Page::General => &[
                (&["Ctrl+F", "/"], "filter"),
                (&["Tab"], "next page"),
                (&["\u{2191}\u{2193}"], "pick"),
                (&["Enter", "Space"], "change"),
                (&["Esc"], "close"),
            ],
            Page::Keys => &[
                (&["Ctrl+F", "/"], "filter"),
                (&["Enter"], "set key"),
                (&["Ins"], "add key"),
                (&["Del"], "remove keys"),
                (&["R"], "default"),
                (&["Ctrl+R"], "all defaults"),
                (&["Esc"], "close"),
            ],
            Page::Colors => &[
                (&["Ctrl+F", "/"], "filter"),
                (&["\u{2190}\u{2192}"], "theme"),
                (&["Enter"], "edit colour"),
                (&["Del"], "theme's colour"),
                (&["Tab"], "next page"),
                (&["Esc"], "close"),
            ],
        };
        let mut buttons = Row::new().spacing(theme::SPACING_NORMAL);
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
        let close = action("Close", SettingsMsg::Close, s.capture.is_none());
        let footer: Element<'a, Message> = if width < FOOTER_STACK_BELOW && s.page == Page::Keys {
            // Narrow: the note on top; the page buttons wrap on the left, Close stays on the right.
            let top: Element<'a, Message> = match note {
                Some((t, color)) => text(t).size(theme::SIZE_SMALL).color(color).width(Length::Fill).into(),
                None => theme::key_hints_row(hints, width - 2.0 * theme::PAD_NORMAL[1] as f32),
            };
            let stacked = column![
                top,
                row![container(buttons.wrap()).width(Length::Fill), close]
                    .spacing(theme::SPACING_NORMAL)
                    .align_y(Alignment::Start),
            ]
            .spacing(6);
            container(stacked).padding(theme::PAD_NORMAL).style(filled(colors.header)).into()
        } else {
            buttons = buttons.push(close);
            let reserved = match (s.page, &s.capture) {
                (Page::Keys, Some(_)) => 260.0,
                (Page::Keys, None) => 520.0,
                _ => 80.0,
            };
            match note {
                Some((t, color)) => theme::hint_footer(t, color, theme::PAD_NORMAL, theme::SPACING_NORMAL, Some(buttons.into())),
                None => {
                    theme::key_hints_footer(hints, width, reserved, theme::PAD_NORMAL, theme::SPACING_NORMAL, Some(buttons.into()))
                }
            }
        };

        let mut page = Column::new().push(header).push(body);
        if let Some(p) = &s.picker {
            let value = p.color();
            let slider_row = |name: &'static str, max: f32, v: f32, make: fn(f32) -> SettingsMsg| {
                row![
                    text(name).size(theme::SIZE_SMALL).width(Length::Fixed(70.0)),
                    iced::widget::slider(0.0..=max, v, move |x| msg(make(x))).step(1.0_f32).width(Length::Fixed(260.0)),
                    text(format!("{}", v.round() as i32)).size(theme::SIZE_SMALL).width(Length::Fixed(36.0)),
                ]
                .spacing(theme::SPACING_NORMAL)
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
                .size(theme::SIZE_BODY)
                .width(Length::Fixed(90.0))
                .style(theme::input);
            let panel = row![
                column![
                    text(format!("Colour: {}", theme::NAMES[p.i])).size(theme::SIZE_BODY),
                    slider_row("Hue", 360.0, p.h, SettingsMsg::PickHue),
                    slider_row("Saturation", 100.0, p.s, SettingsMsg::PickSat),
                    slider_row("Value", 100.0, p.v, SettingsMsg::PickVal),
                ]
                .spacing(6),
                column![preview, input].spacing(theme::SPACING_NORMAL),
                column![
                    action("OK (Enter)", SettingsMsg::PickOk, true),
                    action("Cancel (Esc)", SettingsMsg::PickCancel, false),
                ]
                .spacing(theme::SPACING_NORMAL),
            ]
            .spacing(24)
            .align_y(Alignment::Center);
            page = page.push(container(panel).width(Length::Fill).padding(theme::PAD_NORMAL).style(filled(colors.header)));
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

    #[test]
    fn elide_right_cuts_with_ellipsis() {
        assert_eq!(elide_right("abcdef", 6), "abcdef");
        assert_eq!(elide_right("abcdef", 4), "abc…");
        assert_eq!(elide_right("abcdef", 0), "");
    }

    #[test]
    fn matches_needs_every_word_anywhere_ignoring_case() {
        assert!(matches("", &["anything"]));
        assert!(matches("  ", &[]));
        assert!(matches("GIT", &["Git status", "a status letter"]));
        assert!(matches("git letter", &["Git status", "a status letter"]));
        assert!(!matches("git zzz", &["Git status", "a status letter"]));
    }

    #[test]
    fn general_filter_matches_label_and_description() {
        assert_eq!(visible_rows(Page::General, "", 0), (0..GENERAL_ROWS).collect::<Vec<_>>());
        // "git" is in "GitHub" too; every word must match.
        assert_eq!(visible_rows(Page::General, "git", 0), vec![ROW_GIT, ROW_UPDATES]);
        assert_eq!(visible_rows(Page::General, "git tree", 0), vec![ROW_GIT]);
        // The description counts too ("Ctrl+H" is only in the hidden files row's).
        assert_eq!(visible_rows(Page::General, "ctrl+h", 0), vec![ROW_HIDDEN]);
        // The text rows' summaries and full texts.
        assert!(visible_rows(Page::General, "%1", 0).starts_with(&[ROW_EDITOR, ROW_TERMINAL]));
        assert!(visible_rows(Page::General, "zzzz", 0).is_empty());
        // Every row belongs to one heading, in order.
        let grouped: Vec<usize> = GENERAL_GROUPS.iter().flat_map(|(_, r)| r.clone()).collect();
        assert_eq!(grouped, (0..GENERAL_ROWS).collect::<Vec<_>>());
    }

    #[test]
    fn colors_filter_matches_name_purpose_and_theme_names() {
        let all = visible_rows(Page::Colors, "", 0);
        assert_eq!(all.len(), 1 + theme::NAMES.len());
        let accent = theme::NAMES.iter().position(|&n| n == "accent").unwrap() + 1;
        assert!(visible_rows(Page::Colors, "ACCENT", 0).contains(&accent));
        assert!(!visible_rows(Page::Colors, "accent", 0).contains(&0));
        assert_eq!(visible_rows(Page::Colors, "theme", 0).first(), Some(&0));
        assert!(visible_rows(Page::Colors, &theme::PRESETS[0].name.to_lowercase(), 0).contains(&0));
        // Keys: the already filtered commands, renumbered.
        assert_eq!(visible_rows(Page::Keys, "whatever", 3), vec![0, 1, 2]);
    }

    #[test]
    fn step_moves_over_visible_rows_only() {
        let v = [2, 5, 9];
        assert_eq!(step(&v, 2, 1), Some(5));
        assert_eq!(step(&v, 5, 1), Some(9));
        assert_eq!(step(&v, 9, 1), Some(9));
        assert_eq!(step(&v, 5, -1), Some(2));
        assert_eq!(step(&v, 2, -1), Some(2));
        assert_eq!(step(&v, 5, isize::MIN / 2), Some(2));
        assert_eq!(step(&v, 2, isize::MAX / 2), Some(9));
        // A hidden row counts as the first visible one.
        assert_eq!(step(&v, 7, 1), Some(5));
        assert_eq!(step(&[], 0, 1), None);
    }

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
