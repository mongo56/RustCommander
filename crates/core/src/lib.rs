//! UI-agnostic core of RustCommander.
//!
//! Nothing in here knows about iced (or any other UI toolkit). The UI crate
//! translates input into [`Command`]s and renders [`Panel`] state.

pub mod attrib;
pub mod checksum;
pub mod command;
pub mod compare;
pub mod diff;
pub mod duplicates;
pub mod disk_usage;
pub mod entry;
pub mod format;
pub mod git_status;
pub mod fs;
pub mod ops;
pub mod panel;
pub mod remote;
pub mod rename;
pub mod search;
pub mod split;
pub mod sync;
pub mod treemap;
pub mod undo;
pub mod user_cmd;
pub mod vfs;
mod gz_edit;
mod sevenz_edit;
mod tar_edit;
mod tar_sparse;
mod zip_edit;
pub mod viewer;

pub use command::Command;
pub use entry::{Entry, EntryKind};
pub use panel::{Activated, Nav, Panel, Sort, SortKey};

/// Test helper: true (and says so) when Windows won't follow the junction
/// `link` made by this user: ERROR_UNTRUSTED_MOUNT_POINT (448) under
/// redirection trust, seen on Windows 11 build 26300 (2026-11-06). Junction
/// tests skip then, since nothing can read through such a link.
#[cfg(all(test, windows))]
pub(crate) fn junction_untrusted(link: &std::path::Path) -> bool {
    match std::fs::metadata(link) {
        Err(e) if e.raw_os_error() == Some(448) => {
            eprintln!("skipped: Windows doesn't follow junctions made by this user here ({e})");
            true
        }
        _ => false,
    }
}
