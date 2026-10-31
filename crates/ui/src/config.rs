//! User settings in `config.toml` (next to `session.toml`). The app never
//! rewrites this file: it only creates a commented template when it's missing.
//! It is re-read where a setting is used, so edits apply without a restart.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Editor for F4: a program, optionally with arguments; `%1` stands for
    /// the file (appended when missing). Empty = the platform default.
    pub editor: String,
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
    format!(
        "# RustCommander settings. Changes apply the next time a setting is used.\n\
         \n\
         # Editor for F4 / Shift+F4: a program, optionally with arguments.\n\
         # %1 stands for the file (appended when missing); quote paths with spaces.\n\
         # Empty = Notepad on Windows, else $VISUAL / $EDITOR.\n\
         # Example: editor = '\"C:\\Program Files\\Notepad++\\notepad++.exe\" -multiInst %1'\n\
         editor = {editor}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_back() {
        let editor = r#""C:\Program Files\Ed\ed.exe" -n %1"#;
        assert_eq!(parse(&template(editor)).unwrap().editor, editor);
        assert_eq!(parse(&template("")).unwrap().editor, "");
    }

    #[test]
    fn missing_keys_default_and_typos_fail() {
        assert_eq!(parse("").unwrap().editor, "");
        assert!(parse("editor = ").is_err());
    }
}
