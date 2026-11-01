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
    /// Window position and maximised state (Windows).
    #[serde(default)]
    pub window: Option<WindowState>,
    /// Editor setting from older versions; read once to seed `config.toml`.
    #[serde(default, skip_serializing)]
    pub editor: String,
}

/// [`rc_platform::Placement`] in the session file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub maximized: bool,
}

impl From<rc_platform::Placement> for WindowState {
    fn from(p: rc_platform::Placement) -> Self {
        Self { left: p.left, top: p.top, right: p.right, bottom: p.bottom, maximized: p.maximized }
    }
}

impl From<WindowState> for rc_platform::Placement {
    fn from(w: WindowState) -> Self {
        Self { left: w.left, top: w.top, right: w.right, bottom: w.bottom, maximized: w.maximized }
    }
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
    Some(crate::config::dir()?.join("session.toml"))
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
            window: Some(WindowState { left: -8, top: 0, right: 1200, bottom: 800, maximized: true }),
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
