//! Colours and widget styles. Everything visual lives here; the colours can
//! be overridden in `config.toml` (`[colors]`) and change at runtime.

use iced::widget::{button, container, progress_bar, text_input};
use iced::{Background, Border, Color, Theme};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

const fn rgb(hex: u32) -> Color {
    Color {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// The colour set; `config.toml`'s `[colors]` can override each one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    /// Window background.
    pub bg: Color,
    /// File list background.
    pub panel: Color,
    /// Every other file row.
    pub zebra: Color,
    /// Column headers, dialogs, the current tab.
    pub header: Color,
    /// Lines and frames.
    pub border: Color,
    /// Normal text.
    pub text: Color,
    /// Secondary text (sizes, dates, hints).
    pub muted: Color,
    /// Folder names.
    pub dir: Color,
    /// Focus frames, progress bars, the default button.
    pub accent: Color,
    /// Selected (marked) entries.
    pub selected: Color,
    /// Error messages.
    pub error: Color,
    /// Cursor row in the active panel.
    pub cursor: Color,
    /// Cursor row in the other panel.
    pub cursor_inactive: Color,
    /// Success messages.
    pub success: Color,
    /// Folder row under the mouse while dragging files onto it.
    pub drop: Color,
}

pub const DARK: Colors = Colors {
    bg: rgb(0x14161a),
    panel: rgb(0x191c21),
    zebra: rgb(0x1c1f25),
    header: rgb(0x1f232a),
    border: rgb(0x2a2f38),
    text: rgb(0xd5d9e0),
    muted: rgb(0x7b8494),
    dir: rgb(0x8cb8ff),
    accent: rgb(0x5b9cf5),
    selected: rgb(0xf2b75b),
    error: rgb(0xf27474),
    cursor: rgb(0x263a58),
    cursor_inactive: rgb(0x252a33),
    success: rgb(0x6cc58a),
    drop: rgb(0x24452f),
};

pub const LIGHT: Colors = Colors {
    bg: rgb(0xeceff3),
    panel: rgb(0xffffff),
    zebra: rgb(0xf5f7fa),
    header: rgb(0xe2e6ec),
    border: rgb(0xc9cfd8),
    text: rgb(0x1f2328),
    muted: rgb(0x687180),
    dir: rgb(0x1a5fb4),
    accent: rgb(0x2f6fd6),
    selected: rgb(0xb35900),
    error: rgb(0xc62828),
    cursor: rgb(0xcfe0fb),
    cursor_inactive: rgb(0xe3e7ee),
    success: rgb(0x2e7d32),
    drop: rgb(0xcdeccf),
};

pub const NORD: Colors = Colors {
    bg: rgb(0x2e3440),
    panel: rgb(0x323845),
    zebra: rgb(0x363c4a),
    header: rgb(0x3b4252),
    border: rgb(0x4c566a),
    text: rgb(0xd8dee9),
    muted: rgb(0x8c96a8),
    dir: rgb(0x88c0d0),
    accent: rgb(0x81a1c1),
    selected: rgb(0xebcb8b),
    error: rgb(0xbf616a),
    cursor: rgb(0x40506a),
    cursor_inactive: rgb(0x3d4454),
    success: rgb(0xa3be8c),
    drop: rgb(0x3e5040),
};

pub const GRUVBOX: Colors = Colors {
    bg: rgb(0x1d2021),
    panel: rgb(0x282828),
    zebra: rgb(0x2d2c2a),
    header: rgb(0x32302f),
    border: rgb(0x45403d),
    text: rgb(0xebdbb2),
    muted: rgb(0x928374),
    dir: rgb(0x83a598),
    accent: rgb(0x8ec07c),
    selected: rgb(0xfabd2f),
    error: rgb(0xfb4934),
    cursor: rgb(0x504945),
    cursor_inactive: rgb(0x3c3836),
    success: rgb(0xb8bb26),
    drop: rgb(0x3d4a2a),
};

/// Dark plum with a sunset orange accent and warm sand folders.
pub const SUNSET: Colors = Colors {
    bg: rgb(0x1a1220),
    panel: rgb(0x201726),
    zebra: rgb(0x251b2c),
    header: rgb(0x2b1f32),
    border: rgb(0x45324f),
    text: rgb(0xf3e3d3),
    muted: rgb(0x9a8496),
    dir: rgb(0xf2c38b),
    accent: rgb(0xff7a45),
    selected: rgb(0xffd166),
    error: rgb(0xff4f6d),
    cursor: rgb(0x4a2a3a),
    cursor_inactive: rgb(0x33253a),
    success: rgb(0x9bd17a),
    drop: rgb(0x3a4a2a),
};

/// Seventies browns: burnt orange, mustard and avocado.
pub const SEVENTIES: Colors = Colors {
    bg: rgb(0x1b1410),
    panel: rgb(0x221a14),
    zebra: rgb(0x271e17),
    header: rgb(0x2d231b),
    border: rgb(0x4a3a2c),
    text: rgb(0xf0e2c8),
    muted: rgb(0x9c8a72),
    dir: rgb(0xb5c25e),
    accent: rgb(0xe0712c),
    selected: rgb(0xf6c344),
    error: rgb(0xd9482b),
    cursor: rgb(0x553623),
    cursor_inactive: rgb(0x3a2d22),
    success: rgb(0xa8c256),
    drop: rgb(0x3c4524),
};

/// Drum-machine black and grey with a signal orange accent.
pub const EIGHT_O_EIGHT: Colors = Colors {
    bg: rgb(0x111111),
    panel: rgb(0x161616),
    zebra: rgb(0x1a1a1a),
    header: rgb(0x202020),
    border: rgb(0x333333),
    text: rgb(0xe8e4dc),
    muted: rgb(0x8a8580),
    dir: rgb(0xf59e42),
    accent: rgb(0xff6b1a),
    selected: rgb(0xffe14d),
    error: rgb(0xff4040),
    cursor: rgb(0x3d2a1c),
    cursor_inactive: rgb(0x2a2622),
    success: rgb(0x8fd16a),
    drop: rgb(0x2e4224),
};

/// A built-in colour set, chosen with `theme = "<id>"` in `config.toml`;
/// `[colors]` overrides apply on top.
#[derive(Debug)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub colors: Colors,
}

