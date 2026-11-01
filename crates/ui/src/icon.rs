//! The window icon (see `icon_art` for the artwork).

use iced::window::icon::{self, Icon};

const SIZE: u32 = 64;

pub fn window_icon() -> Option<Icon> {
    icon::from_rgba(crate::icon_art::rgba(SIZE), SIZE, SIZE).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn icon_is_valid() {
        let data = crate::icon_art::rgba(super::SIZE);
        assert_eq!(data.len(), (super::SIZE * super::SIZE * 4) as usize);
        assert!(super::window_icon().is_some());
        // Corners are transparent, centre is opaque.
        assert_eq!(data[3], 0);
        let mid = ((super::SIZE / 2) * super::SIZE + super::SIZE / 2) as usize * 4;
        assert_eq!(data[mid + 3], 255);
    }
}
