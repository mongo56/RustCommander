//! User settings in `config.toml` (next to `session.toml`). A commented
//! template is created when it's missing; the settings view (F10) changes
//! single entries with `toml_edit` (`write`), keeping the user's comments,
//! layout and unknown keys. It is re-read where a setting is used, so edits
//! apply without a restart.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Editor for F4: a program, optionally with arguments; `%1` stands for
    /// the file (appended when missing), `%L` for a line (`editor::launch_at_line`).
    /// Empty = the platform default.
    pub editor: String,
    /// External terminal for Ctrl+F9: a program, optionally with arguments;
    /// `%1` stands for the folder (optional: it is the working directory anyway).
    /// Empty = Windows Terminal / PowerShell, else $TERMINAL.
    pub terminal: String,
    /// Shell of the built-in terminal (F9): a program, optionally with
    /// arguments, started in the active panel's folder. Empty = see
    /// [`Config::builtin_shell`].
    pub builtin_shell: String,
    /// Colour theme: a `theme::PRESETS` id; empty = dark.
    pub theme: String,
    /// Hidden files shown at the first start (no session yet); the
    /// settings view also applies a change to every open tab.
    pub show_hidden: bool,
    /// Enter / double-click on a Markdown file (.md, .markdown) opens it in
    /// the internal viewer, rendered, instead of the default app. On unless
    /// set to false (also when the key is missing).
    pub markdown_in_viewer: bool,
    /// Enter / double-click on a local script (.ps1, .cmd, .bat) runs it in a
    /// new built-in terminal instead of the default app, and the context
    /// menu's "Run with PowerShell" does too. On unless set to false (also
    /// when the key is missing).
    pub run_scripts_in_terminal: bool,
    /// Inside a git work tree, panels show a status letter (M, A, D, R, ?, U)
    /// per entry and the branch name in the path bar. On unless set to false
    /// (also when the key is missing).
    pub git_status: bool,
    /// The function-key bar (F1-F10 buttons) under the panels; off = a
    /// one-line hint instead.
    pub show_fkeys: bool,
    /// The hotlist tree: a button left of the left panel's drive buttons
    /// (and Ctrl+Shift+D) show / hide the hotlist as a tree left of the panels.
    pub hotlist_tree: bool,
    /// Look for a newer release on GitHub at startup (in the background;
    /// a newer one is offered in the status line).
    pub check_updates_at_startup: bool,
    /// Draw with the CPU (iced's tiny-skia renderer) instead of the GPU
    /// (wgpu): the window opens much sooner. Off unless set to true. Read
    /// once at startup.
    pub software_rendering: bool,
    /// The built-in terminal's cursor blinks (unless a program asks for a
    /// steady one).
    pub terminal_cursor_blink: bool,
    /// The built-in terminal's font: a family name (a monospace one); empty =
    /// Cascadia Mono, shipped with the program.
    pub terminal_font: String,
    /// The built-in terminal's font size (Ctrl+0 there goes back to it;
    /// Ctrl++ / Ctrl+- zoom from it, kept in the session); see
    /// [`Config::terminal_font_size`].
    pub terminal_font_size: u32,
    /// Bold text in the built-in terminal in one of the 8 normal ANSI colours
    /// is drawn in the bright variant (as older terminals did).
    pub terminal_bold_is_bright: bool,
    /// Edge of the thumbnail view's pictures in pixels (Ctrl+Shift+F5);
    /// 0 = the default, see [`Config::thumbnail_size`].
    pub thumbnail_size: u32,
    /// Key bindings on top of the defaults: key (`Ctrl+Shift+F5`) ->
    /// command id (`cm_Copy`), `""` unbinds the key.
    pub keys: BTreeMap<String, String>,
    /// Colour overrides: name (see `theme::NAMES`) -> `#rrggbb`.
    pub colors: BTreeMap<String, String>,
    /// Built-in terminal colours: name (see `theme::TERM_NAMES`) -> `#rrggbb`.
    pub terminal_colors: BTreeMap<String, String>,
    /// Compare-folders options (Ctrl+Shift+F2); `None` = no `[compare]`
    /// table (older files kept them in `session.toml`).
    pub compare: Option<CompareState>,
    /// Synchronize-folders options (Alt+Shift+F2); `None` = no `[sync]` table.
    pub sync: Option<SyncState>,
    /// `[[user_commands]]`, as written (see [`resolve_user_commands`]).
    pub user_commands: Vec<UserCommand>,
}

/// One `[[user_commands]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct UserCommand {
    /// Shown in the palette and help; what `[keys]` names as `user:<name>`.
    pub name: String,
    /// Program and arguments as one command line, with the placeholders of
    /// `rc_core::user_cmd` (`%P %T %N %F %S %%`).
    pub run: String,
    /// Working folder (placeholders allowed); empty = the active panel's folder.
    pub cwd: String,
    /// Run in the built-in terminal (typed into a new shell) instead of detached.
    pub terminal: bool,
    /// Optional key, like in `[keys]`.
    pub key: String,
}

/// [`rc_core::compare::CompareOptions`] in `[compare]` (hidden files
/// follow the panels). Older versions kept it in `session.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(default)]
pub struct CompareState {
    pub by_content: bool,
    pub ignore_date: bool,
    pub subfolders: bool,
}

impl From<CompareState> for rc_core::compare::CompareOptions {
    fn from(c: CompareState) -> Self {
        Self { by_content: c.by_content, ignore_date: c.ignore_date, subfolders: c.subfolders, show_hidden: false }
    }
}