pub const PRESETS: &[Preset] = &[
    Preset { id: "dark", name: "Dark", colors: DARK },
    Preset { id: "light", name: "Light", colors: LIGHT },
    Preset { id: "nord", name: "Nord", colors: NORD },
    Preset { id: "gruvbox", name: "Gruvbox", colors: GRUVBOX },
    Preset { id: "sunset", name: "Sunset", colors: SUNSET },
    Preset { id: "seventies", name: "Seventies", colors: SEVENTIES },
    Preset { id: "808", name: "808", colors: EIGHT_O_EIGHT },
];

/// The preset with this id (case-insensitive); empty = the dark default.
pub fn preset(id: &str) -> Option<&'static Preset> {
    let id = id.trim();
    let id = if id.is_empty() { "dark" } else { id };
    PRESETS.iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

/// What each colour is for, by name (`NAMES` order), for the settings view.
pub const PURPOSES: &[&str] = &[
    "Window background",
    "File list background",
    "Every other file row",
    "Column headers, dialogs, the current tab",
    "Lines and frames",
    "Normal text",
    "Secondary text (sizes, dates, hints)",
    "Folder names",
    "Focus frames, progress bars, the default button",
    "Selected (marked) entries",
    "Error messages",
    "Cursor row in the active panel",
    "Cursor row in the other panel",
    "Success messages",
    "Folder row under the mouse while dragging",
];

/// Colour names as written in `config.toml`, in display order.
pub const NAMES: &[&str] = &["bg", "panel", "zebra", "header", "border", "text", "muted", "dir", "accent", "selected", "error", "cursor", "cursor_inactive", "success", "drop"];

