// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod keymap;
mod theme;

fn main() -> iced::Result {
    iced::application("RustCommander", app::App::update, app::App::view)
        .theme(app::App::theme)
        .subscription(app::App::subscription)
        .window_size(iced::Size::new(1280.0, 800.0))
        .antialiasing(true)
        .run_with(app::App::new)
}
