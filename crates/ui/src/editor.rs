//! External editor for F4. The command line comes from `editor` in
//! `config.toml`, e.g. `"C:\Program Files\Notepad++\notepad++.exe" -multiInst %1`.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// Opens `path` in the editor `command` (empty = platform default).
pub fn launch(command: &str, path: &Path) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let command = if command.trim().is_empty() { default_editor() } else { Some(command.to_owned()) };
    let Some(command) = command else {
        return rc_platform::open_default(path);
    };
    let (program, args) = command_line(&command, path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "editor command is empty"))?;
    rc_platform::spawn_detached(&program, &args, dir)
}

fn default_editor() -> Option<String> {
    if cfg!(windows) {
        Some("notepad.exe".into())
    } else {
        std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).ok().filter(|e| !e.trim().is_empty())
    }
}

/// Program and arguments of `command` with `%1` replaced by `path`, or
/// `path` appended when there's no `%1`.
fn command_line(command: &str, path: &Path) -> Option<(String, Vec<OsString>)> {
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
    if !used {
        args.push(path.as_os_str().to_owned());
    }
    Some((program, args))
}

/// Splits on whitespace outside double quotes; quotes are removed.
fn split(command: &str) -> Vec<String> {
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
        let (program, args) = command_line("ed -n", path).unwrap();
        assert_eq!((program.as_str(), args), ("ed", vec![OsString::from("-n"), OsString::from("dir/a b.txt")]));
        let (_, args) = command_line("ed --file=%1 -x", path).unwrap();
        assert_eq!(args, [OsString::from("--file=dir/a b.txt"), OsString::from("-x")]);
        assert!(command_line("", path).is_none());
    }
}
