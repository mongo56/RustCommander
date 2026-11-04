//! Finding installed terminals (used by the settings view). A terminal needs
//! arguments to start in a folder, so each known one carries its command line
//! arguments (`%1` = the folder).

use std::path::PathBuf;

use crate::editors::{app_path, expand, on_path};

/// A terminal found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundTerminal {
    pub name: String,
    pub path: PathBuf,
    /// Arguments to save after the program, e.g. `-d %1`; may be empty.
    pub args: String,
}

impl FoundTerminal {
    /// The command line for `config.toml`: the program (quoted when it has
    /// spaces) followed by its arguments.
    pub fn command(&self) -> String {
        let program = self.path.display().to_string();
        let program = if program.contains(' ') { format!("\"{program}\"") } else { program };
        if self.args.is_empty() {
            program
        } else {
            format!("{program} {}", self.args)
        }
    }
}

/// Known terminals: display name, executable, arguments (`%1` = folder) and
/// install sub-folders relative to the roots in `detect_terminals`. A
/// trailing `*` in a folder name matches any folder with that prefix.
/// The order is the order of the buttons.
const KNOWN: &[(&str, &str, &str, &[&str])] = &[
    ("Windows Terminal", "wt.exe", "-d %1", &[""]),
    ("PowerShell 7", "pwsh.exe", "-NoLogo", &[r"PowerShell\7", r"PowerShell\7-preview"]),
    ("Windows PowerShell", "powershell.exe", "-NoLogo", &[r"WindowsPowerShell\v1.0"]),
    ("Command Prompt", "cmd.exe", "", &[""]),
    ("Git Bash", "git-bash.exe", "--cd=%1", &["Git"]),
    ("WezTerm", "wezterm-gui.exe", "start --cwd %1", &["WezTerm", r"WezTerm\*"]),
    ("Alacritty", "alacritty.exe", "--working-directory %1", &["Alacritty"]),
    ("ConEmu", "ConEmu64.exe", "-Dir %1", &["ConEmu"]),
    ("Cmder", "Cmder.exe", "/START %1", &["Cmder", "cmder"]),
];

/// Looks for the known terminals on PATH, in the registry's App Paths and in
/// the usual install folders. Touches the disk: call it off the UI thread.
pub fn detect_terminals() -> Vec<FoundTerminal> {
    let env = |v: &str| std::env::var_os(v).map(PathBuf::from);
    let local = env("LOCALAPPDATA");
    let mut roots: Vec<PathBuf> =
        ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"].iter().filter_map(|v| env(v)).collect();
    if let Some(l) = &local {
        roots.push(l.join("Programs"));
        // Windows Terminal's app-execution alias lives here.
        roots.push(l.join(r"Microsoft\WindowsApps"));
    }
    if let Some(sys) = env("SystemRoot") {
        roots.push(sys.join("System32"));
        roots.push(sys.join(r"System32\WindowsPowerShell\v1.0"));
    }
    let mut found = Vec::new();
    for (name, exe, args, folders) in KNOWN {
        let mut candidates: Vec<PathBuf> = Vec::new();
        candidates.extend(on_path(exe));
        candidates.extend(app_path(exe));
        for root in &roots {
            for folder in *folders {
                candidates.extend(expand(root, folder).into_iter().map(|d| d.join(exe)));
            }
        }
        if let Some(path) = candidates.into_iter().find(|p| exists(p)) {
            found.push(FoundTerminal { name: (*name).to_owned(), path, args: (*args).to_owned() });
        }
    }
    found
}

/// Like `is_file`, but also true for app-execution aliases (reparse points
/// that `metadata` cannot follow).
fn exists(path: &std::path::Path) -> bool {
    path.is_file() || std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent() {
        for (i, (name, exe, args, folders)) in KNOWN.iter().enumerate() {
            assert!(!name.is_empty() && exe.ends_with(".exe") && !folders.is_empty(), "{name}");
            assert!(KNOWN[..i].iter().all(|k| k.0 != *name && !k.1.eq_ignore_ascii_case(exe)), "duplicate {name}");
            assert_eq!(*args, args.trim(), "{name}");
            // `%1` stands alone or ends an option, never a separate word twice.
            assert!(args.matches("%1").count() <= 1, "{name}");
        }
        // Order of the buttons: the usual ones first.
        let names: Vec<_> = KNOWN.iter().take(4).map(|k| k.0).collect();
        assert_eq!(names, ["Windows Terminal", "PowerShell 7", "Windows PowerShell", "Command Prompt"]);
        // Only programs that take a folder argument carry `%1`.
        assert!(KNOWN.iter().filter(|k| k.2.contains("%1")).all(|k| k.0 != "Command Prompt" && !k.0.contains("PowerShell")));
    }

    #[test]
    fn command_quotes_a_path_with_spaces() {
        let t = FoundTerminal { name: "x".into(), path: PathBuf::from(r"C:\Program Files\Git\git-bash.exe"), args: "--cd=%1".into() };
        assert_eq!(t.command(), r#""C:\Program Files\Git\git-bash.exe" --cd=%1"#);
        let t = FoundTerminal { name: "x".into(), path: PathBuf::from(r"C:\Windows\System32\cmd.exe"), args: String::new() };
        assert_eq!(t.command(), r"C:\Windows\System32\cmd.exe");
    }

    #[cfg(windows)]
    #[test]
    fn finds_cmd() {
        let found = detect_terminals();
        assert!(found.iter().any(|t| t.name == "Command Prompt"), "{found:?}");
    }
}
