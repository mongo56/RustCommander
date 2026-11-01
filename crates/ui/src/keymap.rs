//! Key bindings: Total Commander-like defaults plus the overrides from the
//! `[keys]` table of `config.toml` (`"Ctrl+F5" = "cm_Copy"`, `""` unbinds).
//! Every binding is one exact key + modifier combination: rebinding or
//! unbinding F7 leaves Ctrl+F7, Shift+F7 etc. as they were (unbound unless
//! `DEFAULTS` or `[keys]` names them).

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

    /// The command bound to a canonical key name (`ctrl+k`).
    pub fn command_of(&self, name: &str) -> Option<Command> {
        match self.overrides.get(name) {
            Some(&cmd) => cmd,
            None => default_of(name),
        }
    }

    /// The `[keys]` change that binds the canonical key `name` to `cmd`
    /// (`None` = unbinds it): the entry is dropped when that's the default.
    pub fn bind(&self, name: &str, cmd: Option<Command>) -> KeyEdit {
        let value = (default_of(name) != cmd).then(|| cmd.map_or(String::new(), |c| c.id().to_owned()));
        (name.to_owned(), value)
    }

    /// The changes that leave `cmd` without any key.
    pub fn unbind(&self, cmd: Command) -> Vec<KeyEdit> {
        self.named_bindings().into_iter().filter(|(_, c)| *c == cmd).map(|(n, _)| self.bind(&n, None)).collect()
    }

    /// The changes that give `cmd` back its default keys: drops every
    /// override that binds it or that took one of its default keys.
    pub fn reset(&self, cmd: Command) -> Vec<KeyEdit> {
        let mut names: Vec<&String> = self
            .overrides
            .iter()
            .filter(|(name, over)| **over == Some(cmd) || default_of(name) == Some(cmd))
            .map(|(name, _)| name)
            .collect();
        names.sort();
        names.into_iter().map(|n| (n.clone(), None)).collect()
    }

    /// Whether `[keys]` changes anything for `cmd`.
    pub fn is_changed(&self, cmd: Command) -> bool {
        !self.reset(cmd).is_empty()
    }

    /// Applies `[keys]` changes (as `config.toml` would after writing them).
    #[cfg(test)]
    pub fn apply(&mut self, edits: &[KeyEdit]) {
        for (name, value) in edits {
            match value {
                None => {
                    self.overrides.remove(name);
                }
                Some(id) => {
                    self.overrides.insert(name.clone(), Command::from_id(id));
                }
            }
        }
    }

    /// The command of a key press: exactly that key with exactly those
    /// modifiers (Ctrl+F7 is not F7).
    pub fn command(&self, key: &Key, m: Modifiers) -> Option<Command> {
        self.command_of(&key_name(key, m)?)
    }

    /// The function-key bar: (key label, command bound to exactly that key,
    /// `None` = unbound). Cheap enough to call on every view.
    pub fn fkey_bar(&self) -> Vec<(&'static str, Option<Command>)> {
        FKEY_BAR.iter().map(|&(label, name)| (label, self.command_of(name))).collect()
    }

    /// The hint shown instead of the function-key bar: the first key of
    /// help, the command palette and the settings, as they are bound now.
    pub fn bar_hint(&self) -> String {
        self.bar_hint_with(true)
    }

    /// `bar_hint` without the descriptions ("F1 · Ctrl+Space · F10"), for narrow windows.
    pub fn bar_hint_short(&self) -> String {
        self.bar_hint_with(false)
    }

    fn bar_hint_with(&self, described: bool) -> String {
        let bindings = self.bindings();
        let part = |cmd: Command, what: &str| {
            Keymap::keys_for(&bindings, cmd).first().map(|key| if described { format!("{key} {what}") } else { key.to_string() })
        };
        let parts: Vec<String> = [
            part(Command::Help, "help"),
            part(Command::CommandPalette, "command palette"),
            part(Command::Configuration, "settings"),
        ]
        .into_iter()
        .flatten()
        .collect();
        parts.join(" · ")
    }

    /// Every effective binding as (display name, command), defaults with the
    /// overrides applied. Each command's keys come with the fewest modifiers
    /// first (defaults in `DEFAULTS` order, then the overrides).
    pub fn bindings(&self) -> Vec<(String, Command)> {
        self.named_bindings().into_iter().map(|(name, cmd)| (display_key(&name), cmd)).collect()
    }

    /// `bindings` with canonical key names.
    pub fn named_bindings(&self) -> Vec<(String, Command)> {
        let mut out: Vec<(String, Command)> = DEFAULTS
            .iter()
            .filter(|(name, _)| !self.overrides.contains_key(*name))
            .map(|&(name, cmd)| (name.to_owned(), cmd))
            .collect();
        let mut overrides: Vec<_> = self.overrides.iter().filter_map(|(n, c)| c.map(|c| (n.clone(), c))).collect();
        overrides.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(overrides);
        out.sort_by_key(|(name, _)| modifier_count(name));
        out
    }

    /// The display names of the keys bound to `cmd`, from `bindings`.
    pub fn keys_for(bindings: &[(String, Command)], cmd: Command) -> Vec<&str> {
        bindings.iter().filter(|(_, c)| *c == cmd).map(|(k, _)| k.as_str()).collect()
    }
}

