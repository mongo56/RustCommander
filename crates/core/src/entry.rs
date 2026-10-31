use chrono::{DateTime, Local};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// The synthetic `..` entry.
    Parent,
    Dir,
    File,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<DateTime<Local>>,
    pub hidden: bool,
}

impl Entry {
    pub fn parent(path: PathBuf) -> Self {
        Self {
            name: "..".into(),
            path,
            kind: EntryKind::Parent,
            size: 0,
            modified: None,
            hidden: false,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Parent)
    }

    pub fn is_parent(&self) -> bool {
        self.kind == EntryKind::Parent
    }

    /// Splits the name into (stem, extension) the way Total Commander shows
    /// it. Directories and dotfiles have no extension.
    pub fn split_name(&self) -> (&str, &str) {
        if self.kind != EntryKind::File {
            return (&self.name, "");
        }
        match self.name.rfind('.') {
            Some(i) if i > 0 => (&self.name[..i], &self.name[i + 1..]),
            _ => (&self.name, ""),
        }
    }

    pub fn extension(&self) -> &str {
        self.split_name().1
    }
}
