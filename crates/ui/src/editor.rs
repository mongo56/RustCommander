//! External editor for F4. The command line comes from `editor` in
//! `config.toml`, e.g. `"C:\Program Files\Notepad++\notepad++.exe" -multiInst %1`.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Child;

/// Opens `path` in the editor `command` (empty = platform default). Returns
/// the editor's process when one was started directly (not through the
/// file's default app).
pub fn launch(command: &str, path: &Path) -> io::Result<Option<Child>> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let command = if command.trim().is_empty() { default_editor() } else { Some(command.to_owned()) };
    let Some(command) = command else {
        return rc_platform::open_default(path).map(|_| None);
    };
    let (program, args) = command_line(&command, path, true)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "editor command is empty"))?;
    rc_platform::spawn_detached(&program, &args, dir).map(Some)
}

fn default_editor() -> Option<String> {
    if cfg!(windows) {
        Some("notepad.exe".into())
    } else {
        std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).ok().filter(|e| !e.trim().is_empty())
    }
}

/// Program and arguments of `command` with `%1` replaced by `path`, or
/// (when `append`) `path` appended when there's no `%1`.
pub(crate) fn command_line(command: &str, path: &Path, append: bool) -> Option<(String, Vec<OsString>)> {
    let mut words = split(command).into_iter();
    let program = words.next()?;
    let mut used = false;
    let mut args: Vec<OsString> = words
        .map(|w| {
            if w.contains("%1") {
                used = true;
                OsString::from(w.replace("%1", &path.to_string_lossy()))
            } else {
                OsString::from(w)
            }
        })
        .collect();
    if !used && append {
        args.push(path.as_os_str().to_owned());
    }
    Some((program, args))
}

/// Opens `path` at `line` in the editor `command`, when it can be told the
/// line: `%L` in the command stands for it, else known editors get their own
/// option (Notepad++ `-n`, VS Code `-g file:line`, Sublime Text `file:line`).
/// `None`: the editor (or the default one) takes no line.
pub fn launch_at_line(command: &str, path: &Path, line: u32) -> Option<io::Result<Child>> {
    let (program, args) = line_command(command, path, line)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    Some(rc_platform::spawn_detached(&program, &args, dir))
}

/// Program and arguments that open `path` at `line` (see `launch_at_line`).
pub(crate) fn line_command(command: &str, path: &Path, line: u32) -> Option<(String, Vec<OsString>)> {
    if command.contains("%L") {
        return command_line(&command.replace("%L", &line.to_string()), path, true);
    }
    let (program, mut args) = command_line(command, path, true)?;
    let stem = Path::new(&program).file_stem()?.to_string_lossy().to_lowercase();
    let file = path.as_os_str();
    match stem.as_str() {
        "notepad++" => args.insert(0, OsString::from(format!("-n{line}"))),
        "code" | "code-insiders" | "codium" | "cursor" | "subl" | "sublime_text" => {
            // The file as `file:line`; VS Code needs `-g` for that.
            let at = args.iter().rposition(|a| a == file)?;
            let mut with_line = file.to_owned();
            with_line.push(format!(":{line}"));
            args[at] = with_line;
            if !stem.starts_with("subl") {
                args.insert(at, OsString::from("-g"));
            }
        }
        _ => return None,
    }
    Some((program, args))
}

/// Splits on whitespace outside double quotes; quotes are removed.
pub(crate) fn split(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    for c in command.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_words() {
        assert_eq!(split(r#""C:\Program Files\x.exe" -a  "b c""#), [r"C:\Program Files\x.exe", "-a", "b c"]);
        assert_eq!(split("  "), Vec::<String>::new());
        assert_eq!(split(r#"x """#), ["x", ""]);
    }

    #[test]
    fn places_the_file() {
        let path = Path::new("dir/a b.txt");
        let (program, args) = command_line("ed -n", path, true).unwrap();
        assert_eq!((program.as_str(), args), ("ed", vec![OsString::from("-n"), OsString::from("dir/a b.txt")]));
        let (_, args) = command_line("ed --file=%1 -x", path, true).unwrap();
        assert_eq!(args, [OsString::from("--file=dir/a b.txt"), OsString::from("-x")]);
        assert!(command_line("", path, true).is_none());
        let (_, args) = command_line("ed -n", path, false).unwrap();
        assert_eq!(args, [OsString::from("-n")]);
    }

    #[test]
    fn opens_at_a_line_when_the_editor_takes_one() {
        let path = Path::new(r"C:\a b\x.rs");
        let args = |command: &str| line_command(command, path, 12).map(|(_, a)| a);
        let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(args("ed +%L %1"), Some(os(&["+12", r"C:\a b\x.rs"])));
        let npp = r#""C:\Program Files\Notepad++\notepad++.exe" -multiInst %1"#;
        assert_eq!(args(npp), Some(os(&["-n12", "-multiInst", r"C:\a b\x.rs"])));
        assert_eq!(args("code.cmd --reuse-window"), Some(os(&["--reuse-window", "-g", r"C:\a b\x.rs:12"])));
        assert_eq!(args("subl"), Some(os(&[r"C:\a b\x.rs:12"])));
        assert_eq!(args("notepad.exe"), None);
        assert_eq!(args(""), None);
    }
}
