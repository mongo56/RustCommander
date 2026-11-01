//! Session persistence: tabs, paths, sorting and window size are saved on
//! exit to `session.toml` in the config folder and restored on start.

use crate::config::CompareState;
use crate::hotlist::HotItem;
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
    /// Directory hotlist (Ctrl+D), in menu order. Older versions wrote a
    /// list of paths, which still loads (a folder is a plain path).
    #[serde(default)]
    pub hotlist: Vec<HotItem>,
    /// Window position and maximised state (Windows).
    #[serde(default)]
    pub window: Option<WindowState>,
    /// Share of the width the left panel gets; older files lack it.
    #[serde(default = "default_split")]
    pub split: f32,
    /// Compare-folders options from older versions, moved to `config.toml`
    /// on start (`main`). Written back only while that move failed, so
    /// they aren't lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<CompareState>,
    /// Ids of the commands last run from the command palette (Ctrl+Space),
    /// most recent first.
    #[serde(default)]
    pub recent_commands: Vec<String>,
    /// Editor setting from older versions; read once to seed `config.toml`.
    #[serde(default, skip_serializing)]
    pub editor: String,
}

fn default_split() -> f32 {
    0.5
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
    /// Thumbnail view (Ctrl+Shift+F5) instead of the list.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thumbnails: bool,
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
            thumbnails: panel.thumbnails,
        }
    }

    /// Opens the tab's folder, or its closest existing parent if the folder
    /// is gone. `None` when nothing on the path exists any more.
    pub fn open(&self) -> Option<Panel> {
        // The folder, or the nearest one that still exists (an archive or a
        // folder in one counts).
        let mut panel = self
            .path
            .ancestors()
            .filter(|p| p.is_dir() || rc_core::vfs::in_archive(p))
            .find_map(|p| Panel::open(p).ok())?;
        panel.sort.key = match self.sort.as_str() {
            "ext" => SortKey::Ext,
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            _ => SortKey::Name,
        };
        panel.sort.descending = self.descending;
        panel.show_hidden = self.show_hidden;
        panel.thumbnails = self.thumbnails;
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
                tabs: vec![TabState { path: dir.clone(), sort: "size".into(), descending: true, show_hidden: true, thumbnails: true }],
            },
            right: SideState::default(),
            hotlist: vec![
                HotItem::Folder(dir.clone()),
                HotItem::Menu { name: "Work".into(), items: vec![HotItem::Folder(dir.join("x"))] },
                HotItem::Menu { name: "Empty".into(), items: vec![] },
            ],
            window: Some(WindowState { left: -8, top: 0, right: 1200, bottom: 800, maximized: true }),
            compare: Some(CompareState { by_content: true, ignore_date: false, subfolders: true }),
            recent_commands: vec!["cm_MkDir".into(), "cm_Copy".into()],
            split: 0.3,
            editor: r#""my editor.exe" -n %1"#.into(),
        };
        let text = toml::to_string_pretty(&session).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert_eq!(back.active, 1);
        assert_eq!(back.split, 0.3);
        assert_eq!(back.hotlist, session.hotlist);
        assert_eq!(back.recent_commands, session.recent_commands);
        // Compare options not yet moved to config.toml are kept...
        assert_eq!(back.compare, session.compare);
        // ...and dropped once they were.
        let moved = toml::to_string_pretty(&Session { compare: None, ..back.clone() }).unwrap();
        assert!(!moved.contains("compare"), "{moved}");
        assert_eq!(toml::from_str::<Session>(&moved).unwrap().compare, None);
        let panel = back.left.tabs[0].open().unwrap();
        assert_eq!(panel.sort.key, SortKey::Size);
        assert!(panel.sort.descending && panel.show_hidden && panel.thumbnails);
        // List-view tabs don't write the switch.
        assert_eq!(text.matches("thumbnails").count(), 1, "{text}");
    }

    #[test]
    fn loads_old_plain_hotlist() {
        let text = r#"
            width = 800.0
            height = 600.0
            active = 0
            hotlist = ['C:\Users', 'D:\Work']
            [left]
            current = 0
            tabs = []
            [right]
            current = 0
            tabs = []
            [compare]
            by_content = true
            subfolders = true
        "#;
        let session: Session = toml::from_str(text).unwrap();
        assert_eq!(session.compare, Some(CompareState { by_content: true, ignore_date: false, subfolders: true }));
        let folders = [r"C:\Users", r"D:\Work"].map(|p| HotItem::Folder(PathBuf::from(p)));
        assert_eq!(session.hotlist, folders);
        assert_eq!(session.split, 0.5);
        // Saved again, plain folders keep the old form.
        let saved = toml::to_string_pretty(&session).unwrap();
        assert!(saved.contains("Users"), "{saved}");
        let back: Session = toml::from_str(&saved).unwrap();
        assert_eq!(back.hotlist, folders);
    }

    #[test]
    fn missing_folder_falls_back_to_parent() {
        let tab = TabState {
            path: std::env::temp_dir().join("rc-no-such-folder").join("deeper"),
            sort: default_sort(),
            descending: false,
            show_hidden: false,
            thumbnails: false,
        };
        assert_eq!(tab.open().unwrap().path, std::env::temp_dir());
    }
}
