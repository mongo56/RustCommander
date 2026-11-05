//! Command palette (Ctrl+Space, `cm_CommandPalette`): a small popup over the
//! panels to find any command by its description, id or keys and run it on
//! the active panel as if its key was pressed. With no query the commands
//! last run from here come first.

use super::*;
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, text, Column, Space};
use iced::{Alignment, Element, Length};

/// How many commands the palette remembers as recently run.
const RECENT_MAX: usize = 8;
/// Rows moved by PgUp / PgDn.
const PAGE: usize = 8;

/// Palette state: the commands offered with their keys (taken when it
/// opens), the query and the matches in rank order.
#[derive(Debug)]
pub(super) struct Palette {
    query: String,
    items: Vec<(Command, String)>,
    /// The user commands, listed after `items` (`shown` indexes continue
    /// from `items.len()`): label, run line, keys.
    users: Vec<(String, String, String)>,
    /// Indexes into `items` and then `users`, best match first.
    pub shown: Vec<usize>,
    /// Index into `shown`.
    pub selected: usize,
}

/// Every command but the palette itself, with its effective keys.
pub(super) fn items(keymap: &keymap::Keymap) -> Vec<(Command, String)> {
    let bindings = keymap.bindings();
    Command::ALL
        .iter()
        .copied()
        .filter(|&c| c != Command::CommandPalette)
        .map(|c| (c, keymap::Keymap::keys_for(&bindings, c).join(", ")))
        .collect()
}

/// The user commands as palette rows: "User: name", the run line, the keys.
pub(super) fn user_items(
    cmds: &[crate::config::UserCommand],
    keys: &std::collections::HashMap<String, usize>,
) -> Vec<(String, String, String)> {
    cmds.iter()
        .enumerate()
        .map(|(i, c)| {
            let mut names: Vec<String> =
                keys.iter().filter(|(_, &n)| n == i).map(|(k, _)| keymap::display_key(k)).collect();
            names.sort();
            (format!("User: {}", c.name), c.run.clone(), names.join(", "))
        })
        .collect()
}

/// The indexes of the user commands matching `query` (every word in the
/// label, run line or keys), in their order.
pub(super) fn rank_users(query: &str, users: &[(String, String, String)]) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    (0..users.len())
        .filter(|&i| {
            let (label, run, keys) = &users[i];
            let hay = format!("{label} {run} {keys}").to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .collect()
}

/// Built-in matches, then the user commands' (indexes after `items.len()`).
fn show(query: &str, items: &[(Command, String)], users: &[(String, String, String)], recent: &[Command]) -> Vec<usize> {
    let mut shown = rank(query, items, recent);
    shown.extend(rank_users(query, users).into_iter().map(|i| items.len() + i));
    shown
}

/// Whether `hay[i..]` starts a word: the start, after a non-letter, or a
/// capital after a lower-case letter (`MkDir`).
fn word_start(hay: &str, i: usize) -> bool {
    let mut before = hay[..i].chars().rev();
    let Some(prev) = before.next() else { return true };
    let here = hay[i..].chars().next().unwrap_or(' ');
    !prev.is_alphanumeric() || prev.is_lowercase() && here.is_uppercase()
}

/// How well one lower-case query word matches a command (lower is better),
/// `None` if it doesn't: 0 = a whole key (`f7`) or the start of the
/// description or id, 1 = the start of a word in them, 2 = anywhere.
pub(super) fn word_score(word: &str, cmd: Command, keys: &str) -> Option<u32> {
    let id = cmd.id().strip_prefix("cm_").unwrap_or(cmd.id());
    let texts = [cmd.description(), id, keys];
    if keys.split(", ").any(|k| k.eq_ignore_ascii_case(word)) {
        return Some(0);
    }
    let mut best = None;
    for hay in texts {
        let lower = hay.to_lowercase();
        // Lower-casing keeps byte offsets for the ASCII text used here; skip
        // a match whose offset isn't a boundary in the original.
        for (i, _) in lower.match_indices(word) {
            let score = match i {
                0 => 0,
                _ if hay.is_char_boundary(i) && word_start(hay, i) => 1,
                _ => 2,
            };
            best = Some(best.map_or(score, |b: u32| b.min(score)));
        }
    }
    best
}

/// The indexes of `items` matching `query` (every word somewhere in the
/// description, id or keys), best first: by summed word scores, then
/// recently run, then the usual order. An empty query lists all, recently
/// run first.
pub(super) fn rank(query: &str, items: &[(Command, String)], recent: &[Command]) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut found: Vec<(u32, usize, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, (cmd, keys))| {
            let score = words.iter().try_fold(0, |sum, w| word_score(w, *cmd, keys).map(|s| sum + s))?;
            let recency = recent.iter().position(|c| c == cmd).unwrap_or(usize::MAX);
            Some((score, recency, i))
        })
        .collect();
    found.sort_unstable();
    found.into_iter().map(|(_, _, i)| i).collect()
}

