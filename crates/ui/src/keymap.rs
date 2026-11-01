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

    /// Every effective binding as (display name, command), defaults with the
    /// overrides applied. Defaults are found by trying every bindable key
    /// with every modifier combination; a combination is left out when the
    /// same key with fewer modifiers does the same (Shift+Down = Down).
    /// Each command's keys come with the fewest modifiers first.
    pub fn bindings(&self) -> Vec<(String, Command)> {
        let mut bases: Vec<Key> = Vec::new();
        for (_, named) in NAMED {
            let key = Key::Named(*named);
            if !bases.contains(&key) {
                bases.push(key);
            }
        }
        bases.extend(BINDABLE_CHARS.chars().map(|c| Key::Character(c.to_string().into())));

        // Modifier sets as bit masks: ctrl, alt, shift, logo.
        let mods = |bits: u8| {
            let mut m = Modifiers::empty();
            for (bit, flag) in [(1, Modifiers::CTRL), (2, Modifiers::ALT), (4, Modifiers::SHIFT), (8, Modifiers::LOGO)] {
                if bits & bit != 0 {
                    m |= flag;
                }
            }
            m
        };
        let mut out: Vec<(String, Command, u32)> = Vec::new();
        for key in &bases {
            for bits in 0u8..16 {
                let m = mods(bits);
                let Some(name) = key_name(key, m) else { continue };
                if self.overrides.contains_key(&name) {
                    continue; // listed below
                }
                let Some(cmd) = default_command(key, m) else { continue };
                // Does the key with a proper subset of the modifiers do the same?
                let redundant = (0..bits).any(|sub| sub & bits == sub && self.command(key, mods(sub)) == Some(cmd));
                if !redundant {
                    out.push((name, cmd, bits.count_ones()));
                }
            }
        }
        let mut overrides: Vec<_> = self.overrides.iter().filter_map(|(n, c)| c.map(|c| (n.clone(), c))).collect();
        overrides.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, cmd) in overrides {
            let count = ["ctrl+", "alt+", "shift+", "win+"].iter().filter(|p| name.contains(*p)).count() as u32;
            out.push((name, cmd, count));
        }
        out.sort_by_key(|&(_, _, count)| count);
        out.into_iter().map(|(name, cmd, _)| (display_key(&name), cmd)).collect()
    }

    /// The display names of the keys bound to `cmd`, from `bindings`.
    pub fn keys_for(bindings: &[(String, Command)], cmd: Command) -> Vec<&str> {
        bindings.iter().filter(|(_, c)| *c == cmd).map(|(k, _)| k.as_str()).collect()
    }
}

