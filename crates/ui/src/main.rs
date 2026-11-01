// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod editor;
mod terminal;
mod icon;
mod icon_art;
mod keymap;
mod scrollbar;
mod hotlist;
mod select_range;
mod session;
mod sites;
mod theme;
mod watch;

/// "0.1.0" — the version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Prints the version for `--version` / `-V`. The release exe has no console
/// (windows_subsystem), so attach to the parent's and write to CONOUT$.
fn print_version() {
    let line = format!("RustCommander {VERSION}");
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(process_id: u32) -> i32;
            fn GetStdHandle(which: u32) -> isize;
        }
        use std::io::Write;
        // SAFETY: plain Win32 calls; STD_OUTPUT_HANDLE = -11, ATTACH_PARENT_PROCESS = -1.
        let h = unsafe { GetStdHandle(-11i32 as u32) };
        if h != 0 && h != -1 {
            // Redirected or piped: stdout works as usual.
            println!("{line}");
            return;
        }
        let attached = unsafe { AttachConsole(u32::MAX) } != 0;
        if attached {
            if let Ok(mut con) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
                let _ = writeln!(con, "{line}");
                return;
            }
        }
    }
    println!("{line}");
}

fn main() -> iced::Result {
    if std::env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        print_version();
        return Ok(());
    }
    let session = session::load();
    if let Some(dir) = config::dir() {
        // SFTP host keys are trusted in known_hosts there.
        rc_core::remote::set_settings_dir(dir);
    }
    if let Err(e) = config::create_if_missing(session.as_ref().map_or("", |s| &s.editor)) {
        eprintln!("could not create config.toml: {e}");
    }
    let size = session.as_ref().map_or(iced::Size::new(1280.0, 800.0), |s| {
        iced::Size::new(s.width.max(400.0), s.height.max(300.0))
    });
    iced::application(concat!("RustCommander ", env!("CARGO_PKG_VERSION")), app::App::update, app::App::view)
        .theme(app::App::theme)
        .subscription(app::App::subscription)
        .window(iced::window::Settings {
            size,
            icon: icon::window_icon(),
            visible: !app::hidden_until_placed(&session),
            // Closing goes through the app so it can save the session.
            exit_on_close_request: false,
            ..Default::default()
        })
        .antialiasing(true)
        .run_with(move || app::App::new(session))
}