impl From<rc_core::compare::CompareOptions> for CompareState {
    fn from(o: rc_core::compare::CompareOptions) -> Self {
        Self { by_content: o.by_content, ignore_date: o.ignore_date, subfolders: o.subfolders }
    }
}

/// The options of the synchronize-folders tool in `[sync]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(default)]
pub struct SyncState {
    /// `two-way`, `mirror` or `update` (see [`rc_core::sync::Mode::id`]);
    /// anything else is two-way.
    pub mode: String,
    pub by_content: bool,
    pub ignore_date: bool,
    /// Equal files are listed too.
    pub show_equal: bool,
}

impl SyncState {
    pub fn mode(&self) -> rc_core::sync::Mode {
        rc_core::sync::Mode::from_id(&self.mode).unwrap_or_default()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            editor: String::new(),
            terminal: String::new(),
            builtin_shell: String::new(),
            theme: String::new(),
            show_hidden: false,
            markdown_in_viewer: true,
            run_scripts_in_terminal: true,
            git_status: true,
            show_fkeys: false,
            hotlist_tree: false,
            check_updates_at_startup: false,
            software_rendering: false,
            terminal_cursor_blink: true,
            terminal_font: String::new(),
            terminal_font_size: DEFAULT_TERMINAL_FONT_SIZE,
            terminal_bold_is_bright: false,
            thumbnail_size: 0,
            keys: BTreeMap::new(),
            colors: BTreeMap::new(),
            terminal_colors: BTreeMap::new(),
            compare: None,
            sync: None,
            user_commands: Vec::new(),
        }
    }
}

/// The usable user commands, the key of each (canonical name -> index) and
/// a message per problem. Entries without a name or `run`, with a duplicate
/// name, a bad key, a key that a built-in command (`keymap`) or an earlier
/// user command has, or a `user:<name>` in `[keys]` naming no command, are
/// skipped.
pub fn resolve_user_commands(
    config: &Config,
    keymap: &crate::keymap::Keymap,
) -> (Vec<UserCommand>, std::collections::HashMap<String, usize>, Vec<String>) {
    let mut cmds: Vec<UserCommand> = Vec::new();
    let mut keys = std::collections::HashMap::new();
    let mut problems = Vec::new();
    for (i, c) in config.user_commands.iter().enumerate() {
        let (name, run) = (c.name.trim(), c.run.trim());
        let label = if name.is_empty() { format!("user command #{}", i + 1) } else { format!("user command \"{name}\"") };
        if name.is_empty() || run.is_empty() {
            problems.push(format!("{label}: needs a name and a run line"));
        } else if cmds.iter().any(|o| o.name == name) {
            problems.push(format!("{label}: name used twice"));
        } else {
            let mut cmd = c.clone();
            cmd.name = name.to_owned();
            cmd.run = run.to_owned();
            let key = c.key.trim();
            if !key.is_empty() {
                bind_user_key(key, cmds.len(), &label, keymap, &mut keys, &mut problems);
            }
            cmds.push(cmd);
        }
    }
    for (key, action) in &config.keys {
        let Some(name) = action.trim().strip_prefix("user:") else { continue };
        match cmds.iter().position(|c| c.name == name.trim()) {
            Some(i) => bind_user_key(key, i, &format!("[keys] {key}"), keymap, &mut keys, &mut problems),
            None => problems.push(format!("[keys] {key}: no user command \"{}\"", name.trim())),
        }
    }
    (cmds, keys, problems)
}

fn bind_user_key(
    key: &str,
    index: usize,
    label: &str,
    keymap: &crate::keymap::Keymap,
    keys: &mut std::collections::HashMap<String, usize>,
    problems: &mut Vec<String>,
) {
    let Some(name) = crate::keymap::parse_key(key) else {
        problems.push(format!("{label}: unknown key \"{key}\""));
        return;
    };
    if let Some(cmd) = keymap.command_of(&name) {
        problems.push(format!("{label}: {key} is already {}", cmd.id()));
    } else if keys.get(&name).is_some_and(|&other| other != index) {
        problems.push(format!("{label}: {key} is already used by another user command"));
    } else {
        keys.insert(name, index);
    }
}

/// The built-in terminal's font size when unset, and the sizes it can have.
pub const DEFAULT_TERMINAL_FONT_SIZE: u32 = 14;
pub const TERMINAL_FONT_SIZES: std::ops::RangeInclusive<u32> = 8..=32;

impl Config {
    /// `terminal_font_size`, 14 when 0, kept within `TERMINAL_FONT_SIZES`.
    pub fn terminal_font_size(&self) -> u32 {
        match self.terminal_font_size {
            0 => DEFAULT_TERMINAL_FONT_SIZE,
            size => size.clamp(*TERMINAL_FONT_SIZES.start(), *TERMINAL_FONT_SIZES.end()),
        }
    }

    /// The thumbnail edge in pixels: `thumbnail_size`, 128 when unset,
    /// kept within 32..=512.
    pub fn thumbnail_size(&self) -> u32 {
        if self.thumbnail_size == 0 { 128 } else { self.thumbnail_size.clamp(32, 512) }
    }

    /// The built-in terminal's shell command line: `builtin_shell`, or when
    /// empty `pwsh.exe -NoLogo` if PowerShell 7 is installed, else
    /// `powershell.exe -NoLogo` (elsewhere `$SHELL`, else `/bin/sh`). The
    /// PowerShell 7 lookup touches the disk once (cached).
    pub fn builtin_shell(&self) -> String {
        let setting = self.builtin_shell.trim();
        let pwsh = if cfg!(windows) && setting.is_empty() { rc_platform::find_pwsh() } else { None };
        builtin_shell_or_default(setting, cfg!(windows), std::env::var("SHELL").ok(), pwsh)
    }

