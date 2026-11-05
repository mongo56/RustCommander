//! Local filesystem listing. Panels list through `crate::vfs`, which uses
//! this for local folders (`vfs::LocalFs`) and archive readers otherwise.

use crate::entry::{Entry, EntryKind};
use chrono::{DateTime, Local};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub fn list_dir(path: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();

    if let Some(parent) = path.parent() {
        entries.push(Entry::parent(parent.to_path_buf()));
    } else if let Some((server, _)) = crate::vfs::unc_share_parent(path) {
        // A share's root: up is the server's share list.
        entries.push(Entry::parent(server));
    }

    for item in fs::read_dir(path)? {
        let Ok(item) = item else { continue };
        let item_path = item.path();
        let file_type = item.file_type().ok();
        let is_symlink = file_type.map(|t| t.is_symlink()).unwrap_or(false);

        // Follow symlinks so a link to a directory behaves like a directory.
        let meta = if is_symlink {
            fs::metadata(&item_path).or_else(|_| item.metadata())
        } else {
            item.metadata()
        };

        let name = item.file_name().to_string_lossy().into_owned();
        // Hidden/system attributes belong to the link itself (e.g. the
        // "Application Data" junction), not to its target.
        let hidden = match item.metadata() {
            Ok(m) => is_hidden(&name, &m),
            Err(_) => name.starts_with('.'),
        };
        let (is_dir, size, modified) = match &meta {
            Ok(m) => (
                m.is_dir(),
                if m.is_dir() { 0 } else { m.len() },
                m.modified().ok().map(DateTime::<Local>::from),
            ),
            Err(_) => (false, 0, None),
        };

        if hidden && !show_hidden {
            continue;
        }

        entries.push(Entry {
            name,
            path: item_path,
            kind: if is_dir { EntryKind::Dir } else { EntryKind::File },
            size,
            modified,
            hidden,
        });
    }

    Ok(entries)
}

#[cfg(windows)]
pub(crate) fn is_hidden(_name: &str, meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

#[cfg(not(windows))]
pub(crate) fn is_hidden(name: &str, _meta: &fs::Metadata) -> bool {
    name.starts_with('.')
}

/// Total size of the files under `path`, recursively. Links and junctions
/// count as themselves and are not followed; unreadable parts are skipped.
/// Returns early (with a partial sum) once `cancel` is set.
pub fn dir_size(path: &Path, cancel: &AtomicBool) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(items) = fs::read_dir(&dir) else { continue };
        for item in items.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return total;
            }
            let Ok(meta) = item.metadata() else { continue };
            if meta.is_dir() {
                stack.push(item.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// Expands Windows-style `%NAME%` variables in `text` using `lookup`
/// (which should ignore the case of the name, as Windows does). Unknown
/// variables, a lone `%` and `%%` stay as typed, like in cmd.
pub fn expand_env(text: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(rest_after) = after.strip_prefix('%') {
            out.push_str("%%");
            rest = rest_after;
            continue;
        }
        let name_end = after.find(['%', '/', '\\']).filter(|&i| after[i..].starts_with('%'));
        match name_end.and_then(|i| lookup(&after[..i]).map(|v| (i, v))) {
            Some((i, value)) => {
                out.push_str(&value);
                rest = &after[i + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_size_sums_nested_files() {
        let root = std::env::temp_dir().join(format!("rc-dirsize-{}", std::process::id()));
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("x"), [0u8; 10]).unwrap();
        fs::write(root.join("a/y"), [0u8; 20]).unwrap();
        fs::write(root.join("a/b/z"), [0u8; 30]).unwrap();
        assert_eq!(dir_size(&root, &AtomicBool::new(false)), 60);
        assert_eq!(dir_size(&root, &AtomicBool::new(true)), 0);
        fs::remove_dir_all(&root).unwrap();
    }

    fn fake_env(name: &str) -> Option<String> {
        match name.to_ascii_lowercase().as_str() {
            "temp" => Some("C:\\Users\\me\\Temp".into()),
            "appdata" => Some("C:\\Users\\me\\AppData\\Roaming".into()),
            _ => None,
        }
    }

    #[test]
    fn expands_variables() {
        assert_eq!(expand_env("%temp%", fake_env), "C:\\Users\\me\\Temp");
        assert_eq!(expand_env("%TEMP%\\sub", fake_env), "C:\\Users\\me\\Temp\\sub");
        assert_eq!(
            expand_env("%appdata%\\a\\%temp%", fake_env),
            "C:\\Users\\me\\AppData\\Roaming\\a\\C:\\Users\\me\\Temp"
        );
        assert_eq!(expand_env("plain\\path", fake_env), "plain\\path");
    }

    #[test]
    fn leaves_unknown_and_odd_percents() {
        assert_eq!(expand_env("%nope%\\x", fake_env), "%nope%\\x");
        assert_eq!(expand_env("100%", fake_env), "100%");
        assert_eq!(expand_env("%", fake_env), "%");
        assert_eq!(expand_env("a%%b", fake_env), "a%%b");
        assert_eq!(expand_env("%%temp%", fake_env), "%%temp%");
        assert_eq!(expand_env("%a\\b%temp%", fake_env), "%a\\bC:\\Users\\me\\Temp");
        assert_eq!(expand_env("50% of %temp%", fake_env), "50% of C:\\Users\\me\\Temp");
    }
}
