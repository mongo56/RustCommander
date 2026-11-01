//! Finding installed text editors and picking a program with the native
//! file dialog (used by the settings view).

use std::path::{Path, PathBuf};

/// An editor found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundEditor {
    pub name: String,
    pub path: PathBuf,
}

/// Known editors: display name, executable, and install sub-folders
/// (relative to Program Files, Program Files (x86), %LOCALAPPDATA%\Programs).
/// A trailing `*` in a folder name matches any folder with that prefix.
const KNOWN: &[(&str, &str, &[&str])] = &[
    ("Visual Studio Code", "Code.exe", &["Microsoft VS Code"]),
    ("VS Code Insiders", "Code - Insiders.exe", &["Microsoft VS Code Insiders"]),
    ("VSCodium", "VSCodium.exe", &["VSCodium"]),
    ("Notepad++", "notepad++.exe", &["Notepad++"]),
    ("Sublime Text", "sublime_text.exe", &["Sublime Text", "Sublime Text 3"]),
    ("gVim", "gvim.exe", &[r"Vim\vim*", "Vim"]),
    ("Neovim Qt", "nvim-qt.exe", &[r"Neovim\bin", r"nvim-qt\bin"]),
    ("Kate", "kate.exe", &[r"Kate\bin", "Kate"]),
    ("Geany", "geany.exe", &[r"Geany\bin"]),
    ("Zed", "Zed.exe", &["Zed"]),
    ("Atom", "atom.exe", &["Atom"]),
    ("Notepad", "notepad.exe", &[]),
];

/// Looks for the known editors on PATH, in the registry's App Paths and in
/// the usual install folders. Cheap, but touches the disk: call it off the
/// UI thread.
pub fn detect_editors() -> Vec<FoundEditor> {
    let mut found: Vec<FoundEditor> = Vec::new();
    let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
        .chain(std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("Programs")))
        .collect();
    for (name, exe, folders) in KNOWN {
        let mut candidates: Vec<PathBuf> = Vec::new();
        candidates.extend(on_path(exe));
        candidates.extend(app_path(exe));
        for root in &roots {
            for folder in *folders {
                candidates.extend(expand(root, folder).into_iter().map(|d| d.join(exe)));
            }
        }
        if let Some(sys) = std::env::var_os("SystemRoot") {
            if *exe == "notepad.exe" {
                candidates.push(PathBuf::from(sys).join("System32").join(exe));
            }
        }
        for path in candidates {
            if path.is_file() && !found.iter().any(|f| same_file(&f.path, &path)) {
                found.push(FoundEditor { name: (*name).to_owned(), path });
                break;
            }
        }
    }
    found
}

fn same_file(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// `root\folder`, where a `*` ending a component matches by prefix.
fn expand(root: &Path, folder: &str) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for part in folder.split('\\') {
        let mut next = Vec::new();
        for dir in &dirs {
            if let Some(prefix) = part.strip_suffix('*') {
                let Ok(read) = std::fs::read_dir(dir) else { continue };
                for e in read.flatten() {
                    if e.file_name().to_string_lossy().to_lowercase().starts_with(&prefix.to_lowercase()) {
                        next.push(e.path());
                    }
                }
            } else {
                next.push(dir.join(part));
            }
        }
        dirs = next;
    }
    dirs
}

fn on_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file())
}

#[cfg(windows)]
fn app_path(exe: &str) -> Option<PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let key: Vec<u16> =
        format!("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe}\0").encode_utf16().collect();
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let mut buf = [0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: `key` is NUL-terminated; `buf` and `size` match.
        let status = unsafe {
            RegGetValueW(
                hive,
                PCWSTR(key.as_ptr()),
                PCWSTR::null(),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        
        if status.is_ok() {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let value = String::from_utf16_lossy(&buf[..len]);
            let value = value.trim().trim_matches('"');
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn app_path(_exe: &str) -> Option<PathBuf> {
    None
}

/// Shows the native "Open" dialog for a program; `None` if cancelled.
/// Blocks until closed: call it off the UI thread.
pub fn pick_program() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::UI::Controls::Dialogs::{
            GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
        };
        let filter: Vec<u16> = "Programs (*.exe;*.bat;*.cmd)\0*.exe;*.bat;*.cmd\0All files (*.*)\0*.*\0\0".encode_utf16().collect();
        let title: Vec<u16> = "Choose the editor\0".encode_utf16().collect();
        let mut file = vec![0u16; 4096];
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: PWSTR(file.as_mut_ptr()),
            nMaxFile: file.len() as u32,
            lpstrTitle: PCWSTR(title.as_ptr()),
            Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
            ..Default::default()
        };
        // SAFETY: every pointer in `ofn` outlives the call.
        if !unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
            return None;
        }
        let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
        Some(PathBuf::from(String::from_utf16_lossy(&file[..len])))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn finds_notepad() {
        let found = super::detect_editors();
        assert!(found.iter().any(|e| e.name == "Notepad"), "{found:?}");
    }
}
