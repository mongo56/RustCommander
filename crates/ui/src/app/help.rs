//! F1 help (TC's `cm_HelpIndex`): fills the window with every command, its
//! effective keys (`Keymap::bindings`, so `[keys]` overrides show) and
//! description (`Command::description`), then the hard-coded keys of the
//! panels and the full-window tools. Typing filters; Esc clears the
//! filter, then closes.

use super::*;
use crate::theme::filled;
use iced::widget::scrollable::{AbsoluteOffset, RelativeOffset};
use iced::widget::text::Wrapping;
use iced::widget::{column, container, responsive, row, scrollable, text, Column, Row, Space};
use iced::{Alignment, Element, Length};

const LINE: f32 = 40.0;
const KEYS_W: f32 = 260.0;
const KEYS_W_NARROW: f32 = 180.0;
/// List widths (px) below which the command id column is dropped, the keys
/// column narrows, and the keys are stacked above the description.
const HIDE_ID_BELOW: f32 = 900.0;
const NARROW_KEYS_BELOW: f32 = 760.0;
const STACK_BELOW: f32 = 560.0;
/// Header and footer around the list (for paging).
const HELP_CHROME_H: f32 = 110.0;

/// One key combo as a small boxed key cap.
fn key_chip<'a>(label: &'a str, colors: theme::Colors) -> Element<'a, Message> {
    theme::key_chip(label, colors, colors.header)
}

fn list_id() -> scrollable::Id {
    scrollable::Id::new("help-list")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub keys: String,
    pub text: String,
    /// Command id, empty for built-in keys.
    pub id: &'static str,
}

#[derive(Debug)]
pub(super) struct Section {
    pub title: &'static str,
    pub items: Vec<Item>,
}

#[derive(Debug)]
pub(super) struct Help {
    query: String,
    sections: Vec<Section>,
}

/// The sections with only the items matching `query`: every word must
/// appear (case-insensitive) in the keys, description, id or section title.
pub(super) fn filter<'a>(
    sections: &'a [Section],
    query: &str,
) -> Vec<(&'static str, Vec<&'a Item>)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    sections
        .iter()
        .filter_map(|s| {
            let items: Vec<&Item> = s
                .items
                .iter()
                .filter(|i| {
                    let hay = format!("{} {} {} {}", i.keys, i.text, i.id, s.title).to_lowercase();
                    words.iter().all(|w| hay.contains(w.as_str()))
                })
                .collect();
            (!items.is_empty()).then_some((s.title, items))
        })
        .collect()
}

/// All sections: the commands with their effective keys, then the
/// hard-coded keys.
pub(super) fn sections(keymap: &keymap::Keymap, users: &[(String, String, String)]) -> Vec<Section> {
    let bindings = keymap.bindings();
    let commands = Command::ALL
        .iter()
        .map(|&cmd| Item {
            keys: keymap::Keymap::keys_for(&bindings, cmd).join(", "),
            text: cmd.description().to_owned(),
            id: cmd.id(),
        })
        .collect();
    let fixed = |title, keys: &[(&str, &str)]| Section {
        title,
        items: keys
            .iter()
            .map(|(k, t)| Item {
                keys: (*k).to_owned(),
                text: (*t).to_owned(),
                id: "",
            })
            .collect(),
    };
    let mut out = vec![Section {
        title: "Commands (rebind in the settings, F10, or config.toml [keys], Ctrl+,)",
        items: commands,
    }];
    if !users.is_empty() {
        out.push(Section {
            title: "User commands (config.toml [[user_commands]], Ctrl+,)",
            items: users
                .iter()
                .map(|(label, run, keys)| Item { keys: keys.clone(), text: format!("{label} - {run}"), id: "" })
                .collect(),
        });
    }
    out.extend([
        fixed("Panels: typing and mouse", App::PANEL_KEYS),
        fixed("Directory hotlist (Ctrl+D)", App::HOTLIST_KEYS),
        fixed("Hotlist tree (left of the panels; turn it on in the settings, F10)", App::TREE_KEYS),
        fixed("Folder history list (Alt+Down)", App::HISTORY_KEYS),
        fixed("Command palette (Ctrl+Space)", App::PALETTE_KEYS),
        fixed("Viewer (F3)", App::VIEWER_KEYS),
        fixed("Multi-rename (Ctrl+M)", App::RENAME_KEYS),
        fixed("Compare files (Alt+F2)", App::DIFF_KEYS),
        fixed("Find files (Ctrl+F)", App::FIND_KEYS),
        fixed("Synchronize folders (Alt+Shift+F2)", App::SYNC_KEYS),
        fixed("Disk usage (Alt+Shift+F7)", App::DU_KEYS),
        fixed("Connect to server (Ctrl+F7)", App::CONNECT_KEYS),
        fixed("Built-in terminal (F9)", App::TERM_KEYS),
        fixed("Settings (F10)", App::SETTINGS_KEYS),
    ]);
    out
}

