//! Changing attributes and dates (`cm_SetAttrib`): the pure parts (tri-state
//! flags applied to an attribute mask, date text) and the per-file change
//! that `Job::SetAttrib` in `ops.rs` runs.

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use std::io;
use std::path::Path;
use std::time::SystemTime;

pub const READ_ONLY: u32 = 0x01;
pub const HIDDEN: u32 = 0x02;
pub const SYSTEM: u32 = 0x04;
pub const ARCHIVE: u32 = 0x20;
/// A symlink or junction.
pub const REPARSE_POINT: u32 = 0x400;

/// The flags the dialog offers, in its order.
pub const FLAGS: [(u32, &str); 4] = [(READ_ONLY, "Read-only"), (HIDDEN, "Hidden"), (SYSTEM, "System"), (ARCHIVE, "Archive")];

/// What to do with one flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tri {
    #[default]
    Leave,
    Set,
    Clear,
}

impl Tri {
    /// The next state when toggled: leave, set, clear, leave, ...
    pub fn next(self) -> Tri {
        match self {
            Tri::Leave => Tri::Set,
            Tri::Set => Tri::Clear,
            Tri::Clear => Tri::Leave,
        }
    }
}

/// A change to apply to files: the four flags (in [`FLAGS`] order) and the
/// dates (`None`: leave).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Change {
    pub flags: [Tri; 4],
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        *self == Change::default()
    }
}

/// `attrs` with the flags changed as `flags` says.
pub fn apply_flags(attrs: u32, flags: &[Tri; 4]) -> u32 {
    let mut out = attrs;
    for (&(bit, _), tri) in FLAGS.iter().zip(flags) {
        match tri {
            Tri::Leave => {}
            Tri::Set => out |= bit,
            Tri::Clear => out &= !bit,
        }
    }
    out
}

/// The flags of `attrs` as set / clear states (what the dialog starts from).
pub fn flags_of(attrs: u32) -> [Tri; 4] {
    FLAGS.map(|(bit, _)| if attrs & bit != 0 { Tri::Set } else { Tri::Clear })
}

const DATE_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// `YYYY-MM-DD HH:MM:SS` in local time.
pub fn format_time(t: SystemTime) -> String {
    chrono::DateTime::<Local>::from(t).format(DATE_FORMAT).to_string()
}

/// Parses local `YYYY-MM-DD HH:MM:SS` (the seconds, or the whole time, may be
/// left out). Empty text is `Ok(None)`: leave the date alone.
pub fn parse_time(text: &str) -> Result<Option<SystemTime>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let naive = NaiveDateTime::parse_from_str(text, DATE_FORMAT)
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M"))
        .or_else(|_| NaiveDate::parse_from_str(text, "%Y-%m-%d").map(|d| d.and_time(Default::default())))
        .map_err(|_| format!("\"{text}\" is not a date: use YYYY-MM-DD HH:MM:SS"))?;
    match Local.from_local_datetime(&naive).earliest() {
        Some(t) => Ok(Some(t.into())),
        None => Err(format!("\"{text}\" does not exist in local time (clock change)")),
    }
}

/// The attribute bits of `path` (a link's own).
#[cfg(windows)]
pub fn get_attributes(path: &Path) -> io::Result<u32> {
    use std::os::windows::fs::MetadataExt;
    Ok(std::fs::symlink_metadata(path)?.file_attributes())
}

#[cfg(not(windows))]
pub fn get_attributes(path: &Path) -> io::Result<u32> {
    std::fs::symlink_metadata(path).map(|_| 0)
}

/// Whether `path` is a symlink or junction (not to be followed).
pub fn is_link(path: &Path) -> bool {
    get_attributes(path).is_ok_and(|a| a & REPARSE_POINT != 0)
        || std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Sets the dates of `path` (a link itself, not its target).
#[cfg(windows)]
fn set_times(path: &Path, change: &Change) -> io::Result<()> {
    use std::fs::{FileTimes, OpenOptions};
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut times = FileTimes::new();
    if let Some(t) = change.modified {
        times = times.set_modified(t);
    }
    if let Some(t) = change.created {
        times = times.set_created(t);
    }
    // Backup semantics: folders can be opened. Write-attributes access only:
    // works on read-only files.
    OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?
        .set_times(times)
}

#[cfg(not(windows))]
fn set_times(_path: &Path, _change: &Change) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "dates can only be changed on Windows"))
}

