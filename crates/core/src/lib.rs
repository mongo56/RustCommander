//! UI-agnostic core of RustCommander.
//!
//! Nothing in here knows about iced (or any other UI toolkit). The UI crate
//! translates input into [`Command`]s and renders [`Panel`] state.

pub mod command;
pub mod compare;
pub mod diff;
pub mod disk_usage;
pub mod entry;
pub mod format;
pub mod fs;
pub mod ops;
pub mod panel;
pub mod remote;
pub mod rename;
pub mod search;
pub mod treemap;
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
