//! Notification sequences in a terminal's output that alacritty_terminal
//! drops without telling us (it only reports BEL). `term.rs` runs the PTY's
//! output through `OscScan` on the I/O thread, before alacritty parses it,
//! and turns each `Cue` into a bell (the terminal wants attention).
//!
//! Recognised (BEL- or ST-terminated OSC):
//! - `OSC 777 ; notify ; title ; body`: rxvt/Ghostty/WezTerm notification
//!   (Pi's `notify.ts` example extension sends this).
//! - `OSC 9 ; text`: iTerm2 notification. `OSC 9 ; <number> ...` are ConEmu
//!   commands instead (`9;9` is Windows Terminal's working folder) and are
//!   not notifications.
//! - `OSC 99 ; meta ; payload`: Kitty notification, once its last chunk
//!   arrives (not `d=0`); queries and closes (`p=?`, `p=close`...) don't count.
//!   A query (`p=?`) is answered (`Cue::Reply`): title and body supported.
//!   opentui (opencode's TUI) only notifies once it found a protocol, and for
//!   us that is this answer (it doesn't know our terminal by name).
//! - `OSC 9 ; 4 ; state`: ConEmu/Windows Terminal progress. State 0 (cleared)
//!   after an active one (1-4) means the work ended: `Cue::ProgressDone`.
//!   (Pi sends it when `terminal.showTerminalProgress` is on.)

/// Bytes of an OSC kept: enough to tell what it is (the rest is skipped).
const KEEP: usize = 64;

/// What a sequence in the output asks of the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cue {
    /// Attention: a notification sequence (OSC 777, 9, 99).
    Notify,
    /// Attention: a progress indicator (OSC 9;4) was cleared after being active.
    ProgressDone,
    /// Write this answer to the program (an OSC 99 capability query).
    Reply(String),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    /// After ESC.
    Esc,
    /// In an OSC (after ESC ]).
    Osc,
    /// ESC inside an OSC: `\` ends it (ST), anything else aborts it.
    OscEsc,
}

/// Scans a byte stream (split anywhere) for the sequences above.
#[derive(Debug, Default)]
pub(super) struct OscScan {
    state: State,
    /// The start of the current OSC's payload.
    buf: Vec<u8>,
    /// A progress indicator is shown (OSC 9;4 with a non-zero state).
    progress: bool,
}

impl OscScan {
    /// Scans the next `bytes`; calls `cue` for each sequence that wants attention.
    pub(super) fn feed(&mut self, bytes: &[u8], mut cue: impl FnMut(Cue)) {
        for &b in bytes {
            self.state = match self.state {
                State::Ground => {
                    if b == 0x1b {
                        State::Esc
                    } else {
                        State::Ground
                    }
                }
                State::Esc => self.after_esc(b),
                State::Osc => match b {
                    0x07 => {
                        self.finish(&mut cue);
                        State::Ground
                    }
                    0x1b => State::OscEsc,
                    // CAN, SUB: cancel the sequence.
                    0x18 | 0x1a => State::Ground,
                    _ => {
                        if self.buf.len() < KEEP {
                            self.buf.push(b);
                        }
                        State::Osc
                    }
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.finish(&mut cue);
                        State::Ground
                    } else {
                        // An aborted OSC; the ESC may start another sequence.
                        self.after_esc(b)
                    }
                }
            };
        }
    }

    fn after_esc(&mut self, b: u8) -> State {
        match b {
            b']' => {
                self.buf.clear();
                State::Osc
            }
            0x1b => State::Esc,
            _ => State::Ground,
        }
    }

    /// A complete OSC with the payload in `buf`.
    fn finish(&mut self, cue: &mut impl FnMut(Cue)) {
        match classify(&self.buf) {
            Some(Osc::Notify) => cue(Cue::Notify),
            Some(Osc::Progress(0)) => {
                if std::mem::take(&mut self.progress) {
                    cue(Cue::ProgressDone);
                }
            }
            Some(Osc::Progress(_)) => self.progress = true,
            Some(Osc::KittyQuery(id)) => {
                // Kitty's answer: the query's id, then what we support (keys
                // `p` payload types, `o` when shown, `u` urgency levels).
                let id = String::from_utf8_lossy(id);
                let id = if id.is_empty() { String::new() } else { format!("i={id}:") };
                cue(Cue::Reply(format!("\x1b]99;{id}p=?;p=title,body:o=always:u=0,1,2\x1b\\")));
            }
            None => {}
        }
    }
}

