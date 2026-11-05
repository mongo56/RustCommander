//! Split and join files (`cm_SplitFile`, `cm_CombineFiles`). Split asks for
//! the part size (presets or a number with a unit) and cuts the file under
//! the cursor into `name.001`, `name.002`, ... plus `name.crc` in the other
//! panel's folder; Combine, on a `name.001`, joins the parts into the other
//! panel's folder and checks `name.crc`. Both run as jobs (`Job::Split`,
//! `Job::Combine`, see `rc_core::ops`), so progress, cancel and the overwrite
//! question work as for a copy. The names and sizes are in `rc_core::split`.

use super::*;
use rc_core::split as sp;

/// The preset part sizes' labels, to step through with Up/Down.
pub(super) fn step_preset(input: &str, down: bool) -> String {
    let current = sp::parse_size(input).ok();
    let presets = &sp::PRESETS;
    let at = current.and_then(|c| presets.iter().position(|&(_, b)| b == c));
    let next = match (at, down) {
        (Some(i), true) => (i + 1) % presets.len(),
        (Some(i), false) => (i + presets.len() - 1) % presets.len(),
        (None, true) => 0,
        (None, false) => presets.len() - 1,
    };
    presets[next].0.split(" (").next().unwrap_or_default().to_string()
}

impl App {
    /// The folder a split or join writes into: the other panel's, if it is a
    /// plain local folder (else says why not).
    fn split_target(&mut self) -> Option<PathBuf> {
        let target = self.panel(1 - self.active).path.clone();
        if vfs::is_virtual(&target) {
            self.info("The other panel must show a local folder (not an archive or a server) to write into");
            return None;
        }
        Some(target)
    }

    /// `cm_SplitFile`: asks for the part size.
    pub(super) fn open_split(&mut self) -> Task<Message> {
        let panel = self.panel(self.active);
        if vfs::is_virtual(&panel.path) {
            self.info("Can only split local files, not ones in an archive or on a server");
            return Task::none();
        }
        let Some(entry) = panel.current().filter(|e| !e.is_dir() && !e.is_parent()) else {
            self.info("Put the cursor on the file to split");
            return Task::none();
        };
        let (src, size) = (entry.path.clone(), entry.size);
        let Some(target) = self.split_target() else { return Task::none() };
        self.open_dialog(Dialog::Split { src, size, target, input: "100 MB".into() })
    }

    /// The dialog's OK: starts the job. A size that can't be used keeps the dialog open.
    pub(super) fn split_submit(&mut self, src: PathBuf, size: u64, target: PathBuf, input: String) -> Task<Message> {
        let again = |src, target, input| Dialog::Split { src, size, target, input };
        let part_size = match sp::parse_size(&input) {
            Ok(p) => p,
            Err(e) => {
                self.error(e);
                self.dialog = Some(again(src, target, input));
                return Task::none();
            }
        };
        if size <= part_size {
            self.info(format!(
                "The file is not larger than the part size ({}): nothing to split",
                rc_core::format::size(part_size)
            ));
            self.dialog = Some(again(src, target, input));
            return Task::none();
        }
        let parts = sp::part_count(size, part_size);
        if parts > 100_000 {
            self.error(format!("That would make {parts} parts: use a larger part size"));
            self.dialog = Some(again(src, target, input));
            return Task::none();
        }
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let title = format!("Splitting {name} into {parts} parts of {}", rc_core::format::size(part_size));
        let focus = Some((1 - self.active, sp::part_name(&name, 1, 0)));
        let task = self.start_job(Job::Split { src, target, part_size }, title);
        if let Some(job) = self.jobs.last_mut() {
            job.focus = focus;
        }
        task
    }

    /// `cm_CombineFiles`: joins the parts starting at the file under the cursor.
    pub(super) fn combine_files(&mut self) -> Task<Message> {
        let panel = self.panel(self.active);
        if vfs::is_virtual(&panel.path) {
            self.info("Can only join local files, not ones in an archive or on a server");
            return Task::none();
        }
        let Some(entry) = panel.current().filter(|e| !e.is_dir() && !e.is_parent()) else {
            self.info("Put the cursor on the first part (name.001) to join");
            return Task::none();
        };
        let Some((base, _, _)) = sp::parse_part(&entry.name) else {
            self.info("Put the cursor on a part file (name.001) to join");
            return Task::none();
        };
        let (first, base) = (entry.path.clone(), base.to_string());
        let Some(target) = self.split_target() else { return Task::none() };
        let title = format!("Joining {base}");
        let focus = Some((1 - self.active, base));
        let task = self.start_job(Job::Combine { first, target }, title);
        if let Some(job) = self.jobs.last_mut() {
            job.focus = focus;
        }
        task
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_step_through_and_wrap() {
        assert_eq!(step_preset("100 MB", true), "700 MB");
        assert_eq!(step_preset("100", false), "4095 MB");
        assert_eq!(step_preset("4095 MB", true), "100 MB");
        assert_eq!(step_preset("123 KB", true), "100 MB");
        assert_eq!(step_preset("junk", false), "4095 MB");
        // What a step gives parses to the preset.
        assert_eq!(sp::parse_size(&step_preset("700 MB", true)), Ok(1024 * 1024 * 1024));
    }
}