impl Colors {
    fn get_mut(&mut self, name: &str) -> Option<&mut Color> {
        Some(match name {
            "bg" => &mut self.bg,
            "panel" => &mut self.panel,
            "zebra" => &mut self.zebra,
            "header" => &mut self.header,
            "border" => &mut self.border,
            "text" => &mut self.text,
            "muted" => &mut self.muted,
            "dir" => &mut self.dir,
            "accent" => &mut self.accent,
            "selected" => &mut self.selected,
            "error" => &mut self.error,
            "cursor" => &mut self.cursor,
            "cursor_inactive" => &mut self.cursor_inactive,
            "success" => &mut self.success,
            "drop" => &mut self.drop,
            _ => return None,
        })
    }

    pub fn get(mut self, name: &str) -> Option<Color> {
        self.get_mut(name).copied()
    }

    /// `base` with the `name = "#rrggbb"` overrides applied; the first bad
    /// entry is an error naming it.
    pub fn with<'a>(mut self, overrides: impl IntoIterator<Item = (&'a String, &'a String)>) -> Result<Colors, String> {
        for (name, value) in overrides {
            let color = parse_hex(value).ok_or_else(|| format!("bad colour \"{value}\" for {name} (use \"#rrggbb\")"))?;
            *self.get_mut(&name.to_lowercase()).ok_or_else(|| format!("unknown colour \"{name}\""))? = color;
        }
        Ok(self)
    }
}

impl Colors {
    /// These colours with `name` set to `color` (unknown names change nothing).
    pub fn with_color(mut self, name: &str, color: Color) -> Colors {
        if let Some(slot) = self.get_mut(name) {
            *slot = color;
        }
        self
    }
}

/// Hue (0..360), saturation and value (0..1) of `c`.
pub fn to_hsv(c: Color) -> (f32, f32, f32) {
    let max = c.r.max(c.g).max(c.b);
    let d = max - c.r.min(c.g).min(c.b);
    let h = if d == 0.0 {
        0.0
    } else if max == c.r {
        60.0 * ((c.g - c.b) / d).rem_euclid(6.0)
    } else if max == c.g {
        60.0 * ((c.b - c.r) / d + 2.0)
    } else {
        60.0 * ((c.r - c.g) / d + 4.0)
    };
    (h, if max == 0.0 { 0.0 } else { d / max }, max)
}

/// The colour with hue `h` (0..360), saturation `s` and value `v` (0..1).
pub fn from_hsv(h: f32, s: f32, v: f32) -> Color {
    let f = |n: f32| {
        let k = (n + h / 60.0) % 6.0;
        v - v * s * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    Color { r: f(5.0), g: f(3.0), b: f(1.0), a: 1.0 }
}

/// `#rrggbb` (the `#` is optional).
pub fn parse_hex(text: &str) -> Option<Color> {
    let hex = text.trim();
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(rgb(u32::from_str_radix(hex, 16).ok()?))
}

pub fn to_hex(c: Color) -> String {
    let byte = |v: f32| (v * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

static COLORS: RwLock<Colors> = RwLock::new(DARK);

/// The colours in use.
// Shared text sizes, paddings and spacings for the screens that use them (values are the ones the screens had before).
pub const SIZE_SMALL: u16 = 12;
pub const SIZE_BODY: u16 = 13;
pub const SIZE_HEAD: u16 = 14;
pub const SIZE_TITLE: u16 = 16;
pub const PAD_TIGHT: [u16; 2] = [4, 12];
pub const PAD_NORMAL: [u16; 2] = [8, 14];
pub const PAD_LOOSE: [u16; 2] = [10, 14];
pub const SPACING_TIGHT: u16 = 2;
pub const SPACING_NORMAL: u16 = 8;
pub const SPACING_LOOSE: u16 = 16;

pub fn c() -> Colors {
    *COLORS.read().unwrap_or_else(|e| e.into_inner())
}

pub fn set(colors: Colors) {
    *COLORS.write().unwrap_or_else(|e| e.into_inner()) = colors;
}

/// `a` moved towards `b` by `t` (0..1).
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: 1.0 }
}

/// Clickable list row (Settings): `bg` normally, a soft step towards the inactive cursor colour while hovered.
/// The selected row passes `hover: false` and keeps its colour.
/// Background of a list row under the mouse, given its normal background.
pub fn hover_bg(bg: Color) -> Color {
    mix(bg, c().cursor_inactive, 0.5)
}

pub fn list_row(bg: Color, hover: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = hover && matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(Background::Color(if hovered { hover_bg(bg) } else { bg })),
            text_color: c().text,
            border: Border::default(),
            ..button::Style::default()
        }
    }
}

