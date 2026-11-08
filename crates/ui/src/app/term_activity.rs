//! When a terminal the user isn't looking at wants attention: a command in
//! it finished, or the program in it waits for input. Pure logic, fed with
//! explicit times (`Instant`s) by `term.rs`; nothing here runs per output
//! burst, only on the poll loop's `Info`, OSC title changes, bells and the
//! delayed idle check.
//!
//! Signals:
//! - Finished: the polled foreground program goes from some program back to
//!   none (the shell's prompt), after it ran at least `MIN_RUN` (a quick
//!   `dir` doesn't count). The exit code isn't known from polling.
//! - Waiting, bell: any BEL from the terminal (`TermEvent::Bell`). Claude
//!   Code rings it when it needs input only if its notification channel is
//!   the terminal bell (`/config` -> notifications, `terminal_bell`).
//! - Waiting, title: Claude Code animates a spinner in its OSC title while it
//!   works and leaves the title steady when idle. `BUSY_CHANGES` title
//!   changes within `BUSY_WINDOW` make the terminal busy; a busy terminal
//!   whose title then stays unchanged for `IDLE_AFTER` went idle: Waiting.
//!   This doesn't depend on the spinner's glyphs. If it misbehaves, the bell
//!   setting above is the fallback; `RC_TERM_DEBUG=1` logs titles and bells.

use std::time::{Duration, Instant};

/// A program has to run this long for its end to count as Finished.
pub(super) const MIN_RUN: Duration = Duration::from_secs(2);
/// Title changes within `BUSY_WINDOW` that make a terminal busy.
const BUSY_CHANGES: usize = 3;
const BUSY_WINDOW: Duration = Duration::from_secs(2);
/// A busy terminal whose title stays the same this long went idle.
pub(super) const IDLE_AFTER: Duration = Duration::from_millis(1500);

/// Why a terminal's sidebar entry is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attention {
    /// A command that ran a while finished (back at the prompt).
    Finished,
    /// The program in it waits for input (bell, or its title went steady).
    Waiting,
}

impl Attention {
    /// The sidebar entry's tooltip line.
    pub(super) fn tip(self) -> &'static str {
        match self {
            Attention::Finished => "Finished",
            Attention::Waiting => "Waiting for input",
        }
    }
}

/// What the idle check found (`Activity::check`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Check {
    /// Not busy (any more): no further check.
    Stop,
    /// Still busy: check again after this long.
    Again(Duration),
    /// Busy -> idle: the program waits.
    Idle,
}

/// One terminal's activity, for `Attention`.
#[derive(Debug, Default)]
pub(super) struct Activity {
    /// When the foreground program started (it was none before).
    program_since: Option<Instant>,
    /// The last OSC title (to tell a change from the same title again).
    title: String,
    /// Times of the last `BUSY_CHANGES` title changes (a ring; `next` is
    /// the oldest slot).
    changes: [Option<Instant>; BUSY_CHANGES],
    next: usize,
    /// The title is animating.
    busy: bool,
    /// An idle check is scheduled (at most one at a time).
    checking: bool,
}

impl Activity {
    /// The polled foreground program is `running` (some program or none).
    /// Returns Finished when one that ran at least `MIN_RUN` ended.
    pub(super) fn program(&mut self, running: bool, now: Instant) -> Option<Attention> {
        match (self.program_since, running) {
            (None, true) => self.program_since = Some(now),
            (Some(since), false) => {
                self.program_since = None;
                // Whatever animated the title has ended with it.
                self.busy = false;
                if now.saturating_duration_since(since) >= MIN_RUN {
                    return Some(Attention::Finished);
                }
            }
            _ => {}
        }
        None
    }

    /// The OSC title is now `title`. Returns the delay of an idle check to
    /// schedule (`check`), when one is needed and none is pending.
    pub(super) fn title(&mut self, title: &str, now: Instant) -> Option<Duration> {
        if title == self.title {
            return None;
        }
        title.clone_into(&mut self.title);
        self.changes[self.next] = Some(now);
        self.next = (self.next + 1) % BUSY_CHANGES;
        // The oldest of the last `BUSY_CHANGES` changes.
        if self.changes[self.next].is_some_and(|t| now.saturating_duration_since(t) <= BUSY_WINDOW) {
            self.busy = true;
        }
        if self.busy && !self.checking {
            self.checking = true;
            return Some(IDLE_AFTER);
        }
        None
    }

    /// The scheduled idle check, at `now`.
    pub(super) fn check(&mut self, now: Instant) -> Check {
        self.checking = false;
        if !self.busy {
            return Check::Stop;
        }
        let last = self.changes[(self.next + BUSY_CHANGES - 1) % BUSY_CHANGES];
        let quiet = last.map_or(IDLE_AFTER, |t| now.saturating_duration_since(t));
        if quiet >= IDLE_AFTER {
            self.busy = false;
            self.changes = [None; BUSY_CHANGES];
            return Check::Idle;
        }
        self.checking = true;
        Check::Again(IDLE_AFTER - quiet)
    }
}

