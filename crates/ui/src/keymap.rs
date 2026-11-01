//! Key bindings: Total Commander-like defaults plus the overrides from the
//! `[keys]` table of `config.toml` (`"Ctrl+F5" = "cm_Copy"`, `""` unbinds).

use iced::keyboard::{key::Named, Key, Modifiers};
use rc_core::{Command, SortKey};
use std::collections::{BTreeMap, HashMap};

/// The defaults with the user's overrides on top.
#[derive(Debug, Default)]
pub struct Keymap {
    /// Canonical key name (see `key_name`) -> command, `None` = unbound.
    overrides: HashMap<String, Option<Command>>,
}

impl Keymap {
    /// Builds the overrides from `config.toml`'s `[keys]`; the first bad
    /// entry is an error naming it.
    pub fn new(keys: &BTreeMap<String, String>) -> Result<Keymap, String> {
        let mut overrides = HashMap::new();
        for (key, id) in keys {
            let name = parse_key(key).ok_or_else(|| format!("unknown key \"{key}\""))?;
            let cmd = match id.trim() {
                "" => None,
                id => Some(Command::from_id(id).ok_or_else(|| format!("unknown command \"{id}\" for {key}"))?),
            };
            overrides.insert(name, cmd);
        }
        Ok(Keymap { overrides })
    }

    pub fn command(&self, key: &Key, m: Modifiers) -> Option<Command> {
        match key_name(key, m).and_then(|name| self.overrides.get(&name)) {
            Some(&cmd) => cmd,
            None => default_command(key, m),
        }
    }
}

/// Named keys that can be bound, with their short aliases.
const NAMED: &[(&str, Named)] = &[
    ("up", Named::ArrowUp),
    ("down", Named::ArrowDown),
    ("left", Named::ArrowLeft),
    ("right", Named::ArrowRight),
    ("pageup", Named::PageUp),
    ("pgup", Named::PageUp),
    ("pagedown", Named::PageDown),
    ("pgdn", Named::PageDown),
    ("home", Named::Home),
    ("end", Named::End),
    ("enter", Named::Enter),
    ("return", Named::Enter),
    ("backspace", Named::Backspace),
    ("tab", Named::Tab),
    ("space", Named::Space),
    ("insert", Named::Insert),
    ("ins", Named::Insert),
    ("delete", Named::Delete),
    ("del", Named::Delete),
    ("escape", Named::Escape),
    ("esc", Named::Escape),
    ("f1", Named::F1),
    ("f2", Named::F2),
    ("f3", Named::F3),
    ("f4", Named::F4),
    ("f5", Named::F5),
    ("f6", Named::F6),
    ("f7", Named::F7),
    ("f8", Named::F8),
    ("f9", Named::F9),
    ("f10", Named::F10),
    ("f11", Named::F11),
    ("f12", Named::F12),
];

/// Canonical name of a key press: modifiers in a fixed order, then the key,
/// all lower case, e.g. `ctrl+shift+f5`, `alt+o`.
fn key_name(key: &Key, m: Modifiers) -> Option<String> {
    let base = match key.as_ref() {
        Key::Named(named) => NAMED.iter().find(|(_, n)| *n == named)?.0.to_owned(),
        Key::Character(c) => c.to_lowercase(),
        Key::Unidentified => return None,
    };
    Some(join(m.control(), m.alt(), m.shift(), m.logo(), &base))
}

fn join(ctrl: bool, alt: bool, shift: bool, logo: bool, base: &str) -> String {
    let mut name = String::new();
    for (on, part) in [(ctrl, "ctrl+"), (alt, "alt+"), (shift, "shift+"), (logo, "win+")] {
        if on {
            name.push_str(part);
        }
    }
    name.push_str(base);
    name
}