impl App {
    pub(super) fn open_help(&mut self) -> Task<Message> {
        self.leave_edits();
        self.screen = Some(Screen::Help(Help {
            query: String::new(),
            sections: sections(&self.keymap, &palette::user_items(&self.user_cmds, &self.user_keys)),
        }));
        scrollable::snap_to(list_id(), RelativeOffset::START)
    }

    pub(super) fn help_key(
        &mut self,
        named: Option<Named>,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
        text: Option<&str>,
    ) -> Task<Message> {
        let page = (self.window_height - HELP_CHROME_H).max(LINE);
        // The help key (F1 or whatever it is bound to) closes help again.
        let again = (named.is_some() || mods.control() || mods.alt())
            && self.keymap.command(key, mods) == Some(Command::Help);
        let Some(h) = self.screen.help_mut() else {
            return Task::none();
        };
        let scroll = |y: f32| scrollable::scroll_by(list_id(), AbsoluteOffset { x: 0.0, y });
        let top = || scrollable::snap_to(list_id(), RelativeOffset::START);
        match named {
            Some(Named::Escape) if !h.query.is_empty() => {
                h.query.clear();
                return top();
            }
            Some(Named::Escape | Named::F1) => self.screen = None,
            _ if again => self.screen = None,
            Some(Named::Backspace) => {
                h.query.pop();
                return top();
            }
            Some(Named::ArrowDown) => return scroll(LINE),
            Some(Named::ArrowUp) => return scroll(-LINE),
            Some(Named::PageDown) => return scroll(page),
            Some(Named::PageUp) => return scroll(-page),
            Some(Named::Home) => return top(),
            Some(Named::End) => return scrollable::snap_to(list_id(), RelativeOffset::END),
            _ => {
                let plain = !mods.control() && !mods.alt() && !mods.logo();
                let typed =
                    text.filter(|t| plain && !t.is_empty() && !t.chars().any(char::is_control));
                if let (Some(t), keyboard::Key::Character(_) | keyboard::Key::Named(Named::Space)) =
                    (typed, key)
                {
                    h.query.push_str(t);
                    return top();
                }
            }
        }
        Task::none()
    }

    pub(super) fn help_view<'a>(&'a self, h: &'a Help) -> Element<'a, Message> {
        let colors = theme::c();
        let shown = filter(&h.sections, &h.query);
        let count: usize = shown.iter().map(|(_, items)| items.len()).sum();

        let query: Element<'a, Message> = if h.query.is_empty() {
            text("Type to filter").size(theme::SIZE_BODY).color(colors.muted).wrapping(Wrapping::None).into()
        } else {
            text(format!("{}|", h.query)).size(theme::SIZE_BODY).wrapping(Wrapping::None).into()
        };
        let search = container(query)
            .padding([5, 8])
            .width(Length::Fill)
            .max_width(320.0)
            .clip(true)
            .style(move |_: &Theme| container::Style {
                background: Some(colors.panel.into()),
                border: iced::Border {
                    color: colors.accent,
                    width: 1.0,
                    radius: 3.0.into(),
                },
                ..container::Style::default()
            });
        let header = container(
            row![
                text("Help: keys and features").size(theme::SIZE_TITLE).wrapping(Wrapping::None),
                search,
                Space::with_width(Length::Fill),
                text(format!("{count} shown")).size(theme::SIZE_SMALL).color(colors.muted).wrapping(Wrapping::None),
            ]
            .spacing(theme::SPACING_LOOSE)
            .align_y(Alignment::Center),
        )
        .padding(theme::PAD_LOOSE)
        .style(filled(colors.header));