pub fn theme() -> Theme {
    Theme::custom(
        "RustCommander".to_string(),
        iced::theme::Palette {
            background: c().bg,
            text: c().text,
            primary: c().accent,
            success: c().success,
            danger: c().error,
        },
    )
}

pub fn filled(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        ..container::Style::default()
    }
}

/// Footer with a muted single-line hint (clipped, not wrapped) on the header background and an optional
/// element on the right (version text, Close button). `color` lets the caller tint notes.
pub fn hint_footer<'a, M: 'a>(
    hint: impl iced::widget::text::IntoFragment<'a>,
    color: Color,
    padding: [u16; 2],
    spacing: u16,
    trailing: Option<iced::Element<'a, M>>,
) -> iced::Element<'a, M> {
    let hint = container(iced::widget::text(hint).size(SIZE_SMALL).color(color).wrapping(iced::widget::text::Wrapping::None))
        .width(iced::Length::Fill)
        .clip(true);
    let mut line = iced::widget::Row::new().push(hint).spacing(spacing).align_y(iced::Alignment::Center);
    if let Some(t) = trailing {
        line = line.push(t);
    }
    container(line).padding(padding).width(iced::Length::Fill).clip(true).style(filled(c().header)).into()
}

/// One key combo as a small boxed key cap with background `bg`.
pub fn key_chip<'a, M: 'a>(label: impl iced::widget::text::IntoFragment<'a>, colors: Colors, bg: Color) -> iced::Element<'a, M> {
    chip(label, colors, bg, [1, 5])
}

fn chip<'a, M: 'a>(label: impl iced::widget::text::IntoFragment<'a>, colors: Colors, bg: Color, padding: [u16; 2]) -> iced::Element<'a, M> {
    container(iced::widget::text(label).size(SIZE_SMALL).color(colors.dir).wrapping(iced::widget::text::Wrapping::None))
        .padding(padding)
        .style(move |_: &Theme| container::Style {
            background: Some(Background::Color(bg)),
            border: Border { color: colors.border, width: 1.0, radius: 3.0.into() },
            ..container::Style::default()
        })
        .into()
}

/// A footer hint: the keys (one chip each) and what they do.
pub type Hint = (&'static [&'static str], &'static str);

/// A footer hint whose keys come from the user's keymap at run time.
#[derive(Clone, Debug, PartialEq)]
pub struct HintOwned {
    pub keys: Vec<String>,
    pub action: String,
}

impl HintOwned {
    pub fn new<K: Into<String>>(keys: impl IntoIterator<Item = K>, action: impl Into<String>) -> Self {
        HintOwned { keys: keys.into_iter().map(Into::into).collect(), action: action.into() }
    }
}

/// What the hint rows need of a hint: its key caps and its action text.
pub trait AsHint {
    fn keys(&self) -> Vec<&str>;
    fn action(&self) -> &str;
}

impl AsHint for Hint {
    fn keys(&self) -> Vec<&str> {
        self.0.to_vec()
    }
    fn action(&self) -> &str {
        self.1
    }
}

impl AsHint for HintOwned {
    fn keys(&self) -> Vec<&str> {
        self.keys.iter().map(String::as_str).collect()
    }
    fn action(&self) -> &str {
        &self.action
    }
}

const HINT_GAP: f32 = 8.0;
/// Left/right padding inside a hint's pill.
const HINT_PAD_X: u16 = 8;

/// Estimated width of a hint pair (chips, gap, action text, gap to the next pair).
fn hint_width(h: &impl AsHint) -> f32 {
    let char_w = SIZE_SMALL as f32 * 0.55;
    let keys = h.keys();
    let chips: f32 = keys.iter().map(|k| k.chars().count() as f32 * char_w + 12.0).sum::<f32>() + 3.0 * keys.len().saturating_sub(1) as f32;
    let gap = if keys.is_empty() { 0.0 } else { 6.0 };
    chips + gap + h.action().chars().count() as f32 * char_w + 2.0 * HINT_PAD_X as f32 + HINT_GAP
}

/// The pairs that fit in `room` px as a row of pills (as wide as its content), and that width.
pub fn key_hints_fit<'a, M: 'a>(hints: &[impl AsHint], room: f32) -> (iced::widget::Row<'a, M>, f32) {
    let colors = c();
    key_hints_fit_on(hints, room, mix(colors.header, colors.border, 0.45), colors.panel, colors.text)
}

/// `key_hints_fit` with its own pill and key-cap backgrounds and action text colour (the main window's status
/// bar uses quiet ones that blend in).
pub fn key_hints_fit_on<'a, M: 'a>(
    hints: &[impl AsHint],
    mut room: f32,
    pill_bg: Color,
    chip_bg: Color,
    action_color: Color,
) -> (iced::widget::Row<'a, M>, f32) {
    let colors = c();
    let mut line = iced::widget::Row::new().spacing(HINT_GAP).align_y(iced::Alignment::Center);
    let mut used = 0.0;
    for h in hints {
        let w = hint_width(h);
        room -= w;
        if room < 0.0 {
            break;
        }
        used += w;
        let mut pair = iced::widget::Row::new().spacing(6).align_y(iced::Alignment::Center);
        let keys = h.keys();
        if !keys.is_empty() {
            let mut chips = iced::widget::Row::new().spacing(3).align_y(iced::Alignment::Center);
            for k in keys {
                chips = chips.push(key_chip::<M>(k.to_owned(), colors, chip_bg));
            }
            pair = pair.push(chips);
        }
        // Each key group and its action sit in one pill, so it's clear what belongs together.
        let action = iced::widget::text(h.action().to_owned()).size(SIZE_SMALL).color(action_color).wrapping(iced::widget::text::Wrapping::None);
        let pair = pair.push(action);
        line = line.push(container(pair).padding([2, HINT_PAD_X]).style(move |_: &Theme| container::Style {
            background: Some(Background::Color(pill_bg)),
            border: Border { radius: 4.0.into(), ..Border::default() },
            ..container::Style::default()
        }));
    }
    (line, (used - HINT_GAP).max(0.0))
}

/// The hint pairs as one row of key caps and muted action text; pairs that don't fit in `room` px are dropped
/// from the end.
pub fn key_hints_row<'a, M: 'a>(hints: &[impl AsHint], room: f32) -> iced::Element<'a, M> {
    container(key_hints_fit(hints, room).0).width(iced::Length::Fill).clip(true).into()
}

/// Gap between pairs in `key_hints_compact`.
const COMPACT_GAP: f32 = 10.0;
/// Most lines `key_hints_compact` takes; pairs past them are dropped.
const COMPACT_LINES: usize = 3;

/// Estimated width of a compact hint pair (small chips, no pill).
fn compact_hint_width(h: &Hint) -> f32 {
    let char_w = SIZE_SMALL as f32 * 0.55;
    let keys: f32 = h.0.iter().map(|k| k.chars().count() as f32 * char_w + 8.0).sum::<f32>()
        + 2.0 * h.0.len().saturating_sub(1) as f32;
    let gap = if h.0.is_empty() { 0.0 } else { 4.0 };
    keys + gap + h.1.chars().count() as f32 * char_w
}

/// Splits pairs of widths `widths` into lines of at most `room` px (pairs `gap` apart; a pair wider than `room`
/// gets a line of its own), at most `max_lines` of them: the pair counts of the lines.
fn wrap_lines(widths: &[f32], room: f32, gap: f32, max_lines: usize) -> Vec<usize> {
    let mut lines: Vec<usize> = Vec::new();
    let mut x = 0.0;
    for &w in widths {
        if let Some(n) = lines.last_mut().filter(|_| x + gap + w <= room) {
            *n += 1;
            x += gap + w;
        } else if lines.len() == max_lines {
            break;
        } else {
            lines.push(1);
            x = w;
        }
    }
    lines
}

/// A "(?) `label`" marker whose tooltip lists `hints`, one pair per line (as the "(?)" in Settings does for
/// long descriptions): for sidebars where the full hint list is too much to show all the time.
pub fn key_hints_tip<'a, M: 'a>(label: &'a str, hints: &[Hint]) -> iced::Element<'a, M> {
    use iced::widget::text::Wrapping;
    let colors = c();
    let mut lines = iced::widget::Column::new().spacing(4);
    for h in hints {
        let mut line = iced::widget::Row::new().spacing(4).align_y(iced::Alignment::Center);
        for k in h.0 {
            line = line.push(chip::<M>(*k, colors, colors.panel, [0, 3]));
        }
        lines = lines.push(line.push(iced::widget::text(h.1).size(SIZE_SMALL).color(colors.text).wrapping(Wrapping::None)));
    }
    let mark = iced::widget::Row::new()
        .push(iced::widget::text("(?)").size(SIZE_SMALL).color(colors.accent).wrapping(Wrapping::None))
        .push(iced::widget::text(label).size(SIZE_SMALL).color(colors.muted).wrapping(Wrapping::None))
        .spacing(4);
    iced::widget::tooltip(mark, container(lines).padding([6, 8]).style(drag_label), iced::widget::tooltip::Position::Top)
        .into()
}

/// The hint pairs for narrow places (sidebars): small chips without pills, wrapped onto up to three lines of
/// `room` px; pairs past them are dropped.
pub fn key_hints_compact<'a, M: 'a>(hints: &[Hint], room: f32) -> iced::Element<'a, M> {
    let colors = c();
    let widths: Vec<f32> = hints.iter().map(compact_hint_width).collect();
    let mut lines = iced::widget::Column::new().spacing(3);
    let mut rest = hints;
    for n in wrap_lines(&widths, room, COMPACT_GAP, COMPACT_LINES) {
        let (now, later) = rest.split_at(n);
        rest = later;
        let mut line = iced::widget::Row::new().spacing(COMPACT_GAP).align_y(iced::Alignment::Center);
        for h in now {
            let action =
                iced::widget::text(h.1).size(SIZE_SMALL).color(colors.text).wrapping(iced::widget::text::Wrapping::None);
            if h.0.is_empty() {
                line = line.push(action);
                continue;
            }
            let mut chips = iced::widget::Row::new().spacing(2).align_y(iced::Alignment::Center);
            for k in h.0 {
                chips = chips.push(chip::<M>(*k, colors, colors.panel, [0, 3]));
            }
            let pair = iced::widget::Row::new().push(chips).push(action).spacing(4);
            line = line.push(pair.align_y(iced::Alignment::Center));
        }
        lines = lines.push(line);
    }
    container(lines).width(iced::Length::Fill).clip(true).into()
}

/// Footer with key hints drawn as key caps plus muted action text. `width` is the window/page width, `reserved`
/// the width taken by `trailing` (Close button, version text); pairs that don't fit are dropped from the end.
pub fn key_hints_footer<'a, M: 'a>(
    hints: &[impl AsHint],
    width: f32,
    reserved: f32,
    padding: [u16; 2],
    spacing: u16,
    trailing: Option<iced::Element<'a, M>>,
) -> iced::Element<'a, M> {
    let room = width - reserved - 2.0 * padding[1] as f32 - spacing as f32;
    let mut row = iced::widget::Row::new().push(key_hints_row(hints, room)).spacing(spacing).align_y(iced::Alignment::Center);
    if let Some(t) = trailing {
        row = row.push(t);
    }
    container(row).padding(padding).width(iced::Length::Fill).clip(true).style(filled(c().header)).into()
}

