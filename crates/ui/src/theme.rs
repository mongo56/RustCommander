//! Colours and widget styles. Everything visual lives here so a light theme
//! (or user themes) can be added later without touching the app logic.

use iced::widget::{button, container, progress_bar, text_input};
use iced::{Background, Border, Color, Theme};

const fn rgb(hex: u32) -> Color {
    Color {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

pub const BG: Color = rgb(0x14161a);
pub const PANEL: Color = rgb(0x191c21);
pub const ZEBRA: Color = rgb(0x1c1f25);
pub const HEADER: Color = rgb(0x1f232a);
pub const BORDER: Color = rgb(0x2a2f38);
pub const TEXT: Color = rgb(0xd5d9e0);
pub const MUTED: Color = rgb(0x7b8494);
pub const DIR: Color = rgb(0x8cb8ff);
pub const ACCENT: Color = rgb(0x5b9cf5);
pub const SELECTED: Color = rgb(0xf2b75b);
pub const ERROR: Color = rgb(0xf27474);
pub const CURSOR: Color = rgb(0x263a58);
pub const CURSOR_INACTIVE: Color = rgb(0x252a33);

pub fn theme() -> Theme {
    Theme::custom(
        "RustCommander Dark".to_string(),
        iced::theme::Palette {
            background: BG,
            text: TEXT,
            primary: ACCENT,
            success: rgb(0x6cc58a),
            danger: ERROR,
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
            button::Status::Hovered | button::Status::Pressed => TEXT,
            _ => MUTED,
        },
        border: Border::default(),
        ..button::Style::default()
    }
}

/// Function-key bar buttons.
pub fn fkey(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(match status {
            button::Status::Hovered => HEADER,
            button::Status::Pressed => CURSOR,
            _ => PANEL,
        })),
        text_color: TEXT,
        border: Border { color: BORDER, width: 1.0, radius: 4.0.into() },
        ..button::Style::default()
    }
}

/// Highlighted dialog button (the default action).
pub fn primary(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(match status {
            button::Status::Hovered | button::Status::Pressed => rgb(0x6eaaff),
            _ => ACCENT,
        })),
        text_color: rgb(0x0d1117),
        border: Border { color: ACCENT, width: 1.0, radius: 4.0.into() },
        ..button::Style::default()
    }
}

pub fn dialog(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(HEADER)),
        border: Border { color: BORDER, width: 1.0, radius: 8.0.into() },
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
        background: Background::Color(PANEL),
        border: Border {
            color: match status {
                text_input::Status::Focused => ACCENT,
                _ => BORDER,
            },
            width: 1.0,
            radius: 4.0.into(),
        },
        icon: MUTED,
        placeholder: MUTED,
        value: TEXT,
        selection: CURSOR,
    }
}

pub fn progress(_theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(PANEL),
        bar: Background::Color(ACCENT),
        border: Border { color: BORDER, width: 0.0, radius: 3.0.into() },
    }
}