/// Whether an `Attention` for a terminal is shown: not while the user looks
/// at it (it is shown and the window has the focus).
pub(super) fn wants_cue(shown: bool, window_focused: bool) -> bool {
    !(shown && window_focused)
}

/// Sets a terminal's `Attention` to `a`. Returns whether the taskbar button
/// should flash: `enabled`, the window isn't focused, and the terminal had no
/// attention yet (so once per terminal until it is looked at).
pub(super) fn raise(slot: &mut Option<Attention>, a: Attention, window_focused: bool, enabled: bool) -> bool {
    let first = slot.replace(a).is_none();
    first && enabled && !window_focused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_once_until_looked_at() {
        let mut slot = None;
        assert!(raise(&mut slot, Attention::Waiting, false, true));
        assert!(!raise(&mut slot, Attention::Waiting, false, true));
        assert!(!raise(&mut slot, Attention::Finished, false, true));
        assert_eq!(slot, Some(Attention::Finished));
        slot = None;
        assert!(raise(&mut slot, Attention::Finished, false, true));
    }

    #[test]
    fn no_flash_when_focused_or_off() {
        let mut slot = None;
        assert!(!raise(&mut slot, Attention::Waiting, true, true));
        assert_eq!(slot, Some(Attention::Waiting));
        let mut slot = None;
        assert!(!raise(&mut slot, Attention::Waiting, false, false));
        assert_eq!(slot, Some(Attention::Waiting));
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn long_program_finishes_short_one_does_not() {
        let t0 = Instant::now();
        let mut a = Activity::default();
        assert_eq!(a.program(true, t0), None);
        // Polled again while it runs: still the start time of t0.
        assert_eq!(a.program(true, t0 + ms(1000)), None);
        assert_eq!(a.program(false, t0 + ms(2500)), Some(Attention::Finished));
        // A quick one (dir) doesn't count.
        assert_eq!(a.program(true, t0 + ms(3000)), None);
        assert_eq!(a.program(false, t0 + ms(4000)), None);
        // Nothing running, nothing ends.
        assert_eq!(a.program(false, t0 + ms(9000)), None);
    }

    /// Titles every 100 ms from `start` for `n` frames; the check delays
    /// `title` asked for.
    fn spin(a: &mut Activity, start: Instant, n: u64) -> Vec<Duration> {
        (0..n).filter_map(|i| a.title(&format!("{} working", i % 10), start + ms(i * 100))).collect()
    }

    #[test]
    fn spinner_then_steady_title_is_waiting() {
        let t0 = Instant::now();
        let mut a = Activity::default();
        // Two changes aren't busy yet; the third within 2 s is, and asks for one check.
        let asked = spin(&mut a, t0, 20);
        assert_eq!(asked, vec![IDLE_AFTER]);
        // Checked while still animating: again, for the rest of the quiet time.
        assert_eq!(a.check(t0 + ms(1500)), Check::Again(IDLE_AFTER));
        // Last change at 1900 ms; 1.5 s quiet after it is idle.
        assert_eq!(a.check(t0 + ms(2000)), Check::Again(ms(1400)));
        assert_eq!(a.check(t0 + ms(3400)), Check::Idle);
        // Once: not busy any more.
        assert_eq!(a.check(t0 + ms(5000)), Check::Stop);
    }

    #[test]
    fn same_title_again_and_slow_changes_are_not_busy() {
        let t0 = Instant::now();
        let mut a = Activity::default();
        for i in 0..20 {
            assert_eq!(a.title("steady", t0 + ms(i * 100)), None);
        }
        // A title change every 1.5 s (a shell showing the command it runs).
        for i in 0..6 {
            assert_eq!(a.title(&format!("cmd {i}"), t0 + ms(5000 + i * 1500)), None);
        }
        assert_eq!(a.check(t0 + ms(20_000)), Check::Stop);
    }

    #[test]
    fn busy_again_after_idle() {
        let t0 = Instant::now();
        let mut a = Activity::default();
        spin(&mut a, t0, 5);
        assert_eq!(a.check(t0 + ms(400) + IDLE_AFTER), Check::Idle);
        // A new turn: it needs three fresh changes, then asks for a check again.
        let t1 = t0 + ms(10_000);
        assert_eq!(spin(&mut a, t1, 3), vec![IDLE_AFTER]);
        assert_eq!(a.check(t1 + ms(200) + IDLE_AFTER), Check::Idle);
    }

    #[test]
    fn program_ending_stops_busy() {
        let t0 = Instant::now();
        let mut a = Activity::default();
        a.program(true, t0);
        spin(&mut a, t0, 5);
        assert_eq!(a.program(false, t0 + ms(3000)), Some(Attention::Finished));
        assert_eq!(a.check(t0 + ms(5000)), Check::Stop);
    }

    #[test]
    fn cue_only_when_not_looked_at() {
        assert!(!wants_cue(true, true));
        assert!(wants_cue(true, false));
        assert!(wants_cue(false, true));
        assert!(wants_cue(false, false));
    }
}