/// Character keys tried when listing the default bindings.
const BINDABLE_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789`-=[]\\;',./+*";

/// Display form of a canonical key name: `ctrl+shift+pageup` -> `Ctrl+Shift+PgUp`.
pub fn display_key(name: &str) -> String {
    // A trailing "+" after a "+" separator is the plus key itself.
    let (mods, base) = match name.strip_suffix("++") {
        Some(rest) => (rest, "+"),
        None if name == "+" => ("", "+"),
        None => name.rsplit_once('+').unwrap_or(("", name)),
    };
    let base = match base {
        "up" => "Up".to_owned(),
        "down" => "Down".to_owned(),
        "left" => "Left".to_owned(),
        "right" => "Right".to_owned(),
        "pageup" => "PgUp".to_owned(),
        "pagedown" => "PgDn".to_owned(),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        "enter" => "Enter".to_owned(),
        "backspace" => "Backspace".to_owned(),
        "tab" => "Tab".to_owned(),
        "space" => "Space".to_owned(),
        "insert" => "Ins".to_owned(),
        "delete" => "Del".to_owned(),
        "escape" => "Esc".to_owned(),
        other => other.to_uppercase(),
    };
    let mut out = String::new();
    for part in mods.split('+').filter(|p| !p.is_empty()) {
        out.push_str(match part {
            "ctrl" => "Ctrl+",
            "alt" => "Alt+",
            "shift" => "Shift+",
            _ => "Win+",
        });
    }
    out.push_str(&base);
    out
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
            Named::ArrowDown if alt => DirHistory,
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
            Named::F1 => Help,
            Named::F3 if ctrl => SortBy(SortKey::Name),
            Named::F4 if ctrl => SortBy(SortKey::Ext),
            Named::F5 if ctrl => SortBy(SortKey::Modified),
            Named::F6 if ctrl => SortBy(SortKey::Size),
            Named::F4 if alt => Quit,
            Named::F4 if shift => EditNew,
            Named::F2 if ctrl && shift && !alt => CompareDirsOptions,
            Named::F2 if ctrl => Refresh,
            Named::F2 if shift && !alt => CompareDirs,
            Named::F2 if alt && !ctrl && !shift => CompareFiles,
            Named::F3 => View,
            Named::F4 => Edit,
            Named::F5 if alt && !ctrl && !shift => Pack,
            Named::F9 if alt && !ctrl && !shift => Unpack,
            Named::F5 => Copy,
            Named::F6 if shift => Rename,
            Named::F2 if !ctrl && !alt => Rename,
            Named::F6 => Move,
            Named::F7 if alt && !ctrl && !shift => FindFiles,
            Named::F7 => MakeDir,
            Named::F8 | Named::Delete if shift => DeletePermanent,
            Named::F8 | Named::Delete => Delete,
            _ => return None,
        },
        Key::Character(c) if ctrl => match c.to_ascii_lowercase().as_str() {
            "f" if shift => Disconnect,
            "f" => Connect,
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
    fn default_bindings_are_listed_without_redundant_modifiers() {
        let bindings = Keymap::default().bindings();
        let keys = |cmd| Keymap::keys_for(&bindings, cmd);
        assert_eq!(keys(Command::Help), ["F1"]);
        assert_eq!(keys(Command::Copy), ["F5"]);
        assert_eq!(keys(Command::Open), ["Enter", "Ctrl+PgDn"]);
        assert_eq!(keys(Command::CountDirSizes), ["Alt+Shift+Enter"]);
        assert_eq!(keys(Command::CursorDown), ["Down"]);
        assert_eq!(keys(Command::CursorUp), ["Up"]);
        assert_eq!(keys(Command::OpenInNewTab), ["Ctrl+Up"]);
        assert_eq!(keys(Command::GoRoot), ["Ctrl+\\"]);
        assert_eq!(keys(Command::DeletePermanent), ["Shift+Del", "Shift+F8"]);
    }

    #[test]
    fn display_names() {
        for (name, shown) in [
            ("ctrl+shift+pageup", "Ctrl+Shift+PgUp"),
            ("alt+o", "Alt+O"),
            ("ctrl++", "Ctrl++"),
            ("+", "+"),
            ("win+delete", "Win+Del"),
            ("f12", "F12"),
        ] {
            assert_eq!(display_key(name), shown);
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

        let bindings = map.bindings();
        let keys = |cmd| Keymap::keys_for(&bindings, cmd);
        assert_eq!(keys(Command::Move), ["F6", "F5"]);
        assert_eq!(keys(Command::MakeDir), ["F7", "Ctrl+K"]);
        // F5 now moves, but Shift+F5 still copies.
        assert!(keys(Command::Copy).contains(&"Shift+F5"));
        assert!(!keys(Command::Copy).contains(&"F5"));
        // F8 unbound: Ctrl+F8 and Alt+F8 still delete, so they show up.
        assert!(keys(Command::Delete).starts_with(&["Del", "Ctrl+F8", "Alt+F8"]));
        assert!(!keys(Command::Delete).contains(&"F8"));

        let bad = |k: &str, v: &str| Keymap::new(&BTreeMap::from([(k.to_owned(), v.to_owned())])).unwrap_err();
        assert!(bad("F5", "cm_Nope").contains("cm_Nope"));
        assert!(bad("Ctrl+Nope", "cm_Copy").contains("Ctrl+Nope"));
    }
}
