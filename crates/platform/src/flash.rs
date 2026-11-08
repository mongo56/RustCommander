//! Flashing the window's taskbar button to ask for attention.

/// Flashes the taskbar button of `window` (an HWND) until the window comes to
/// the foreground. No sound, the window itself isn't flashed.
#[cfg(windows)]
pub fn flash_taskbar(window: isize) {
    use std::ffi::c_void;
    use std::mem::size_of;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{FlashWindowEx, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY};

    let info = FLASHWINFO {
        cbSize: size_of::<FLASHWINFO>() as u32,
        hwnd: HWND(window as *mut c_void),
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: u32::MAX,
        dwTimeout: 0,
    };
    unsafe {
        let _ = FlashWindowEx(&info);
    }
}

#[cfg(not(windows))]
pub fn flash_taskbar(_window: isize) {}