/// Flat text-only button (column headers).
pub fn flat(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => c().text,
            _ => c().muted,
        },
        border: Border::default(),
        ..button::Style::default()
    }
}

/// Function-key bar buttons.
pub fn fkey(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(match status {
            button::Status::Hovered => c().header,
            button::Status::Pressed => c().cursor,
            _ => c().panel,
        })),
        text_color: c().text,
        border: Border { color: c().border, width: 1.0, radius: 4.0.into() },
        ..button::Style::default()
    }
}

/// Highlighted dialog button (the default action).
pub fn primary(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(match status {
            button::Status::Hovered | button::Status::Pressed => mix(c().accent, Color::WHITE, 0.15),
            _ => c().accent,
        })),
        text_color: c().bg,
        border: Border { color: c().accent, width: 1.0, radius: 4.0.into() },
        ..button::Style::default()
    }
}

/// A dialog button with the keyboard focus: a thick ring (the accent, or the
/// text colour on the accent-filled default button so it stays visible).
pub fn focused(primary_button: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = if primary_button { primary(theme, status) } else { fkey(theme, status) };
        style.border = Border {
            color: if primary_button { c().text } else { c().accent },
            width: 3.0,
            radius: 4.0.into(),
        };
        style
    }
}

/// Whether the software renderer (tiny-skia) draws the window; `main` sets
/// it. iced_tiny_skia 0.13 redraws only the damaged part of the window, so
/// with it box shadows are off (it blends a quad's whole shadow every time,
/// so each redraw inside a dialog darkened the area around it a little
/// more) and dividers show a line while dragged instead of resizing the
/// panels (which redraws the whole window) on every mouse move.
static SOFTWARE: AtomicBool = AtomicBool::new(false);