    /// The theme preset with the `[colors]` overrides on top.
    pub fn colors(&self) -> Result<crate::theme::Colors, String> {
        let preset = crate::theme::preset(&self.theme).ok_or_else(|| format!("unknown theme \"{}\"", self.theme))?;
        preset.colors.with(&self.colors).map_err(|e| format!("[colors]: {e}"))
    }

    /// `[terminal_colors]`, parsed.
    pub fn terminal_colors(&self) -> Result<crate::theme::TermColors, String> {
        crate::theme::term_colors(&self.terminal_colors).map_err(|e| format!("[terminal_colors]: {e}"))
    }
}

fn builtin_shell_or_default(
    setting: &str,
    windows: bool,
    shell_env: Option<String>,
    pwsh: Option<std::path::PathBuf>,
) -> String {
    match setting.trim() {
        "" if windows => match pwsh {
            Some(path) => {
                let program = path.display().to_string();
                let program = if program.contains(' ') { format!("\"{program}\"") } else { program };
                format!("{program} -NoLogo")
            }
            None => DEFAULT_BUILTIN_SHELL.to_owned(),
        },
        "" => shell_env.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "/bin/sh".into()),
        s => s.to_owned(),
    }
}

/// The built-in terminal's shell on Windows when `builtin_shell` is empty and
/// PowerShell 7 isn't installed.
pub const DEFAULT_BUILTIN_SHELL: &str = "powershell.exe -NoLogo";

/// One change to `config.toml`.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// A top-level text setting (`editor`, `terminal`, `builtin_shell`, `theme`).
    Text(&'static str, String),
    /// A top-level switch (`show_hidden`).
    Flag(&'static str, bool),
    /// A `[keys]` entry by canonical key name (`keymap::KeyEdit`): `Some`
    /// sets it, `None` drops every spelling of that key.
    Key(crate::keymap::KeyEdit),
    /// A `[colors]` entry: `Some("#rrggbb")` sets it, `None` drops it.
    Color(String, Option<String>),
    /// A `[terminal_colors]` entry: `Some("#rrggbb")` sets it, `None` drops it.
    TermColor(String, Option<String>),
    /// Drops every `[colors]` entry (a theme was picked).
    ClearColors,
    /// Sets the three `[compare]` switches.
    Compare(CompareState),
    /// Adds `[compare]` with these values unless the file has one already
    /// (moving the options over from an old `session.toml`).
    CompareIfMissing(CompareState),
    /// Sets the `[sync]` options.
    Sync(SyncState),
}

/// Applies `edits` to the text of `config.toml`; comments, layout and
/// unknown keys stay as they are. A changed value keeps its trailing
/// comment; new `[keys]` entries are spelled like `Ctrl+Shift+F5`.
pub fn apply(text: &str, edits: &[Edit]) -> Result<String, String> {
    use toml_edit::{DocumentMut, Item, TableLike, Value};

    /// Sets `key` in `table` to `value`, keeping the old value's comments.
    fn put(table: &mut dyn TableLike, key: &str, value: Value) {
        match table.get_mut(key) {
            Some(Item::Value(old)) => {
                let decor = old.decor().clone();
                *old = value;
                *old.decor_mut() = decor;
            }
            _ => {
                table.insert(key, Item::Value(value));
            }
        }
    }
    fn table<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut dyn TableLike, String> {
        doc.entry(name).or_insert(toml_edit::table()).as_table_like_mut().ok_or_else(|| format!("{name} is not a table"))
    }
    /// Sets (`Some`) or drops the entry of `table` whose key `same` matches;
    /// the first spelling found is kept, other spellings of it go.
    fn change(table: &mut dyn TableLike, same: impl Fn(&str) -> bool, new_key: String, value: Option<&str>) {
        let mut found: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).filter(|k| same(k)).collect();
        if let Some(value) = value {
            let key = if found.is_empty() { new_key } else { found.remove(0) };
            put(table, &key, Value::from(value));
        }
        for key in found {
            table.remove(&key);
        }
    }

    /// Adds an empty table `name` above the first table (as in the
    /// template), not at the end where it would take the comments that
    /// close the file (the template's commented-out colours).
    fn add_table_first(doc: &mut DocumentMut, name: &str) {
        fn shift(item: &mut Item, from: usize) {
            match item {
                Item::Table(t) => {
                    if let Some(p) = t.position().filter(|p| *p >= from) {
                        t.set_position(p + 1);
                    }
                    t.iter_mut().for_each(|(_, item)| shift(item, from));
                }
                Item::ArrayOfTables(a) => {
                    for t in a.iter_mut() {
                        if let Some(p) = t.position().filter(|p| *p >= from) {
                            t.set_position(p + 1);
                        }
                        t.iter_mut().for_each(|(_, item)| shift(item, from));
                    }
                }
                _ => {}
            }
        }
        fn first(item: &Item) -> Option<usize> {
            match item {
                Item::Table(t) => t.position(),
                Item::ArrayOfTables(a) => a.iter().filter_map(|t| t.position()).min(),
                _ => None,
            }
        }
        let mut table = toml_edit::Table::new();
        table.decor_mut().set_prefix("
");
        if let Some(from) = doc.iter().filter_map(|(_, item)| first(item)).min() {
            doc.iter_mut().for_each(|(_, item)| shift(item, from));
            table.set_position(from);
        }
        doc.insert(name, Item::Table(table));
    }

    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.message().to_owned())?;
    for edit in edits {
        match edit {
            Edit::Text(key, value) => put(doc.as_table_mut(), key, Value::from(value.as_str())),
            Edit::Flag(key, value) => put(doc.as_table_mut(), key, Value::from(*value)),
            Edit::Key((name, value)) => {
                let same = |k: &str| crate::keymap::parse_key(k).as_deref() == Some(name.as_str());
                change(table(&mut doc, "keys")?, same, crate::keymap::display_key(name), value.as_deref());
            }
            Edit::Color(name, value) => {
                let same = |k: &str| k.eq_ignore_ascii_case(name);
                change(table(&mut doc, "colors")?, same, name.clone(), value.as_deref());
            }
            Edit::TermColor(name, value) => {
                let same = |k: &str| k.eq_ignore_ascii_case(name);
                change(table(&mut doc, "terminal_colors")?, same, name.clone(), value.as_deref());
            }
            Edit::ClearColors => {
                if let Some(colors) = doc.get_mut("colors").and_then(Item::as_table_like_mut) {
                    change(colors, |_| true, String::new(), None);
                }
            }
            Edit::CompareIfMissing(_) if doc.contains_key("compare") => {}
            Edit::Compare(c) | Edit::CompareIfMissing(c) => {
                if !doc.contains_key("compare") {
                    add_table_first(&mut doc, "compare");
                }
                let compare = table(&mut doc, "compare")?;
                put(compare, "by_content", Value::from(c.by_content));
                put(compare, "ignore_date", Value::from(c.ignore_date));
                put(compare, "subfolders", Value::from(c.subfolders));
            }
            Edit::Sync(c) => {
                if !doc.contains_key("sync") {
                    add_table_first(&mut doc, "sync");
                }
                let sync = table(&mut doc, "sync")?;
                put(sync, "mode", Value::from(c.mode.as_str()));
                put(sync, "by_content", Value::from(c.by_content));
                put(sync, "ignore_date", Value::from(c.ignore_date));
                put(sync, "show_equal", Value::from(c.show_equal));
            }
        }
    }
    let out = doc.to_string();
    parse(&out)?;
    Ok(out)
}

