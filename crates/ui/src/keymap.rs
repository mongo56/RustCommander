//! Default key bindings, modelled on Total Commander. Will become
//! user-configurable (keyed by `Command::id`).

use iced::keyboard::{key::Named, Key, Modifiers};
use rc_core::{Command, SortKey};

pub fn command_for(key: &Key, m: Modifiers) -> Option<Command> {
    use Command::*;
    let ctrl = m.control();
    let alt = m.alt();
    let shift = m.shift();

    let cmd = match key.as_ref() {
        Key::Named(named) => match named {
            Named::ArrowUp if !ctrl => CursorUp,
            Named::ArrowDown => CursorDown,
            Named::PageUp if ctrl => GoParent,
            Named::PageUp => PageUp,
            Named::PageDown if ctrl => Open,
            Named::PageDown => PageDown,
            Named::Home => CursorHome,
            Named::End => CursorEnd,
            Named::Enter => Open,
            Named::Backspace => GoParent,
            Named::Tab if ctrl && shift => PrevTab,
            Named::Tab if ctrl => NextTab,
            Named::Tab => SwitchPanel,
            Named::ArrowUp if ctrl => OpenInNewTab,
            Named::Insert => ToggleSelect { advance: true },
            Named::Space => ToggleSelect { advance: false },
            Named::Escape => DeselectAll,
            Named::F3 if ctrl => SortBy(SortKey::Name),
            Named::F4 if ctrl => SortBy(SortKey::Ext),
            Named::F5 if ctrl => SortBy(SortKey::Modified),
            Named::F6 if ctrl => SortBy(SortKey::Size),
            Named::F4 if alt => Quit,
            Named::F2 if ctrl => Refresh,
            Named::F3 => View,
            Named::F4 => Edit,
            Named::F5 => Copy,
            Named::F6 if shift => Rename,
            Named::F2 if !ctrl && !alt => Rename,
            Named::F6 => Move,
            Named::F7 => MakeDir,
            Named::F8 | Named::Delete if shift => DeletePermanent,
            Named::F8 | Named::Delete => Delete,
            _ => return None,
        },
        Key::Character(c) if ctrl => match c.to_ascii_lowercase().as_str() {
            "a" => SelectAll,
            "t" => NewTab,
            "w" => CloseTab,
            "d" => DeselectAll,
            "r" => Refresh,
            "h" => ToggleHidden,
            "\\" => GoRoot,
            "q" => Quit,
            _ => return None,
        },
        Key::Character(c) if alt => match c.to_ascii_lowercase().as_str() {
            "o" => SyncOtherPanel,
            _ => return None,
        },
        _ => return None,
    };
    Some(cmd)
}
