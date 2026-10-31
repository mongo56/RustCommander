//! Session persistence: tabs, paths, sorting and window size are saved on
//! exit to `session.toml` in the config folder and restored on start.

use rc_core::{Panel, SortKey};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub width: f32,
    pub height: f32,
    /// Active side: 0 = left, 1 = right.
    pub active: usize,
    pub left: SideState,
    pub right: SideState,
    /// Directory hotlist (Ctrl+D), in menu order.
    #[serde(default)]
    pub hotlist: Vec<PathBuf>,
    /// Editor for F4: a program, optionally with arguments; `%1` stands for
    /// the file (appended when missing). Empty = the platform default.
    #[serde(default)]
    pub editor: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SideState {
    pub current: usize,
    pub tabs: Vec<TabState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabState {
    pub path: PathBuf,
    #[serde(default = "default_sort")]
    pub sort: String,
    #[serde(default)]
    pub descending: bool,
    #[serde(default)]
    pub show_hidden: bool,
}

fn default_sort() -> String {
    "name".into()
}

impl TabState {
    pub fn from_panel(panel: &Panel) -> Self {
        let sort = match panel.sort.key {
            SortKey::Name => "name",
            SortKey::Ext => "ext",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
        };
        Self {
            path: panel.path.clone(),
            sort: sort.into(),
            descending: panel.sort.descending,
            show_hidden: panel.show_hidden,
        }
    }

    /// Opens the tab's folder, or its closest existing parent if the folder
    /// is gone. `None` when nothing on the path exists any more.
    pub fn open(&self) -> Option<Panel> {
        let mut panel = self.path.ancestors().find(|p| p.is_dir()).and_then(|p| Panel::open(p).ok())?;
        panel.sort.key = match self.sort.as_str() {
            "ext" => SortKey::Ext,
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            _ => SortKey::Name,
        };
        panel.sort.descending = self.descending;
        panel.show_hidden = self.show_hidden;
        panel.reload().ok()?;
        Some(panel)
    }
}

fn file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("RustCommander").join("session.toml"))
}

pub fn load() -> Option<Session> {
    let text = std::fs::read_to_string(file()?).ok()?;
    toml::from_str(&text).ok()
}

pub fn save(session: &Session) -> std::io::Result<()> {
    let path = file().ok_or_else(|| std::io::Error::other("no config folder"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(session).map_err(std::io::Error::other)?;
    // Write then rename, so a crash never leaves a half-written file.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let dir = std::env::temp_dir();
        let session = Session {
            width: 1000.0,
            height: 700.0,
            active: 1,
            left: SideState {
                current: 0,
                tabs: vec![TabState { path: dir.clone(), sort: "size".into(), descending: true, show_hidden: true }],
            },
            right: SideState::default(),
            hotlist: vec![dir.clone()],
            editor: r#""my editor.exe" -n %1"#.into(),
        };
        let text = toml::to_string_pretty(&session).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert_eq!(back.active, 1);
        assert_eq!(back.hotlist, std::slice::from_ref(&dir));
        let panel = back.left.tabs[0].open().unwrap();
        assert_eq!(panel.sort.key, SortKey::Size);
        assert!(panel.sort.descending && panel.show_hidden);
    }

    #[test]
    fn missing_folder_falls_back_to_parent() {
        let tab = TabState {
            path: std::env::temp_dir().join("rc-no-such-folder").join("deeper"),
            sort: default_sort(),
            descending: false,
            show_hidden: false,
        };
        assert_eq!(tab.open().unwrap().path, std::env::temp_dir());
    }
}