        let query_text = h.query.as_str();
        let list = responsive(move |size| {
            let wide = size.width >= HIDE_ID_BELOW;
            let stacked = size.width < STACK_BELOW;
            let keys_w = if size.width >= NARROW_KEYS_BELOW { KEYS_W } else { KEYS_W_NARROW };
            let mut list = Column::new().spacing(theme::SPACING_TIGHT).padding(theme::PAD_NORMAL);
            if shown.is_empty() {
                list = list.push(
                    text(format!("Nothing matches \"{query_text}\""))
                        .size(theme::SIZE_BODY)
                        .color(colors.muted),
                );
            }
            for &(title, ref items) in &shown {
                list = list.push(container(text(title).size(theme::SIZE_HEAD).color(colors.accent)).padding([10, 0]));
                for &item in items.iter() {
                    let keys_el = |width: Length| -> Element<'_, Message> {
                        if item.keys.is_empty() {
                            text("(no key)").size(theme::SIZE_BODY).color(colors.muted).width(width).into()
                        } else {
                            let chips = item.keys.split(", ").map(|k| key_chip(k, colors));
                            container(
                                Row::with_children(chips)
                                    .spacing(theme::SPACING_TIGHT * 2)
                                    .align_y(Alignment::Center)
                                    .wrap(),
                            )
                            .width(width)
                            .into()
                        }
                    };
                    if stacked {
                        list = list.push(
                            column![
                                keys_el(Length::Shrink),
                                container(text(item.text.as_str()).size(theme::SIZE_BODY).width(Length::Fill)).padding([0, 12]),
                            ]
                            .padding([2, 0]),
                        );
                        continue;
                    }
                    let mut line = row![
                        keys_el(Length::Fixed(keys_w)),
                        text(item.text.as_str()).size(theme::SIZE_BODY).width(Length::Fill),
                    ]
                    .spacing(12)
                    .padding([2, 0])
                    .align_y(Alignment::Center);
                    if wide {
                        line = line.push(text(item.id).size(theme::SIZE_SMALL).color(colors.muted).wrapping(Wrapping::None));
                    }
                    list = list.push(line);
                }
            }
            scrollbar::thin(list).id(list_id()).height(Length::Fill).into()
        });
        let body = container(list)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.panel));

        let version = text(concat!("RustCommander ", env!("CARGO_PKG_VERSION")))
            .size(theme::SIZE_SMALL)
            .color(colors.accent)
            .wrapping(Wrapping::None);
        const HINTS: &[theme::Hint] = &[
            (&[], "Type to filter"),
            (&["Backspace"], "edit"),
            (&["Esc"], "clear filter / close"),
            (&["↑↓", "PgUp", "PgDn", "Home", "End"], "scroll"),
        ];
        let footer = theme::key_hints_footer(
            HINTS,
            self.window_width,
            170.0,
            theme::PAD_TIGHT,
            theme::SPACING_LOOSE,
            Some(version.into()),
        );

        container(column![header, body, footer])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.bg))
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_is_listed_once() {
        let all = sections(&keymap::Keymap::default(), &[]);
        assert_eq!(all[0].items.len(), Command::ALL.len());
        let help = all[0]
            .items
            .iter()
            .find(|i| i.id == "cm_HelpIndex")
            .unwrap();
        assert_eq!(help.keys, "F1");
        assert!(all
            .iter()
            .all(|s| !s.items.is_empty() && s.items.iter().all(|i| !i.text.is_empty())));
    }

    #[test]
    fn lists_user_commands_after_the_built_in_ones() {
        let users = [("User: Code".to_owned(), "code %P".to_owned(), "Ctrl+Alt+1".to_owned())];
        let all = sections(&keymap::Keymap::default(), &users);
        assert_eq!(all[0].items.len(), Command::ALL.len());
        assert!(all[1].title.starts_with("User commands"));
        assert_eq!(all[1].items[0].keys, "Ctrl+Alt+1");
        assert!(all[1].items[0].text.contains("code %P"));
    }

    #[test]
    fn filter_needs_every_word() {
        let all = sections(&keymap::Keymap::default(), &[]);
        let found = |q: &str| -> Vec<String> {
            filter(&all, q)
                .into_iter()
                .flat_map(|(_, items)| items.into_iter().map(|i| i.text.clone()))
                .collect()
        };
        assert_eq!(filter(&all, "").len(), all.len());
        assert_eq!(found("cm_copy f5"), ["Copy to the other panel"]);
        assert_eq!(found("cm_copytoclipboard"), ["Copy the files to the clipboard"]);
        assert_eq!(found("COPY f5"), ["Copy to the other panel"]);
        // Section titles match too: "viewer" lists all viewer keys.
        assert!(found("viewer hex").contains(&"Hex mode".to_owned()));
        assert!(found("zzzz").is_empty());
    }
}