/// Parses a key as written in `config.toml` (`Ctrl+Shift+F5`, `Alt+O`,
/// `Ctrl++`) into its canonical name.
fn parse_key(text: &str) -> Option<String> {
    let text = text.trim().to_lowercase();
    // A trailing "+" is the plus key itself ("Ctrl++", "+").
    let (mods, base) = match text.strip_suffix('+') {
        Some(rest) if rest.is_empty() || rest.ends_with('+') => (rest.strip_suffix('+').unwrap_or(""), "+"),
        _ => text.rsplit_once('+').unwrap_or(("", &text)),
    };
    let (mut ctrl, mut alt, mut shift, mut logo) = (false, false, false, false);
    for part in mods.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let flag = match part {
            "ctrl" | "control" => &mut ctrl,
            "alt" => &mut alt,
            "shift" => &mut shift,
            "win" | "super" | "logo" | "cmd" => &mut logo,
            _ => return None,
        };
        *flag = true;
    }
    let base = base.trim();
    let base = match NAMED.iter().find(|(alias, _)| *alias == base) {
        // Aliases map to the first (canonical) name of the same key.
        Some((_, named)) => NAMED.iter().find(|(_, n)| n == named)?.0,
        None if base.chars().count() == 1 => base,
        None => return None,
    };
    Some(join(ctrl, alt, shift, logo, base))
}

fn default_command(key: &Key, m: Modifiers) -> Option<Command> {
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
            Named::ArrowLeft if alt => HistoryBack,
            Named::ArrowRight if alt => HistoryForward,
            Named::Home => CursorHome,
            Named::End => CursorEnd,
            Named::Enter if alt && shift => CountDirSizes,
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
            Named::F4 if shift => EditNew,
            Named::F2 if ctrl => Refresh,
            Named::F2 if shift && !alt => CompareDirs,
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
            "d" => Hotlist,
            "r" => Refresh,
            "h" => ToggleHidden,
            "s" => QuickFilter,
            "m" => MultiRename,
            "\\" => GoRoot,
            "q" => Quit,
            "," => EditConfig,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key, m: Modifiers) -> (Key, Modifiers) {
        (key, m)
    }

    #[test]
    fn parsed_names_match_key_presses() {
        let cases = [
            ("Ctrl+Shift+F5", press(Key::Named(Named::F5), Modifiers::CTRL | Modifiers::SHIFT)),
            ("shift + ctrl + f5", press(Key::Named(Named::F5), Modifiers::CTRL | Modifiers::SHIFT)),
            ("Alt+O", press(Key::Character("o".into()), Modifiers::ALT)),
            ("Shift+A", press(Key::Character("A".into()), Modifiers::SHIFT)),
            ("Del", press(Key::Named(Named::Delete), Modifiers::empty())),
            ("Ctrl+PgUp", press(Key::Named(Named::PageUp), Modifiers::CTRL)),
            ("Ctrl++", press(Key::Character("+".into()), Modifiers::CTRL)),
            ("+", press(Key::Character("+".into()), Modifiers::empty())),
            ("Win+E", press(Key::Character("e".into()), Modifiers::LOGO)),
        ];
        for (text, (key, m)) in cases {
            assert_eq!(parse_key(text), key_name(&key, m), "{text}");
        }
        for bad in ["", "Ctrl+", "Hyper+X", "F99", "Ctrl+Foo"] {
            assert_eq!(parse_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn overrides_replace_and_unbind_defaults() {
        let keys = BTreeMap::from([
            ("F5".to_owned(), "cm_RenMov".to_owned()),
            ("F8".to_owned(), String::new()),
            ("Ctrl+K".to_owned(), "cm_mkdir".to_owned()),
        ]);
        let map = Keymap::new(&keys).unwrap();
        let none = Modifiers::empty();
        assert_eq!(map.command(&Key::Named(Named::F5), none), Some(Command::Move));
        assert_eq!(map.command(&Key::Named(Named::F8), none), None);
        assert_eq!(map.command(&Key::Character("k".into()), Modifiers::CTRL), Some(Command::MakeDir));
        // Untouched keys keep their default.
        assert_eq!(map.command(&Key::Named(Named::F6), none), Some(Command::Move));
        assert_eq!(map.command(&Key::Named(Named::Delete), none), Some(Command::Delete));

        let bad = |k: &str, v: &str| Keymap::new(&BTreeMap::from([(k.to_owned(), v.to_owned())])).unwrap_err();
        assert!(bad("F5", "cm_Nope").contains("cm_Nope"));
        assert!(bad("Ctrl+Nope", "cm_Copy").contains("Ctrl+Nope"));
    }
}