/// Applies `edits` to `config.toml` (created from the template first if
/// it's missing). The new text goes to a temporary file that then replaces
/// the old one, so a failed write never leaves it cut short; nothing is
/// written when the edits change nothing or the file doesn't parse.
pub fn write(edits: &[Edit]) -> Result<(), String> {
    create_if_missing("", None).map_err(|e| e.to_string())?;
    let path = file().ok_or("no config folder")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let out = apply(&text, edits).map_err(|e| format!("{}: {e}", path.display()))?;
    if out != text {
        replace(&path, &out).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Writes `text` to `path` through a temporary file next to it.
fn replace(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The folder holding `config.toml` and `session.toml`.
pub fn dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("RustCommander"))
}

pub fn file() -> Option<PathBuf> {
    Some(dir()?.join("config.toml"))
}

/// Reads `config.toml`; defaults when it's missing. A broken file is
/// reported as an error so the user learns about the typo.
pub fn load() -> Result<Config, String> {
    let Some(path) = file() else { return Ok(Config::default()) };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.message().to_owned())
}

/// Writes the commented template unless `config.toml` exists. `editor`
/// and `compare` seed those settings (carried over from older
/// `session.toml` files).
pub fn create_if_missing(editor: &str, compare: Option<CompareState>) -> std::io::Result<()> {
    let path = file().ok_or_else(|| std::io::Error::other("no config folder"))?;
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    replace(&path, &template(editor, compare.unwrap_or_default()))
}

