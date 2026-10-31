// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod icon;
mod keymap;
mod session;
mod theme;
mod watch;

fn main() -> iced::Result {
    let session = session::load();
    let size = session.as_ref().map_or(iced::Size::new(1280.0, 800.0), |s| {
        iced::Size::new(s.width.max(400.0), s.height.max(300.0))
    });
    iced::application("RustCommander", app::App::update, app::App::view)
        .theme(app::App::theme)
        .subscription(app::App::subscription)
        .window(iced::window::Settings {
            size,
            icon: icon::window_icon(),
            // Closing goes through the app so it can save the session.
            exit_on_close_request: false,
            ..Default::default()
        })
        .antialiasing(true)
        .run_with(move || app::App::new(session))
}
