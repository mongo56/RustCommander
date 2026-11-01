//! User settings in `config.toml` (next to `session.toml`). The app never
//! rewrites this file: it only creates a commented template when it's missing.
//! It is re-read where a setting is used, so edits apply without a restart.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Editor for F4: a program, optionally with arguments; `%1` stands for
    /// the file (appended when missing). Empty = the platform default.
    pub editor: String,
    /// Key bindings on top of the defaults: key (`Ctrl+Shift+F5`) ->
    /// command id (`cm_Copy`), `""` unbinds the key.
    pub keys: BTreeMap<String, String>,
    /// Colour overrides: name (see `theme::NAMES`) -> `#rrggbb`.
    pub colors: BTreeMap<String, String>,
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
         # Ctrl+, opens this file in the editor.\n\
         \n\
         # Editor for F4 / Shift+F4: a program, optionally with arguments.\n\
         # %1 stands for the file (appended when missing); quote paths with spaces.\n\
         # Empty = Notepad on Windows, else $VISUAL / $EDITOR.\n\
         # Example: editor = '\"C:\\Program Files\\Notepad++\\notepad++.exe\" -multiInst %1'\n\
         editor = {editor}\n\
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
         # Colours as \"#rrggbb\"; the values below are the defaults.\n\
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
    fn missing_keys_default_and_typos_fail() {
        assert_eq!(parse("").unwrap().editor, "");
        assert!(parse("editor = ").is_err());
    }
}
