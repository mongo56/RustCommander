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
    /// Key bindings on top of the defaults: key (`Ctrl+Shift+F5`) ->
    /// command id (`cm_Copy`), `""` unbinds the key.
    pub keys: BTreeMap<String, String>,
    /// Colour overrides: name (see `theme::NAMES`) -> `#rrggbb`.
    pub colors: BTreeMap<String, String>,
}

impl Config {
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
        }
    }
    let out = doc.to_string();
    parse(&out)?;
    Ok(out)
}

/// Applies `edits` to `config.toml` (created from the template first if
/// it's missing).
pub fn write(edits: &[Edit]) -> Result<(), String> {
    create_if_missing("").map_err(|e| e.to_string())?;
    let path = file().ok_or("no config folder")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let out = apply(&text, edits).map_err(|e| format!("{}: {e}", path.display()))?;
    if out != text {
        std::fs::write(&path, out).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
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
/// seeds the setting (carried over from older `session.toml` files).
pub fn create_if_missing(editor: &str) -> std::io::Result<()> {
    let path = file().ok_or_else(|| std::io::Error::other("no config folder"))?;
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, template(editor))
}

fn template(editor: &str) -> String {
    let editor = toml::Value::String(editor.to_owned());
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
         # Colour theme: {themes}. [colors] below overrides single colours.\n\
         theme = \"dark\"\n\
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
        assert_eq!(parse(&template(editor)).unwrap().editor, editor);
        let config = parse(&template("")).unwrap();
        assert_eq!(config.editor, "");
        assert!(config.keys.is_empty() && config.colors.is_empty());
        // The commented-out colour lines are valid settings once uncommented.
        let uncommented = template("").replace("\n# bg = ", "\nbg = ");
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
        assert!(config.show_hidden);
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
        let text = template("notepad");
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
}