/// Applies `change` to the one item `path` (not what is inside it). The dates
/// are set first (with the read-only flag cleared meanwhile if the open needs
/// that), then the flags.
pub fn apply(path: &Path, change: &Change) -> io::Result<()> {
    let old = get_attributes(path)?;
    let new = apply_flags(old, &change.flags);
    if change.modified.is_some() || change.created.is_some() {
        if let Err(e) = set_times(path, change) {
            if e.kind() != io::ErrorKind::PermissionDenied || old & READ_ONLY == 0 {
                return Err(e);
            }
            rc_platform::set_file_attributes(path, old & !READ_ONLY)?;
            let retry = set_times(path, change);
            // Back to how it was (whatever happened), then the new flags.
            rc_platform::set_file_attributes(path, old)?;
            retry?;
        }
    }
    if new != old {
        rc_platform::set_file_attributes(path, new)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn flags_are_set_cleared_or_left() {
        let attrs = READ_ONLY | ARCHIVE | 0x10;
        assert_eq!(apply_flags(attrs, &[Tri::Leave; 4]), attrs);
        let flags = [Tri::Clear, Tri::Set, Tri::Leave, Tri::Clear];
        assert_eq!(apply_flags(attrs, &flags), HIDDEN | 0x10);
        assert_eq!(apply_flags(0, &[Tri::Set; 4]), READ_ONLY | HIDDEN | SYSTEM | ARCHIVE);
    }

    #[test]
    fn tri_cycles() {
        assert_eq!(Tri::Leave.next(), Tri::Set);
        assert_eq!(Tri::Set.next(), Tri::Clear);
        assert_eq!(Tri::Clear.next(), Tri::Leave);
    }

    #[test]
    fn flags_of_reads_the_mask() {
        assert_eq!(flags_of(READ_ONLY | SYSTEM), [Tri::Set, Tri::Clear, Tri::Set, Tri::Clear]);
    }

    #[test]
    fn dates_parse_and_format() {
        assert_eq!(parse_time("  "), Ok(None));
        let t = parse_time("2021-03-04 05:06:07").unwrap().unwrap();
        assert_eq!(format_time(t), "2021-03-04 05:06:07");
        assert_eq!(format_time(parse_time("2021-03-04 05:06").unwrap().unwrap()), "2021-03-04 05:06:00");
        assert_eq!(format_time(parse_time("2021-03-04").unwrap().unwrap()), "2021-03-04 00:00:00");
        assert!(parse_time("2021-13-04 00:00:00").is_err());
        assert!(parse_time("yesterday").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn changes_flags_and_dates_of_a_file_and_a_folder() {
        let dir = std::env::temp_dir().join(format!("rc-attrib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, b"x").unwrap();

        let modified = parse_time("2020-02-03 04:05:06").unwrap();
        let created = parse_time("2019-01-02 03:04:05").unwrap();
        let change = Change { flags: [Tri::Set, Tri::Set, Tri::Leave, Tri::Leave], modified, created };
        let secs = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        for path in [&file, &sub] {
            apply(path, &change).unwrap();
            let meta = std::fs::metadata(path).unwrap();
            assert_eq!(secs(meta.modified().unwrap()), secs(modified.unwrap()), "{path:?}");
            assert_eq!(secs(meta.created().unwrap()), secs(created.unwrap()), "{path:?}");
            assert_eq!(get_attributes(path).unwrap() & (READ_ONLY | HIDDEN), READ_ONLY | HIDDEN, "{path:?}");
        }
        // A read-only file still gets new dates, and keeps its flag.
        let later = modified.unwrap() + Duration::from_secs(3600);
        apply(&file, &Change { modified: Some(later), ..Default::default() }).unwrap();
        assert_eq!(secs(std::fs::metadata(&file).unwrap().modified().unwrap()), secs(later));
        assert_ne!(get_attributes(&file).unwrap() & READ_ONLY, 0);
        // Clear the flags again.
        let clear = Change { flags: [Tri::Clear, Tri::Clear, Tri::Leave, Tri::Leave], ..Default::default() };
        apply(&file, &clear).unwrap();
        apply(&sub, &clear).unwrap();
        assert_eq!(get_attributes(&file).unwrap() & (READ_ONLY | HIDDEN), 0);
        assert_eq!(get_attributes(&sub).unwrap() & (READ_ONLY | HIDDEN), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