/// What an OSC payload is, of the ones we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Osc<'a> {
    Notify,
    /// OSC 9;4 with this state.
    Progress(u32),
    /// OSC 99 `p=?` with this id (may be empty).
    KittyQuery(&'a [u8]),
}

fn classify(payload: &[u8]) -> Option<Osc<'_>> {
    let (code, rest) = match payload.iter().position(|&b| b == b';') {
        Some(i) => (&payload[..i], &payload[i + 1..]),
        None => (payload, &[][..]),
    };
    match code {
        b"777" => rest.starts_with(b"notify;").then_some(Osc::Notify),
        b"99" => kitty_query(rest).map(Osc::KittyQuery).or_else(|| kitty_notify(rest).then_some(Osc::Notify)),
        b"9" => {
            if let Some(progress) = rest.strip_prefix(b"4;") {
                let digits = progress.iter().take_while(|b| b.is_ascii_digit()).count();
                let state = std::str::from_utf8(&progress[..digits]).ok()?.parse().ok()?;
                return Some(Osc::Progress(state));
            }
            if rest == b"4" {
                return Some(Osc::Progress(0));
            }
            // ConEmu commands: `9;<number>` or `9;<number>;...`.
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            let command = digits > 0 && matches!(rest.get(digits), None | Some(b';'));
            (!rest.is_empty() && !command).then_some(Osc::Notify)
        }
        _ => None,
    }
}

/// The id of a Kitty `OSC 99` capability query (`p=?` in its metadata),
/// empty when it has none. Only id characters Kitty allows are kept, so the
/// answer can't carry other bytes back to the program.
fn kitty_query(rest: &[u8]) -> Option<&[u8]> {
    let meta = rest.split(|&b| b == b';').next().unwrap_or_default();
    let mut id: &[u8] = &[];
    let mut query = false;
    for kv in meta.split(|&b| b == b':') {
        if kv == b"p=?" {
            query = true;
        } else if let Some(v) = kv.strip_prefix(b"i=") {
            id = v;
        }
    }
    let valid = id.iter().all(|&b| b.is_ascii_alphanumeric() || b"-_+.".contains(&b));
    (query && valid).then_some(id)
}