pub fn set_software(on: bool) {
    SOFTWARE.store(on, Ordering::Relaxed);
}

pub fn software() -> bool {
    SOFTWARE.load(Ordering::Relaxed)
}

pub fn dialog(_theme: &Theme) -> container::Style {
    let shadow = if !software() {
        iced::Shadow { color: Color { a: 0.5, ..Color::BLACK }, offset: iced::Vector::new(0.0, 8.0), blur_radius: 24.0 }
    } else {
        iced::Shadow::default()
    };
    container::Style {
        background: Some(Background::Color(c().header)),
        border: Border { color: c().border, width: 1.0, radius: 8.0.into() },
        shadow,
        ..container::Style::default()
    }
}

pub fn backdrop(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.55, ..Color::BLACK })),
        ..container::Style::default()
    }
}

pub fn input(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(c().panel),
        border: Border {
            color: match status {
                text_input::Status::Focused => c().accent,
                _ => c().border,
            },
            width: 1.0,
            radius: 4.0.into(),
        },
        icon: c().muted,
        placeholder: c().muted,
        value: c().text,
        selection: Color { a: 0.45, ..c().accent },
    }
}

pub fn progress(_theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(c().panel),
        bar: Background::Color(c().accent),
        border: Border { color: c().border, width: 0.0, radius: 3.0.into() },
    }
}