/// A change to `[keys]`: canonical key name and the value to write
/// (`Some("cm_Copy")`, `Some("")` unbinds) or `None` to drop the entry.
pub type KeyEdit = (String, Option<String>);

/// The slots of the function-key bar: display label and canonical key name.
const FKEY_BAR: [(&str, &str); 11] = [
    ("F1", "f1"),
    ("F2", "f2"),
    ("F3", "f3"),
    ("F4", "f4"),
    ("F5", "f5"),
    ("F6", "f6"),
    ("F7", "f7"),
    ("F8", "f8"),
    ("F9", "f9"),
    ("F10", "f10"),
    ("Alt+F4", "alt+f4"),
];

/// The default command of a canonical key name.
fn default_of(name: &str) -> Option<Command> {
    DEFAULTS.iter().find(|(n, _)| *n == name).map(|&(_, cmd)| cmd)
}

/// How many modifiers a canonical key name has (`ctrl++` has one).
fn modifier_count(name: &str) -> usize {
    let mods = match name.strip_suffix("++") {
        Some(rest) => rest,
        None => name.rsplit_once('+').map_or("", |(mods, _)| mods),
    };
    mods.split('+').filter(|p| !p.is_empty()).count()
}

/// The key press a canonical key name stands for.
#[cfg(test)]
fn press_of(name: &str) -> Option<(Key, Modifiers)> {
    let (mods, base) = match name.strip_suffix("++") {
        Some(rest) => (rest, "+"),
        None if name == "+" => ("", "+"),
        None => name.rsplit_once('+').unwrap_or(("", name)),
    };
    let mut m = Modifiers::empty();
    for part in mods.split('+').filter(|p| !p.is_empty()) {
        m |= match part {
            "ctrl" => Modifiers::CTRL,
            "alt" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            "win" => Modifiers::LOGO,
            _ => return None,
        };
    }
    let key = match NAMED.iter().find(|(alias, _)| *alias == base) {
        Some((_, named)) => Key::Named(*named),
        None => Key::Character(base.into()),
    };
    Some((key, m))
}

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
pub fn key_name(key: &Key, m: Modifiers) -> Option<String> {
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
pub fn parse_key(text: &str) -> Option<String> {
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

/// The default bindings (Total Commander's where it has one), one entry per
/// exact key + modifier combination as a canonical name (see `key_name`);
/// a combination not listed is unbound. Among keys with as many modifiers,
/// this is also the order they are listed in.
const DEFAULTS: &[(&str, Command)] = {
    use Command::*;
    &[
        ("up", CursorUp),
        ("down", CursorDown),
        ("pageup", PageUp),
        ("pagedown", PageDown),
        ("home", CursorHome),
        ("end", CursorEnd),
        ("enter", Open),
        ("backspace", GoParent),
        ("tab", SwitchPanel),
        ("space", ToggleSelect { advance: false }),
        ("insert", ToggleSelect { advance: true }),
        ("delete", Delete),
        ("escape", DeselectAll),
        ("f1", Help),
        ("f2", Rename),
        ("f3", View),
        ("f4", Edit),
        ("f5", Copy),
        ("f6", Move),
        ("f7", MakeDir),
        ("f8", Delete),
        ("f9", Terminal),
        ("f10", Configuration),
        // Shift+cursor keys move and extend the selection from the anchor
        // (`App::run` checks the Shift state).
        ("shift+up", CursorUp),
        ("shift+down", CursorDown),
        ("shift+pageup", PageUp),
        ("shift+pagedown", PageDown),
        ("shift+home", CursorHome),
        ("shift+end", CursorEnd),
        ("shift+tab", SwitchPanel),
        ("ctrl+up", OpenInNewTab),
        ("alt+down", DirHistory),
        ("alt+left", HistoryBack),
        ("alt+right", HistoryForward),
        ("ctrl+pageup", GoParent),
        ("ctrl+pagedown", Open),
        ("ctrl+tab", NextTab),
        ("ctrl+space", CommandPalette),
        ("shift+delete", DeletePermanent),
        ("ctrl+f2", Refresh),
        ("alt+f2", CompareFiles),
        ("shift+f2", CompareDirs),
        ("ctrl+f3", SortBy(SortKey::Name)),
        ("ctrl+f4", SortBy(SortKey::Ext)),
        ("alt+f4", Quit),
        ("shift+f4", EditNew),
        ("ctrl+f5", SortBy(SortKey::Modified)),
        ("alt+f5", Pack),
        ("shift+f5", Copy),
        ("ctrl+f6", SortBy(SortKey::Size)),
        ("shift+f6", Rename),
        ("alt+f7", FindFiles),
        ("shift+f8", DeletePermanent),
        ("alt+f9", Unpack),
        ("ctrl+a", SelectAll),
        ("ctrl+d", Hotlist),
        ("ctrl+f", Connect),
        ("ctrl+h", ToggleHidden),
        ("ctrl+m", MultiRename),
        ("alt+o", SyncOtherPanel),
        ("ctrl+q", Quit),
        ("ctrl+r", Refresh),
        ("ctrl+s", QuickFilter),
        ("ctrl+t", NewTab),
        ("ctrl+w", CloseTab),
        ("ctrl+\\", GoRoot),
        ("ctrl+,", EditConfig),
        ("alt+shift+enter", CountDirSizes),
        ("ctrl+shift+tab", PrevTab),
        ("ctrl+shift+f2", CompareDirsOptions),
        ("ctrl+shift+f", Disconnect),
    ]
};

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
    fn default_bindings_are_listed_fewest_modifiers_first() {
        let bindings = Keymap::default().bindings();
        let keys = |cmd| Keymap::keys_for(&bindings, cmd);
        assert_eq!(keys(Command::Help), ["F1"]);
        assert_eq!(keys(Command::Copy), ["F5", "Shift+F5"]);
        assert_eq!(keys(Command::SwitchPanel), ["Tab", "Shift+Tab"]);
        assert_eq!(keys(Command::PageDown), ["PgDn", "Shift+PgDn"]);
        assert_eq!(keys(Command::Open), ["Enter", "Ctrl+PgDn"]);
        assert_eq!(keys(Command::CountDirSizes), ["Alt+Shift+Enter"]);
        assert_eq!(keys(Command::CursorDown), ["Down", "Shift+Down"]);
        assert_eq!(keys(Command::CursorUp), ["Up", "Shift+Up"]);
        assert_eq!(keys(Command::OpenInNewTab), ["Ctrl+Up"]);
        assert_eq!(keys(Command::GoRoot), ["Ctrl+\\"]);
        assert_eq!(keys(Command::DeletePermanent), ["Shift+Del", "Shift+F8"]);
        assert_eq!(keys(Command::CommandPalette), ["Ctrl+Space"]);
        assert!(!keys(Command::ToggleSelect { advance: false }).contains(&"Ctrl+Space"));
        // Plain Space still selects; Ctrl+Space opens the palette.
        let map = Keymap::default();
        let space = Key::Named(Named::Space);
        assert_eq!(map.command(&space, Modifiers::empty()), Some(Command::ToggleSelect { advance: false }));
        assert_eq!(map.command(&space, Modifiers::CTRL), Some(Command::CommandPalette));
        assert_eq!(parse_key("Ctrl+Space").as_deref(), Some("ctrl+space"));
    }

    #[test]
    fn defaults_resolve_exactly() {
        use Command::*;
        let map = Keymap::default();
        let cases = [
            ("f1", Some(Help)),
            ("f2", Some(Rename)),
            ("shift+f2", Some(CompareDirs)),
            ("ctrl+shift+f2", Some(CompareDirsOptions)),
            ("ctrl+f2", Some(Refresh)),
            ("alt+f2", Some(CompareFiles)),
            ("f3", Some(View)),
            ("ctrl+f3", Some(SortBy(SortKey::Name))),
            ("f4", Some(Edit)),
            ("shift+f4", Some(EditNew)),
            ("alt+f4", Some(Quit)),
            ("ctrl+f4", Some(SortBy(SortKey::Ext))),
            ("f5", Some(Copy)),
            ("alt+f5", Some(Pack)),
            ("ctrl+f5", Some(SortBy(SortKey::Modified))),
            ("f6", Some(Move)),
            ("shift+f6", Some(Rename)),
            ("ctrl+f6", Some(SortBy(SortKey::Size))),
            ("f7", Some(MakeDir)),
            ("alt+f7", Some(FindFiles)),
            ("f8", Some(Delete)),
            ("shift+f8", Some(DeletePermanent)),
            ("delete", Some(Delete)),
            ("shift+delete", Some(DeletePermanent)),
            ("f9", Some(Terminal)),
            ("alt+f9", Some(Unpack)),
            ("f10", Some(Configuration)),
            ("enter", Some(Open)),
            ("alt+shift+enter", Some(CountDirSizes)),
            ("ctrl+pagedown", Some(Open)),
            ("ctrl+pageup", Some(GoParent)),
            ("backspace", Some(GoParent)),
            ("up", Some(CursorUp)),
            ("down", Some(CursorDown)),
            ("ctrl+up", Some(OpenInNewTab)),
            ("alt+down", Some(DirHistory)),
            ("alt+left", Some(HistoryBack)),
            ("alt+right", Some(HistoryForward)),
            ("tab", Some(SwitchPanel)),
            ("ctrl+tab", Some(NextTab)),
            ("ctrl+shift+tab", Some(PrevTab)),
            ("space", Some(ToggleSelect { advance: false })),
            ("insert", Some(ToggleSelect { advance: true })),
            ("ctrl+space", Some(CommandPalette)),
            ("escape", Some(DeselectAll)),
            ("ctrl+f", Some(Connect)),
            ("ctrl+shift+f", Some(Disconnect)),
            ("ctrl+a", Some(SelectAll)),
            ("ctrl+q", Some(Quit)),
            ("ctrl+\\", Some(GoRoot)),
            ("ctrl+,", Some(EditConfig)),
            ("alt+o", Some(SyncOtherPanel)),
            ("ctrl+k", None),
            ("alt+x", None),
            ("f11", None),
            ("ctrl+left", None),
            ("shift+up", Some(CursorUp)),
            ("shift+down", Some(CursorDown)),
            ("shift+pageup", Some(PageUp)),
            ("shift+pagedown", Some(PageDown)),
            ("shift+home", Some(CursorHome)),
            ("shift+end", Some(CursorEnd)),
            ("shift+tab", Some(SwitchPanel)),
            ("shift+f5", Some(Copy)),
            ("ctrl+f8", None),
            ("alt+f8", None),
            ("ctrl+delete", None),
            ("ctrl+alt+a", None),
            ("ctrl+alt+\\", None),
        ];
        for (name, cmd) in cases {
            assert_eq!(map.command_of(name), cmd, "{name}");
            let (key, m) = press_of(name).unwrap();
            assert_eq!(map.command(&key, m), cmd, "{name}");
        }
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
        // F5 now moves; Shift+F5 is its own binding and still copies.
        assert_eq!(keys(Command::Copy), ["Shift+F5"]);
        assert_eq!(map.command(&Key::Named(Named::F5), Modifiers::SHIFT), Some(Command::Copy));
        assert_eq!(map.command(&Key::Named(Named::F5), Modifiers::ALT | Modifiers::SHIFT), None);
        // F8 unbound: only Del deletes, Shift+F8 still deletes permanently.
        assert_eq!(keys(Command::Delete), ["Del"]);
        assert_eq!(map.command(&Key::Named(Named::F8), Modifiers::CTRL), None);
        assert_eq!(map.command(&Key::Named(Named::F8), Modifiers::SHIFT), Some(Command::DeletePermanent));

        // Canonical names map back to the same presses.
        for (name, cmd) in map.named_bindings() {
            assert_eq!(map.command_of(&name), Some(cmd), "{name}");
            let (key, m) = press_of(&name).unwrap();
            assert_eq!(key_name(&key, m).as_deref(), Some(name.as_str()));
        }

        let bad = |k: &str, v: &str| Keymap::new(&BTreeMap::from([(k.to_owned(), v.to_owned())])).unwrap_err();
        assert!(bad("F5", "cm_Nope").contains("cm_Nope"));
        assert!(bad("Ctrl+Nope", "cm_Copy").contains("Ctrl+Nope"));
    }

    #[test]
    fn fkey_bar_follows_bindings() {
        let label = |map: &Keymap, key: &str| {
            map.fkey_bar().into_iter().find(|(k, _)| *k == key).map(|(_, c)| c.map(Command::short_label)).unwrap()
        };
        let map = Keymap::default();
        assert_eq!(label(&map, "F1"), Some("Help"));
        assert_eq!(label(&map, "F2"), Some("Rename"));
        assert_eq!(label(&map, "F9"), Some("Term"));
        assert_eq!(map.bar_hint(), "F1 help · Ctrl+Space command palette · F10 settings");
        assert_eq!(label(&map, "F3"), Some("View"));
        assert_eq!(label(&map, "F7"), Some("MkDir"));
        assert_eq!(label(&map, "Alt+F4"), Some("Exit"));

        let keys = BTreeMap::from([("F7".to_owned(), "cm_Copy".to_owned()), ("F8".to_owned(), String::new())]);
        let map = Keymap::new(&keys).unwrap();
        assert_eq!(label(&map, "F7"), Some("Copy"));
        assert_eq!(label(&map, "F8"), None);
        assert_eq!(label(&map, "F5"), Some("Copy"));

        let keys = BTreeMap::from([("F10".to_owned(), String::new()), ("Ctrl+K".to_owned(), "cm_Configuration".to_owned())]);
        let hint = Keymap::new(&keys).unwrap().bar_hint();
        assert!(hint.contains("settings") && !hint.ends_with(" F10 settings"), "{hint}");
    }

    #[test]
    fn rebinding_reports_conflicts_and_resets() {
        let mut map = Keymap::default();
        // Binding a default key to its default command writes nothing.
        assert_eq!(map.bind("f5", Some(Command::Copy)), ("f5".to_owned(), None));
        // F7 is taken by MakeDir: a conflict the settings view asks about.
        assert_eq!(map.command_of("f7"), Some(Command::MakeDir));
        assert_eq!(map.command_of("ctrl+k"), None);
        let edits = vec![map.bind("f7", Some(Command::Copy)), map.bind("ctrl+k", Some(Command::MakeDir))];
        assert_eq!(edits[0], ("f7".to_owned(), Some("cm_Copy".to_owned())));
        map.apply(&edits);
        let bindings = map.bindings();
        // Only Ctrl+K makes folders now: no modifier variant of F7 is left.
        assert_eq!(Keymap::keys_for(&bindings, Command::MakeDir), ["Ctrl+K"]);
        let f7 = Key::Named(Named::F7);
        for m in [Modifiers::CTRL, Modifiers::SHIFT, Modifiers::CTRL | Modifiers::SHIFT, Modifiers::LOGO] {
            assert_eq!(map.command(&f7, m), None, "{m:?}");
        }
        // Alt+F7 is its own default and still finds files.
        assert_eq!(map.command(&f7, Modifiers::ALT), Some(Command::FindFiles));
        assert!(Keymap::keys_for(&bindings, Command::Copy).starts_with(&["F5", "F7"]));
        assert!(map.is_changed(Command::MakeDir) && map.is_changed(Command::Copy));
        assert!(!map.is_changed(Command::Move));

        // Resetting MakeDir drops Ctrl+K and gives F7 back.
        let reset = map.reset(Command::MakeDir);
        assert_eq!(reset, [("ctrl+k".to_owned(), None), ("f7".to_owned(), None)]);
        map.apply(&reset);
        assert_eq!(Keymap::keys_for(&map.bindings(), Command::MakeDir), ["F7"]);
        assert!(!map.is_changed(Command::Copy));

        // Unbinding Delete writes exactly its two keys.
        let unbind = map.unbind(Command::Delete);
        assert_eq!(unbind, [("delete".to_owned(), Some(String::new())), ("f8".to_owned(), Some(String::new()))]);
        map.apply(&unbind);
        assert!(Keymap::keys_for(&map.bindings(), Command::Delete).is_empty());
        assert_eq!(map.command(&Key::Named(Named::Delete), Modifiers::CTRL), None);
        assert_eq!(map.command(&Key::Named(Named::Delete), Modifiers::SHIFT), Some(Command::DeletePermanent));
        map.apply(&map.reset(Command::Delete));
        assert_eq!(Keymap::keys_for(&map.bindings(), Command::Delete), ["Del", "F8"]);

        // Unbinding a command that has an override drops that entry too.
        map.apply(&[map.bind("ctrl+k", Some(Command::Copy))]);
        map.apply(&map.unbind(Command::Copy));
        assert!(Keymap::keys_for(&map.bindings(), Command::Copy).is_empty());
        assert_eq!(map.command_of("ctrl+k"), None);
        assert_eq!(map.command_of("f5"), None);
    }

    #[test]
    fn only_listed_combinations_are_bound() {
        let keys = BTreeMap::from([
            ("F7".to_owned(), "cm_Copy".to_owned()),
            ("Ctrl+Shift+F7".to_owned(), "cm_MkDir".to_owned()),
            ("Del".to_owned(), String::new()),
        ]);
        for map in [Keymap::default(), Keymap::new(&keys).unwrap()] {
            let listed: HashMap<String, Command> = map.named_bindings().into_iter().collect();
            let mut bases: Vec<Key> = NAMED.iter().map(|(_, n)| Key::Named(*n)).collect();
            bases.extend("abcdefghijklmnopqrstuvwxyz0123456789`-=[]\\;',./+*".chars().map(|c| Key::Character(c.to_string().into())));
            for key in &bases {
                for bits in 0u8..16 {
                    let mut m = Modifiers::empty();
                    for (bit, flag) in [(1, Modifiers::CTRL), (2, Modifiers::ALT), (4, Modifiers::SHIFT), (8, Modifiers::LOGO)] {
                        if bits & bit != 0 {
                            m |= flag;
                        }
                    }
                    let name = key_name(key, m).unwrap();
                    assert_eq!(map.command(key, m), listed.get(&name).copied(), "{name}");
                }
            }
        }
        // The rebound key and the one override, nothing else of F7.
        let map = Keymap::new(&keys).unwrap();
        let f7 = Key::Named(Named::F7);
        assert_eq!(map.command(&f7, Modifiers::empty()), Some(Command::Copy));
        assert_eq!(map.command(&f7, Modifiers::CTRL | Modifiers::SHIFT), Some(Command::MakeDir));
        assert_eq!(map.command(&f7, Modifiers::CTRL), None);
        assert_eq!(map.command(&f7, Modifiers::SHIFT), None);
        assert_eq!(map.command(&Key::Named(Named::Delete), Modifiers::empty()), None);
        assert_eq!(map.command(&Key::Named(Named::Delete), Modifiers::SHIFT), Some(Command::DeletePermanent));
        // Every default name is canonical and distinct.
        let mut seen = std::collections::HashSet::new();
        for (name, _) in DEFAULTS {
            assert_eq!(parse_key(name).as_deref(), Some(*name));
            assert!(seen.insert(*name), "{name} twice");
        }
    }
}
