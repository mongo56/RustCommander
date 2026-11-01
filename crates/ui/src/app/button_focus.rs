//! Keyboard focus between the buttons of a dialog (Left/Right, Tab/Shift+Tab).

use rc_core::ops::Resolution;

/// The buttons of the overwrite question, in focus order.
pub const CONFLICT_ANSWERS: [(&str, Resolution); 6] = [
    ("Overwrite", Resolution::Overwrite),
    ("Overwrite all", Resolution::OverwriteAll),
    ("Overwrite older", Resolution::OverwriteOlder),
    ("Skip", Resolution::Skip),
    ("Skip all", Resolution::SkipAll),
    ("Cancel job", Resolution::Cancel),
];

/// Index of the button that gets the focus after one step among `count`
/// buttons, wrapping around. Without a focused button, forward starts at
/// the first and back at the last.
pub fn step(current: Option<usize>, count: usize, back: bool) -> usize {
    match (current, back) {
        (None, false) => 0,
        (None, true) => count.saturating_sub(1),
        (Some(i), false) => (i + 1) % count.max(1),
        (Some(i), true) => (i + count.max(1) - 1) % count.max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_wrap_around() {
        assert_eq!(step(Some(1), 2, true), 0);
        assert_eq!(step(Some(0), 2, true), 1);
        assert_eq!(step(Some(1), 2, false), 0);
        assert_eq!(step(Some(0), 2, false), 1);
        assert_eq!(step(Some(5), 6, false), 0);
        assert_eq!(step(Some(0), 6, true), 5);
    }

    #[test]
    fn unfocused_starts_at_the_ends() {
        assert_eq!(step(None, 6, false), 0);
        assert_eq!(step(None, 6, true), 5);
    }

    #[test]
    fn delete_dialog_left_is_cancel() {
        // Default focus is the last button (OK); Left moves to Cancel (0).
        assert_eq!(step(Some(1), 2, true), 0);
    }

    #[test]
    fn single_button_stays() {
        assert_eq!(step(Some(0), 1, false), 0);
        assert_eq!(step(Some(0), 1, true), 0);
    }
}
