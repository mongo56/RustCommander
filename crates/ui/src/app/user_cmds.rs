//! User commands (`[[user_commands]]` in `config.toml`, `config::UserCommand`):
//! run from their key (`App::user_keys`) or the command palette. The line is
//! expanded by `rc_core::user_cmd` and started detached (like the editor) or
//! typed into a new built-in terminal (`terminal = true`).

use super::*;
use rc_core::user_cmd;

impl App {
    /// The index of the user command a key press is bound to.
    pub(super) fn user_key(&self, key: &keyboard::Key, mods: keyboard::Modifiers) -> Option<usize> {
        if self.user_keys.is_empty() {
            return None;
        }
        self.user_keys.get(&keymap::key_name(key, mods)?).copied()
    }

    /// Runs user command `i` on the active panel.
    pub(super) fn run_user(&mut self, i: usize) -> Task<Message> {
        let Some(cmd) = self.user_cmds.get(i).cloned() else { return Task::none() };
        self.status = None;
        let (active, other) = (self.active, 1 - self.active);
        let mut used = user_cmd::used(&cmd.run);
        used.extend(user_cmd::used(&cmd.cwd));
        let virtual_path = |p: &Path| vfs::is_virtual(p);
        let active_virtual = virtual_path(&self.panel(active).path);
        let what = if active_virtual && used.iter().any(|c| matches!(c, 'P' | 'N' | 'F' | 'S')) {
            Some("the active panel")
        } else if virtual_path(&self.panel(other).path) && used.contains(&'T') {
            Some("the other panel")
        } else {
            None
        };
        if let Some(what) = what {
            self.info(format!(
                "\"{}\" needs a folder on disk, but {what} shows an archive or a server: copy the files to a local folder first",
                cmd.name
            ));
            return Task::none();
        }

        let panel = self.panel(active);
        let cursor = panel.current().filter(|e| !e.is_parent());
        let vars = user_cmd::Vars {
            active: panel.path.to_string_lossy().into_owned(),
            other: self.panel(other).path.to_string_lossy().into_owned(),
            name: cursor.map(|e| e.name.clone()).unwrap_or_default(),
            file: cursor.map(|e| e.path.to_string_lossy().into_owned()).unwrap_or_default(),
            selected: if panel.selected_count() > 0 {
                panel.targets().iter().map(|e| e.path.to_string_lossy().into_owned()).collect()
            } else {
                Vec::new()
            },
        };
        let dir = if cmd.cwd.trim().is_empty() {
            panel.path.ancestors().find(|p| !vfs::is_virtual(p) && p.is_dir()).map(Path::to_owned)
        } else {
            user_cmd::split_args(&user_cmd::expand(&cmd.cwd, &vars)).into_iter().next().map(PathBuf::from)
        };
        let Some(dir) = dir.filter(|d| d.is_dir()) else {
            self.error(format!("\"{}\": the working folder doesn't exist", cmd.name));
            return Task::none();
        };

        let line = user_cmd::expand(&cmd.run, &vars);
        if cmd.terminal {
            self.success(format!("Running {} in a terminal", cmd.name));
            return self.run_line_in_term(&dir, line);
        }
        let mut words = user_cmd::split_args(&line).into_iter();
        let Some(program) = words.next() else {
            self.error(format!("\"{}\": the run line is empty", cmd.name));
            return Task::none();
        };
        let args: Vec<std::ffi::OsString> = words.map(Into::into).collect();
        match rc_platform::spawn_detached(&program, &args, &dir) {
            Ok(_) => self.success(format!("Started {}", cmd.name)),
            Err(e) => self.error(format!("Can't start {program} for \"{}\": {e}", cmd.name)),
        }
        Task::none()
    }
}