/// Drive bar button; `current` marks the drive the panel is on.
pub fn drive(current: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: Some(Background::Color(match (current, status) {
            (true, _) => c().cursor,
            (false, button::Status::Hovered | button::Status::Pressed) => c().cursor_inactive,
            _ => Color::TRANSPARENT,
        })),
        text_color: if current { c().text } else { c().muted },
        border: Border { color: if current { c().accent } else { Color::TRANSPARENT }, width: 1.0, radius: 4.0.into() },
        ..button::Style::default()
    }
}

/// Terminal sidebar entry: like `drive`, but without the accent border (the
/// current entry has an accent bar beside it instead) and in the normal text colour.
pub fn side_entry(current: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: Some(Background::Color(match (current, status) {
            (true, _) => c().cursor,
            (false, button::Status::Hovered | button::Status::Pressed) => c().cursor_inactive,
            _ => Color::TRANSPARENT,
        })),
        text_color: c().text,
        border: Border { radius: 4.0.into(), ..Border::default() },
        ..button::Style::default()
    }
}

/// Small rounded number badge of a collapsed terminal sidebar entry. With an
/// attention cue its border is that colour and its fill the same colour, faint.
pub fn badge(cue: Option<Color>) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: cue.map(|color| Background::Color(Color { a: 0.25, ..color })),
        text_color: Some(c().text),
        border: Border { color: cue.unwrap_or(c().border), width: 1.0, radius: 4.0.into() },
        ..container::Style::default()
    }
}

