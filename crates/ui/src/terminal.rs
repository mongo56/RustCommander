//! Terminal for F9, opened in the active panel's folder. The command line
//! comes from `terminal` in `config.toml`, e.g. `wt -d %1`; `%1` stands for
//! the folder (it is always the working directory, so `%1` is optional).

use std::io;
use std::path::Path;

use crate::editor::command_line;

/// Opens a terminal in `dir` using `command` (empty = platform default).
pub fn launch(command: &str, dir: &Path) -> io::Result<()> {
    let command = if command.trim().is_empty() { default_terminal() } else { command.to_owned() };
    let (program, args) = command_line(&command, dir, false)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "terminal command is empty"))?;
    rc_platform::spawn_detached(&program, &args, dir).map(|_| ())
}

/// Windows Terminal when installed, else PowerShell; elsewhere `$TERMINAL`
/// or `x-terminal-emulator`.
fn default_terminal() -> String {
    default_for(cfg!(windows), on_path("wt.exe"), std::env::var("TERMINAL").ok())
}

fn default_for(windows: bool, has_wt: bool, terminal_env: Option<String>) -> String {
    if windows {
        if has_wt { "wt.exe -d %1" } else { "powershell.exe -NoLogo" }.to_owned()
    } else {
        terminal_env.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "x-terminal-emulator".into())
    }
}

/// True when `name` is found in a `PATH` folder.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// Program and arguments for tests and callers that only need the line.
#[cfg(test)]
fn build(command: &str, dir: &Path) -> Option<(String, Vec<std::ffi::OsString>)> {
    command_line(command, dir, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn defaults() {
        assert_eq!(default_for(true, true, None), "wt.exe -d %1");
        assert_eq!(default_for(true, false, None), "powershell.exe -NoLogo");
        assert_eq!(default_for(false, false, Some("kitty".into())), "kitty");
        assert_eq!(default_for(false, false, Some(" ".into())), "x-terminal-emulator");
        assert_eq!(default_for(false, false, None), "x-terminal-emulator");
    }

    #[test]
    fn folder_is_only_placed_with_a_placeholder() {
        let dir = Path::new(r"C:\My Files");
        let (program, args) = build("wt.exe -d %1", dir).unwrap();
        assert_eq!((program.as_str(), args), ("wt.exe", vec![OsString::from("-d"), OsString::from(r"C:\My Files")]));
        let (_, args) = build("kitty -1", dir).unwrap();
        assert_eq!(args, [OsString::from("-1")]);
        assert!(build(" ", dir).is_none());
    }
}
