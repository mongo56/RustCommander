//! Colours and widget styles. Everything visual lives here; the colours can
//! be overridden in `config.toml` (`[colors]`) and change at runtime.

use iced::widget::{button, container, progress_bar, text_input};
use iced::{Background, Border, Color, Theme};
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
pub fn c() -> Colors {
    *COLORS.read().unwrap_or_else(|e| e.into_inner())
}

pub fn set(colors: Colors) {
    *COLORS.write().unwrap_or_else(|e| e.into_inner()) = colors;
}

/// `a` moved towards `b` by `t` (0..1).
fn mix(a: Color, b: Color, t: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: 1.0 }
}

pub fn theme() -> Theme {
    Theme::custom(
        "RustCommander Dark".to_string(),
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

pub fn dialog(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().header)),
        border: Border { color: c().border, width: 1.0, radius: 8.0.into() },
        shadow: iced::Shadow {
            color: Color { a: 0.5, ..Color::BLACK },
            offset: iced::Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
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
        selection: c().cursor,
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
}