/// Floating label that follows the mouse while dragging.
pub fn drag_label(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().cursor)),
        text_color: Some(c().text),
        border: Border { color: c().accent, width: 1.0, radius: 4.0.into() },
        ..container::Style::default()
    }
}

/// Tab in a panel's tab bar.
pub fn tab(current: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: Some(Background::Color(match (current, status) {
            (true, _) => c().header,
            (false, button::Status::Hovered | button::Status::Pressed) => c().zebra,
            _ => c().bg,
        })),
        text_color: if current { c().text } else { c().muted },
        border: Border {
            color: if current { c().border } else { Color::TRANSPARENT },
            width: 1.0,
            radius: iced::border::Radius::default().top(6.0),
        },
        ..button::Style::default()
    }
}

/// Row in the path autocomplete list.
pub fn suggestion(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: Some(Background::Color(match (selected, status) {
            (true, _) => c().cursor,
            (false, button::Status::Hovered | button::Status::Pressed) => c().cursor_inactive,
            _ => Color::TRANSPARENT,
        })),
        text_color: if selected { c().text } else { c().dir },
        border: Border { radius: 4.0.into(), ..Border::default() },
        ..button::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_hints_wrap_and_stop_after_the_last_line() {
        // Pairs fit while the gaps fit too.
        assert_eq!(wrap_lines(&[40.0, 40.0, 40.0], 100.0, 10.0, 3), vec![2, 1]);
        assert_eq!(wrap_lines(&[40.0, 50.0], 100.0, 10.0, 3), vec![2]);
        // A pair wider than the room gets a line of its own.
        assert_eq!(wrap_lines(&[150.0, 40.0], 100.0, 10.0, 3), vec![1, 1]);
        // Pairs past the last line are dropped.
        assert_eq!(wrap_lines(&[60.0; 5], 100.0, 10.0, 3), vec![1, 1, 1]);
        assert!(wrap_lines(&[], 100.0, 10.0, 3).is_empty());
    }

    #[test]
    fn hsv_round_trips_every_preset_colour() {
        for p in PRESETS {
            for name in NAMES {
                let hex = to_hex(p.colors.get(name).unwrap());
                let (h, s, v) = to_hsv(parse_hex(&hex).unwrap());
                assert_eq!(to_hex(from_hsv(h, s, v)), hex, "{} {name}", p.id);
            }
        }
    }

    #[test]
    fn hex_round_trips_and_overrides_apply() {
        assert_eq!(parse_hex("#5b9cf5").map(to_hex).as_deref(), Some("#5b9cf5"));
        assert_eq!(parse_hex("ABCDEF").map(to_hex).as_deref(), Some("#abcdef"));
        for bad in ["", "#12345", "#1234567", "#gg0000"] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
        for name in NAMES {
            assert!(DARK.get(name).is_some(), "{name}");
        }
        let set = |k: &str, v: &str| DARK.with(&std::collections::BTreeMap::from([(k.to_owned(), v.to_owned())]));
        assert_eq!(set("Accent", "#ff0000").unwrap().accent, rgb(0xff0000));
        assert!(set("nope", "#ff0000").unwrap_err().contains("nope"));
        assert!(set("accent", "red").unwrap_err().contains("red"));
    }

    #[test]
    fn presets_are_complete_and_distinct() {
        assert_eq!(PURPOSES.len(), NAMES.len());
        assert_eq!(preset("").unwrap().colors, DARK);
        assert_eq!(preset("Light").unwrap().colors, LIGHT);
        assert!(preset("nope").is_none());
        for (i, p) in PRESETS.iter().enumerate() {
            assert_eq!(preset(p.id).unwrap().id, p.id);
            // Every colour survives the hex round trip used by config.toml.
            for name in NAMES {
                let hex = to_hex(p.colors.get(name).unwrap());
                assert_eq!(parse_hex(&hex).map(to_hex), Some(hex), "{} {name}", p.id);
            }
            // Text stands out from the list background.
            let lum = |c: Color| 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
            assert!((lum(p.colors.text) - lum(p.colors.panel)).abs() > 0.5, "{}", p.id);
            assert!(PRESETS[..i].iter().all(|q| q.colors != p.colors));
        }
    }
}
