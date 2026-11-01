//! F1 help (TC's `cm_HelpIndex`): fills the window with every command, its
//! effective keys (`Keymap::bindings`, so `[keys]` overrides show) and
//! description (`Command::description`), then the hard-coded keys of the
//! panels and the full-window tools. Typing filters; Esc clears the
//! filter, then closes.

use super::*;
use crate::theme::filled;
use iced::widget::scrollable::{AbsoluteOffset, RelativeOffset};
use iced::widget::text::Wrapping;
use iced::widget::{column, container, row, scrollable, text, Column, Space};
use iced::{Alignment, Element, Length};

const LINE: f32 = 40.0;
const KEYS_W: f32 = 260.0;
/// Header and footer around the list (for paging).
const HELP_CHROME_H: f32 = 110.0;

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
pub(super) fn sections(keymap: &keymap::Keymap) -> Vec<Section> {
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
    vec![
        Section {
            title: "Commands (rebind in the settings, F10, or config.toml [keys], Ctrl+,)",
            items: commands,
        },
        fixed("Panels: typing and mouse", App::PANEL_KEYS),
        fixed("Directory hotlist (Ctrl+D)", App::HOTLIST_KEYS),
        fixed("Folder history list (Alt+Down)", App::HISTORY_KEYS),
        fixed("Viewer (F3)", App::VIEWER_KEYS),
        fixed("Multi-rename (Ctrl+M)", App::RENAME_KEYS),
        fixed("Compare files (Alt+F2)", App::DIFF_KEYS),
        fixed("Find files (Alt+F7)", App::FIND_KEYS),
        fixed("Connect to server (Ctrl+F)", App::CONNECT_KEYS),
        fixed("Settings (F10)", App::SETTINGS_KEYS),
    ]
}

impl App {
    pub(super) fn open_help(&mut self) -> Task<Message> {
        self.leave_edits();
        self.help = Some(Help {
            query: String::new(),
            sections: sections(&self.keymap),
        });
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
        let Some(h) = &mut self.help else {
            return Task::none();
        };
        let scroll = |y: f32| scrollable::scroll_by(list_id(), AbsoluteOffset { x: 0.0, y });
        let top = || scrollable::snap_to(list_id(), RelativeOffset::START);
        match named {
            Some(Named::Escape) if !h.query.is_empty() => {
                h.query.clear();
                return top();
            }
            Some(Named::Escape | Named::F1) => self.help = None,
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
            text("Type to filter").size(13).color(colors.muted).into()
        } else {
            text(format!("{}|", h.query)).size(13).into()
        };
        let search = container(query)
            .padding([5, 8])
            .width(Length::Fixed(320.0))
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
                text("Help: keys and features").size(16),
                search,
                Space::with_width(Length::Fill),
                text(format!("{count} shown")).size(12).color(colors.muted),
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        )
        .padding([10, 14])
        .style(filled(colors.header));

        let mut list = Column::new().spacing(2).padding([8, 14]);
        if shown.is_empty() {
            list = list.push(
                text(format!("Nothing matches \"{}\"", h.query))
                    .size(13)
                    .color(colors.muted),
            );
        }
        for (title, items) in shown {
            list = list.push(container(text(title).size(14).color(colors.accent)).padding([10, 0]));
            for item in items {
                let keys = if item.keys.is_empty() {
                    "(no key)"
                } else {
                    item.keys.as_str()
                };
                list = list.push(
                    row![
                        text(keys)
                            .size(13)
                            .color(if item.keys.is_empty() {
                                colors.muted
                            } else {
                                colors.dir
                            })
                            .width(Length::Fixed(KEYS_W)),
                        text(item.text.as_str()).size(13).width(Length::Fill),
                        text(item.id)
                            .size(12)
                            .color(colors.muted)
                            .wrapping(Wrapping::None),
                    ]
                    .spacing(12)
                    .padding([2, 0])
                    .align_y(Alignment::Center),
                );
            }
        }
        let body = container(scrollbar::thin(list).id(list_id()).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(colors.panel));

        let footer = container(
            text("Type to filter · Backspace edit · Esc clear filter / close · Up/Down/PgUp/PgDn/Home/End scroll")
                .size(12)
                .color(colors.muted)
                .wrapping(Wrapping::None),
        )
        .padding([4, 12])
        .width(Length::Fill)
        .clip(true)
        .style(filled(colors.header));

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
        let all = sections(&keymap::Keymap::default());
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
    fn filter_needs_every_word() {
        let all = sections(&keymap::Keymap::default());
        let found = |q: &str| -> Vec<String> {
            filter(&all, q)
                .into_iter()
                .flat_map(|(_, items)| items.into_iter().map(|i| i.text.clone()))
                .collect()
        };
        assert_eq!(filter(&all, "").len(), all.len());
        assert_eq!(found("cm_copy"), ["Copy to the other panel"]);
        assert_eq!(found("COPY f5"), ["Copy to the other panel"]);
        // Section titles match too: "viewer" lists all viewer keys.
        assert!(found("viewer hex").contains(&"Hex mode".to_owned()));
        assert!(found("zzzz").is_empty());
    }
}