impl App {
    /// Keys of `palette_key`, for the F1 help. Keep in sync.
    pub(super) const PALETTE_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Typing, Backspace", "Find a command by description, id or key (F7, Ctrl+Q); recently run first"),
        ("Up, Down, PgUp, PgDn, Home, End", "Pick a command"),
        ("Enter, click", "Run it on the active panel, as its key would"),
        ("Esc, Ctrl+Space", "Close"),
    ];

    pub(super) fn open_palette(&mut self) -> Task<Message> {
        self.leave_edits();
        let items = items(&self.keymap);
        let users = user_items(&self.user_cmds, &self.user_keys);
        let shown = show("", &items, &users, &self.recent_commands);
        self.dialog = Some(Dialog::Palette(Palette { query: String::new(), items, users, shown, selected: 0 }));
        self.popup_snap()
    }

    pub(super) fn palette_key(
        &mut self,
        named: Option<Named>,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
        text: Option<&str>,
    ) -> Task<Message> {
        let again = self.keymap.command(key, mods) == Some(Command::CommandPalette);
        let Some(Dialog::Palette(p)) = &mut self.dialog else { return Task::none() };
        let count = p.shown.len();
        let last = count.saturating_sub(1);
        let requery = |p: &mut Palette, query: String| {
            p.query = query;
            p.shown = show(&p.query, &p.items, &p.users, &self.recent_commands);
            p.selected = 0;
        };
        match named {
            _ if again => self.dialog = None,
            Some(Named::Escape) => self.dialog = None,
            Some(Named::Enter) => {
                let selected = p.selected;
                return self.palette_pick(selected);
            }
            Some(Named::ArrowUp) if count > 0 => p.selected = (p.selected + count - 1) % count,
            Some(Named::ArrowDown) if count > 0 => p.selected = (p.selected + 1) % count,
            Some(Named::PageUp) => p.selected = p.selected.saturating_sub(PAGE),
            Some(Named::PageDown) => p.selected = (p.selected + PAGE).min(last),
            Some(Named::Home) => p.selected = 0,
            Some(Named::End) => p.selected = last,
            Some(Named::Backspace) => {
                let mut query = p.query.clone();
                query.pop();
                requery(p, query);
            }
            _ => {
                let plain = !mods.control() && !mods.alt() && !mods.logo();
                if let Some(t) = text.filter(|t| plain && !t.is_empty() && !t.chars().any(char::is_control)) {
                    let query = format!("{}{t}", p.query);
                    requery(p, query);
                }
            }
        }
        Task::none()
    }

    /// Runs the `i`th shown command on the active panel (closing the
    /// palette) and remembers it as recently run.
    pub(super) fn palette_pick(&mut self, i: usize) -> Task<Message> {
        let Some(Dialog::Palette(p)) = &self.dialog else { return Task::none() };
        let Some(&j) = p.shown.get(i) else { return Task::none() };
        if j >= p.items.len() {
            let user = j - p.items.len();
            self.dialog = None;
            return self.run_user(user);
        }
        let (cmd, _) = p.items[j];
        self.dialog = None;
        self.recent_commands.retain(|&c| c != cmd);
        self.recent_commands.insert(0, cmd);
        self.recent_commands.truncate(RECENT_MAX);
        self.save_session();
        self.run(cmd)
    }

    pub(super) fn palette_view<'a>(&'a self, p: &'a Palette) -> Element<'a, Message> {
        let colors = theme::c();
        let query: Element<'a, Message> = if p.query.is_empty() {
            text("Type to find a command").size(13).color(colors.muted).into()
        } else {
            text(format!("{}|", p.query)).size(13).into()
        };
        let search = container(query).padding([6, 8]).width(Length::Fill).style(move |_: &Theme| container::Style {
            background: Some(colors.panel.into()),
            border: iced::Border { color: colors.accent, width: 1.0, radius: 3.0.into() },
            ..container::Style::default()
        });

        let recent = p.query.is_empty() && !self.recent_commands.is_empty();
        let mut list = Column::new();
        for (n, &i) in p.shown.iter().enumerate() {
            let (desc, keys, id) = match p.items.get(i) {
                Some((cmd, keys)) => {
                    let mut desc = cmd.description().to_owned();
                    if recent && self.recent_commands.contains(cmd) {
                        desc.push_str("  \u{b7} recent");
                    }
                    (desc, keys.as_str(), cmd.id())
                }
                None => {
                    let (label, run, keys) = &p.users[i - p.items.len()];
                    (format!("{label}  \u{b7}  {run}"), keys.as_str(), "user command")
                }
            };
            let content = row![
                text(desc).size(13).width(Length::Fill).wrapping(Wrapping::None),
                text(keys).size(12).color(colors.dir).wrapping(Wrapping::None),
                text(id).size(11).color(colors.muted).wrapping(Wrapping::None),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            list = list.push(
                button(container(content).clip(true))
                    .on_press(Message::PalettePick(n))
                    .width(Length::Fill)
                    .padding([4, 8])
                    .style(theme::suggestion(n == p.selected)),
            );
        }
        let body: Element<'a, Message> = if p.shown.is_empty() {
            text(format!("No command matches \"{}\"", p.query)).size(13).color(colors.muted).into()
        } else {
            container(scrollbar::thin(list).id(hotlist::popup_list_id()))
                .max_height((self.window_height - 220.0).clamp(120.0, 420.0))
                .into()
        };
        let header = row![
            text("Command palette").size(16),
            Space::with_width(Length::Fill),
            text(format!("{} shown", p.shown.len())).size(12).color(colors.muted),
        ]
        .align_y(Alignment::Center);
        let hint = text("Enter run \u{b7} Up/Down pick \u{b7} Esc close").size(11).color(colors.muted);
        let content = column![header, search, body, hint].spacing(10);
        container(content).padding(16).width(Length::Fixed(600.0)).style(theme::dialog).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(query: &str, recent: &[Command]) -> Vec<Command> {
        let items = items(&keymap::Keymap::default());
        rank(query, &items, recent).into_iter().map(|i| items[i].0).collect()
    }

    #[test]
    fn offers_every_command_but_itself() {
        let all = ranked("", &[]);
        assert_eq!(all.len(), Command::ALL.len() - 1);
        assert!(!all.contains(&Command::CommandPalette));
        assert_eq!(all[0], Command::ALL[0]);
    }

    #[test]
    fn recent_commands_come_first() {
        let all = ranked("", &[Command::MakeDir, Command::Copy]);
        assert_eq!(all[..2], [Command::MakeDir, Command::Copy]);
        assert_eq!(all.len(), Command::ALL.len() - 1);
        // Among equal matches too.
        let size = Command::SortBy(rc_core::SortKey::Size);
        let sorts = ranked("sort", &[size]);
        assert_eq!(sorts[0], size);
    }

    #[test]
    fn prefix_and_word_starts_rank_higher() {
        // The id starts with it.
        assert_eq!(ranked("mkdir", &[])[0], Command::MakeDir);
        // A whole key beats a key containing it (Shift+F5).
        assert_eq!(ranked("f5", &[])[0], Command::Copy);
        assert_eq!(ranked("ctrl+q", &[])[0], Command::Quit);
        assert_eq!(ranked("tab", &[])[0], Command::SwitchPanel);
        // Start of the description before a word inside another one.
        let copy = ranked("copy", &[]);
        assert_eq!(copy[0], Command::Copy);
        // Every word must match.
        assert_eq!(ranked("new tab", &[])[0], Command::NewTab);
        assert!(ranked("zzzz", &[]).is_empty());
        assert!(ranked("COPY", &[]).contains(&Command::Copy));
    }

    #[test]
    fn user_commands_follow_the_built_in_ones() {
        let users = vec![
            ("User: Code".to_owned(), "code %P".to_owned(), "Ctrl+Alt+1".to_owned()),
            ("User: Hash".to_owned(), "sha %F".to_owned(), String::new()),
        ];
        let items = items(&keymap::Keymap::default());
        let n = items.len();
        let all = show("", &items, &users, &[]);
        assert_eq!(all.len(), n + 2);
        assert_eq!(all[n..], [n, n + 1]);
        assert_eq!(show("user hash", &items, &users, &[]), [n + 1]);
        assert_eq!(show("ctrl+alt+1", &items, &users, &[]), [n]);
        assert_eq!(rank_users("%f", &users), [1]);
    }

    #[test]
    fn word_scores() {
        assert_eq!(word_score("mkdir", Command::MakeDir, "F7"), Some(0));
        assert_eq!(word_score("f7", Command::MakeDir, "F7"), Some(0));
        assert_eq!(word_score("dir", Command::MakeDir, "F7"), Some(1)); // MkDir
        assert_eq!(word_score("kdi", Command::MakeDir, "F7"), Some(2));
        assert_eq!(word_score("zz", Command::MakeDir, "F7"), None);
    }
}
