//! User settings in `config.toml` (next to `session.toml`). A commented
//! template is created when it's missing; the settings view (F10) changes
//! single entries with `toml_edit` (`write`), keeping the user's comments,
//! layout and unknown keys. It is re-read where a setting is used, so edits
//! apply without a restart.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Editor for F4: a program, optionally with arguments; `%1` stands for
    /// the file (appended when missing). Empty = the platform default.
    pub editor: String,
    /// Terminal for F9: a program, optionally with arguments; `%1` stands
    /// for the folder (optional: it is the working directory anyway).
    /// Empty = Windows Terminal / PowerShell, else $TERMINAL.
    pub terminal: String,
    /// Colour theme: a `theme::PRESETS` id; empty = dark.
    pub theme: String,
    /// Hidden files shown at the first start (no session yet); the
    /// settings view also applies a change to every open tab.
    pub show_hidden: bool,
    /// The function-key bar (F1-F10 buttons) under the panels; off = a
    /// one-line hint instead.
    pub show_fkeys: bool,
    /// The hotlist tree: a button left of the left panel's drive buttons
    /// (and Ctrl+Shift+D) show / hide the hotlist as a tree left of the panels.
    pub hotlist_tree: bool,
    /// Look for a newer release on GitHub at startup (in the background;
    /// a newer one is offered in the status line).
    pub check_updates_at_startup: bool,
    /// Edge of the thumbnail view's pictures in pixels (Ctrl+Shift+F5);
    /// 0 = the default, see [`Config::thumbnail_size`].
    pub thumbnail_size: u32,
    /// Key bindings on top of the defaults: key (`Ctrl+Shift+F5`) ->
    /// command id (`cm_Copy`), `""` unbinds the key.
    pub keys: BTreeMap<String, String>,
    /// Colour overrides: name (see `theme::NAMES`) -> `#rrggbb`.
    pub colors: BTreeMap<String, String>,
    /// Compare-folders options (Ctrl+Shift+F2); `None` = no `[compare]`
    /// table (older files kept them in `session.toml`).
    pub compare: Option<CompareState>,
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

impl Config {
    /// The thumbnail edge in pixels: `thumbnail_size`, 128 when unset,
    /// kept within 32..=512.
    pub fn thumbnail_size(&self) -> u32 {
        if self.thumbnail_size == 0 { 128 } else { self.thumbnail_size.clamp(32, 512) }
    }

    /// The theme preset with the `[colors]` overrides on top.
    pub fn colors(&self) -> Result<crate::theme::Colors, String> {
        let preset = crate::theme::preset(&self.theme).ok_or_else(|| format!("unknown theme \"{}\"", self.theme))?;
        preset.colors.with(&self.colors).map_err(|e| format!("[colors]: {e}"))
    }
}

/// One change to `config.toml`.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// A top-level text setting (`editor`, `terminal`, `theme`).
    Text(&'static str, String),
    /// A top-level switch (`show_hidden`).
    Flag(&'static str, bool),
    /// A `[keys]` entry by canonical key name (`keymap::KeyEdit`): `Some`
    /// sets it, `None` drops every spelling of that key.
    Key(crate::keymap::KeyEdit),
    /// A `[colors]` entry: `Some("#rrggbb")` sets it, `None` drops it.
    Color(String, Option<String>),
    /// Drops every `[colors]` entry (a theme was picked).
    ClearColors,
    /// Sets the three `[compare]` switches.
    Compare(CompareState),
    /// Adds `[compare]` with these values unless the file has one already
    /// (moving the options over from an old `session.toml`).
    CompareIfMissing(CompareState),
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
        let value = crate::theme::DARK.get(name).map(crate::theme::to_hex).unwrap_or_default();
        colors.push_str(&format!("# {name} = \"{value}\"\n"));
    }
    format!(
        "# RustCommander settings. Changes apply as soon as the file is saved.\n\
         # Ctrl+, opens this file in the editor; F10 opens the settings view, which\n\
         # changes single entries and keeps your comments.\n\
         \n\
         # Editor for F4 / Shift+F4: a program, optionally with arguments.\n\
         # %1 stands for the file (appended when missing); quote paths with spaces.\n\
         # Empty = Notepad on Windows, else $VISUAL / $EDITOR.\n\
         # Example: editor = '\"C:\\Program Files\\Notepad++\\notepad++.exe\" -multiInst %1'\n\
         editor = {editor}\n\
         \n\
         # Terminal for F9: a program, optionally with arguments; it starts in the\n\
         # active panel's folder. %1 stands for that folder (optional).\n\
         # Empty = Windows Terminal if installed, else PowerShell; elsewhere $TERMINAL\n\
         # or x-terminal-emulator. Example: terminal = 'wt.exe -d %1'\n\
         terminal = \"\"\n\
         \n\
         # Show hidden files at the first start (Ctrl+H toggles them per tab).\n\
         show_hidden = false\n\
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
         # Size of the pictures in the thumbnail view (Ctrl+Shift+F5), in pixels\n\
         # (32-512).\n\
         thumbnail_size = 128\n\
         \n\
         # Colour theme: {themes}. [colors] below overrides single colours.\n\
         theme = \"dark\"\n\
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
         \n\
         # Colours as \"#rrggbb\"; the values below are the dark theme's.\n\
         [colors]\n\
         {colors}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_back() {
        let editor = r#""C:\Program Files\Ed\ed.exe" -n %1"#;
        assert_eq!(parse(&template(editor, CompareState::default())).unwrap().editor, editor);
        let config = parse(&template("", CompareState::default())).unwrap();
        assert_eq!(config.editor, "");
        assert!(config.keys.is_empty() && config.colors.is_empty());
        // The commented-out colour lines are valid settings once uncommented.
        let uncommented = template("", CompareState::default()).replace("\n# bg = ", "\nbg = ");
        assert_eq!(parse(&uncommented).unwrap().colors["bg"], "#14161a");
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
        assert!(!parse(&template("", CompareState::default())).unwrap().hotlist_tree);
        assert!(parse(&apply(&out, &[Edit::Flag("hotlist_tree", true)]).unwrap()).unwrap().hotlist_tree);
        assert!(!parse(&template("", CompareState::default())).unwrap().check_updates_at_startup);
        let on = apply(&out, &[Edit::Flag("check_updates_at_startup", true)]).unwrap();
        assert!(parse(&on).unwrap().check_updates_at_startup);
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
        assert!(parse("editor = ").is_err());
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