/// Whether a Kitty `OSC 99` (`meta;payload`, `meta` as `key=value:...`) shows a
/// notification now: its last chunk (not `d=0`) of a title or body.
fn kitty_notify(rest: &[u8]) -> bool {
    let meta = rest.split(|&b| b == b';').next().unwrap_or_default();
    let mut done = true;
    let mut text = true;
    for kv in meta.split(|&b| b == b':') {
        match kv {
            b"d=0" => done = false,
            _ if kv.starts_with(b"p=") => text = matches!(&kv[2..], b"title" | b"body"),
            _ => {}
        }
    }
    done && text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cues(chunks: &[&[u8]]) -> Vec<Cue> {
        let mut scan = OscScan::default();
        let mut out = Vec::new();
        for chunk in chunks {
            scan.feed(chunk, |c| out.push(c));
        }
        out
    }

    #[test]
    fn osc_777_notify_with_bel_or_st() {
        assert_eq!(cues(&[b"hi\x1b]777;notify;Pi;Ready for input\x07"]), vec![Cue::Notify]);
        assert_eq!(cues(&[b"\x1b]777;notify;Pi;Ready\x1b\\"]), vec![Cue::Notify]);
        // Other 777 commands aren't.
        assert_eq!(cues(&[b"\x1b]777;preexec\x07"]), vec![]);
    }

    #[test]
    fn split_anywhere() {
        let seq: &[u8] = b"text\x1b]777;notify;Pi;Ready\x1b\\more";
        for i in 0..seq.len() {
            assert_eq!(cues(&[&seq[..i], &seq[i..]]), vec![Cue::Notify], "split at {i}");
        }
    }

    #[test]
    fn osc_9_notification_but_not_conemu_commands() {
        assert_eq!(cues(&[b"\x1b]9;Build finished\x07"]), vec![Cue::Notify]);
        // Windows Terminal's working folder, ConEmu sleep / message box.
        assert_eq!(cues(&[b"\x1b]9;9;\"C:\\Users\"\x1b\\"]), vec![]);
        assert_eq!(cues(&[b"\x1b]9;1;500\x07"]), vec![]);
        assert_eq!(cues(&[b"\x1b]9;12\x07"]), vec![]);
        assert_eq!(cues(&[b"\x1b]9\x07", b"\x1b]9;\x07"]), vec![]);
    }

    #[test]
    fn progress_cleared_after_active() {
        // Pi: indeterminate every second while working, then cleared.
        let working: &[u8] = b"\x1b]9;4;3\x07";
        assert_eq!(cues(&[working, b"output", working, working, b"\x1b]9;4;0\x07"]), vec![Cue::ProgressDone]);
        // A clear with nothing shown (startup, exit) doesn't count, nor a second one.
        assert_eq!(cues(&[b"\x1b]9;4;0\x07"]), vec![]);
        assert_eq!(cues(&[b"\x1b]9;4;1;50\x07\x1b]9;4;0;0\x07\x1b]9;4;0\x07"]), vec![Cue::ProgressDone]);
        // `9;4` alone clears too.
        assert_eq!(cues(&[b"\x1b]9;4;2;100\x07\x1b]9;4\x07"]), vec![Cue::ProgressDone]);
    }

    #[test]
    fn kitty_last_chunk_only() {
        assert_eq!(cues(&[b"\x1b]99;i=1:d=0;Pi\x1b\\"]), vec![]);
        assert_eq!(cues(&[b"\x1b]99;i=1:d=0;Pi\x1b\\\x1b]99;i=1:p=body;Ready\x1b\\"]), vec![Cue::Notify]);
        assert_eq!(cues(&[b"\x1b]99;;Hello\x1b\\"]), vec![Cue::Notify]);
        assert_eq!(cues(&[b"\x1b]99;i=1:p=close;\x1b\\", b"\x1b]99;i=1:p=alive;\x1b\\"]), vec![]);
    }

    #[test]
    fn kitty_query_is_answered() {
        // opentui's probe at startup.
        let reply = "\x1b]99;i=opentui-notifications:p=?;p=title,body:o=always:u=0,1,2\x1b\\";
        assert_eq!(cues(&[b"\x1b]99;i=opentui-notifications:p=?;\x1b\\"]), vec![Cue::Reply(reply.into())]);
        assert_eq!(
            cues(&[b"\x1b]99;p=?;\x07"]),
            vec![Cue::Reply("\x1b]99;p=?;p=title,body:o=always:u=0,1,2\x1b\\".into())]
        );
        // An id that would put other bytes into the answer isn't answered.
        assert_eq!(cues(&[b"\x1b]99;i=a\x1b[2J:p=?;\x07"]), vec![]);
        assert_eq!(cues(&[b"\x1b]99;i=a b:p=?;\x07"]), vec![]);
    }

    #[test]
    fn titles_and_aborted_sequences_are_not_cues() {
        assert_eq!(cues(&[b"\x1b]0;777;notify\x07\x1b]2;9;hi\x07"]), vec![]);
        // Cancelled by CAN; an ESC that aborts one OSC can start the next.
        assert_eq!(cues(&[b"\x1b]9;hi\x18\x07"]), vec![]);
        assert_eq!(cues(&[b"\x1b]9;hi\x1b]9;there\x07"]), vec![Cue::Notify]);
        // A long notification is still one.
        let long = [b"\x1b]9;".as_slice(), &[b'x'; 500], b"\x07"].concat();
        assert_eq!(cues(&[&long]), vec![Cue::Notify]);
    }
}