fn template(editor: &str, compare: CompareState) -> String {
    let editor = toml::Value::String(editor.to_owned());
    let CompareState { by_content, ignore_date, subfolders } = compare;
    let themes: Vec<&str> = crate::theme::PRESETS.iter().map(|p| p.id).collect();
    let themes = themes.join(", ");
    let commands: Vec<&str> = rc_core::Command::ALL.iter().map(|c| c.id()).collect();
    let mut command_list = String::new();
    for line in commands.chunks(4) {
        command_list.push_str(&format!("#   {}\n", line.join(", ")));
    }
    let mut colors = String::new();
    for name in crate::theme::NAMES {
        let value = crate::theme::GRUVBOX.get(name).map(crate::theme::to_hex).unwrap_or_default();
        colors.push_str(&format!("# {name} = \"{value}\"\n"));
    }
    let mut term_colors = String::new();
    for (i, name) in crate::theme::TERM_NAMES.iter().enumerate() {
        let ansi = crate::theme::preset("gruvbox").map_or(crate::theme::TERM_DARK_ANSI, |p| p.ansi);
        let value = ansi.get(i).map_or("#rrggbb".to_owned(), |v| format!("#{v:06x}"));
        term_colors.push_str(&format!("# {name} = \"{value}\"\n"));
    }
    format!(
        "# RustCommander settings. Changes apply as soon as the file is saved.\n\
         # Ctrl+, opens this file in the editor; F10 opens the settings view, which\n\
         # changes single entries and keeps your comments.\n\
         \n\
         # Editor for F4 / Shift+F4: a program, optionally with arguments.\n\
         # %1 stands for the file (appended when missing); quote paths with spaces.\n\
         # %L stands for a line (Ctrl+click on file:line in the built-in terminal;\n\
         # Notepad++, VS Code and Sublime Text get the line without it).\n\
         # Empty = Notepad on Windows, else $VISUAL / $EDITOR.\n\
         # Example: editor = '\"C:\\Program Files\\Notepad++\\notepad++.exe\" -multiInst %1'\n\
         editor = {editor}\n\
         \n\
         # External terminal for Ctrl+F9: a program, optionally with arguments; it\n\
         # starts in the active panel's folder. %1 stands for that folder (optional).\n\
         # Empty = Windows Terminal if installed, else PowerShell; elsewhere $TERMINAL\n\
         # or x-terminal-emulator. Example: terminal = 'wt.exe -d %1'\n\
         terminal = \"\"\n\
         \n\
         # Shell of the built-in terminal (F9; Ctrl+F9 opens the external one above):\n\
         # a program, optionally with arguments; it starts in the active panel's\n\
         # folder. Empty = PowerShell 7 (pwsh.exe -NoLogo) if installed, else\n\
         # powershell.exe -NoLogo (elsewhere $SHELL).\n\
         # Examples: builtin_shell = 'pwsh.exe -NoLogo', builtin_shell = 'cmd.exe'\n\
         builtin_shell = \"\"\n\
         \n\
         # The built-in terminal's cursor blinks (false = steady). Programs that ask\n\
         # for a steady cursor get one either way.\n\
         terminal_cursor_blink = true\n\
         \n\
         # The built-in terminal's font: a monospace family name, e.g. 'Consolas'.\n\
         # Empty = Cascadia Mono (shipped with RustCommander, with real bold and italic).\n\
         terminal_font = \"\"\n\
         \n\
         # Its font size, 8-32. Ctrl++ / Ctrl+- in a terminal zoom all terminals (the\n\
         # zoom is kept for the next start); Ctrl+0 goes back to this size.\n\
         terminal_font_size = 14\n\
         \n\
         # Bold text in one of the 8 normal colours is drawn in the bright colour\n\
         # (as older terminals did). Off = bold only changes the weight.\n\
         terminal_bold_is_bright = false\n\
         \n\
         # Show hidden and system files at the first start (Ctrl+H toggles them per tab).\n\
         show_hidden = false\n\
         \n\
         # Open Markdown files (.md) with Enter / double-click in the internal viewer\n\
         # (shown rendered) instead of the default app. F3 always uses the viewer.\n\
         markdown_in_viewer = true\n\
         \n\
         # Run scripts (.ps1, .cmd, .bat) with Enter / double-click in a new built-in\n\
         # terminal (F9) in their folder instead of the default app; the context\n\
         # menu's Run with PowerShell does too. The context menu also offers Run in\n\
         # terminal and Run in external terminal.\n\
         run_scripts_in_terminal = true\n\
         \n\
         # Inside a git work tree (needs git on PATH) show a status letter (M modified,\n\
         # A added, D deleted, R renamed, ? untracked, U conflict) after each name and\n\
         # the branch in the path bar. Folders show the strongest status inside.\n\
         git_status = true\n\
         \n\
         # Show the function-key bar (F1-F10 buttons) under the panels. Off = only a\n\
         # one-line hint (F1 help, command palette, F10 settings).\n\
         show_fkeys = false\n\
         \n\
         # Hotlist tree: a button left of the left panel's drive buttons (and\n\
         # Ctrl+Shift+D) shows the hotlist (Ctrl+D) as a folder tree left of the\n\
         # panels; folders picked in it open in the last active panel.\n\
         hotlist_tree = false\n\
         \n\
         # Look for a newer RustCommander release on GitHub at startup (in the\n\
         # background; a newer one is offered in the status line). The command\n\
         # palette's Check for updates (cm_CheckForUpdates) looks any time.\n\
         check_updates_at_startup = false\n\
         \n\
         # Draw with the CPU instead of the graphics card: the window opens sooner\n\
         # (no GPU setup), but scrolling and big folders are slower. Applies at\n\
         # the next start; the ICED_BACKEND environment variable (tiny-skia /\n\
         # wgpu) overrides it.\n\
         software_rendering = false\n\
         \n\
         # Size of the pictures in the thumbnail view (Ctrl+Shift+F5), in pixels\n\
         # (32-512).\n\
         thumbnail_size = 128\n\
         \n\
         # Colour theme: {themes}. [colors] below overrides single colours.\n\
         theme = \"gruvbox\"\n\
         \n\
         # Compare folders (Shift+F2; Ctrl+Shift+F2 or F10 change these): read the\n\
         # contents of same-size files, ignore dates (mark differing files on both\n\
         # sides), and look into subfolders found on both sides.\n\
         [compare]\n\
         by_content = {by_content}\n\
         ignore_date = {ignore_date}\n\
         subfolders = {subfolders}\n\
         \n\
         # Key bindings for the panels, on top of the Total Commander-like defaults.\n\
         # \"Key\" = \"command\"; an empty command unbinds the key. Modifiers: Ctrl, Alt,\n\
         # Shift, Win. Keys: a letter or symbol, F1-F12, Up, Down, Left, Right, PgUp,\n\
         # PgDn, Home, End, Enter, Backspace, Tab, Space, Ins, Del, Esc. Plain letters\n\
         # start a quick search, so bind them with Ctrl or Alt.\n\
         # Commands:\n\
         {command_list}\
         [keys]\n\
         # \"Ctrl+K\" = \"cm_MkDir\"\n\
         # \"F8\" = \"\"\n\
         # \"Ctrl+Alt+1\" = \"user:Name\"   (runs the [[user_commands]] entry with that name)\n\
         \n\
         # Your own commands (command palette, F1 help, optional \"key\"). Placeholders: %P\n\
         # active folder, %T other folder, %N cursor name, %F its full path, %S selected\n\
         # full paths, %% a percent sign. Paths with spaces are quoted for you.\n\
         # [[user_commands]]\n\
         # name = \"Open in VS Code\"\n\
         # run = \"code %P\"\n\
         # terminal = false   # true: type the line into a new built-in terminal\n\
         # cwd = \"%P\"         # default: the active folder\n\
         # key = \"Ctrl+Alt+1\"\n\
         \n\
         # Colours as \"#rrggbb\"; the values below are the gruvbox theme's.\n\
         [colors]\n\
         {colors}\
         \n\
         # Colours of the built-in terminal (F9) as \"#rrggbb\". Missing ones follow the\n\
         # theme: its own 16 colours (the gruvbox ones below), its text as foreground\n\
         # and cursor, and its file list background.\n\
         [terminal_colors]\n\
         {term_colors}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_commands_are_validated() {
        let config = parse(
            r#"
[keys]
"Ctrl+Alt+2" = "user:Two"
"Ctrl+Alt+3" = "user:Nope"
"F5" = "user:Two"

[[user_commands]]
name = "One"
run = "tool %P"
key = "Ctrl+Alt+1"
terminal = true

[[user_commands]]
name = "Two"
run = "other %S"

[[user_commands]]
name = "One"
run = "again"

[[user_commands]]
name = ""
run = "x"

[[user_commands]]
name = "NoRun"

[[user_commands]]
name = "Clash"
run = "x"
key = "Ctrl+Alt+1"

[[user_commands]]
name = "BadKey"
run = "x"
key = "Hyper+Q"
"#,
        )
        .unwrap();
        let keymap = crate::keymap::Keymap::new(&config.keys).unwrap();
        let (cmds, keys, problems) = resolve_user_commands(&config, &keymap);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["One", "Two", "Clash", "BadKey"]);
        assert!(cmds[0].terminal && !cmds[1].terminal);
        assert_eq!(keys["ctrl+alt+1"], 0);
        assert_eq!(keys["ctrl+alt+2"], 1);
        // [keys] "F5" = "user:Two" unbinds the built-in F5 and gives it to the command.
        assert_eq!(keys["f5"], 1);
        assert_eq!(keys.len(), 3);
        assert_eq!(problems.len(), 6, "{problems:?}");
        assert!(crate::config::parse("").unwrap().user_commands.is_empty());
    }

    #[test]
    fn built_in_keys_win_over_user_command_keys() {
        let config = parse("[[user_commands]]
name = \"A\"
run = \"x\"
key = \"F7\"
").unwrap();
        let (cmds, keys, problems) = resolve_user_commands(&config, &crate::keymap::Keymap::default());
        assert_eq!(cmds.len(), 1);
        assert!(keys.is_empty());
        assert!(problems[0].contains("cm_MkDir"), "{problems:?}");
    }

    #[test]
    fn template_parses_back() {
        let editor = r#""C:\Program Files\Ed\ed.exe" -n %1"#;
        assert_eq!(parse(&template(editor, CompareState::default())).unwrap().editor, editor);
        let config = parse(&template("", CompareState::default())).unwrap();
        assert_eq!(config.editor, "");
        assert!(config.keys.is_empty() && config.colors.is_empty());
        // The commented-out colour lines are valid settings once uncommented.
        let uncommented = template("", CompareState::default()).replace("\n# bg = ", "\nbg = ");
        assert_eq!(parse(&uncommented).unwrap().colors["bg"], "#1d2021");
        assert_eq!(parse(&uncommented).unwrap().theme, "gruvbox");
    }

    #[test]
    fn edits_keep_comments_and_unknown_keys() {
        let text = "# my editor\neditor = 'ed' # trailing\nfuture = 1\n\n[keys]\n# mine\n\"ctrl+k\" = \"cm_MkDir\"\nF8 = \"\"\n\n[colors]\nAccent = \"#ff0000\"\n";
        let out = apply(
            text,
            &[
                Edit::Text("editor", "vim %1".into()),
                Edit::Text("theme", "light".into()),
                Edit::Flag("show_hidden", true),
                Edit::Flag("show_fkeys", true),
                Edit::Key(("ctrl+k".into(), Some("cm_Copy".into()))),
                Edit::Key(("f8".into(), None)),
                Edit::Key(("ctrl+shift+f5".into(), Some("cm_RenMov".into()))),
                Edit::Color("accent".into(), Some("#00ff00".into())),
                Edit::Color("bg".into(), Some("#000000".into())),
            ],
        )
        .unwrap();
        for kept in ["# my editor\n", "# trailing", "future = 1", "# mine\n\"ctrl+k\" = \"cm_Copy\""] {
            assert!(out.contains(kept), "{kept} in {out}");
        }
        assert!(!out.contains("F8"));
        let config = parse(&out).unwrap();
        assert_eq!(config.editor, "vim %1");
        assert_eq!(config.theme, "light");
        assert!(config.show_hidden && config.show_fkeys);
        assert!(!parse(&template("", CompareState::default())).unwrap().show_fkeys);
        assert!(Config::default().markdown_in_viewer);
        assert!(parse("").unwrap().markdown_in_viewer, "on when the key is missing");
        assert!(parse(&template("", CompareState::default())).unwrap().markdown_in_viewer);
        let off = apply(&out, &[Edit::Flag("markdown_in_viewer", false)]).unwrap();
        assert!(!parse(&off).unwrap().markdown_in_viewer);
        assert!(Config::default().run_scripts_in_terminal);
        assert!(parse("").unwrap().run_scripts_in_terminal, "on when the key is missing");
        assert!(parse(&template("", CompareState::default())).unwrap().run_scripts_in_terminal);
        let off = apply(&out, &[Edit::Flag("run_scripts_in_terminal", false)]).unwrap();
        assert!(!parse(&off).unwrap().run_scripts_in_terminal);
        assert!(Config::default().git_status);
        assert!(parse("").unwrap().git_status, "on when the key is missing");
        assert!(parse(&template("", CompareState::default())).unwrap().git_status);
        let off = apply(&out, &[Edit::Flag("git_status", false)]).unwrap();
        assert!(!parse(&off).unwrap().git_status);
        assert!(!parse(&template("", CompareState::default())).unwrap().hotlist_tree);
        assert!(parse(&apply(&out, &[Edit::Flag("hotlist_tree", true)]).unwrap()).unwrap().hotlist_tree);
        assert!(!parse(&template("", CompareState::default())).unwrap().check_updates_at_startup);
        let on = apply(&out, &[Edit::Flag("check_updates_at_startup", true)]).unwrap();
        assert!(parse(&on).unwrap().check_updates_at_startup);
        assert!(!parse(&template("", CompareState::default())).unwrap().software_rendering);
        assert!(!parse("").unwrap().software_rendering, "off when the key is missing");
        let on = apply(&out, &[Edit::Flag("software_rendering", true)]).unwrap();
        assert!(parse(&on).unwrap().software_rendering);
        assert_eq!(config.keys["Ctrl+Shift+F5"], "cm_RenMov");
        assert_eq!(config.colors["Accent"], "#00ff00");
        assert_eq!(config.colors["bg"], "#000000");
        // New top-level settings land above the first table.
        assert!(out.find("theme =").unwrap() < out.find("[keys]").unwrap());

        let cleared = apply(&out, &[Edit::ClearColors, Edit::Color("bg".into(), None)]).unwrap();
        assert!(parse(&cleared).unwrap().colors.is_empty());
        assert!(cleared.contains("[colors]"));
        assert!(apply("editor = ", &[]).is_err());
        assert!(apply("keys = 1", &[Edit::Key(("f5".into(), None))]).is_err());
    }

    #[test]
    fn template_edits_round_trip() {
        let text = template("notepad", CompareState::default());
        assert_eq!(apply(&text, &[]).unwrap(), text);
        let edits = [Edit::Text("theme", "nord".into()), Edit::Key(("ctrl+k".into(), Some("cm_MkDir".into())))];
        let out = apply(&text, &edits).unwrap();
        // Only the changed lines differ.
        let changed: Vec<_> = out.lines().filter(|l| !text.lines().any(|t| t == *l)).collect();
        assert_eq!(changed, ["theme = \"nord\"", "\"Ctrl+K\" = \"cm_MkDir\""]);
        let config = parse(&out).unwrap();
        assert_eq!(config.colors().unwrap(), crate::theme::NORD);
        assert!(crate::keymap::Keymap::new(&config.keys).is_ok());
        assert!(parse("theme = 'pink'").unwrap().colors().is_err());
    }

    #[test]
    fn missing_keys_default_and_typos_fail() {
        assert_eq!(parse("").unwrap().editor, "");
        assert_eq!(parse("").unwrap().builtin_shell, "");
        assert!(parse("editor = ").is_err());
    }

    #[test]
    fn terminal_font_settings() {
        let config = parse("").unwrap();
        assert_eq!((config.terminal_font.as_str(), config.terminal_font_size()), ("", 14));
        assert_eq!(parse("terminal_font_size = 0").unwrap().terminal_font_size(), 14);
        assert_eq!(parse("terminal_font_size = 3").unwrap().terminal_font_size(), 8);
        assert_eq!(parse("terminal_font_size = 99").unwrap().terminal_font_size(), 32);
        assert_eq!(parse("terminal_font_size = 16").unwrap().terminal_font_size(), 16);
        assert_eq!(parse("terminal_font = 'Consolas'").unwrap().terminal_font, "Consolas");
        assert!(parse("terminal_font_size = -1").is_err());
        assert!(!config.terminal_bold_is_bright);
        assert!(parse("terminal_bold_is_bright = true").unwrap().terminal_bold_is_bright);
        assert!(!parse(&template("", CompareState::default())).unwrap().terminal_bold_is_bright);
    }

    #[test]
    fn builtin_shell_defaults() {
        let pwsh = || Some(std::path::PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"));
        let plain = || Some(std::path::PathBuf::from(r"C:\pwsh\pwsh.exe"));
        assert_eq!(builtin_shell_or_default("", true, None, None), "powershell.exe -NoLogo");
        assert_eq!(builtin_shell_or_default("  ", true, Some("/bin/zsh".into()), None), "powershell.exe -NoLogo");
        let quoted = builtin_shell_or_default("", true, None, pwsh());
        assert_eq!(quoted, r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#);
        assert_eq!(builtin_shell_or_default("", true, None, plain()), r"C:\pwsh\pwsh.exe -NoLogo");
        assert_eq!(builtin_shell_or_default("cmd.exe", true, None, pwsh()), "cmd.exe");
        assert_eq!(builtin_shell_or_default("", false, Some("/bin/zsh".into()), pwsh()), "/bin/zsh");
        assert_eq!(builtin_shell_or_default("", false, None, None), "/bin/sh");
        assert_eq!(builtin_shell_or_default(" cmd.exe /k ", true, None, None), "cmd.exe /k");
        let config = parse(&template("", CompareState::default())).unwrap();
        assert_eq!(config.builtin_shell, "");
        assert_eq!(parse("builtin_shell = 'pwsh -NoLogo'").unwrap().builtin_shell, "pwsh -NoLogo");
    }

    #[test]
    fn sync_options_round_trip() {
        let text = template("", CompareState::default());
        assert_eq!(parse(&text).unwrap().sync, None);
        let state = SyncState { mode: "mirror".into(), by_content: true, ignore_date: false, show_equal: true };
        let out = apply(&text, &[Edit::Sync(state.clone())]).unwrap();
        let back = parse(&out).unwrap().sync.unwrap();
        assert_eq!(back, state);
        assert_eq!(back.mode(), rc_core::sync::Mode::Mirror);
        // The rest of the file is untouched.
        assert!(text.lines().all(|l| out.lines().any(|o| o == l)));
        let again = apply(&out, &[Edit::Sync(SyncState { mode: "update".into(), ..state })]).unwrap();
        assert_eq!(parse(&again).unwrap().sync.unwrap().mode(), rc_core::sync::Mode::UpdateRight);
        assert_eq!(SyncState { mode: "bogus".into(), ..SyncState::default() }.mode(), rc_core::sync::Mode::TwoWay);
    }

    const OLD: CompareState = CompareState { by_content: true, ignore_date: false, subfolders: true };

    #[test]
    fn compare_migrates_once_and_keeps_comments() {
        // A user's file without [compare]: comments, unknown keys and the
        // other tables stay; [compare] is added with the old values.
        let text = "# mine
editor = 'ed' # trailing
future = 1

[keys]
# a key
F8 = \"\"

[colors]
# red
accent = \"#ff0000\"
";
        let out = apply(text, &[Edit::CompareIfMissing(OLD)]).unwrap();
        let added = "future = 1

[compare]
by_content = true
ignore_date = false
subfolders = true

[keys]
# a key
";
        assert_eq!(out.replace(added, "future = 1

[keys]
# a key
"), text, "{out}");
        assert_eq!(parse(&out).unwrap().compare, Some(OLD));
        // Again (a crash before the session was saved): nothing changes.
        assert_eq!(apply(&out, &[Edit::CompareIfMissing(CompareState::default())]).unwrap(), out);
        // A file that has [compare] already keeps its own values, even
        // partial or empty ones.
        for own in ["[compare]
ignore_date = true # me
", "[compare]
", "compare = { subfolders = true }
"] {
            let text = format!("# c
editor = ''
{own}");
            assert_eq!(apply(&text, &[Edit::CompareIfMissing(OLD)]).unwrap(), text);
        }
        assert_eq!(parse("[compare]
ignore_date = true
").unwrap().compare, Some(CompareState { ignore_date: true, ..Default::default() }));
        assert_eq!(parse("editor = ''").unwrap().compare, None);
        // A file made from an older template: [compare] lands where the
        // template has it now, the closing commented colours stay under
        // [colors].
        let now = template("", OLD);
        let start = now.find("# Compare folders").unwrap();
        let block = &now[start..now.find("[keys]").unwrap()];
        let block = &block[..block.find("

").unwrap() + 2];
        let older = now.replace(block, "");
        assert_eq!(parse(&older).unwrap().compare, None);
        let comments = &block[..block.find("[compare]").unwrap()];
        assert_eq!(apply(&older, &[Edit::CompareIfMissing(OLD)]).unwrap(), now.replace(comments, ""));
        // No tables: appended. Nested tables and arrays keep their order.
        let added = "
[compare]
by_content = true
ignore_date = false
subfolders = true
";
        assert_eq!(apply("x = 1
", &[Edit::Compare(OLD)]).unwrap(), format!("x = 1
{added}"));
        let nested = "x = 1
[a]
y = 1
[[b]]
z = 1
[a.c]
w = 1
[[b]]
z = 2
";
        let out = apply(nested, &[Edit::Compare(OLD)]).unwrap();
        assert!(out.starts_with("x = 1

[compare]
"), "{out}");
        assert!(out.ends_with(&nested[6..]), "{out}");
        // A broken file is reported, not overwritten.
        assert!(apply("editor = ", &[Edit::CompareIfMissing(OLD)]).is_err());
        assert!(apply("compare = 1", &[Edit::Compare(OLD)]).is_err());
    }

    #[test]
    fn compare_round_trips() {
        // The template carries the seed; changing it touches only those lines.
        let text = template("", OLD);
        assert_eq!(parse(&text).unwrap().compare, Some(OLD));
        assert_eq!(apply(&text, &[Edit::CompareIfMissing(CompareState::default())]).unwrap(), text);
        let new = CompareState { by_content: false, ignore_date: true, subfolders: true };
        let out = apply(&text, &[Edit::Compare(new)]).unwrap();
        assert_eq!(parse(&out).unwrap().compare, Some(new));
        let changed: Vec<_> = out.lines().filter(|l| !text.lines().any(|t| t == *l)).collect();
        assert_eq!(changed, ["by_content = false", "ignore_date = true"]);
        // A trailing comment on a value survives a change.
        let out = apply("[compare]
subfolders = false # deep
", &[Edit::Compare(new)]).unwrap();
        assert!(out.contains("subfolders = true # deep"), "{out}");
        let back: rc_core::compare::CompareOptions = parse(&out).unwrap().compare.unwrap().into();
        assert_eq!(CompareState::from(back), new);
    }
}
