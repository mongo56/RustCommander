//! Built-in terminal (F9): a shell in a ConPTY, drawn over the whole window
//! like the other full-window tools. `alacritty_terminal` parses the output
//! into a grid on its own I/O thread (`EventLoop`); we draw that grid
//! (`Grid` widget) and turn key presses into the bytes a terminal sends.
//!
//! Terminals live in `App::terms`, apart from the screen: `Screen::Term(id)`
//! only says which one has the keyboard, so a hidden terminal keeps running.
//! The sidebar's entries are `App::term_tabs`: each shows one or more
//! terminals in split panes (`pane_grid`); `Screen::Term(id)` shows the
//! entry holding `id` with `id` focused. Each
//! has its own event subscription keyed by its id (`TermSession::events`),
//! running whether or not it is shown. The one key the terminal keeps for
//! itself is the `ToggleTerminal` command (Ctrl+Shift+F9): back to the
//! panels. When the shell exits, its terminal is removed.

use super::*;
use super::term_activity::{wants_cue, Activity, Attention, Check};
use super::term_glyphs::{self, Piece};
use super::term_links::{Link, LinkTarget, Scan};
use crate::theme::{filled, HintOwned};
use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column as GridColumn, Direction, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors as TermColors;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Pty, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, CursorStyle, NamedColor, Rgb};
use iced::advanced::layout::{self, Layout};
use iced::advanced::text::Renderer as _;
use iced::advanced::widget::{Tree, Widget};
use iced::advanced::{renderer, Clipboard, Renderer as _, Shell as IcedShell};
use iced::alignment;
use iced::font::{Style as FontStyle, Weight};
use iced::widget::text::{LineHeight, Shaping, Wrapping};
use iced::widget::{button, column, container, mouse_area, pane_grid, row, text, tooltip, Column, Row, Space};
use iced::{Background, Color, Element, Font, Length, Pixels, Rectangle};
use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;
use std::thread::JoinHandle;

/// Space around the grid.
const PAD: f32 = 6.0;
/// Height of the line under the grid (how to switch, how to get back).
const FOOTER_H: f32 = 26.0;
/// Width of the sidebar (Files, the terminals, +) incl. its separator line.
const SIDE_W: f32 = 160.0;
/// Width of the sidebar while the panels are shown: collapsed to short
/// entries (Files, ">_1", ">_2", ..., +) with the details in tooltips.
const SIDE_W_COLLAPSED: f32 = 44.0;
/// Gap between split panes, and the extra width around it that grabs the
/// line to resize them.
const PANE_GAP: f32 = 3.0;
const PANE_GRAB: f32 = 5.0;
/// Characters a sidebar entry's label is cut to (it is clipped too).
const LABEL_CHARS: usize = 18;
/// How often `poll_info` looks at the shells.
const POLL: Duration = Duration::from_secs(1);
/// Scrollback lines kept per terminal.
const HISTORY: usize = 10_000;
/// Redraws happen at most this often (output arriving faster is coalesced).
const FRAME: Duration = Duration::from_millis(16);
/// Half a blink cycle of the cursor and blinking text.
const BLINK: Duration = Duration::from_millis(530);

/// A terminal's scrollback find bar.
#[derive(Debug, Default)]
struct Find {
    text: String,
    /// The text isn't in the scrollback.
    missing: bool,
}

/// `text` as a regex that matches it literally.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Looks for `text` in the grid and its history: `older` `None` from the
/// current match (the selection; the bottom of the grid without one)
/// including it, else the next match older (`Some(true)`) or newer than it.
/// The match becomes the selection and is scrolled into view; the search
/// wraps around. False: no match.
fn search<T: EventListener>(term: &mut Term<T>, text: &str, older: Option<bool>) -> bool {
    let Ok(mut regex) = RegexSearch::new(&regex_escape(text)) else { return false };
    let current = term.selection.as_ref().and_then(|s| s.to_range(term)).map(|r| r.start);
    let bottom = GridPoint::new(Line(term.screen_lines() as i32 - 1), term.last_column());
    let (origin, direction) = match (current, older) {
        (None, _) => (bottom, Direction::Left),
        (Some(start), None) => (start, Direction::Left),
        (Some(start), Some(true)) => (start.sub(term, Boundary::None, 1), Direction::Left),
        (Some(start), Some(false)) => (start.add(term, Boundary::None, 1), Direction::Right),
    };
    let Some(m) = term.search_next(&mut regex, origin, direction, Side::Left, None) else { return false };
    let mut selection = Selection::new(SelectionType::Simple, *m.start(), Side::Left);
    selection.update(*m.end(), Side::Right);
    term.selection = Some(selection);
    term.scroll_to_point(*m.start());
    true
}

fn find_input_id() -> iced::widget::text_input::Id {
    iced::widget::text_input::Id::new("terminal-find")
}

/// The blink cycle shared by the terminals.
#[derive(Debug)]
pub(super) struct Blink {
    /// In the "off" half.
    off: bool,
    /// The last key typed: the cursor stays on for `BLINK` after it.
    input: Instant,
}

impl Default for Blink {
    fn default() -> Self {
        Blink { off: false, input: Instant::now() }
    }
}

/// Line height of the grid at font size `size`: 18 at 14, in proportion.
fn line_height(size: f32) -> f32 {
    (size * 18.0 / 14.0).round()
}

/// The advance of "0" and the baseline (from the top of the line) of `font` at
/// font size `size` and line height `cell_h`.
fn measure(font: Font, size: f32, cell_h: f32) -> (f32, f32) {
    use iced::advanced::text::Paragraph as _;
    const N: usize = 100;
    let sample = "0".repeat(N);
    let paragraph = iced::advanced::graphics::text::Paragraph::with_text(iced::advanced::Text {
        content: sample.as_str(),
        bounds: Size::INFINITY,
        size: Pixels(size),
        line_height: LineHeight::Absolute(Pixels(cell_h)),
        font,
        horizontal_alignment: alignment::Horizontal::Left,
        vertical_alignment: alignment::Vertical::Top,
        shaping: Shaping::Basic,
        wrapping: Wrapping::None,
    });
    let baseline = paragraph.buffer().layout_runs().next().map_or(0.0, |run| run.line_y);
    (paragraph.min_width() / N as f32, baseline)
}

/// The terminal's default font, shipped with the program (static faces, SIL
/// OFL, see `assets/fonts/OFL.txt`): the system's Cascadia Code / Mono are
/// variable fonts cosmic-text only sees at weight 400, so they have no bold.
pub(super) const BUNDLED_FONT: Font = Font::with_name("Cascadia Mono");

const BUNDLED_FACES: [&[u8]; 4] = [
    include_bytes!("../../assets/fonts/CascadiaMono-Regular.ttf"),
    include_bytes!("../../assets/fonts/CascadiaMono-Bold.ttf"),
    include_bytes!("../../assets/fonts/CascadiaMono-Italic.ttf"),
    include_bytes!("../../assets/fonts/CascadiaMono-BoldItalic.ttf"),
];

/// Adds the bundled faces to iced's font system (once; before anything is
/// shaped). An installed Cascadia Mono (the variable one Windows ships) is
/// dropped from it first: it has the same family name and weight 400, and
/// cosmic-text would pick it for regular text and ours for bold, two
/// versions that needn't share their metrics.
pub fn load_fonts() {
    use iced::advanced::graphics::text::cosmic_text::fontdb::Source;
    use iced::advanced::graphics::text::font_system;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut fonts = font_system().write().unwrap_or_else(|e| e.into_inner());
        let Font { family: iced::font::Family::Name(family), .. } = BUNDLED_FONT else { unreachable!() };
        let db = fonts.raw().db_mut();
        let installed: Vec<_> = db
            .faces()
            .filter(|f| !matches!(f.source, Source::Binary(_)) && f.families.iter().any(|(n, _)| n == family))
            .map(|f| f.id)
            .collect();
        for id in installed {
            db.remove_face(id);
        }
        for face in BUNDLED_FACES {
            fonts.load_font(std::borrow::Cow::Borrowed(face));
        }
    });
}

/// `terminal_font` as a font: empty = `BUNDLED_FONT`. iced wants a
/// `'static` family name, so each name is leaked once.
pub(super) fn font_named(name: &str) -> Font {
    static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let name = name.trim();
    if name.is_empty() {
        return BUNDLED_FONT;
    }
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let name = match names.iter().find(|n| **n == name) {
        Some(n) => *n,
        None => {
            let n: &'static str = Box::leak(name.to_owned().into_boxed_str());
            names.push(n);
            n
        }
    };
    Font::with_name(name)
}

/// How far the second copy of synthetic bold text is drawn to the right.
const BOLD_OFFSET: f32 = 0.6;

/// What the grid is drawn with: the font size, the cell measured from the
/// font at that size, and the face of each style (`style_font`). Measured
/// when the font or size changes (`App::term_metrics`).
#[derive(Debug, Clone, Copy)]
pub(super) struct Metrics {
    size: f32,
    /// Width of a cell: the advance of a monospace character.
    cell_w: f32,
    /// Line height.
    cell_h: f32,
    /// By `bold | italic << 1`.
    styles: [(Font, bool); 4],
}

impl Metrics {
    /// Measures `font` at font size `size`.
    pub(super) fn new(font: Font, size: f32) -> Metrics {
        let cell_h = line_height(size);
        let regular = measure(font, size, cell_h);
        let cell_w = if regular.0.is_finite() && regular.0 > 1.0 { regular.0 } else { (size * 0.6).max(2.0) };
        // A bold or italic face is only used when it has the regular face's
        // advance and baseline: a font can resolve the styles to other
        // families (`Font::MONOSPACE` on Windows is Cascadia Code, a variable
        // font cosmic-text sees only at weight 400, so bold and italic come
        // from Consolas, narrower and with another baseline), which would put
        // the text off the cell grid. Then bold is synthetic and italic is
        // drawn upright. `BUNDLED_FONT` has all four faces.
        let face = |bold: bool, italic: bool| Font {
            weight: if bold { Weight::Bold } else { Weight::Normal },
            style: if italic { FontStyle::Italic } else { FontStyle::Normal },
            ..font
        };
        let fits = |font: Font| {
            let (w, baseline) = measure(font, size, cell_h);
            (w - regular.0).abs() < 0.05 && (baseline - regular.1).abs() < 0.5
        };
        let italic_fits = fits(face(false, true));
        let pick = |bold: bool, italic: bool| {
            if (bold || italic) && fits(face(bold, italic)) {
                (face(bold, italic), false)
            } else if italic && italic_fits {
                (face(false, true), bold)
            } else {
                (font, bold)
            }
        };
        let styles = [pick(false, false), pick(true, false), pick(false, true), pick(true, true)];
        Metrics { size, cell_w, cell_h, styles }
    }

    /// The font a style is drawn with, and whether to embolden it by drawing
    /// it twice (`BOLD_OFFSET` apart).
    fn style_font(&self, bold: bool, italic: bool) -> (Font, bool) {
        self.styles[bold as usize | (italic as usize) << 1]
    }

    /// Columns and lines that fit in an area of `size` pixels (the grid widget).
    fn grid_size(&self, size: Size) -> (usize, usize) {
        let cols = ((size.width - 2.0 * PAD - crate::scrollbar::WIDTH) / self.cell_w).floor().max(2.0) as usize;
        let lines = ((size.height - 2.0 * PAD) / self.cell_h).floor().max(1.0) as usize;
        (cols.min(u16::MAX as usize), lines.min(u16::MAX as usize))
    }

    /// Whether an area of `size` pixels holds at least the smallest grid (two
    /// columns, one line). Smaller ones (a minimized window lays everything out
    /// at 0x0) aren't sent to the shell: shrinking the ConPTY to 2x1 and back
    /// can lose or hard-wrap its output.
    fn fits_grid(&self, size: Size) -> bool {
        size.width - 2.0 * PAD - crate::scrollbar::WIDTH >= 2.0 * self.cell_w && size.height - 2.0 * PAD >= self.cell_h
    }

    fn window_size(&self, cols: usize, lines: usize) -> WindowSize {
        WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: self.cell_w.round() as u16,
            cell_height: self.cell_h as u16,
        }
    }

    /// The pixel rectangle of `cells` cells at (line, col); edges rounded so
    /// neighbouring fills meet without seams.
    fn cell_rect(&self, origin: Point, line: u16, col: u16, cells: u16) -> Rectangle {
        let cw = self.cell_w;
        let x0 = (origin.x + col as f32 * cw).round();
        let x1 = (origin.x + (col + cells) as f32 * cw).round();
        let y = (origin.y + line as f32 * self.cell_h).round();
        Rectangle { x: x0, y, width: x1 - x0, height: self.cell_h }
    }

    /// The cell under `at` (relative to the widget): line, column, right half.
    /// Outside the grid it is the nearest cell.
    fn cell_at(&self, at: Point, cols: usize, lines: usize) -> (u16, u16, bool) {
        let (x, y) = (at.x - PAD, at.y - PAD);
        let col = (x / self.cell_w).floor();
        let right = col >= cols as f32 || (x / self.cell_w).fract() >= 0.5;
        let col = col.clamp(0.0, cols.saturating_sub(1) as f32) as u16;
        let line = (y / self.cell_h).floor().clamp(0.0, lines.saturating_sub(1) as f32) as u16;
        (line, col, right && x >= 0.0)
    }
}

/// The zoom (font size steps from the configured size `base`) after `step`
/// (+1 / -1; `None` = back to `base`), the size kept within
/// `config::TERMINAL_FONT_SIZES`.
fn zoom_after(base: u32, zoom: i32, step: Option<i32>) -> i32 {
    match step {
        None => 0,
        Some(step) => zoomed_size(base, zoomed_size(base, zoom) as i32 - base as i32 + step) as i32 - base as i32,
    }
}

/// The font size at `zoom` steps from `base`.
fn zoomed_size(base: u32, zoom: i32) -> u32 {
    let range = crate::config::TERMINAL_FONT_SIZES;
    (base as i32).saturating_add(zoom).clamp(*range.start() as i32, *range.end() as i32) as u32
}

#[derive(Debug, Clone)]
pub enum TermMsg {
    /// An event from terminal `id`'s I/O thread.
    Event(u64, TermEvent),
    /// The grid widget of terminal `id` has room for (columns, lines).
    Resize(u64, usize, usize),
    /// Sidebar: show terminal `id`, or Files for `None`.
    Show(Option<u64>),
    /// Sidebar: "+".
    New,
    /// Sidebar: close the entry that holds terminal `id` (all its split panes).
    Close(u64),
    /// A split between terminal panes was dragged.
    PaneResize(pane_grid::ResizeEvent),
    /// What `poll_info` found out about terminal `id`'s shell (sent on change).
    Info(u64, ShellInfo),
    /// The mouse in terminal `id`'s grid.
    Mouse(u64, Mouse),
    /// The scrollbar of terminal `id` was moved: the history line at the top.
    ScrollTo(u64, f32),
    /// The clipboard text to paste into terminal `id` (`None`: no text).
    Paste(u64, Option<String>),
    /// Type terminal `id`'s pending script line now, if no prompt was seen
    /// in time (`SCRIPT_WAIT`).
    SendPending(u64),
    /// Whether terminal `id`'s busy title went steady (`Activity::check`).
    IdleCheck(u64),
    /// The find bar's text of terminal `id` changed.
    FindInput(u64, String),
    /// The blink cycle's next half (`blink_ticks`).
    Blink,
    /// Ctrl+click on a link in a grid (`term_links`).
    OpenLink(LinkTarget),
}

/// What the mouse did in the grid; cells are viewport positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mouse {
    /// A button pressed over the grid. Left: `clicks` 1 (character
    /// selection), 2 (word) or 3 (line); `right`: the press was on the right
    /// half of the cell. `mods`: the keyboard modifiers held. The session
    /// decides whether it goes to the program (`reports_mouse`) or is ours:
    /// left selects, right copies the selection (Shift: as one line,
    /// `join_lines`) or pastes.
    Down { button: Btn, line: u16, col: u16, right: bool, clicks: u8, mods: keyboard::Modifiers },
    /// Moved to another cell with a button down.
    Drag { line: u16, col: u16, right: bool, mods: keyboard::Modifiers },
    /// A button pressed in the grid was released.
    Up { button: Btn, line: u16, col: u16, mods: keyboard::Modifiers },
    /// Moved to another cell with no button down; only sent while the
    /// program asked for all motion (`Frame::hover`).
    Hover { line: u16, col: u16, mods: keyboard::Modifiers },
    /// Wheel: lines, positive scrolls back into the history; the cell
    /// under the mouse.
    Wheel { lines: i32, line: u16, col: u16, mods: keyboard::Modifiers },
}

/// A mouse button of the grid; `code` is its number in mouse reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    Left,
    Middle,
    Right,
}

impl Btn {
    fn code(self) -> u8 {
        match self {
            Btn::Left => 0,
            Btn::Middle => 1,
            Btn::Right => 2,
        }
    }
}

/// What a mouse action on the grid leaves to the app.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MouseOutcome {
    None,
    /// Copy this text (right click on a selection).
    Copy(String),
    /// Paste the clipboard (right click with no selection).
    Paste,
}

/// A terminal shell's folder (`None`: not known, keep the last one) and the
/// program it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellInfo {
    dir: Option<PathBuf>,
    program: Option<String>,
}

/// Forwards the I/O thread's events into a channel the subscription reads.
/// A `Wakeup` is only queued when none is waiting (`wakeup`), so a burst of
/// output becomes one redraw.
#[derive(Clone)]
pub(super) struct Proxy {
    tx: std_mpsc::Sender<TermEvent>,
    wakeup: Arc<AtomicBool>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: TermEvent) {
        if matches!(event, TermEvent::Wakeup) && self.wakeup.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.tx.send(event);
    }
}

/// One running shell.
pub(super) struct TermSession {
    pub(super) id: u64,
    term: Arc<FairMutex<Term<Proxy>>>,
    chan: EventLoopSender,
    thread: Option<JoinHandle<(EventLoop<Pty, Proxy>, State)>>,
    /// The events, until the subscription takes them (it gives them back if
    /// it is dropped while the terminal lives).
    events: Arc<Mutex<Option<std_mpsc::Receiver<TermEvent>>>>,
    wakeup: Arc<AtomicBool>,
    /// Shown: while hidden, output doesn't wake the UI (the frame is rebuilt
    /// when it is shown again).
    visible: Arc<AtomicBool>,
    /// Window title a program set (OSC 0/2), when it says more than an exe
    /// path (`meaningful_title`); shown in the sidebar entry's tooltip.
    osc_title: Option<String>,
    /// Folder the shell is in: where it started, then what `poll_info` finds.
    pub(super) dir: PathBuf,
    /// Exe name (no `.exe`) of the program the shell runs, if any (polled).
    program: Option<String>,
    /// What the shell is (PowerShell, cmd, ...): how `cd_line` quotes a folder.
    family: ShellFamily,
    /// Process id of the shell (Windows).
    pub(super) pid: Option<u32>,
    /// Exit code once the shell has exited.
    exit_code: Option<i32>,
    /// What the widget draws; rebuilt on output while shown.
    frame: Frame,
    /// The scrollback find bar (Ctrl+Shift+F), while open.
    find: Option<Find>,
    /// A line to type once the shell shows its prompt (running a script,
    /// `App::run_script`); sent once.
    pending: Option<String>,
    /// Wheel lines not yet reported to a program that tracks the mouse
    /// (one report per `WHEEL_LINES`).
    wheel_rest: i32,
    /// The held button whose press went to the program (its drags and
    /// release go there too, whatever the mode or Shift does meanwhile).
    reported: Option<Btn>,
    /// A left press started a selection (its drags and release extend it).
    selecting: bool,
    /// Highlight of its sidebar entry while the user doesn't look at it (a
    /// command finished, the program waits); cleared when it is shown.
    attention: Option<Attention>,
    /// What `attention` is worked out from (`term_activity`).
    activity: Activity,
}

impl TermSession {
    /// Starts `command` (program and arguments) in `dir` with a grid of
    /// `cols` x `lines` cells of `m`.
    pub(super) fn spawn(
        id: u64,
        command: &str,
        dir: &Path,
        m: &Metrics,
        cols: usize,
        lines: usize,
    ) -> io::Result<TermSession> {
        let (program, args) = shell_command(command)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the builtin_shell setting is empty"))?;
        safe_dll_search();
        allow_ctrl_c();
        let (tx, rx) = std_mpsc::channel();
        let wakeup = Arc::new(AtomicBool::new(false));
        let proxy = Proxy { tx, wakeup: wakeup.clone() };
        // Not `tty::setup_env()`: it would change our own environment.
        let env = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ]);
        let options = tty::Options {
            shell: Some(Shell::new(program, args)),
            working_directory: Some(dir.to_owned()),
            drain_on_exit: true,
            env,
            #[cfg(windows)]
            escape_args: true,
        };
        let pty = tty::new(&options, m.window_size(cols, lines), id)?;
        #[cfg(windows)]
        let pid = pty.child_watcher().pid().map(|p| p.get());
        #[cfg(not(windows))]
        let pid = None;
        // A blinking block by default, like most terminals; programs can ask for another.
        let default_cursor_style = CursorStyle { shape: CursorShape::Block, blinking: true };
        let config = TermConfig { scrolling_history: HISTORY, default_cursor_style, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &TermSize::new(cols, lines), proxy.clone())));
        let event_loop = EventLoop::new(term.clone(), proxy, pty, true, false)?;
        let chan = event_loop.channel();
        let thread = event_loop.spawn();
        let mut session = TermSession {
            id,
            term,
            chan,
            thread: Some(thread),
            events: Arc::new(Mutex::new(Some(rx))),
            wakeup,
            visible: Arc::new(AtomicBool::new(true)),
            osc_title: None,
            dir: dir.to_owned(),
            program: None,
            family: shell_family(command),
            pid,
            exit_code: None,
            frame: Frame::default(),
            find: None,
            pending: None,
            wheel_rest: 0,
            reported: None,
            selecting: false,
            attention: None,
            activity: Activity::default(),
        };
        session.refresh();
        Ok(session)
    }

    /// The events of this terminal as messages; keyed by its id, so it keeps
    /// running across `subscription` calls (shown or not).
    pub(super) fn events(&self) -> Subscription<Message> {
        let (id, slot) = (self.id, self.events.clone());
        let (wakeup, visible) = (self.wakeup.clone(), self.visible.clone());
        Subscription::run_with_id(
            ("terminal", id),
            iced::stream::channel(64, move |output| async move {
                let rx = slot.lock().ok().and_then(|mut s| s.take());
                if let Some(rx) = rx {
                    std::thread::spawn(move || forward(id, rx, wakeup, visible, output, slot));
                }
                std::future::pending::<()>().await
            }),
        )
    }

    /// Sends `bytes` to the shell, scrolled back to the bottom.
    pub(super) fn write(&self, bytes: Vec<u8>) {
        {
            let mut term = self.term.lock();
            if term.grid().display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
            }
            // Typing ends a selection, like in Windows Terminal.
            term.selection = None;
        }
        let _ = self.chan.send(Msg::Input(bytes.into()));
    }

    /// The selected text, if any is selected; `one_line`: with the line
    /// breaks and the indentation after them removed (`join_lines`).
    fn selected_text(&self, one_line: bool) -> Option<String> {
        let term = self.term.lock();
        if term.selection.as_ref().is_none_or(Selection::is_empty) {
            return None;
        }
        let text = selection_text(&term)?;
        let text = if one_line { join_lines(&text) } else { text };
        Some(text).filter(|s| !s.is_empty())
    }

    /// Looks for the find bar's text: `None` from the current match (the
    /// selection; the bottom of the grid without one) including it, else the
    /// next match older (`Some(true)`) or newer than it. The match becomes
    /// the selection and is scrolled into view; the search wraps around.
    fn find(&mut self, older: Option<bool>) {
        let Some(find) = &mut self.find else { return };
        find.missing = false;
        if find.text.is_empty() {
            return;
        }
        find.missing = !search(&mut self.term.lock(), &find.text, older);
        self.refresh();
    }

    /// Scrolls the view and redraws it.
    fn scroll(&mut self, scroll: Scroll) {
        self.term.lock().scroll_display(scroll);
        self.refresh();
    }

    /// Pastes `text` as the program wants it (see `paste_bytes`).
    fn paste(&self, text: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        self.write(paste_bytes(text, bracketed));
    }

    /// Applies a mouse action on the grid: reported to a program that tracks
    /// the mouse (`reports_mouse`), else our selection, history scroll, copy
    /// and paste.
    fn mouse(&mut self, mouse: Mouse) -> MouseOutcome {
        let mut term = self.term.lock();
        let mode = *term.mode();
        let offset = term.grid().display_offset();
        let cols = term.columns().max(1);
        let point = |line: u16, col: u16| {
            GridPoint::new(Line(line as i32 - offset as i32), GridColumn((col as usize).min(cols - 1)))
        };
        let side = |right: bool| if right { Side::Right } else { Side::Left };
        match mouse {
            Mouse::Down { button, line, col, right, clicks, mods } => {
                if reports_mouse(mode, mods) {
                    drop(term);
                    self.reported = Some(button);
                    self.selecting = false;
                    self.write_raw(mouse_report(mode, button.code(), true, line, col, mods));
                    return MouseOutcome::None;
                }
                match button {
                    Btn::Left => {
                        let ty = match clicks {
                            1 => SelectionType::Simple,
                            2 => SelectionType::Semantic,
                            _ => SelectionType::Lines,
                        };
                        term.selection = Some(Selection::new(ty, point(line, col), side(right)));
                        self.selecting = true;
                    }
                    Btn::Middle => return MouseOutcome::None,
                    Btn::Right => {
                        drop(term);
                        let outcome = match self.selected_text(mods.shift()) {
                            Some(text) => {
                                self.term.lock().selection = None;
                                MouseOutcome::Copy(text)
                            }
                            None => MouseOutcome::Paste,
                        };
                        self.refresh();
                        return outcome;
                    }
                }
            }
            Mouse::Drag { line, col, right, mods } => {
                if let Some(button) = self.reported {
                    // 1002 and 1003 report moves with a button held (+32).
                    if mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
                        drop(term);
                        self.write_raw(mouse_report(mode, button.code() + 32, true, line, col, mods));
                    }
                    return MouseOutcome::None;
                }
                if !self.selecting {
                    return MouseOutcome::None;
                }
                let to = point(line, col);
                if let Some(selection) = &mut term.selection {
                    selection.update(to, side(right));
                }
            }
            Mouse::Up { button, line, col, mods } => {
                if self.reported == Some(button) {
                    self.reported = None;
                    if mode.intersects(TermMode::MOUSE_MODE) {
                        drop(term);
                        self.write_raw(mouse_report(mode, button.code(), false, line, col, mods));
                    }
                    return MouseOutcome::None;
                }
                if button != Btn::Left || !self.selecting {
                    return MouseOutcome::None;
                }
                self.selecting = false;
                if term.selection.as_ref().is_some_and(Selection::is_empty) {
                    term.selection = None;
                }
            }
            Mouse::Hover { line, col, mods } => {
                // 1003: moves with no button are reported too (button 3, +32).
                if self.reported.is_none() && mode.contains(TermMode::MOUSE_MOTION) {
                    drop(term);
                    self.write_raw(mouse_report(mode, 3 + 32, true, line, col, mods));
                }
                return MouseOutcome::None;
            }
            Mouse::Wheel { lines, line, col, mods } => {
                if reports_mouse(mode, mods) {
                    // The program handles the wheel itself (Claude Code, vim
                    // with mouse=a, htop, ...): report wheel buttons.
                    drop(term);
                    let total = self.wheel_rest + lines;
                    self.wheel_rest = total % WHEEL_LINES;
                    self.write_raw(wheel_reports(mode, total / WHEEL_LINES, line, col, mods));
                    return MouseOutcome::None;
                } else if !mode.contains(TermMode::ALT_SCREEN) {
                    // Also Shift+wheel while a program tracks the mouse.
                    term.scroll_display(Scroll::Delta(lines));
                } else if mode.contains(TermMode::ALTERNATE_SCROLL) && !mode.intersects(TermMode::MOUSE_MODE) {
                    drop(term);
                    self.write_raw(wheel_arrows(mode.contains(TermMode::APP_CURSOR), lines));
                    self.refresh();
                    return MouseOutcome::None;
                }
            }
        }
        drop(term);
        self.refresh();
        MouseOutcome::None
    }

    /// Types the pending line (if any) and Enter.
    fn send_pending(&mut self) {
        if let Some(line) = self.pending.take() {
            self.write(format!("{line}\r").into_bytes());
        }
    }

    /// Sends `bytes` without touching the view or the selection.
    fn write_raw(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.chan.send(Msg::Input(bytes.into()));
        }
    }

    /// Resizes the grid and the ConPTY when the size changed.
    fn resize(&mut self, m: &Metrics, cols: usize, lines: usize) {
        let mut term = self.term.lock();
        if (term.columns(), term.screen_lines()) == (cols, lines) {
            return;
        }
        term.resize(TermSize::new(cols, lines));
        drop(term);
        let _ = self.chan.send(Msg::Resize(m.window_size(cols, lines)));
    }

    /// Rebuilds what the widget draws from the grid.
    fn refresh(&mut self) {
        let pal = Palette::from_theme();
        self.frame = Frame::build(&self.term.lock(), &pal);
    }

    /// Shown or hidden; showing brings the frame up to date.
    fn set_visible(&mut self, on: bool) {
        self.visible.store(on, Ordering::Release);
        if on {
            self.refresh();
        }
    }

    fn app_cursor(&self) -> bool {
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    /// A full-screen program (alternate screen) runs.
    fn alt_screen(&self) -> bool {
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }
}

impl Drop for TermSession {
    /// Stops the I/O thread; dropping its `Pty` closes the ConPTY, which ends
    /// the shell (and the programs it runs in the console).
    fn drop(&mut self) {
        let _ = self.chan.send(Msg::Shutdown);
        if let Some(thread) = self.thread.take() {
            let end = Instant::now() + Duration::from_secs(2);
            while !thread.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                // Drops the event loop and with it the Pty, here.
                let _ = thread.join();
            }
        }
    }
}

/// The forwarding thread: the I/O thread's events to the subscription,
/// with `Wakeup`s at most once per `FRAME`, and none while hidden.
fn forward(
    id: u64,
    rx: std_mpsc::Receiver<TermEvent>,
    wakeup: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    mut output: iced::futures::channel::mpsc::Sender<Message>,
    slot: Arc<Mutex<Option<std_mpsc::Receiver<TermEvent>>>>,
) {
    use iced::futures::SinkExt;
    let mut last = Instant::now() - FRAME;
    // Ends when the terminal is gone (every sender dropped).
    while let Ok(event) = rx.recv() {
        if matches!(event, TermEvent::Wakeup) {
            if !visible.load(Ordering::Acquire) {
                wakeup.store(false, Ordering::Release);
                continue;
            }
            let wait = (last + FRAME).saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            // Output parsed from here on queues the next Wakeup.
            wakeup.store(false, Ordering::Release);
            last = Instant::now();
        }
        if iced::futures::executor::block_on(output.send(Message::Term(TermMsg::Event(id, event)))).is_err() {
            if let Ok(mut s) = slot.lock() {
                *s = Some(rx);
            }
            return;
        }
    }
}

/// Program and arguments of a `builtin_shell` command line (double quotes
/// group words). A program path with spaces is quoted again, since the
/// ConPTY command line is the program followed by the escaped arguments.
fn shell_command(command: &str) -> Option<(String, Vec<String>)> {
    let mut words = crate::editor::split(command).into_iter();
    let program = words.next()?;
    let program = if cfg!(windows) && program.contains(' ') { format!("\"{program}\"") } else { program };
    Some((program, words.collect()))
}

/// How long a script's terminal waits for the shell's prompt before typing
/// the script's line anyway (a custom prompt `parse_prompt` doesn't know).
const SCRIPT_WAIT: Duration = Duration::from_secs(4);

/// A script that Enter / the context menu runs in a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Script {
    /// `.ps1`
    PowerShell,
    /// `.cmd`, `.bat`
    Batch,
}

/// The kind of script `path` is, by its extension (any case).
pub(super) fn script_kind(path: &Path) -> Option<Script> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "ps1" => Some(Script::PowerShell),
        "cmd" | "bat" => Some(Script::Batch),
        _ => None,
    }
}

/// What a `builtin_shell` command line starts, by its program's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFamily {
    /// Windows PowerShell 5 (`powershell.exe`).
    WindowsPowerShell,
    /// PowerShell 7 (`pwsh.exe`).
    Pwsh,
    Cmd,
    /// Anything else (bash, nu, ...): not typed into, see `script_run`.
    Other,
}

fn shell_family(command: &str) -> ShellFamily {
    let Some((program, _)) = shell_command(command) else { return ShellFamily::Other };
    let program = program.trim_matches('"');
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program).to_ascii_lowercase();
    match name.strip_suffix(".exe").unwrap_or(&name) {
        "powershell" => ShellFamily::WindowsPowerShell,
        "pwsh" => ShellFamily::Pwsh,
        "cmd" => ShellFamily::Cmd,
        _ => ShellFamily::Other,
    }
}

/// `s` as a PowerShell single-quoted string: `'` (and the typographic
/// quotes PowerShell also takes for it) doubled.
fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// How cmd.exe names the script `name` in `dir`: the bare name, or the full
/// path in a UNC folder (cmd can't be in one and starts in Windows instead).
fn cmd_target(dir: &Path, name: &str) -> String {
    if dir.as_os_str().to_string_lossy().starts_with(r"\\") { dir.join(name).display().to_string() } else { name.to_owned() }
}

/// `s` as a POSIX shell single-quoted string: each `'` becomes `'\''`.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// The line that changes a shell of `family` to `dir`, typed at its prompt.
fn cd_line(family: ShellFamily, dir: &Path) -> String {
    let dir = dir.to_string_lossy();
    match family {
        ShellFamily::WindowsPowerShell | ShellFamily::Pwsh => {
            format!("Set-Location -LiteralPath {}", ps_quote(&dir))
        }
        // Without `/d` cmd stays on the current drive; it can't be in a UNC
        // folder at all, and `pushd` maps one to a free drive.
        ShellFamily::Cmd if dir.starts_with(r"\\") => format!("pushd \"{dir}\""),
        ShellFamily::Cmd => format!("cd /d \"{dir}\""),
        // bash, zsh, ... (fish and nu quote differently; best effort).
        ShellFamily::Other => format!("cd -- {}", sh_quote(&dir)),
    }
}

/// The shell command line to start in `dir` for running the script `name`
/// (in `dir`) of `kind`, and the line to type into it at its prompt. The
/// configured `shell` when it is PowerShell or cmd; otherwise one of those
/// (the script needs it anyway).
fn script_run(shell: &str, kind: Script, dir: &Path, name: &str) -> (String, String) {
    let (shell, family) = match (shell_family(shell), kind) {
        (ShellFamily::Other, Script::PowerShell) => {
            (crate::config::DEFAULT_BUILTIN_SHELL.to_owned(), ShellFamily::WindowsPowerShell)
        }
        (ShellFamily::Other, Script::Batch) => ("cmd.exe".to_owned(), ShellFamily::Cmd),
        (family, _) => (shell.to_owned(), family),
    };
    // A .ps1 in PowerShell: the shell starts with the execution policy
    // bypassed for its own process only (what Explorer's "Run with
    // PowerShell" does too), so the typed line can stay short.
    let shell = match (family, kind) {
        (ShellFamily::Cmd, _) | (_, Script::Batch) => shell,
        _ if !takes_policy(&shell) => shell,
        _ => format!("{shell} -ExecutionPolicy Bypass"),
    };
    let line = match family {
        ShellFamily::Cmd => {
            let target = cmd_target(dir, name);
            match kind {
                Script::Batch => format!("\"{target}\""),
                Script::PowerShell => format!("powershell -NoProfile -ExecutionPolicy Bypass -File \"{target}\""),
            }
        }
        // `.\x.ps1`, or `& '.\x y.ps1'` when the name needs quoting.
        _ if name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) => format!(".\\{name}"),
        _ => format!("& {}", ps_quote(&format!(".\\{name}"))),
    };
    (shell, line)
}

/// Whether `-ExecutionPolicy Bypass` can be appended to the PowerShell
/// command line `shell`: not when it sets a policy itself or ends in a
/// command or file (later arguments would go to those).
fn takes_policy(shell: &str) -> bool {
    const STOP: [&str; 10] = ["executionpolicy", "ep", "ex", "command", "c", "file", "f", "encodedcommand", "e", "ec"];
    !shell.split_whitespace().skip(1).any(|arg| {
        let arg = arg.to_ascii_lowercase();
        arg.starts_with(['-', '/']) && STOP.contains(&arg.trim_start_matches(['-', '/']))
    })
}

/// Program and raw arguments that run the script `name` (in `dir`) in a new
/// console window that stays open afterwards.
fn external_script_command(kind: Script, dir: &Path, name: &str) -> (&'static str, String) {
    match kind {
        Script::PowerShell => ("powershell.exe", format!("-NoExit -ExecutionPolicy Bypass -File \"{name}\"")),
        // `/k ""x""`: cmd drops the outer quotes and keeps the inner ones.
        Script::Batch => ("cmd.exe", format!("/k \"\"{}\"\"", cmd_target(dir, name))),
    }
}

/// Keeps `LoadLibraryW("conpty.dll")` in `tty::new` from loading a DLL from
/// the process's working folder: removes that folder from the DLL search
/// order (process-wide, once). System32 and the program's folder still come
/// first, as before.
fn safe_dll_search() {
    #[cfg(windows)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let empty = [0u16];
            // SAFETY: a valid, NUL-terminated (empty) wide string.
            unsafe { windows_sys::Win32::System::LibraryLoader::SetDllDirectoryW(empty.as_ptr()) };
        });
    }
}

/// Clears the "ignore Ctrl+C" flag of our own process before a shell starts:
/// children inherit it, and with it set Ctrl+C (0x03 into the ConPTY) can't
/// stop a running command. It is set when we were started by a process that
/// ignores Ctrl+C itself (some launchers, `start /b`, ...). We have no
/// console of our own, so this changes nothing for us.
fn allow_ctrl_c() {
    #[cfg(windows)]
    // SAFETY: a NULL handler with FALSE only resets the process's ignore flag.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0);
    }
}

fn folder_title(dir: &Path) -> String {
    dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// A sidebar entry: the folder's name, then the program the shell runs
/// (`Documents · ping`), in at most `max` characters. The folder is cut
/// first, so the program stays readable.
fn term_label(dir: &Path, program: Option<&str>, max: usize) -> String {
    let folder = folder_title(dir);
    match program {
        None => cut(&folder, max),
        Some(program) => {
            let program = cut(program, (max / 2).max(4));
            let room = max.saturating_sub(program.chars().count() + 3).max(4);
            format!("{} \u{b7} {program}", cut(&folder, room))
        }
    }
}

/// `s` in at most `max` characters, with an ellipsis when cut.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// `RC_TERM_DEBUG` is set: OSC title changes and bells are logged to
/// stderr (to see what a program sends when the attention cue misbehaves).
fn term_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RC_TERM_DEBUG").is_some())
}

/// Whether a title a program set says something: not empty and not an exe
/// path (what cmd and PowerShell set: `C:\...\powershell.exe`, or cmd's
/// `C:\WINDOWS\system32\cmd.exe - ping`).
fn meaningful_title(title: &str) -> bool {
    let t = title.trim();
    !t.is_empty() && !t.to_ascii_lowercase().contains(".exe")
}

/// The footer's hints for switching terminals, from the keys of the previous/next/new terminal commands
/// (`Ctrl+Shift+Up/Down` switch, `Ctrl+Shift+T` new).
fn switch_hints(prev: Option<String>, next: Option<String>, new: Option<String>) -> Vec<HintOwned> {
    let switch = match (prev, next) {
        (Some(prev), Some(next)) => {
            // Shared modifiers once: Ctrl+Shift+Up/Down.
            Some(match (prev.rsplit_once('+'), next.rsplit_once('+')) {
                (Some((a, up)), Some((b, down))) if a == b => HintOwned::new([format!("{a}+{up}/{down}")], "switch"),
                _ => HintOwned::new([prev, next], "switch"),
            })
        }
        (Some(key), None) => Some(HintOwned::new([key], "previous")),
        (None, Some(key)) => Some(HintOwned::new([key], "next")),
        (None, None) => None,
    };
    let new = new.map(|key| HintOwned::new([key], "new"));
    match (switch, new) {
        (Some(s), Some(n)) => vec![s, n],
        (Some(s), None) => vec![s],
        (None, Some(n)) => vec![HintOwned::new(Vec::<String>::new(), "Sidebar: switch"), n],
        (None, None) => vec![HintOwned::new(Vec::<String>::new(), "The sidebar on the right switches terminals")],
    }
}

/// `keys` for the four arrow directions (Left, Right, Up, Down) of one
/// command family: `Alt+Shift+arrows` when they share their modifiers, else
/// the keys there are.
fn arrow_keys(keys: [Option<String>; 4]) -> Vec<String> {
    let parts: Vec<Option<(&str, &str)>> = keys.iter().map(|k| k.as_deref().and_then(|k| k.rsplit_once('+'))).collect();
    if let [Some((m, "Left")), Some((m2, "Right")), Some((m3, "Up")), Some((m4, "Down"))] = parts[..] {
        if m == m2 && m == m3 && m == m4 {
            return vec![format!("{m}+arrows")];
        }
    }
    keys.into_iter().flatten().collect()
}

/// What the terminal footer's "(?)" lists: the keys of the terminal
/// commands (`key`: a command's first key) and the terminal's own keys.
fn term_key_tip(key: &dyn Fn(Command) -> Option<String>) -> Vec<HintOwned> {
    let mut tip = vec![HintOwned::new(Vec::<String>::new(), "Terminals")];
    let add = |tip: &mut Vec<HintOwned>, keys: Vec<String>, action: &str| {
        if !keys.is_empty() {
            tip.push(HintOwned::new(keys, action));
        }
    };
    add(&mut tip, key(Command::ToggleTerminal).into_iter().collect(), "back to the panels");
    add(&mut tip, [Command::NextTab, Command::PrevTab].into_iter().filter_map(key).collect(), "next / previous terminal");
    let switch = switch_hints(key(Command::PrevTerminal), key(Command::NextTerminal), None);
    if let Some(h) = switch.into_iter().find(|h| !h.keys.is_empty()) {
        add(&mut tip, h.keys, "next / previous sidebar entry (Files too)");
    }
    add(&mut tip, key(Command::NewTerminal).into_iter().collect(), "new terminal");
    add(&mut tip, key(Command::CloseTerminal).into_iter().collect(), "close the terminal");
    add(&mut tip, key(Command::TerminalFolderToPanel).into_iter().collect(), "its folder in the active panel");
    add(&mut tip, key(Command::SendFolderToTerminal).into_iter().collect(), "the active panel's folder here (cd)");
    add(&mut tip, key(Command::Configuration).into_iter().collect(), "settings (Esc comes back here)");
    let zoom = [Command::TerminalZoomIn, Command::TerminalZoomOut, Command::TerminalZoomReset];
    add(&mut tip, zoom.into_iter().filter_map(key).collect(), "bigger / smaller / normal text");
    tip.push(HintOwned::new(Vec::<String>::new(), "Split panes"));
    add(&mut tip, key(Command::SplitTerminalRight).into_iter().collect(), "split: new shell to the right");
    add(&mut tip, key(Command::SplitTerminalDown).into_iter().collect(), "split: new shell below");
    let dirs = |l, r, u, d| arrow_keys([key(l), key(r), key(u), key(d)]);
    use Command as C;
    add(
        &mut tip,
        dirs(C::FocusTerminalLeft, C::FocusTerminalRight, C::FocusTerminalUp, C::FocusTerminalDown),
        "focus the pane in that direction",
    );
    add(
        &mut tip,
        dirs(C::MoveTerminalLeft, C::MoveTerminalRight, C::MoveTerminalUp, C::MoveTerminalDown),
        "move the pane (swap, or to that edge)",
    );
    add(&mut tip, key(Command::ZoomTerminalPane).into_iter().collect(), "zoom the pane / all panes");
    let fixed = |keys: &[&str], action: &str| HintOwned::new(keys.iter().map(|k| k.to_string()), action);
    tip.extend([
        HintOwned::new(Vec::<String>::new(), "Scrollback"),
        fixed(&["Ctrl+Shift+F"], "find text in the scrollback"),
        fixed(&["Enter", "Up"], "find: older match"),
        fixed(&["Shift+Enter", "Down"], "find: newer match"),
        fixed(&["Esc"], "find: close"),
        fixed(&["Shift+PgUp", "Shift+PgDn"], "scroll a page"),
        fixed(&["Shift+Home", "Shift+End"], "top / bottom"),
        fixed(&["Shift+wheel"], "scroll (also in programs that use the mouse)"),
        HintOwned::new(Vec::<String>::new(), "Clipboard"),
        fixed(&["Ctrl+Shift+C", "Ctrl+Ins"], "copy the selection"),
        fixed(&["Ctrl+Shift+J"], "copy as one line"),
        fixed(&["Ctrl+Shift+V", "Shift+Ins"], "paste"),
        fixed(&["Shift+Right-click"], "copy as one line, or paste"),
        HintOwned::new(Vec::<String>::new(), "Links"),
        fixed(&["Ctrl+click"], "open a web address; show a file or folder in the panel"),
    ]);
    tip
}

/// `switch_hints` as one line of text (`Ctrl+Shift+Up/Down: switch \u{b7} Ctrl+Shift+T: new`).
#[cfg(test)]
fn switch_tip(prev: Option<String>, next: Option<String>, new: Option<String>) -> String {
    let part = |h: HintOwned| if h.keys.is_empty() { h.action } else { format!("{}: {}", h.keys.join(" / "), h.action) };
    switch_hints(prev, next, new).into_iter().map(part).collect::<Vec<_>>().join(" \u{b7} ")
}

/// The folder a shell's default prompt shows, from the text of the line up
/// to the cursor: `PS C:\Users>` (PowerShell) or `C:\Users>` (cmd).
/// PowerShell doesn't change its process's current folder on `cd`, so this
/// is how its folder is known.
fn parse_prompt(line: &str) -> Option<PathBuf> {
    let s = line.trim_end().strip_suffix('>')?.trim_end_matches('>');
    let s = s.strip_prefix("PS ").unwrap_or(s).trim();
    // PowerShell shows UNC folders with the provider's name.
    let s = s.strip_prefix(r"Microsoft.PowerShell.Core\FileSystem::").unwrap_or(s);
    let b = s.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    let unc = s.len() > 2 && s.starts_with(r"\\");
    (drive || unc).then(|| PathBuf::from(s))
}

/// `parse_prompt` of the cursor's line (with the lines it wrapped from).
fn prompt_dir(term: &Term<Proxy>) -> Option<PathBuf> {
    use alacritty_terminal::index::{Column as Col, Line};
    let grid = term.grid();
    let cols = grid.columns();
    let text = |line: Line, end: usize| -> String {
        let row = &grid[line];
        (0..end.min(cols))
            .map(|c| &row[Col(c)])
            .filter(|cell| !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER))
            .map(|cell| cell.c)
            .collect()
    };
    let cursor = grid.cursor.point;
    let mut line = cursor.line;
    let mut s = text(line, cursor.column.0);
    for _ in 0..8 {
        let prev: Line = line - 1;
        if prev < grid.topmost_line() || cols == 0 || !grid[prev][Col(cols - 1)].flags.contains(Flags::WRAPLINE) {
            break;
        }
        s.insert_str(0, &text(prev, cols));
        line = prev;
    }
    parse_prompt(&s)
}

/// Looks at every terminal's shell once per `POLL` (off the UI thread): the
/// folder it is in and the program it runs, sent as `TermMsg::Info` when
/// they change. Restarts when the set of terminals changes; none while no
/// terminal runs.
impl App {
    /// Ticks of the blink cycle while the focused terminal's cursor blinks
    /// (`terminal_cursor_blink`, and the program didn't ask for a steady one).
    pub(super) fn blink_ticks(&self) -> Option<Subscription<Message>> {
        let &id = self.screen.term()?;
        if !self.focused {
            return None;
        }
        let frame = &self.term(id)?.frame;
        if !(self.config.terminal_cursor_blink && frame.cursor_blinks && frame.cursor.is_some()) {
            return None;
        }
        Some(Subscription::run_with_id(
            "terminal-blink",
            iced::stream::channel(1, |mut output| async move {
                std::thread::spawn(move || loop {
                    std::thread::sleep(BLINK);
                    if let Err(e) = output.try_send(Message::Term(TermMsg::Blink)) {
                        if e.is_disconnected() {
                            return;
                        }
                    }
                });
                std::future::pending::<()>().await
            }),
        ))
    }
}

pub(super) fn poll_info(terms: &[TermSession]) -> Option<Subscription<Message>> {
    let shells: Vec<Polled> =
        terms.iter().filter_map(|t| Some((t.id, t.pid?, t.term.clone(), t.dir.clone()))).collect();
    if shells.is_empty() {
        return None;
    }
    let key: Vec<(u64, u32)> = shells.iter().map(|s| (s.0, s.1)).collect();
    Some(Subscription::run_with_id(
        ("terminal-info", key),
        iced::stream::channel(8, move |output| async move {
            std::thread::spawn(move || poll(shells, output));
            std::future::pending::<()>().await
        }),
    ))
}

/// A polled shell: terminal id, process id, its grid, last known folder.
type Polled = (u64, u32, Arc<FairMutex<Term<Proxy>>>, PathBuf);

fn poll(shells: Vec<Polled>, mut output: iced::futures::channel::mpsc::Sender<Message>) {
    // Per shell: the process's own current folder seen last (a change means
    // the shell changed it: cmd's `cd`), and what was sent last.
    let mut seen: Vec<(Option<PathBuf>, ShellInfo)> = shells
        .iter()
        .map(|(_, pid, _, dir)| (rc_platform::process_cwd(*pid), ShellInfo { dir: Some(dir.clone()), program: None }))
        .collect();
    loop {
        for ((id, pid, term, _), (peb, sent)) in shells.iter().zip(seen.iter_mut()) {
            let program = rc_platform::foreground_child(*pid);
            let mut dir = sent.dir.clone();
            let now = rc_platform::process_cwd(*pid);
            if now.is_some() && now != *peb {
                dir.clone_from(&now);
                *peb = now;
            }
            // At the prompt (nothing running), what it shows wins.
            if program.is_none() {
                let shown = prompt_dir(&term.lock());
                if let Some(shown) = shown.filter(|d| d.is_dir()) {
                    dir = Some(shown);
                }
            }
            let info = ShellInfo { dir, program };
            if info != *sent && output.try_send(Message::Term(TermMsg::Info(*id, info.clone()))).is_ok() {
                *sent = info;
            }
        }
        if output.is_closed() {
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// The colours a terminal draws with: 16 ANSI colours (light or dark set,
/// picked by the theme's background), and the theme's text and background.
#[derive(Debug, Clone, Copy)]
struct Palette {
    ansi: [Color; 16],
    fg: Color,
    bg: Color,
    cursor: Color,
    /// Bold text in one of the 8 normal colours uses the bright one
    /// (`terminal_bold_is_bright`).
    bold_bright: bool,
}

/// `terminal_bold_is_bright` from `config.toml` (`set_bold_bright`).
static BOLD_BRIGHT: AtomicBool = AtomicBool::new(false);

/// Sets `terminal_bold_is_bright`; shown terminals redraw with `App::refresh_terms`.
pub(super) fn set_bold_bright(on: bool) {
    BOLD_BRIGHT.store(on, Ordering::Relaxed);
}

/// What bold text in `color` is drawn with when bold is bright: the 8 normal
/// ANSI colours (named or indexed 0-7) become their bright variants.
fn bold_color(color: AnsiColor) -> AnsiColor {
    match color {
        AnsiColor::Named(n) if (NamedColor::Black as usize..=NamedColor::White as usize).contains(&(n as usize)) => {
            AnsiColor::Named(n.to_bright())
        }
        AnsiColor::Indexed(i) if i < 8 => AnsiColor::Indexed(i + 8),
        c => c,
    }
}

use theme::{TERM_DARK_ANSI as DARK_ANSI, TERM_LIGHT_ANSI as LIGHT_ANSI};

fn hex(v: u32) -> Color {
    Color::from_rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn luminance(c: Color) -> f32 {
    0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
}

/// `a` moved towards `b` by `t` (0..1).
fn mix(a: Color, b: Color, t: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: 1.0 }
}

fn from_rgb(c: Rgb) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

fn to_rgb(c: Color) -> Rgb {
    let [r, g, b, _] = c.into_rgba8();
    Rgb { r, g, b }
}

impl Palette {
    fn new(bg: Color, fg: Color) -> Palette {
        let set = if luminance(bg) > 0.5 { LIGHT_ANSI } else { DARK_ANSI };
        Palette { ansi: set.map(hex), fg, bg, cursor: fg, bold_bright: false }
    }

    fn from_theme() -> Palette {
        Palette { bold_bright: BOLD_BRIGHT.load(Ordering::Relaxed), ..Palette::theme_base().with(&theme::term()) }
    }

    /// The theme's colours without `[terminal_colors]`: its text, file list
    /// background and ANSI set (`Preset::ansi`).
    fn theme_base() -> Palette {
        let c = theme::c();
        Palette { ansi: theme::term_ansi().map(hex), ..Palette::new(c.panel, c.text) }
    }

    /// With the `[terminal_colors]` overrides: a background override that
    /// turns dark into light (or back) brings the generic set for it, then
    /// single colours replace its entries.
    fn with(self, o: &theme::TermColors) -> Palette {
        let fg = o[16].unwrap_or(self.fg);
        let bg = o[17].unwrap_or(self.bg);
        let flipped = (luminance(bg) > 0.5) != (luminance(self.bg) > 0.5);
        let mut pal = if flipped { Palette::new(bg, fg) } else { Palette { bg, fg, ..self } };
        for (slot, color) in pal.ansi.iter_mut().zip(o) {
            if let Some(color) = color {
                *slot = *color;
            }
        }
        pal.cursor = o[18].unwrap_or(fg);
        pal
    }

    /// Colour `index` (0-255 the indexed colours, 256 and up `NamedColor`s)
    /// without the terminal's overrides.
    fn color(&self, index: usize) -> Color {
        const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let dim = |c: Color| mix(c, self.bg, 0.35);
        match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let i = index - 16;
                Color::from_rgb8(CUBE[i / 36], CUBE[i / 6 % 6], CUBE[i % 6])
            }
            232..=255 => {
                let v = 8 + 10 * (index - 232) as u8;
                Color::from_rgb8(v, v, v)
            }
            _ => match index {
                i if i == NamedColor::Background as usize => self.bg,
                i if i == NamedColor::Cursor as usize => self.cursor,
                i if i == NamedColor::DimForeground as usize => dim(self.fg),
                i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                    dim(self.ansi[i - NamedColor::DimBlack as usize])
                }
                // Foreground, BrightForeground.
                _ => self.fg,
            },
        }
    }

    /// `color` with the terminal's overrides (OSC 4/10/11) applied.
    fn resolve(&self, color: AnsiColor, overrides: &TermColors) -> Color {
        let index = match color {
            AnsiColor::Spec(rgb) => return from_rgb(rgb),
            AnsiColor::Named(n) => n as usize,
            AnsiColor::Indexed(i) => i as usize,
        };
        overrides[index].map_or_else(|| self.color(index), from_rgb)
    }
}

/// Colour `j` of `theme::TERM_NAMES` the built-in terminal draws with now;
/// `default`: what it would be without its own `[terminal_colors]` entry.
pub(super) fn term_color(j: usize, default: bool) -> Color {
    let mut o = theme::term();
    if default {
        o[j] = None;
    }
    let pal = Palette::theme_base().with(&o);
    match j {
        0..=15 => pal.ansi[j],
        16 => pal.fg,
        17 => pal.bg,
        _ => pal.cursor,
    }
}

/// What the grid widget draws, built from the terminal under its lock
/// (the lock is not held while drawing). Coordinates are in cells.
#[derive(Debug, Default)]
struct Frame {
    cols: usize,
    lines: usize,
    bg: Color,
    /// Cell backgrounds that differ from `bg`: (line, column, cells, colour).
    fills: Vec<(u16, u16, u16, Color)>,
    runs: Vec<Run>,
    /// Block elements and box drawing lines, drawn as shapes rather than
    /// text (`term_glyphs`): (line, column, character, colour).
    shapes: Vec<(u16, u16, char, Color)>,
    cursor: Option<Cursor>,
    /// Selected cells: (line, column, cells), drawn over the text.
    selection: Vec<(u16, u16, u16)>,
    /// Lines of history, and how many the view is scrolled back.
    history: usize,
    offset: usize,
    /// The program asked for every mouse move (1003): the grid reports
    /// moves with no button down too (`Mouse::Hover`).
    hover: bool,
    /// The cursor blinks: the default, unless the program asked for a
    /// steady one (DECSCUSR, DECRST 12).
    cursor_blinks: bool,
    /// OSC 8 links and the addresses and paths in the text (`term_links`).
    links: Vec<Link>,
}

/// Text of one style on one line, starting at a cell.
#[derive(Debug, Clone, PartialEq)]
struct Run {
    line: u16,
    col: u16,
    /// Cells covered (wide characters take two).
    cells: u16,
    text: String,
    color: Color,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    /// Needs full shaping (characters outside the simple ranges).
    complex: bool,
}


#[derive(Debug, Clone)]
struct Cursor {
    line: u16,
    col: u16,
    cells: u16,
    shape: CursorShape,
    color: Color,
    /// The character under a block cursor, drawn in the background colour.
    text: Option<Run>,
}

/// Characters drawn in runs with the rest of their line: ones the monospace
/// font has at cell width (ASCII, Latin, Greek, Cyrillic, box drawing,
/// blocks). Others get a run of their own, placed at their cell, so a
/// fallback font's different width can't shift the rest of the line.
fn simple_char(c: char) -> bool {
    matches!(c as u32, 0x20..=0x7e | 0xa0..=0x24f | 0x370..=0x4ff | 0x2010..=0x2027 | 0x2500..=0x259f)
}

/// A line whose last cell is this goes on in the next one: alacritty marked
/// it wrapped, or it ends in a character (ConPTY often re-sends a soft-wrapped
/// line as a hard break, see `selection_text`), not box drawing.
fn wraps_on(flags: Flags, c: char) -> bool {
    flags.intersects(Flags::WRAPLINE | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        || (!flags.contains(Flags::HIDDEN) && !c.is_whitespace() && !matches!(c as u32, 0x2500..=0x259f))
}

/// The cells of the viewport inside `range`, a run per line.
/// The selected text, like `Term::selection_to_string`, but a line that ends
/// in a character in its last column is joined to the next one without a
/// line break: ConPTY often re-sends a soft-wrapped line as a hard break, so
/// alacritty never marks it wrapped (`Flags::WRAPLINE`). Box drawing at the
/// edge (rules across the window) still ends the line.
fn selection_text<T>(term: &Term<T>) -> Option<String> {
    let selection = term.selection.as_ref()?;
    let text = term.selection_to_string()?;
    if selection.ty == SelectionType::Block {
        return Some(text);
    }
    let range = selection.to_range(term)?;
    let last = term.last_column();
    // The line ends `selection_to_string` turned into "\n", in order: every
    // selected line but the last that isn't marked wrapped.
    let mut join = (range.start.line.0..range.end.line.0).filter_map(|line| {
        let row = &term.grid()[Line(line)];
        let cell = &row[last];
        if cell.flags.contains(Flags::WRAPLINE) {
            return None;
        }
        let edge = cell.flags.contains(Flags::WIDE_CHAR_SPACER)
            || (!cell.c.is_whitespace() && !matches!(cell.c as u32, 0x2500..=0x259f));
        Some(edge)
    });
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' && join.next() == Some(true) {
            continue;
        }
        out.push(c);
    }
    Some(out)
}

/// Text as one line, for paths, URLs and commands a program wrapped itself
/// (with its own indentation): each line break goes, with the whitespace
/// around it.
fn join_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        out.push_str(if i == 0 { line.trim_end() } else { line.trim() });
    }
    out.trim().to_owned()
}

fn selection_cells(range: &SelectionRange, offset: usize, cols: usize, lines: usize) -> Vec<(u16, u16, u16)> {
    let mut cells = Vec::new();
    for view in 0..lines {
        let line = Line(view as i32 - offset as i32);
        if line < range.start.line || line > range.end.line {
            continue;
        }
        let (from, to) = if range.is_block {
            (range.start.column.0, range.end.column.0)
        } else {
            (
                if line == range.start.line { range.start.column.0 } else { 0 },
                if line == range.end.line { range.end.column.0 } else { cols.saturating_sub(1) },
            )
        };
        if to >= from {
            cells.push((view as u16, from as u16, (to - from + 1) as u16));
        }
    }
    cells
}

impl Frame {
    fn build(term: &Term<Proxy>, pal: &Palette) -> Frame {
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let overrides = content.colors;
        let mut frame = Frame {
            cols: term.columns(),
            lines: term.screen_lines(),
            bg: pal.resolve(AnsiColor::Named(NamedColor::Background), overrides),
            selection: content
                .selection
                .map(|r| selection_cells(&r, content.display_offset, term.columns(), term.screen_lines()))
                .unwrap_or_default(),
            history: term.history_size(),
            offset: content.display_offset,
            hover: term.mode().contains(TermMode::MOUSE_MOTION),
            cursor_blinks: term.cursor_style().blinking,
            ..Frame::default()
        };
        let cursor_point = content.cursor.point;
        let cursor_shape = content.cursor.shape;
        let mut cursor_cell = None;
        // The open run and the spaces seen after it (added once more text
        // of its style follows).
        let mut open: Option<Run> = None;
        let mut spaces = 0u16;
        let mut scan = Scan::new(frame.lines);
        let last_col = frame.cols.saturating_sub(1);
        for item in content.display_iter {
            let flags = item.flags;
            let line = (item.point.line.0 + offset) as u16;
            let col = item.point.column.0 as u16;
            if item.point.column.0 == last_col && wraps_on(flags, item.c) {
                scan.joins(line);
            }
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let cells = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            let bright = pal.bold_bright && flags.contains(Flags::BOLD) && !flags.contains(Flags::DIM);
            let mut fg = pal.resolve(if bright { bold_color(item.fg) } else { item.fg }, overrides);
            let mut bg = pal.resolve(item.bg, overrides);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::DIM) {
                fg = mix(fg, bg, 0.35);
            }
            if bg != frame.bg {
                match frame.fills.last_mut() {
                    Some((l, c, n, color)) if *l == line && *c + *n == col && *color == bg => *n += cells,
                    _ => frame.fills.push((line, col, cells, bg)),
                }
            }
            let blank = flags.contains(Flags::HIDDEN) || item.c == ' ' || item.c == '\t';
            let zerowidth = item.zerowidth().filter(|z| !z.is_empty());
            let hyperlink = item.hyperlink();
            let link = hyperlink.as_ref().map(|h| (h.id(), h.uri()));
            let extra = zerowidth.filter(|_| !blank).unwrap_or(&[]);
            scan.cell(line, col, cells, if blank { ' ' } else { item.c }, extra, link);
            let style = (
                fg,
                flags.contains(Flags::BOLD),
                flags.contains(Flags::ITALIC),
                flags.intersects(Flags::ALL_UNDERLINES),
                flags.contains(Flags::STRIKEOUT),
            );
            let new_run = |text: String, complex: bool| Run {
                line,
                col,
                cells,
                text,
                color: style.0,
                bold: style.1,
                italic: style.2,
                underline: style.3,
                strike: style.4,
                complex,
            };
            let glyph = || {
                let mut g = String::new();
                if !blank {
                    g.push(item.c);
                    g.extend(zerowidth.into_iter().flatten());
                }
                g
            };
            let simple = cells == 1 && zerowidth.is_none() && simple_char(item.c);
            if item.point == cursor_point {
                cursor_cell = Some(new_run(glyph(), !simple));
            }
            if !blank && simple && term_glyphs::drawn(item.c) {
                frame.shapes.push((line, col, item.c, fg));
                continue;
            }
            if blank {
                // A space: part of the open run if text of its style follows.
                if open.as_ref().is_some_and(|o| o.line == line) {
                    spaces += 1;
                }
                continue;
            }
            // Strings are only allocated per run, not per cell.
            if let Some(o) = &mut open {
                let same = o.line == line
                    && !o.complex
                    && simple
                    && o.col + o.cells + spaces == col
                    && (o.color, o.bold, o.italic, o.underline, o.strike) == style;
                if same {
                    o.text.extend(std::iter::repeat_n(' ', spaces as usize));
                    o.text.push(item.c);
                    o.cells += spaces + 1;
                    spaces = 0;
                    continue;
                }
            }
            frame.runs.extend(open.take());
            spaces = 0;
            open = Some(new_run(glyph(), !simple));
        }
        frame.runs.extend(open);
        frame.links = scan.finish();

        let in_view = cursor_point.line.0 + offset;
        if cursor_shape != CursorShape::Hidden && (0..frame.lines as i32).contains(&in_view) {
            let cell = cursor_cell;
            let color = pal.resolve(AnsiColor::Named(NamedColor::Cursor), overrides);
            let text = cell
                .as_ref()
                .filter(|r| !r.text.is_empty() && cursor_shape == CursorShape::Block)
                .map(|r| Run { color: frame.bg, ..r.clone() });
            frame.cursor = Some(Cursor {
                line: in_view as u16,
                col: cursor_point.column.0 as u16,
                cells: cell.map_or(1, |r| r.cells),
                shape: cursor_shape,
                color,
                text,
            });
        }
        frame
    }

    /// The link that covers cell (line, col) of the viewport.
    fn link_at(&self, line: u16, col: u16) -> Option<&Link> {
        self.links.iter().find(|l| l.covers(line, col))
    }
}

/// Draws a `Frame` and reports the size that fits (`TermMsg::Resize`).
struct Grid<'a> {
    id: u64,
    frame: &'a Frame,
    /// The window has focus (else the cursor is drawn hollow).
    focused: bool,
    /// The cursor is left out (blinking, in its "off" half).
    cursor_off: bool,
    metrics: Metrics,
}

impl Grid<'_> {
    fn draw_run(&self, renderer: &mut iced::Renderer, run: &Run, origin: Point, clip: Rectangle) {
        let m = &self.metrics;
        let (cw, ch) = (m.cell_w, m.cell_h);
        let x = origin.x + run.col as f32 * cw;
        let y = origin.y + run.line as f32 * ch;
        let (font, synthetic_bold) = m.style_font(run.bold, run.italic);
        let text = || iced::advanced::Text {
            content: run.text.clone(),
            bounds: Size::new((run.cells + 1) as f32 * cw, ch),
            size: Pixels(m.size),
            line_height: LineHeight::Absolute(Pixels(ch)),
            font,
            horizontal_alignment: alignment::Horizontal::Left,
            vertical_alignment: alignment::Vertical::Top,
            shaping: if run.complex { Shaping::Advanced } else { Shaping::Basic },
            wrapping: Wrapping::None,
        };
        renderer.fill_text(text(), Point::new(x, y), run.color, clip);
        if synthetic_bold {
            renderer.fill_text(text(), Point::new(x + BOLD_OFFSET, y), run.color, clip);
        }
        let width = run.cells as f32 * cw;
        if run.underline {
            fill(renderer, Rectangle { x, y: y + ch - 2.0, width, height: 1.0 }, run.color);
        }
        if run.strike {
            fill(renderer, Rectangle { x, y: y + (ch / 2.0).round(), width, height: 1.0 }, run.color);
        }
    }

    /// The link under the mouse while Ctrl is held (and no button is).
    fn hovered_link(&self, state: &GridState, cursor: mouse::Cursor, bounds: Rectangle) -> Option<&Link> {
        if !state.mods.control() || state.held.is_some() {
            return None;
        }
        let at = cursor.position_in(bounds)?;
        let (x, y) = (at.x - PAD, at.y - PAD);
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let (col, line) = ((x / self.metrics.cell_w) as usize, (y / self.metrics.cell_h) as usize);
        if col >= self.frame.cols || line >= self.frame.lines {
            return None;
        }
        self.frame.link_at(line as u16, col as u16)
    }
}

fn fill(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    renderer.fill_quad(renderer::Quad { bounds, ..renderer::Quad::default() }, Background::Color(color));
}

/// Draws a block element or box drawing line (`term_glyphs::drawn`) in
/// `cell`, a `cell_rect`.
fn draw_shape(m: &Metrics, renderer: &mut iced::Renderer, cell: Rectangle, c: char, color: Color) {
    // Light lines: about the font's stroke.
    let t = (m.cell_w / 8.0).round().max(1.0);
    term_glyphs::pieces(c, cell, t, &mut |piece| match piece {
        Piece::Rect(r, alpha) => fill(renderer, r, Color { a: color.a * alpha, ..color }),
        Piece::Arc { rect, radius, width, clip } => renderer.with_layer(clip, |r| {
            r.fill_quad(
                renderer::Quad {
                    bounds: rect,
                    border: iced::Border { color, width, radius: radius.into() },
                    ..renderer::Quad::default()
                },
                Background::Color(Color::TRANSPARENT),
            )
        }),
    });
}

/// Mouse state of the grid widget.
#[derive(Debug, Default)]
struct GridState {
    /// The button pressed in the grid and still down (its moves are drags).
    held: Option<Btn>,
    /// The last drag or hover position sent, to send changes only.
    last: Option<(u16, u16, bool)>,
    /// The last left press: when, where, and in a row how many.
    click: Option<(Instant, u16, u16, u8)>,
    /// Pixels of wheel movement not yet worth a line.
    wheel: f32,
    /// The keyboard modifiers held (Shift+click bypasses mouse reporting,
    /// Shift+right-click copies as one line).
    mods: keyboard::Modifiers,
}

/// Longest time between presses of a double or triple click.
const MULTI_CLICK: Duration = Duration::from_millis(500);

impl Widget<Message, Theme, iced::Renderer> for Grid<'_> {
    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        iced::advanced::widget::tree::Tag::of::<GridState>()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        iced::advanced::widget::tree::State::new(GridState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&self, _tree: &mut Tree, _renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let bounds = layout.bounds();
        if self.hovered_link(tree.state.downcast_ref::<GridState>(), cursor, bounds).is_some() {
            mouse::Interaction::Pointer
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut IcedShell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        // Checked before every frame, so a window resize resizes the grid.
        if let Event::Window(window::Event::RedrawRequested(_)) = event {
            if !self.metrics.fits_grid(layout.bounds().size()) {
                return event::Status::Ignored;
            }
            let (cols, lines) = self.metrics.grid_size(layout.bounds().size());
            if (cols, lines) != (self.frame.cols, self.frame.lines) {
                shell.publish(Message::Term(TermMsg::Resize(self.id, cols, lines)));
            }
            return event::Status::Ignored;
        }
        let state = tree.state.downcast_mut::<GridState>();
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(mods)) = event {
            if mods.control() != state.mods.control() {
                // Ctrl shows or hides the link under the mouse.
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
            state.mods = mods;
            return event::Status::Ignored;
        }
        let bounds = layout.bounds();
        if let Event::Mouse(mouse::Event::CursorMoved { .. }) = event {
            if state.mods.control() {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
        }
        // Ctrl+click on a link opens it; the program doesn't get the click.
        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            if let Some(link) = self.hovered_link(state, cursor, bounds) {
                shell.publish(Message::Term(TermMsg::OpenLink(link.target.clone())));
                return event::Status::Captured;
            }
        }
        let (cols, lines) = (self.frame.cols, self.frame.lines);
        let mods = state.mods;
        let m = self.metrics;
        let cell_at = |at: Point, cols: usize, lines: usize| m.cell_at(at, cols, lines);
        let mut send = |m: Mouse| shell.publish(Message::Term(TermMsg::Mouse(self.id, m)));
        let button = |b: mouse::Button| match b {
            mouse::Button::Left => Some(Btn::Left),
            mouse::Button::Middle => Some(Btn::Middle),
            mouse::Button::Right => Some(Btn::Right),
            _ => None,
        };
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(b)) => {
                let Some(b) = button(b) else { return event::Status::Ignored };
                let Some(at) = cursor.position_in(bounds) else { return event::Status::Ignored };
                // The rim belongs to the line between split panes (dragging it resizes them).
                let rim = PANE_GRAB / 2.0;
                if at.x < rim || at.y < rim || at.x > bounds.width - rim || at.y > bounds.height - rim {
                    return event::Status::Ignored;
                }
                if state.held.is_some() {
                    // A second button while one is down: ignored.
                    return event::Status::Captured;
                }
                let (line, col, right) = cell_at(at, cols, lines);
                let mut clicks = 1;
                if b == Btn::Left {
                    let now = Instant::now();
                    clicks = match state.click {
                        Some((when, l, c, n)) if (l, c) == (line, col) && now.duration_since(when) < MULTI_CLICK => {
                            n % 3 + 1
                        }
                        _ => 1,
                    };
                    state.click = Some((now, line, col, clicks));
                }
                state.held = Some(b);
                state.last = Some((line, col, right));
                send(Mouse::Down { button: b, line, col, right, clicks, mods });
                event::Status::Captured
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if state.held.is_some() => {
                let at = Point::new(position.x - bounds.x, position.y - bounds.y);
                let (line, col, right) = cell_at(at, cols, lines);
                if state.last != Some((line, col, right)) {
                    state.last = Some((line, col, right));
                    send(Mouse::Drag { line, col, right, mods });
                }
                event::Status::Captured
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if self.frame.hover => {
                let Some(at) = cursor.position_in(bounds) else { return event::Status::Ignored };
                let (line, col, _) = cell_at(at, cols, lines);
                if state.last != Some((line, col, false)) {
                    state.last = Some((line, col, false));
                    send(Mouse::Hover { line, col, mods });
                }
                event::Status::Ignored
            }
            Event::Mouse(mouse::Event::ButtonReleased(b)) if state.held.is_some() && button(b) == state.held => {
                let Some(b) = state.held.take() else { return event::Status::Ignored };
                let at = cursor.position().map(|p| Point::new(p.x - bounds.x, p.y - bounds.y)).unwrap_or(Point::ORIGIN);
                let (line, col, _) = cell_at(at, cols, lines);
                send(Mouse::Up { button: b, line, col, mods });
                event::Status::Captured
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let amount = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => (y * 3.0).round() as i32,
                    mouse::ScrollDelta::Pixels { y, .. } => {
                        state.wheel += y / m.cell_h;
                        let whole = state.wheel.trunc();
                        state.wheel -= whole;
                        whole as i32
                    }
                };
                if amount != 0 {
                    let at = cursor.position_in(bounds).unwrap_or(Point::ORIGIN);
                    let (line, col, _) = cell_at(at, cols, lines);
                    send(Mouse::Wheel { lines: amount, line, col, mods });
                }
                event::Status::Captured
            }
            _ => event::Status::Ignored,
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let f = self.frame;
        fill(renderer, bounds, f.bg);
        let origin = Point::new(bounds.x + PAD, bounds.y + PAD);
        let m = &self.metrics;
        let cell_rect = |line, col, cells| m.cell_rect(origin, line, col, cells);
        for &(line, col, cells, color) in &f.fills {
            fill(renderer, cell_rect(line, col, cells), color);
        }
        for run in &f.runs {
            self.draw_run(renderer, run, origin, bounds);
        }
        for &(line, col, c, color) in &f.shapes {
            draw_shape(m, renderer, cell_rect(line, col, 1), c, color);
        }
        if let Some(link) = self.hovered_link(tree.state.downcast_ref::<GridState>(), cursor, bounds) {
            let color = theme::c().accent;
            for &(line, col, cells) in &link.spans {
                let r = cell_rect(line, col, cells);
                fill(renderer, Rectangle { y: r.y + r.height - 2.0, height: 1.0, ..r }, color);
            }
        }
        if !f.selection.is_empty() {
            let color = Color { a: 0.4, ..theme::c().accent };
            for &(line, col, cells) in &f.selection {
                fill(renderer, cell_rect(line, col, cells), color);
            }
        }
        let Some(c) = f.cursor.as_ref().filter(|_| !self.cursor_off) else { return };
        let r = cell_rect(c.line, c.col, c.cells);
        match c.shape {
            CursorShape::Block if self.focused => {
                fill(renderer, r, c.color);
                if let Some(run) = &c.text {
                    let mut chars = run.text.chars();
                    match (chars.next(), chars.next()) {
                        (Some(ch), None) if !run.complex && term_glyphs::drawn(ch) => draw_shape(m, renderer, r, ch, run.color),
                        _ => self.draw_run(renderer, run, origin, bounds),
                    }
                }
            }
            CursorShape::Underline => fill(renderer, Rectangle { y: r.y + r.height - 2.0, height: 2.0, ..r }, c.color),
            CursorShape::Beam => fill(renderer, Rectangle { width: 2.0, ..r }, c.color),
            // Hollow: an unfocused window, or asked for.
            _ => renderer.fill_quad(
                renderer::Quad {
                    bounds: r,
                    border: iced::Border { color: c.color, width: 1.0, radius: 0.0.into() },
                    ..renderer::Quad::default()
                },
                Background::Color(Color::TRANSPARENT),
            ),
        }
    }
}

/// What pasting `text` sends: line breaks as Enter (`\r`); in bracketed
/// paste mode (the program asked for it) wrapped in `ESC [ 200 ~` ..
/// `ESC [ 201 ~`, with ESC characters dropped so the text can't end the
/// bracket itself.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if !bracketed {
        return text.into_bytes();
    }
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend(text.replace('\x1b', "").into_bytes());
    bytes.extend(b"\x1b[201~");
    bytes
}

/// Dropped paths as typed text: separated by spaces, each in double quotes
/// when it has a space or a character shells treat specially (Windows
/// Terminal does the same).
fn drop_text(paths: &[PathBuf]) -> String {
    let quote = |p: &PathBuf| {
        let s = p.to_string_lossy();
        if s.chars().any(|c| c.is_whitespace() || "&()[]{}^=;!'+,`~$#@%".contains(c)) {
            format!("\"{s}\"")
        } else {
            s.into_owned()
        }
    };
    paths.iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// The arrow keys the wheel sends on the alternate screen (programs like
/// `less`, which don't scroll a history): Up for positive `lines`.
fn wheel_arrows(app_cursor: bool, lines: i32) -> Vec<u8> {
    let key = match (lines > 0, app_cursor) {
        (true, true) => "\x1bOA",
        (true, false) => "\x1b[A",
        (false, true) => "\x1bOB",
        (false, false) => "\x1b[B",
    };
    key.repeat(lines.unsigned_abs() as usize).into_bytes()
}

/// Wheel lines per wheel report sent to a program tracking the mouse (one
/// notch; programs scroll a few lines per report themselves).
const WHEEL_LINES: i32 = 3;

/// Whether a mouse action goes to the program: it turned on mouse tracking
/// (1000, 1002 or 1003) and Shift isn't held (Shift+click, drag and wheel
/// stay ours: selection, copy/paste, history; the xterm convention).
fn reports_mouse(mode: TermMode, mods: keyboard::Modifiers) -> bool {
    mode.intersects(TermMode::MOUSE_MODE) && !mods.shift()
}

/// One mouse report for a program that turned on mouse tracking. `code`:
/// button 0 left, 1 middle, 2 right, 3 none, 64/65 wheel up/down, plus 32
/// for a move; the modifiers add Shift 4, Alt 8, Ctrl 16. At viewport cell
/// `line`, `col`. In SGR form (`ESC [ < b ; col ; row M`, `m` for a
/// release) if the program asked for it, else the legacy X10 form
/// (`ESC [ M` and three bytes, a release is button 3), where cells past 222
/// are clamped (past 2014 in UTF-8 mode, 1005).
fn mouse_report(mode: TermMode, code: u8, pressed: bool, line: u16, col: u16, mods: keyboard::Modifiers) -> Vec<u8> {
    let mods = 4 * mods.shift() as u8 + 8 * mods.alt() as u8 + 16 * mods.control() as u8;
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if pressed { 'M' } else { 'm' };
        return format!("[<{};{};{}{end}", code + mods, col as u32 + 1, line as u32 + 1).into_bytes();
    }
    let code = if pressed { code } else { 3 };
    let mut bytes = vec![0x1b, b'[', b'M', 32 + code + mods];
    for v in [col, line] {
        if mode.contains(TermMode::UTF8_MOUSE) {
            let c = char::from_u32(32 + 1 + v.min(2014) as u32).unwrap_or(' ');
            bytes.extend(c.to_string().into_bytes());
        } else {
            bytes.push((32 + 1 + v.min(222)) as u8);
        }
    }
    bytes
}

/// Wheel reports (buttons 64/65): `notches` reports up (positive) or down,
/// at viewport cell `line`, `col` (`mouse_report`).
fn wheel_reports(mode: TermMode, notches: i32, line: u16, col: u16, mods: keyboard::Modifiers) -> Vec<u8> {
    let code = if notches > 0 { 64 } else { 65 };
    mouse_report(mode, code, true, line, col, mods).repeat(notches.unsigned_abs() as usize)
}

/// Whether closing a terminal asks first: a program other than the idle
/// shell runs in it.
fn close_needs_confirm(program: Option<&str>) -> bool {
    program.is_some()
}

/// The question at quit, if terminals are still running.
fn quit_question(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 terminal is still running. Quit anyway?".to_owned()),
        n => Some(format!("{n} terminals are still running. Quit anyway?")),
    }
}

/// An entry of the sidebar: one terminal, or several in split panes.
pub(super) struct TermTab {
    panes: pane_grid::State<u64>,
    /// The pane that gets the keys (the one `Screen::Term` names while shown).
    focus: pane_grid::Pane,
}

impl TermTab {
    fn new(id: u64) -> TermTab {
        let (panes, focus) = pane_grid::State::new(id);
        TermTab { panes, focus }
    }

    /// Its terminals, oldest first.
    fn ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.panes.iter().map(|(_, &id)| id).collect();
        ids.sort_unstable();
        ids
    }

    /// The terminals drawn: all, or the zoomed one.
    fn drawn(&self) -> Vec<u64> {
        match self.panes.maximized().and_then(|p| self.panes.get(p)) {
            Some(&id) => vec![id],
            None => self.ids(),
        }
    }

    fn pane(&self, id: u64) -> Option<pane_grid::Pane> {
        self.panes.iter().find(|(_, &t)| t == id).map(|(&p, _)| p)
    }

    /// `MoveTerminalLeft/Right/Up/Down`: the focused pane swaps places with
    /// the pane next to it in `direction`; with none there, it moves to that
    /// edge of the entry, across all the others (how a side-by-side pair
    /// becomes a stacked one). `false` if nothing changed.
    fn move_focused(&mut self, direction: pane_grid::Direction) -> bool {
        use pane_grid::{Axis, Direction, Edge, Node};
        if self.panes.len() < 2 {
            return false;
        }
        if let Some(next) = self.panes.adjacent(self.focus, direction) {
            self.panes.swap(self.focus, next);
            return true;
        }
        let (edge, axis, first) = match direction {
            Direction::Left => (Edge::Left, Axis::Vertical, true),
            Direction::Right => (Edge::Right, Axis::Vertical, false),
            Direction::Up => (Edge::Top, Axis::Horizontal, true),
            Direction::Down => (Edge::Bottom, Axis::Horizontal, false),
        };
        // Already alone along that edge: moving would only reset the split ratio.
        if let Node::Split { axis: a, a: left, b: right, .. } = self.panes.layout() {
            let side = if first { left } else { right };
            if *a == axis && matches!(**side, Node::Pane(p) if p == self.focus) {
                return false;
            }
        }
        let id = self.focused();
        self.panes.move_to_edge(self.focus, edge);
        // The moved terminal gets a new pane.
        if let Some(pane) = self.pane(id) {
            self.focus = pane;
        }
        true
    }

    /// The focused terminal.
    fn focused(&self) -> u64 {
        self.panes.get(self.focus).copied().unwrap_or_default()
    }
}

/// The bytes a terminal sends for a key press, or `None` for keys it
/// doesn't send anything for. `text` is what the key typed (with the
/// keyboard layout and AltGr applied); `app_cursor`: the program asked for
/// application cursor keys (`ESC O A` instead of `ESC [ A`).
pub(super) fn key_bytes(
    key: &keyboard::Key,
    mods: keyboard::Modifiers,
    text: Option<&str>,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    let (ctrl, alt, shift) = (mods.control(), mods.alt(), mods.shift());
    // xterm's modifier parameter: 1 + Shift 1, Alt 2, Ctrl 4.
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let esc = |s: &str| -> Vec<u8> { if alt { [b"\x1b", s.as_bytes()].concat() } else { s.as_bytes().to_vec() } };
    // Cursor keys: `ESC [ A`, `ESC O A` in app mode, `ESC [ 1 ; m A` with modifiers.
    let cursor = |c: char| -> Vec<u8> {
        if m > 1 {
            format!("\x1b[1;{m}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    // `ESC [ n ~`, `ESC [ n ; m ~` with modifiers.
    let tilde = |n: u8| -> Vec<u8> {
        if m > 1 { format!("\x1b[{n};{m}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
    };
    // F1-F4: `ESC O P`, `ESC [ 1 ; m P` with modifiers.
    let pf = |c: char| -> Vec<u8> {
        if m > 1 { format!("\x1b[1;{m}{c}").into_bytes() } else { format!("\x1bO{c}").into_bytes() }
    };
    match key {
        keyboard::Key::Named(named) => Some(match named {
            Named::Enter => esc("\r"),
            Named::Tab if shift => b"\x1b[Z".to_vec(),
            Named::Tab => esc("\t"),
            Named::Backspace if ctrl => esc("\x08"),
            Named::Backspace => esc("\x7f"),
            Named::Escape => esc("\x1b"),
            Named::Space if ctrl => b"\0".to_vec(),
            Named::Space => esc(" "),
            Named::ArrowUp => cursor('A'),
            Named::ArrowDown => cursor('B'),
            Named::ArrowRight => cursor('C'),
            Named::ArrowLeft => cursor('D'),
            Named::Home => cursor('H'),
            Named::End => cursor('F'),
            Named::Insert => tilde(2),
            Named::Delete => tilde(3),
            Named::PageUp => tilde(5),
            Named::PageDown => tilde(6),
            Named::F1 => pf('P'),
            Named::F2 => pf('Q'),
            Named::F3 => pf('R'),
            Named::F4 => pf('S'),
            Named::F5 => tilde(15),
            Named::F6 => tilde(17),
            Named::F7 => tilde(18),
            Named::F8 => tilde(19),
            Named::F9 => tilde(20),
            Named::F10 => tilde(21),
            Named::F11 => tilde(23),
            Named::F12 => tilde(24),
            _ => return None,
        }),
        keyboard::Key::Character(c) => {
            let typed = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
            // Ctrl+Alt is AltGr on Windows: send what it typed (`@`, `\`, ...).
            if ctrl && alt {
                if let Some(t) = typed {
                    return Some(t.as_bytes().to_vec());
                }
            }
            if ctrl {
                let ch = c.chars().next()?.to_ascii_lowercase();
                let code = match ch {
                    'a'..='z' => ch as u8 - b'a' + 1,
                    '@' | '2' | ' ' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '7' => 0x1f,
                    '?' | '8' => 0x7f,
                    _ => return typed.map(esc),
                };
                return Some(if alt { vec![0x1b, code] } else { vec![code] });
            }
            typed.or(Some(c.as_str())).map(esc)
        }
        keyboard::Key::Unidentified => text.filter(|t| !t.is_empty()).map(esc),
    }
}

impl App {
    pub(super) const TERM_KEYS: &'static [(&'static str, &'static str)] = &[
        ("Ctrl+Shift+F9 (cm_ToggleTerminal)", "Back to the panels; the terminal keeps running (F9 shows it again)"),
        ("Ctrl+Shift+Down/Up (cm_NextTerminal, cm_PrevTerminal)", "Next/previous entry of the sidebar: Files, terminal 1, 2, ..."),
        ("Ctrl+Tab, Ctrl+Shift+Tab (cm_NextTab, cm_PrevTab)", "Next/previous terminal (Files is skipped; in the panels they switch panel tabs)"),
        ("Ctrl+Shift+T (cm_NewTerminal)", "Another terminal in the active panel's folder (the + of the sidebar)"),
        ("Ctrl+Shift+W (cm_CloseTerminal)", "End the focused terminal and close it (middle-click or x on its sidebar entry closes the entry with all its split panes)"),
        ("Ctrl+Shift+E, Ctrl+Shift+O (cm_SplitTerminalRight, cm_SplitTerminalDown)", "Split: another shell to the right of / below the focused one, in its folder; drag the line between panes to resize"),
        ("Alt+Shift+arrows (cm_FocusTerminalLeft/Right/Up/Down)", "Move the keyboard focus to the split pane in that direction (or click a pane)"),
        ("Ctrl+Alt+Shift+arrows (cm_MoveTerminalLeft/Right/Up/Down)", "Rearrange split panes: the focused pane swaps places with the one in that direction, or, with none there, moves to that edge (side by side becomes stacked and back)"),
        ("Ctrl+Shift+Z (cm_ZoomTerminalPane)", "Show the focused split pane alone, or all panes again"),
        ("Ctrl++ / Ctrl+-, Ctrl+0 (cm_TerminalZoomIn, cm_TerminalZoomOut, cm_TerminalZoomReset)", "Bigger / smaller text in all terminals, or back to the terminal_font_size setting (kept for the next start; the font is the terminal_font setting)"),
        ("F10 (cm_Configuration)", "Settings; closing them comes back to the terminal (F10 doesn't reach programs in the terminal; bind cm_Configuration to another key in [keys] if one needs it)"),
        ("cm_TerminalFolderToPanel (no key by default)", "Active panel goes to the terminal's current folder and Files is shown"),
        ("Ctrl+Shift+Enter (cm_SendFolderToTerminal)", "This terminal goes to the folder selected in the active panel (cd); from the panels it goes to the last used terminal"),
        ("Ctrl+Shift+F", "Find text in the scrollback: typing finds the newest match, Enter / Up the next older one, Shift+Enter / Down the next newer one (wraps around); the match is selected (Ctrl+Shift+C copies it). Case-insensitive unless the text has capitals; Esc closes"),
        ("Ctrl+Shift+C or Ctrl+Insert", "Copy the selected text (select with the mouse: drag, double-click a word, triple-click a line)"),
        ("Ctrl+Shift+J", "Copy the selected text as one line: line breaks and the indentation after them removed (paths, URLs, commands a program wrapped)"),
        ("Right-click", "Copies the selection and clears it; with no selection pastes the clipboard"),
        ("Shift+Right-click", "Copies the selection as one line (like Ctrl+Shift+J) and clears it; with no selection pastes"),
        ("Ctrl+Shift+V or Shift+Insert", "Paste the clipboard (bracketed when the program asks for it)"),
        ("Mouse wheel, Shift+PgUp/PgDn, Shift+Home/End", "Scroll the history (programs that use the mouse get wheel events, other full-screen programs arrow keys); typing jumps back to the bottom"),
        ("Mouse in programs that use it", "Programs that track the mouse (Claude Code, vim with mouse=a, htop, ...) get the clicks, drags and wheel instead; hold Shift to select, copy/paste with Shift+Right-click, or scroll the history with Shift+wheel as usual"),
        ("Every other key", "Goes to the shell (Esc, Tab, Ctrl+C, arrows, F1-F12, ...)"),
        ("exit", "Ends the shell and closes the terminal"),
    ];

    fn term(&self, id: u64) -> Option<&TermSession> {
        self.terms.iter().find(|t| t.id == id)
    }

    fn term_mut(&mut self, id: u64) -> Option<&mut TermSession> {
        self.terms.iter_mut().find(|t| t.id == id)
    }

    /// Redraws the shown terminals with the current colours (hidden ones
    /// rebuild when shown).
    pub(super) fn refresh_terms(&mut self) {
        for t in &mut self.terms {
            if t.visible.load(Ordering::Acquire) {
                t.refresh();
            }
        }
    }

    /// The terminals' font size: `terminal_font_size` zoomed by
    /// `TerminalZoomIn/Out`.
    fn term_font_size(&self) -> u32 {
        zoomed_size(self.config.terminal_font_size(), self.term_zoom)
    }

    /// The grid's metrics at the current font and size, measured on first use
    /// after a change.
    pub(super) fn term_metrics(&self) -> Metrics {
        *self.term_metrics.get_or_init(|| {
            Metrics::new(font_named(&self.config.terminal_font), self.term_font_size() as f32)
        })
    }

    /// `TerminalZoomIn/Out/Reset`: one font size step bigger or smaller
    /// (`step`), or back to `terminal_font_size` (`None`), in every terminal.
    /// The shown grids get their new size on the next frame (like after a
    /// window resize) and resize their shells; hidden ones when shown.
    pub(super) fn zoom_term(&mut self, step: Option<i32>) -> Task<Message> {
        if self.screen.term().is_none() {
            self.info("Zooming works in the built-in terminal (F9)");
            return Task::none();
        }
        let zoom = zoom_after(self.config.terminal_font_size(), self.term_zoom, step);
        if zoom != self.term_zoom {
            self.term_zoom = zoom;
            self.term_metrics = std::cell::OnceCell::new();
        }
        Task::none()
    }

    /// Width taken by the terminal sidebar: only over the panels and the
    /// terminals, and only while a terminal exists; collapsed over the panels.
    pub(super) fn term_side_w(&self) -> f32 {
        if self.terms.is_empty() {
            return 0.0;
        }
        match self.screen {
            None => SIDE_W_COLLAPSED,
            Some(Screen::Term(_)) => SIDE_W,
            _ => 0.0,
        }
    }

    /// Window width left of the sidebar: what the panels share.
    pub(super) fn area_width(&self) -> f32 {
        self.window_width - self.term_side_w()
    }

    /// F9: shows the terminal used last, starting one in the active panel's
    /// folder if none is running (`NewTerminal` always starts another).
    pub(super) fn open_term(&mut self) -> Task<Message> {
        self.leave_edits();
        let last = self.term_last.filter(|&id| self.term(id).is_some()).or(self.terms.first().map(|t| t.id));
        match last {
            Some(id) => {
                self.show_term(id);
                Task::none()
            }
            None => self.new_term(),
        }
    }

    /// Starts a shell in the active panel's folder and shows it.
    pub(super) fn new_term(&mut self) -> Task<Message> {
        self.leave_edits();
        self.refresh_config();
        let dir = self.panel(self.active).path.clone();
        if vfs::in_archive(&dir) {
            let place = if remote::is_remote(&dir) { "on a server" } else { "inside an archive" };
            self.error(format!("The built-in terminal only opens in local folders, not {place}"));
            return Task::none();
        }
        if !dir.is_dir() {
            self.error(format!("Can't open a terminal in {}: not a folder", dir.display()));
            return Task::none();
        }
        let shell = self.config.builtin_shell();
        self.start_term(&dir, &shell);
        Task::none()
    }

    /// `SplitTerminalRight/Down`: another shell beside or below the focused
    /// terminal, in its folder, in the same sidebar entry.
    pub(super) fn split_term(&mut self, axis: pane_grid::Axis) -> Task<Message> {
        let Some(&id) = self.screen.term() else {
            self.info("Splitting works in the built-in terminal (F9)");
            return Task::none();
        };
        self.refresh_config();
        let Some(dir) = self.term(id).map(|t| t.dir.clone()) else { return Task::none() };
        if !dir.is_dir() {
            self.error(format!("Can't open a terminal in {}: not a folder", dir.display()));
            return Task::none();
        }
        let shell = self.config.builtin_shell();
        self.spawn_term(&dir, &shell, Some(axis));
        Task::none()
    }

    /// Starts `shell` in `dir` as a new terminal (a new sidebar entry) and
    /// shows it; its id, or `None` (with an error) if it didn't start.
    fn start_term(&mut self, dir: &Path, shell: &str) -> Option<u64> {
        self.spawn_term(dir, shell, None)
    }

    /// `start_term`; with `split`, in a new pane split off the focused one.
    fn spawn_term(&mut self, dir: &Path, shell: &str, split: Option<pane_grid::Axis>) -> Option<u64> {
        let id = self.next_term_id;
        self.next_term_id += 1;
        // The grid widget reports the size it really gets on the next frame.
        let m = self.term_metrics();
        let (cols, lines) = m.grid_size(Size::new(self.window_width - SIDE_W, self.window_height));
        match TermSession::spawn(id, shell, dir, &m, cols, lines) {
            Ok(session) => {
                self.terms.push(session);
                let tab = self.shown_tab();
                let added = split.zip(tab).and_then(|(axis, tab)| {
                    let t = &mut self.term_tabs[tab];
                    t.panes.restore();
                    let (pane, _) = t.panes.split(axis, t.focus, id)?;
                    t.focus = pane;
                    Some(())
                });
                match added {
                    // Same entry: `show_term` only makes the entry's terminals visible when it changes.
                    Some(()) => {
                        if let Some(t) = self.term_mut(id) {
                            t.set_visible(true);
                        }
                    }
                    None => self.term_tabs.push(TermTab::new(id)),
                }
                self.show_term(id);
                Some(id)
            }
            Err(e) => {
                self.error(format!("Can't start the shell \"{shell}\": {e}"));
                None
            }
        }
    }

    /// Enter on a script (setting `run_scripts_in_terminal`) or the context
    /// menu's "Run in terminal": a new terminal in the script's folder,
    /// which types the line that runs it once the shell's prompt is up and
    /// stays open afterwards. Only for local files.
    pub(super) fn run_script(&mut self, path: &Path) -> Task<Message> {
        self.leave_edits();
        self.refresh_config();
        let Some((kind, dir, name)) = self.script_parts(path) else { return Task::none() };
        let (shell, line) = script_run(&self.config.builtin_shell(), kind, &dir, &name);
        let Some(id) = self.start_term(&dir, &shell) else { return Task::none() };
        if let Some(t) = self.term_mut(id) {
            t.pending = Some(line);
        }
        delayed(SCRIPT_WAIT, Message::Term(TermMsg::SendPending(id)))
    }

    /// A user command with `terminal = true`: a new terminal in `dir` that
    /// types `line` once the shell's prompt is up (like `run_script`).
    pub(super) fn run_line_in_term(&mut self, dir: &Path, line: String) -> Task<Message> {
        self.leave_edits();
        self.refresh_config();
        let shell = self.config.builtin_shell();
        let Some(id) = self.start_term(dir, &shell) else { return Task::none() };
        if let Some(t) = self.term_mut(id) {
            t.pending = Some(line);
        }
        delayed(SCRIPT_WAIT, Message::Term(TermMsg::SendPending(id)))
    }

    /// The context menu's "Run in external terminal": a new console window
    /// in the script's folder that runs it and stays open.
    pub(super) fn run_script_external(&mut self, path: &Path) {
        let Some((kind, dir, name)) = self.script_parts(path) else { return };
        let (program, args) = external_script_command(kind, &dir, &name);
        match rc_platform::spawn_console(program, &args, &dir) {
            Ok(()) => self.success(format!("Running {name} in a new console window")),
            Err(e) => self.error(format!("Can't start {program}: {e}")),
        }
    }

    /// Kind, folder and file name of a local script (else an error).
    fn script_parts(&mut self, path: &Path) -> Option<(Script, PathBuf, String)> {
        if vfs::is_virtual(path) {
            self.error("Scripts only run from local folders: copy it to one first (F5)");
            return None;
        }
        let parts = script_kind(path).zip(path.parent().filter(|d| d.is_dir())).zip(path.file_name());
        match parts {
            Some(((kind, dir), name)) => Some((kind, dir.to_owned(), name.to_string_lossy().into_owned())),
            None => {
                self.error(format!("Not a script that can run in a terminal: {}", path.display()));
                None
            }
        }
    }

    /// The sidebar entry holding terminal `id`.
    fn tab_of(&self, id: u64) -> Option<usize> {
        self.term_tabs.iter().position(|t| t.pane(id).is_some())
    }

    /// The sidebar entry shown.
    fn shown_tab(&self) -> Option<usize> {
        self.screen.term().and_then(|&id| self.tab_of(id))
    }

    /// The focused terminal of each sidebar entry, in sidebar order.
    fn tab_order(&self) -> Vec<u64> {
        self.term_tabs.iter().map(TermTab::focused).collect()
    }

    /// The terminals on screen (the cues are for the others).
    fn drawn_terms(&self) -> Vec<u64> {
        self.shown_tab().map(|t| self.term_tabs[t].drawn()).unwrap_or_default()
    }

    /// Shows or hides the terminals of entry `tab`; shown ones are looked at.
    fn set_tab_visible(&mut self, tab: usize, on: bool) {
        let Some(t) = self.term_tabs.get(tab) else { return };
        let (ids, drawn) = (t.ids(), t.drawn());
        for id in ids {
            if let Some(t) = self.term_mut(id) {
                t.set_visible(on);
                if on && drawn.contains(&id) {
                    t.attention = None;
                }
            }
        }
    }

    /// Shows the sidebar entry holding terminal `id` with `id` focused
    /// (hiding the entry shown before).
    pub(super) fn show_term(&mut self, id: u64) {
        let Some(tab) = self.tab_of(id) else { return };
        let old = self.shown_tab();
        if old != Some(tab) {
            if let Some(old) = old {
                self.set_tab_visible(old, false);
            }
            self.set_tab_visible(tab, true);
        }
        let t = &mut self.term_tabs[tab];
        if let Some(pane) = t.pane(id) {
            t.focus = pane;
        }
        if let Some(t) = self.term_mut(id) {
            t.attention = None;
        }
        self.term_last = Some(id);
        self.screen = Some(Screen::Term(id));
    }

    /// `FocusTerminalLeft/Right/Up/Down`: the split pane next to the focused
    /// one gets the keys (a zoomed pane is shown with the others again).
    pub(super) fn focus_term_pane(&mut self, direction: pane_grid::Direction) {
        let Some(tab) = self.shown_tab() else { return };
        let t = &mut self.term_tabs[tab];
        let Some(next) = t.panes.adjacent(t.focus, direction) else { return };
        let Some(&id) = t.panes.get(next) else { return };
        if t.panes.maximized().is_some() {
            t.panes.restore();
            self.set_tab_visible(tab, true);
        }
        self.show_term(id);
    }

    /// `MoveTerminalLeft/Right/Up/Down`: see `TermTab::move_focused` (a
    /// zoomed pane is shown with the others again).
    pub(super) fn move_term_pane(&mut self, direction: pane_grid::Direction) {
        let Some(tab) = self.shown_tab() else { return };
        let t = &mut self.term_tabs[tab];
        if t.panes.len() < 2 {
            return;
        }
        if t.panes.maximized().is_some() {
            t.panes.restore();
            self.set_tab_visible(tab, true);
        }
        // The focused terminal keeps the keys; pane sizes follow when drawn.
        self.term_tabs[tab].move_focused(direction);
    }

    /// `ZoomTerminalPane`: the focused pane alone, or all panes again.
    pub(super) fn zoom_term_pane(&mut self) {
        let Some(tab) = self.shown_tab() else { return };
        let t = &mut self.term_tabs[tab];
        if t.panes.maximized().is_some() {
            t.panes.restore();
            // The other panes' frames weren't kept up to date meanwhile.
            self.set_tab_visible(tab, true);
        } else if t.panes.len() > 1 {
            t.panes.maximize(t.focus);
        }
    }

    /// The window got the focus: the terminals on screen are looked at again.
    pub(super) fn term_looked_at(&mut self) {
        for id in self.drawn_terms() {
            if let Some(t) = self.term_mut(id) {
                t.attention = None;
            }
        }
    }

    /// `ToggleTerminal` from the panels: like F9.
    pub(super) fn toggle_term(&mut self) -> Task<Message> {
        if self.screen.term().is_some() {
            self.hide_term();
            return Task::none();
        }
        self.open_term()
    }

    /// Back to the panels; the terminal keeps running.
    pub(super) fn hide_term(&mut self) {
        if let Some(tab) = self.shown_tab() {
            self.set_tab_visible(tab, false);
        }
        if self.screen.term().is_some() {
            self.screen = None;
        }
    }

    /// Sidebar entries are Files (0) and the terminal entries (1..); this is
    /// the shown one.
    fn side_index(&self) -> usize {
        self.shown_tab().map_or(0, |i| i + 1)
    }

    /// Shows entry `index` of the sidebar.
    fn show_entry(&mut self, index: usize) {
        match index.checked_sub(1).and_then(|i| self.term_tabs.get(i)).map(TermTab::focused) {
            Some(id) => self.show_term(id),
            None => self.hide_term(),
        }
    }

    /// `NextTerminal` / `PrevTerminal`: around Files and the terminal entries.
    /// Ctrl+Tab / Ctrl+Shift+Tab in a terminal: the next/previous entry,
    /// around, skipping Files (nothing with fewer than two entries).
    fn cycle_among_terms(&mut self, delta: isize) {
        let Some(&current) = self.screen.term() else { return };
        if let Some(id) = next_terminal(&self.tab_order(), current, delta) {
            self.show_term(id);
        }
    }

    pub(super) fn cycle_term(&mut self, delta: isize) {
        if self.term_tabs.is_empty() || !matches!(self.screen, None | Some(Screen::Term(_))) {
            return;
        }
        self.leave_edits();
        let next = cycle(self.side_index(), self.term_tabs.len() + 1, delta);
        self.show_entry(next);
    }

    /// `CloseTerminal`: ends the focused terminal (its split pane closes).
    pub(super) fn close_shown_term(&mut self) {
        if let Some(&id) = self.screen.term() {
            self.request_close_terms(vec![id]);
        }
    }

    /// The sidebar's x or middle-click: ends every terminal of the entry
    /// holding `id`.
    fn request_close_tab(&mut self, id: u64) {
        if let Some(tab) = self.tab_of(id) {
            let ids = self.term_tabs[tab].ids();
            self.request_close_terms(ids);
        }
    }

    /// Closes terminals `ids`, asking first when a program runs in one (not
    /// for the idle shell).
    fn request_close_terms(&mut self, ids: Vec<u64>) {
        let running: Vec<String> = ids
            .iter()
            .filter_map(|&id| self.term(id))
            .filter(|t| close_needs_confirm(t.program.as_deref()))
            .filter_map(|t| t.program.clone())
            .collect();
        if running.is_empty() {
            self.close_term_now(&ids);
        } else {
            self.dialog_focus = None;
            self.dialog = Some(Dialog::CloseTerm { ids, program: running.join(", ") });
        }
    }

    /// The window is closing (or Quit): asks first while terminals run.
    pub(super) fn request_quit(&mut self) -> Task<Message> {
        let asking = matches!(self.dialog, Some(Dialog::QuitTerms));
        if quit_question(self.terms.len()).is_some() && !asking {
            self.dialog_focus = None;
            self.dialog = Some(Dialog::QuitTerms);
            return Task::none();
        }
        self.quit()
    }

    /// `TerminalFolderToPanel`: the active panel goes to the folder of the
    /// shown (or last used) terminal; back to the panels.
    pub(super) fn term_folder_to_panel(&mut self) -> Task<Message> {
        let id = self.screen.term().copied().or(self.term_last);
        let Some(dir) = id.and_then(|id| self.term(id)).map(|t| t.dir.clone()) else {
            self.info("No built-in terminal is open");
            return Task::none();
        };
        self.hide_term();
        let side = self.active;
        self.start_nav_to(side, Nav::to(dir), true)
    }

    /// Ctrl+click on a link in a terminal: a web address opens in the
    /// browser, a folder in the active panel, a file is selected in its
    /// folder's panel; a file with a line opens in the editor at that line
    /// when the editor takes one (`editor::launch_at_line`). Nothing is run.
    fn open_term_link(&mut self, target: LinkTarget) -> Task<Message> {
        let path = match target {
            LinkTarget::Url(url) => {
                match rc_platform::open_url(&url) {
                    Ok(()) => self.info(format!("Opened {url}")),
                    Err(e) => self.error(format!("Can't open {url}: {e}")),
                }
                return Task::none();
            }
            LinkTarget::Path { path, line: Some(line) } if path.is_file() => {
                self.refresh_config();
                let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                match crate::editor::launch_at_line(&self.config.editor, &path, line) {
                    Some(Ok(_)) => {
                        self.info(format!("Editing {name} at line {line}"));
                        return Task::none();
                    }
                    Some(Err(e)) => {
                        self.error(format!("Can't start the editor: {e}"));
                        return Task::none();
                    }
                    // The editor takes no line: select the file instead.
                    None => path,
                }
            }
            LinkTarget::Path { path, .. } => path,
        };
        self.hide_term();
        self.start_nav_to(self.active, Nav::to(path), true)
    }

    /// `SendFolderToTerminal`: types `cd <folder>` into the focused terminal
    /// (from the panels: the one used last), or starts one there when none
    /// runs. The folder is the one under the cursor, else the panel's.
    pub(super) fn send_folder_to_terminal(&mut self) -> Task<Message> {
        let dir = {
            let panel = self.panel(self.active);
            match panel.current().filter(|entry| entry.is_dir()) {
                Some(entry) => entry.path.clone(),
                None => panel.path.clone(),
            }
        };
        if vfs::in_archive(&dir) {
            let place = if remote::is_remote(&dir) { "on a server" } else { "inside an archive" };
            self.error(format!("The built-in terminal only opens in local folders, not {place}"));
            return Task::none();
        }
        if !dir.is_dir() {
            self.error(format!("Can't change to {}: not a folder", dir.display()));
            return Task::none();
        }
        let target = self
            .screen
            .term()
            .copied()
            .or_else(|| self.term_last.filter(|&id| self.term(id).is_some()));
        let Some(id) = target else {
            self.refresh_config();
            let shell = self.config.builtin_shell();
            self.start_term(&dir, &shell);
            return Task::none();
        };
        let (family, refusal) = {
            let Some(t) = self.term(id) else { return Task::none() };
            let refusal = if let Some(program) = &t.program {
                Some(format!(
                    "Can't change the terminal's folder while {program} runs; end it, or open a new terminal here (Ctrl+Shift+T)"
                ))
            } else if t.alt_screen() {
                Some("Can't change the terminal's folder while a full-screen program runs; end it first".to_owned())
            } else if t.pending.is_some() {
                Some("Can't change the terminal's folder while a line for it is still waiting to be typed".to_owned())
            } else {
                None
            };
            (t.family, refusal)
        };
        if let Some(text) = refusal {
            self.error(text);
            return Task::none();
        }
        let line = cd_line(family, &dir);
        if let Some(t) = self.term_mut(id) {
            t.write(format!("{line}\r").into_bytes());
            t.dir = dir.clone();
        }
        self.info(format!("cd {}", dir.display()));
        Task::none()
    }

    /// Files dropped on a shown terminal: their paths go to the focused pane
    /// as a paste, like other terminals do (programs like Claude Code take
    /// dropped images and files this way).
    pub(super) fn term_drop(&mut self, sources: &[PathBuf]) {
        let Some(&id) = self.screen.term() else { return };
        if let Some(t) = self.term(id) {
            t.paste(&drop_text(sources));
        }
    }

    /// The text of the quit question (for the dialog).
    pub(super) fn quit_text(&self) -> String {
        quit_question(self.terms.len()).unwrap_or_else(|| "Quit?".to_owned())
    }

    /// The dialog's OK: ends terminals `ids` (those that still exist).
    pub(super) fn close_term_now(&mut self, ids: &[u64]) {
        for &id in ids {
            if self.term(id).is_some() {
                self.close_term(id);
            }
        }
    }

    /// Ends terminal `id` (dropping it stops its shell). Its pane closes and
    /// a neighbouring pane gets the focus; when it was the entry's last
    /// terminal the entry goes, and if it was shown the entry before it is
    /// shown, or Files when it was the first.
    fn close_term(&mut self, id: u64) {
        let tab = self.tab_of(id);
        let was_shown = self.screen.term() == Some(&id);
        let tab_shown = tab.is_some() && self.shown_tab() == tab;
        self.terms.retain(|t| t.id != id);
        let Some(tab) = tab else { return };
        let t = &mut self.term_tabs[tab];
        if t.panes.len() > 1 {
            if let Some((_, sibling)) = t.pane(id).and_then(|pane| t.panes.close(pane)) {
                if t.panes.get(t.focus).is_none() {
                    t.focus = sibling;
                }
            }
            let next = t.focused();
            if self.term_last == Some(id) {
                self.term_last = Some(next);
            }
            if was_shown {
                self.show_term(next);
            }
            return;
        }
        // Its only terminal: the entry goes.
        let fallback = fallback_after_close(&self.tab_order(), id);
        self.term_tabs.remove(tab);
        if self.term_last == Some(id) {
            self.term_last = fallback;
        }
        if tab_shown {
            self.screen = None;
            if let Some(next) = fallback {
                self.show_term(next);
            }
        }
    }

    /// Ends every terminal (at quit): no shell is left running.
    pub(super) fn close_terms(&mut self) {
        if self.screen.term().is_some() {
            self.screen = None;
        }
        // Ask all to stop first, so they wind down together.
        for t in &self.terms {
            let _ = t.chan.send(Msg::Shutdown);
        }
        self.terms.clear();
        self.term_tabs.clear();
    }

    pub(super) fn term_update(&mut self, msg: TermMsg) -> Task<Message> {
        let drawn = self.drawn_terms();
        // Whether a cue for terminal `id` is wanted (the user doesn't look at it).
        let focused = self.focused;
        let cue = |id: u64| wants_cue(drawn.contains(&id), focused);
        match msg {
            TermMsg::Show(Some(id)) => {
                self.leave_edits();
                self.show_term(id);
            }
            TermMsg::Show(None) => self.hide_term(),
            TermMsg::New => return self.new_term(),
            TermMsg::Close(id) => self.request_close_tab(id),
            TermMsg::PaneResize(pane_grid::ResizeEvent { split, ratio }) => {
                if let Some(tab) = self.shown_tab() {
                    self.term_tabs[tab].panes.resize(split, ratio);
                }
            }
            TermMsg::Mouse(id, mouse) => {
                // A press in another split pane gives it the keys.
                if matches!(mouse, Mouse::Down { .. }) && self.screen.term() != Some(&id) {
                    self.show_term(id);
                }
                let Some(t) = self.term_mut(id) else { return Task::none() };
                match t.mouse(mouse) {
                    MouseOutcome::None => {}
                    MouseOutcome::Copy(text) => return iced::clipboard::write(text),
                    MouseOutcome::Paste => {
                        return iced::clipboard::read().map(move |text| Message::Term(TermMsg::Paste(id, text)));
                    }
                }
            }
            TermMsg::ScrollTo(id, top) => {
                if let Some(t) = self.term_mut(id) {
                    let history = t.frame.history;
                    let offset = history.saturating_sub(top.round().max(0.0) as usize);
                    let delta = offset as i32 - t.frame.offset as i32;
                    if delta != 0 {
                        t.scroll(Scroll::Delta(delta));
                    }
                }
            }
            TermMsg::Paste(id, text) => {
                if let (Some(t), Some(text)) = (self.term(id), text.filter(|t| !t.is_empty())) {
                    t.paste(&text);
                }
            }
            TermMsg::SendPending(id) => {
                if let Some(t) = self.term_mut(id) {
                    t.send_pending();
                }
            }
            TermMsg::Info(id, info) => {
                let cue = cue(id);
                if let Some(t) = self.term_mut(id) {
                    if let Some(dir) = info.dir {
                        t.dir = dir;
                    }
                    if let Some(a) = t.activity.program(info.program.is_some(), Instant::now()).filter(|_| cue) {
                        t.attention = Some(a);
                    }
                    t.program = info.program;
                }
            }
            TermMsg::FindInput(id, text) => {
                if let Some(t) = self.term_mut(id) {
                    if let Some(find) = &mut t.find {
                        find.text = text;
                    }
                    t.find(None);
                }
            }
            TermMsg::OpenLink(target) => return self.open_term_link(target),
            TermMsg::Blink => {
                let typing = self.term_blink.input.elapsed() < BLINK;
                self.term_blink.off = !self.term_blink.off && !typing;
            }
            TermMsg::IdleCheck(id) => {
                let cue = cue(id);
                let Some(t) = self.term_mut(id) else { return Task::none() };
                match t.activity.check(Instant::now()) {
                    Check::Stop => {}
                    Check::Again(wait) => return delayed(wait, Message::Term(TermMsg::IdleCheck(id))),
                    Check::Idle => {
                        if term_debug() {
                            eprintln!("[term {id}] title steady: waiting");
                        }
                        if cue {
                            t.attention = Some(Attention::Waiting);
                        }
                    }
                }
            }
            TermMsg::Resize(id, cols, lines) => {
                let m = self.term_metrics();
                if let Some(t) = self.term_mut(id) {
                    t.resize(&m, cols, lines);
                    t.refresh();
                }
            }
            TermMsg::Event(id, event) => {
                let cue = cue(id);
                let m = self.term_metrics();
                let Some(t) = self.term_mut(id) else { return Task::none() };
                match event {
                    // (A late one for a terminal just hidden is dropped: showing
                    // it rebuilds the frame anyway.)
                    TermEvent::Wakeup => {
                        // A script waits for the shell's prompt.
                        if t.pending.is_some() && prompt_dir(&t.term.lock()).is_some() {
                            t.send_pending();
                        }
                        if drawn.contains(&id) {
                            t.refresh();
                        }
                    }
                    TermEvent::Title(title) => {
                        if term_debug() {
                            eprintln!("[term {id}] title {title:?}");
                        }
                        let check = t.activity.title(&title, Instant::now());
                        t.osc_title = meaningful_title(&title).then_some(title);
                        if let Some(wait) = check {
                            return delayed(wait, Message::Term(TermMsg::IdleCheck(id)));
                        }
                    }
                    TermEvent::ResetTitle => {
                        if term_debug() {
                            eprintln!("[term {id}] title reset");
                        }
                        t.osc_title = None;
                        if let Some(wait) = t.activity.title("", Instant::now()) {
                            return delayed(wait, Message::Term(TermMsg::IdleCheck(id)));
                        }
                    }
                    TermEvent::Bell => {
                        if term_debug() {
                            eprintln!("[term {id}] bell");
                        }
                        if cue {
                            t.attention = Some(Attention::Waiting);
                        }
                    }
                    // Answers to queries (cursor position, device attributes...):
                    // a program waits for them.
                    TermEvent::PtyWrite(text) => {
                        let _ = t.chan.send(Msg::Input(text.into_bytes().into()));
                    }
                    TermEvent::ColorRequest(index, format) => {
                        let pal = Palette::from_theme();
                        let color = t.term.lock().colors()[index].unwrap_or_else(|| to_rgb(pal.color(index)));
                        let _ = t.chan.send(Msg::Input(format(color).into_bytes().into()));
                    }
                    TermEvent::TextAreaSizeRequest(format) => {
                        let size = {
                            let term = t.term.lock();
                            m.window_size(term.columns(), term.screen_lines())
                        };
                        let _ = t.chan.send(Msg::Input(format(size).into_bytes().into()));
                    }
                    TermEvent::ClipboardStore(_, text) => return iced::clipboard::write(text),
                    TermEvent::ChildExit(status) => t.exit_code = status.code(),
                    TermEvent::Exit => {
                        let code = t.exit_code;
                        self.close_term(id);
                        match code {
                            Some(0) | None => self.info("The terminal's shell has exited"),
                            Some(code) => self.info(format!("The terminal's shell has exited (exit code {code})")),
                        }
                    }
                    // Pasting the clipboard on a program's request is left out
                    // on purpose; the rest needs nothing here.
                    TermEvent::ClipboardLoad(..)
                    | TermEvent::MouseCursorDirty
                    | TermEvent::CursorBlinkingChange => {}
                }
            }
        }
        Task::none()
    }

    /// Every key goes to the shell except the keys of the terminal commands
    /// (back to the panels, next/previous entry, new, close): what the keymap
    /// binds them to.
    pub(super) fn term_key(
        &mut self,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
        text: Option<&str>,
    ) -> Task<Message> {
        let Some(&id) = self.screen.term() else { return Task::none() };
        // The cursor shows while typing.
        self.term_blink = Blink::default();
        let reserved = [
            Command::ToggleTerminal,
            Command::NewTerminal,
            Command::NextTerminal,
            Command::PrevTerminal,
            Command::CloseTerminal,
            Command::TerminalFolderToPanel,
            Command::SendFolderToTerminal,
            Command::SplitTerminalRight,
            Command::SplitTerminalDown,
            Command::FocusTerminalLeft,
            Command::FocusTerminalRight,
            Command::FocusTerminalUp,
            Command::FocusTerminalDown,
            Command::MoveTerminalLeft,
            Command::MoveTerminalRight,
            Command::MoveTerminalUp,
            Command::MoveTerminalDown,
            Command::ZoomTerminalPane,
            Command::TerminalZoomIn,
            Command::TerminalZoomOut,
            Command::TerminalZoomReset,
            Command::NextTab,
            Command::PrevTab,
            // Settings (F10); closing them comes back here.
            Command::Configuration,
        ];
        if let Some(cmd) = self.keymap.command(key, mods).filter(|c| reserved.contains(c)) {
            match cmd {
                // Panel tabs in the panels, terminals here.
                Command::NextTab => self.cycle_among_terms(1),
                Command::PrevTab => self.cycle_among_terms(-1),
                _ => return self.run(cmd),
            }
            return Task::none();
        }
        let Some(t) = self.term(id) else { return Task::none() };
        let named = match key {
            keyboard::Key::Named(n) => Some(*n),
            _ => None,
        };
        let letter = |l: &str| matches!(key, keyboard::Key::Character(c) if c.eq_ignore_ascii_case(l));
        let (ctrl, alt, shift) = (mods.control(), mods.alt(), mods.shift());
        if ctrl && shift && !alt && letter("f") {
            if let Some(t) = self.term_mut(id) {
                t.find.get_or_insert_with(Find::default);
            }
            return Task::batch([text_input::focus(find_input_id()), text_input::select_all(find_input_id())]);
        }
        if t.find.is_some() {
            let step = match named {
                Some(Named::Escape) => {
                    if let Some(t) = self.term_mut(id) {
                        t.find = None;
                    }
                    return Task::none();
                }
                Some(Named::Enter) => Some(!shift),
                Some(Named::F3) => Some(!shift),
                Some(Named::ArrowUp) => Some(true),
                Some(Named::ArrowDown) => Some(false),
                _ => None,
            };
            if let Some(older) = step {
                if let Some(t) = self.term_mut(id) {
                    t.find(Some(older));
                }
                return Task::none();
            }
            let Some(t) = self.term(id) else { return Task::none() };
            let copy = (ctrl && shift && !alt && letter("c")) || (ctrl && !shift && !alt && named == Some(Named::Insert));
            if copy {
                return t.selected_text(false).map_or_else(Task::none, iced::clipboard::write);
            }
            // The rest is the find box's (typing) or nobody's.
            return Task::none();
        }
        // Keys the terminal keeps for itself (TERM_KEYS): copy, paste, scrolling.
        let copy = (ctrl && shift && !alt && letter("c")) || (ctrl && !shift && !alt && named == Some(Named::Insert));
        let copy_one_line = ctrl && shift && !alt && letter("j");
        if copy || copy_one_line {
            return match t.selected_text(copy_one_line) {
                Some(text) => iced::clipboard::write(text),
                None => Task::none(),
            };
        }
        if (ctrl && shift && !alt && letter("v")) || (shift && !ctrl && !alt && named == Some(Named::Insert)) {
            return iced::clipboard::read().map(move |text| Message::Term(TermMsg::Paste(id, text)));
        }
        if shift && !ctrl && !alt && !t.alt_screen() {
            let scroll = match named {
                Some(Named::PageUp) => Some(Scroll::PageUp),
                Some(Named::PageDown) => Some(Scroll::PageDown),
                Some(Named::Home) => Some(Scroll::Top),
                Some(Named::End) => Some(Scroll::Bottom),
                _ => None,
            };
            if let Some(scroll) = scroll {
                if let Some(t) = self.term_mut(id) {
                    t.scroll(scroll);
                }
                return Task::none();
            }
        }
        let Some(t) = self.term(id) else { return Task::none() };
        if let Some(bytes) = key_bytes(key, mods, text, t.app_cursor()) {
            t.write(bytes);
        }
        Task::none()
    }

    pub(super) fn term_view(&self, id: u64) -> Element<'_, Message> {
        let colors = theme::c();
        let Some(t) = self.term(id) else { return Space::new(Length::Fill, Length::Fill).into() };
        let Some(tab) = self.tab_of(id).map(|i| &self.term_tabs[i]) else {
            return Space::new(Length::Fill, Length::Fill).into();
        };
        let bindings = self.keymap.bindings();
        let back = keymap::Keymap::keys_for(&bindings, Command::ToggleTerminal).first().map(|k| k.to_string());
        // The terminal area's width (the sidebar takes the rest), less the footer's padding and spacing.
        let total = self.area_width() - 20.0 - 12.0;
        // The keys are in the sidebar's "(?)"; a footer only points out a missing way back.
        let footer = back.is_none().then(|| {
            row![
                Space::with_width(Length::Fill),
                text("Bind cm_ToggleTerminal to a key to get back to the panels")
                    .size(12)
                    .color(colors.muted)
                    .wrapping(Wrapping::None),
            ]
            .padding([2, 10])
            .align_y(iced::Alignment::Center)
            .height(Length::Fixed(FOOTER_H))
        });
        let split = tab.panes.len() > 1;
        let focus = tab.focus;
        let panes = pane_grid(&tab.panes, move |pane, &tid, zoomed| {
            // Split: a frame around each pane, in the accent colour for the focused one.
            let frame = (split && !zoomed).then_some(pane == focus);
            pane_grid::Content::new(self.term_pane(tid, pane == focus, frame))
        })
        .spacing(PANE_GAP)
        .on_resize(PANE_GRAB, |e| Message::Term(TermMsg::PaneResize(e)));
        let mut body = column![panes];
        if let Some(find) = &t.find {
            let status: Element<'_, Message> = if find.missing {
                text("not found").size(12).color(colors.error).into()
            } else {
                theme::key_hints_row::<Message>(
                    &[
                        HintOwned::new(["Enter"], "older"),
                        HintOwned::new(["Shift+Enter"], "newer"),
                        HintOwned::new(["Esc"], "close"),
                    ],
                    total - 320.0,
                )
            };
            body = body.push(
                container(
                    row![
                        text("Find:").size(13),
                        text_input("text in the scrollback", &find.text)
                            .id(find_input_id())
                            .on_input(move |s| Message::Term(TermMsg::FindInput(id, s)))
                            .padding(4)
                            .size(13)
                            .width(Length::Fixed(260.0))
                            .style(theme::input),
                        status,
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                )
                .padding([3, 10])
                .width(Length::Fill)
                .style(filled(colors.header)),
            );
        }
        container(body.push_maybe(footer))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(t.frame.bg))
            .into()
    }

    /// Terminal `id`'s grid and scrollbar in its pane; `focus`: it gets the
    /// keys; `frame`: drawn framed (split panes), in the accent colour if `true`.
    fn term_pane(&self, id: u64, focus: bool, frame: Option<bool>) -> Element<'_, Message> {
        let Some(t) = self.term(id) else { return Space::new(Length::Fill, Length::Fill).into() };
        // An unfocused pane draws its cursor hollow, like an unfocused window.
        let focused = self.focused && focus;
        let cursor_off = self.term_blink.off && focused && t.frame.cursor_blinks && self.config.terminal_cursor_blink;
        let grid = Element::new(Grid { id, frame: &t.frame, focused, cursor_off, metrics: self.term_metrics() });
        // The bar's position is the history line at the top of the view.
        let (history, offset, lines) = (t.frame.history, t.frame.offset, t.frame.lines);
        let grid = crate::scrollbar::attach(
            grid,
            history.saturating_sub(offset) as f32,
            lines as f32,
            (history + lines) as f32,
            move |top| Message::Term(TermMsg::ScrollTo(id, top)),
        );
        let c = theme::c();
        let bg = t.frame.bg;
        let border = match frame {
            Some(true) => c.accent,
            Some(false) => c.border,
            None => Color::TRANSPARENT,
        };
        container(grid)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(if frame.is_some() { 1 } else { 0 })
            .style(move |_| container::Style {
                background: Some(Background::Color(bg)),
                border: iced::Border { color: border, width: 1.0, radius: 0.0.into() },
                ..container::Style::default()
            })
            .into()
    }

    /// The sidebar on the right: Files, one entry per terminal (middle-click
    /// or x closes it) and "+".
    pub(super) fn term_side_view(&self) -> Element<'_, Message> {
        let c = theme::c();
        let shown = self.screen.term().copied();
        // Over the panels only short entries fit; the x is left out (middle-click still closes).
        let collapsed = self.screen.is_none();
        // Rows have fixed heights so the accent bar beside them matches and the
        // list doesn't jump when a program starts or ends (expanded terminal
        // entries always have room for the program line).
        let line_h = 24.0;
        let term_h = if collapsed { line_h } else { 34.0 };
        let entry = |content: Element<'static, Message>,
                     tip: Option<String>,
                     current: bool,
                     press: TermMsg,
                     close: Option<u64>,
                     h: f32|
         -> Element<'static, Message> {
            let content = container(content).width(Length::Fill).height(Length::Fill).align_y(alignment::Vertical::Center);
            let content = if collapsed { content.align_x(alignment::Horizontal::Center) } else { content };
            let name = button(content)
                .on_press(Message::Term(press))
                .width(Length::Fill)
                .height(Length::Fixed(h))
                .padding(if collapsed { [0, 0] } else { [0, 6] })
                .style(theme::side_entry(current));
            // The current entry's accent bar; a transparent one keeps the others lined up.
            let accent = if current { c.accent } else { Color::TRANSPARENT };
            let bar = container(Space::new(Length::Fixed(3.0), Length::Fixed(h - 6.0))).style(filled(accent));
            let mut line = Row::new().spacing(2).align_y(iced::Alignment::Center).push(bar).push(name);
            let mut hover_x = None;
            if let Some(id) = close.filter(|_| !collapsed) {
                let x = button(text("\u{d7}").size(13))
                    .on_press(Message::Term(TermMsg::Close(id)))
                    .padding([2, 6])
                    .style(theme::flat);
                if current {
                    line = line.push(x);
                } else {
                    // An invisible x of the same size keeps the label in place; the real
                    // one is laid over it only while the mouse is over the entry.
                    line = line.push(container(text("\u{d7}").size(13).color(Color::TRANSPARENT)).padding([2, 6]));
                    hover_x = Some(
                        container(x)
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .align_x(alignment::Horizontal::Right)
                            .align_y(alignment::Vertical::Center),
                    );
                }
            }
            let line: Element<'_, Message> = match hover_x {
                Some(x) => iced::widget::hover(line, x),
                None => line.into(),
            };
            let mut line: Element<'_, Message> = container(line).clip(true).width(Length::Fill).into();
            if let Some(tip) = tip {
                line = tooltip(
                    line,
                    container(text(tip).size(12)).padding([3, 8]).max_width(420).style(theme::drag_label),
                    tooltip::Position::Left,
                )
                .into();
            }
            let line: Element<'_, Message> = match close {
                Some(id) => mouse_area(line).on_middle_press(Message::Term(TermMsg::Close(id))).into(),
                // Files sits in its own band, above a separator line.
                None => container(line).padding([2, 0]).width(Length::Fill).style(filled(c.header)).into(),
            };
            line
        };
        let mut list = Column::new().spacing(2).width(Length::Fill);
        // Files: the system's folder icon (what the panels show for a folder), the word if there is none.
        let files: Element<'static, Message> = match self.icon(&std::env::temp_dir(), true) {
            // Centred in its band in both modes.
            Some(icon) => container(iced::widget::image(icon).width(16.0).height(16.0))
                .width(Length::Fill)
                .align_x(alignment::Horizontal::Center)
                .into(),
            None => text(if collapsed { "F" } else { "Files" })
                .size(13)
                .font(Font { weight: Weight::Bold, ..Font::DEFAULT })
                .into(),
        };
        let files_tip = Some("Files: back to the panels".to_owned());
        list = list.push(entry(files, files_tip, shown.is_none(), TermMsg::Show(None), None, line_h));
        list = list.push(
            container(Space::new(Length::Fill, Length::Fixed(1.0))).width(Length::Fill).style(filled(c.border)),
        );
        if !collapsed {
            // Left edge = bar slot 3 + spacing 2 + button padding 6, like the entry labels.
            let heading = text("TERMINALS").size(10).color(c.muted).font(Font { weight: Weight::Bold, ..Font::DEFAULT });
            list = list.push(container(heading).padding([4, 11]));
        }
        for (i, tab) in self.term_tabs.iter().enumerate() {
            let Some(t) = self.term(tab.focused()) else { continue };
            let others: Vec<&TermSession> =
                tab.ids().into_iter().filter(|&id| id != t.id).filter_map(|id| self.term(id)).collect();
            let full = term_label(&t.dir, t.program.as_deref(), LABEL_CHARS);
            // Any pane's cue; waiting wins over finished.
            let cues: Vec<Attention> = std::iter::once(t).chain(others.iter().copied()).filter_map(|t| t.attention).collect();
            let attention = cues.iter().copied().find(|a| matches!(a, Attention::Waiting)).or(cues.first().copied());
            // A cue: a dot, in green (finished) or amber (waiting); the label keeps its colour.
            let cue = attention.map(|a| match a {
                Attention::Finished => c.success,
                Attention::Waiting => c.selected,
            });
            let dot = |color: Color| text("\u{25cf}").size(10).color(color);
            let content: Element<'static, Message> = if collapsed {
                // A number badge; the cue colours its border and fill.
                container(text(format!("{}", i + 1)).size(12))
                    .padding([0, 5])
                    .style(theme::badge(cue))
                    .into()
            } else {
                // Folder name, the running program below it in a dimmer, smaller font.
                let folder = text(cut(&folder_title(&t.dir), LABEL_CHARS)).size(12).wrapping(Wrapping::None);
                let mut names = Column::new().push(folder);
                if let Some(program) = &t.program {
                    let program = text(cut(program, LABEL_CHARS)).size(11).color(c.muted);
                    names = names.push(program.wrapping(Wrapping::None));
                }
                let mut content = Row::new()
                    .spacing(4)
                    .align_y(iced::Alignment::Center)
                    .push(container(names).width(Length::Fill).clip(true));
                if let Some(color) = cue {
                    content = content.push(dot(color));
                }
                // Split: how many panes the entry holds.
                if !others.is_empty() {
                    content = content.push(text(format!("({})", others.len() + 1)).size(11).color(c.muted));
                }
                content.into()
            };
            let mut tip = t.dir.display().to_string();
            if collapsed {
                tip = format!("{}: {full}\n{tip}", i + 1);
            }
            if let Some(program) = &t.program {
                tip.push_str(&format!("\nRunning: {program}"));
            }
            if let Some(title) = &t.osc_title {
                tip.push_str(&format!("\nTitle: {title}"));
            }
            if let Some(a) = t.attention {
                tip.push_str(&format!("\n{}", a.tip()));
            }
            if !others.is_empty() {
                tip.push_str(&format!("\n\nSplit with {}:", others.len()));
                for o in &others {
                    tip.push_str(&format!("\n{}", term_label(&o.dir, o.program.as_deref(), 60)));
                    if let Some(a) = o.attention {
                        tip.push_str(&format!(" ({})", a.tip()));
                    }
                }
            }
            let (current, press) = (shown == Some(t.id), TermMsg::Show(Some(t.id)));
            list = list.push(entry(content, Some(tip), current, press, Some(t.id), term_h));
        }
        let new_content: Element<'static, Message> = if collapsed {
            text("+").size(13).into()
        } else {
            let bindings = self.keymap.bindings();
            let key = keymap::Keymap::keys_for(&bindings, Command::NewTerminal).first().map(|k| k.to_string());
            let mut r = Row::new()
                .spacing(8)
                .align_y(iced::Alignment::Center)
                // "+ New terminal" plus "Ctrl+Shift+T" don't both fit in the sidebar: the key got clipped.
                .push(text(if key.is_some() { "+ New" } else { "+ New terminal" }).size(12).wrapping(Wrapping::None));
            if let Some(key) = key {
                r = r.push(Space::with_width(Length::Fill));
                r = r.push(text(key).size(11).color(c.muted).wrapping(Wrapping::None));
            }
            r.into()
        };
        let content = container(new_content).width(Length::Fill).height(Length::Fill).align_y(alignment::Vertical::Center);
        let content = if collapsed { content.align_x(alignment::Horizontal::Center) } else { content.clip(true) };
        let new_button = button(content)
            .on_press(Message::Term(TermMsg::New))
            .width(Length::Fill)
            .height(Length::Fixed(line_h))
            .padding(if collapsed { [0, 0] } else { [0, 6] })
            .style(theme::side_entry(false));
        // The same 3px bar slot + spacing as the entries, so the label lines up with theirs.
        list = list.push(
            Row::new()
                .spacing(2)
                .align_y(iced::Alignment::Center)
                .push(Space::with_width(Length::Fixed(3.0)))
                .push(new_button),
        );
        let line = container(Space::new(Length::Fixed(1.0), Length::Fill)).style(filled(c.border));
        // Over a terminal: the "(?)" with every terminal key at the bottom.
        let mut side = Column::new().push(container(list).height(Length::Fill));
        if !collapsed {
            let bindings = self.keymap.bindings();
            let key = |cmd| keymap::Keymap::keys_for(&bindings, cmd).first().map(|k| k.to_string());
            let tip = theme::key_hints_tip_owned::<Message>("keys", &term_key_tip(&key));
            side = side.push(container(tip).width(Length::Fill).align_x(alignment::Horizontal::Center).padding([2, 4]));
        }
        row![line, container(side).padding(4).width(Length::Fill).height(Length::Fill).style(filled(c.bg))]
            .width(Length::Fixed(self.term_side_w()))
            .height(Length::Fill)
            .into()
    }
}

/// Entry `index` of `count` moved by `delta`, around.
fn cycle(index: usize, count: usize, delta: isize) -> usize {
    (index as isize + delta).rem_euclid(count.max(1) as isize) as usize
}

/// The terminal after (`delta` 1) or before (-1) `current` in `order`, around;
/// `None` when there is no other terminal.
fn next_terminal(order: &[u64], current: u64, delta: isize) -> Option<u64> {
    if order.len() < 2 {
        return None;
    }
    let pos = order.iter().position(|&id| id == current)?;
    Some(order[cycle(pos, order.len(), delta)])
}

/// The terminal to show when `closed` of `order` ends: the one before it,
/// or Files (`None`) when it was the first.
fn fallback_after_close(order: &[u64], closed: u64) -> Option<u64> {
    let pos = order.iter().position(|&id| id == closed)?;
    pos.checked_sub(1).map(|i| order[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::Key;
    use iced::keyboard::Modifiers as M;

    /// Measures how the grid's font resolves: run with
    /// `cargo test -p rustcommander measure_fonts -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_fonts() {
        use iced::advanced::graphics::text::{font_system, Paragraph};
        use iced::advanced::text::Paragraph as _;
        load_fonts();
        let fonts = [
            ("MONOSPACE", Font::MONOSPACE),
            ("bundled", BUNDLED_FONT),
            ("Consolas", Font::with_name("Consolas")),
        ];
        for (name, base) in fonts {
            for (style, w, it) in [("regular", Weight::Normal, false), ("bold", Weight::Bold, false),
                ("italic", Weight::Normal, true), ("bold italic", Weight::Bold, true)] {
                let font = Font { weight: w, style: if it { FontStyle::Italic } else { FontStyle::Normal }, ..base };
                let sample = "0".repeat(100);
                let p = Paragraph::with_text(iced::advanced::Text {
                    content: sample.as_str(),
                    bounds: Size::INFINITY,
                    size: Pixels(14.0),
                    line_height: LineHeight::Absolute(Pixels(18.0)),
                    font,
                    horizontal_alignment: alignment::Horizontal::Left,
                    vertical_alignment: alignment::Vertical::Top,
                    shaping: Shaping::Basic,
                    wrapping: Wrapping::None,
                });
                let mut face = String::new();
                let mut y = 0.0;
                if let Some(run) = p.buffer().layout_runs().next() {
                    y = run.line_y;
                    if let Some(g) = run.glyphs.first() {
                        let mut fs = font_system().write().unwrap();
                        if let Some(f) = fs.raw().db().face(g.font_id) {
                            face = format!("{} ({:?} {:?})", f.post_script_name, f.weight, f.style);
                        }
                    }
                }
                println!("{name:14} {style:12} w={:.4} baseline={y:.2} face={face}", p.min_width() / 100.0);
            }
        }
        for base in [Font::MONOSPACE, BUNDLED_FONT] {
            let m = Metrics::new(base, 14.0);
            for (bold, italic) in [(true, false), (false, true), (true, true)] {
                let (font, synthetic) = m.style_font(bold, italic);
                println!("bold={bold} italic={italic}: drawn with {font:?}, synthetic bold {synthetic}");
            }
        }
    }

    #[test]
    fn scripts_are_known_by_extension() {
        assert_eq!(script_kind(Path::new(r"C:\a\b.ps1")), Some(Script::PowerShell));
        assert_eq!(script_kind(Path::new(r"C:\a\B.PS1")), Some(Script::PowerShell));
        assert_eq!(script_kind(Path::new("run.Cmd")), Some(Script::Batch));
        assert_eq!(script_kind(Path::new("run.BAT")), Some(Script::Batch));
        for no in ["a.psm1", "a.ps1.txt", "ps1", "a.exe", "cmd", "a."] {
            assert_eq!(script_kind(Path::new(no)), None, "{no}");
        }
    }

    #[test]
    fn shell_families() {
        assert_eq!(shell_family("powershell.exe -NoLogo"), ShellFamily::WindowsPowerShell);
        assert_eq!(shell_family(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#), ShellFamily::Pwsh);
        assert_eq!(shell_family("PWSH"), ShellFamily::Pwsh);
        assert_eq!(shell_family(r"C:\Windows\System32\CMD.EXE /k"), ShellFamily::Cmd);
        assert_eq!(shell_family("bash.exe -l"), ShellFamily::Other);
        assert_eq!(shell_family(""), ShellFamily::Other);
    }

    #[test]
    fn script_lines() {
        let dir = Path::new(r"C:\Scripts");
        let (shell, line) = script_run("powershell.exe -NoLogo", Script::PowerShell, dir, "it's.ps1");
        assert_eq!(shell, "powershell.exe -NoLogo -ExecutionPolicy Bypass");
        assert_eq!(line, "& '.\\it''s.ps1'");
        let (shell, line) = script_run("pwsh -NoLogo", Script::PowerShell, dir, "build-all_2.ps1");
        assert_eq!((shell.as_str(), line.as_str()), ("pwsh -NoLogo -ExecutionPolicy Bypass", ".\\build-all_2.ps1"));
        // A policy of its own or a trailing command: left as it is.
        assert_eq!(script_run("pwsh -ep RemoteSigned", Script::PowerShell, dir, "a.ps1").0, "pwsh -ep RemoteSigned");
        assert_eq!(script_run("pwsh -NoExit -Command x", Script::PowerShell, dir, "a.ps1").0, "pwsh -NoExit -Command x");
        // Batch files need no policy.
        assert_eq!(script_run("pwsh", Script::Batch, dir, "go.cmd"), ("pwsh".into(), ".\\go.cmd".into()));
        assert_eq!(script_run("pwsh", Script::Batch, dir, "x & y.cmd").1, "& '.\\x & y.cmd'");
        assert_eq!(script_run("pwsh", Script::Batch, dir, "\u{2019}q.bat").1, "& '.\\\u{2019}\u{2019}q.bat'");
        assert_eq!(script_run("cmd.exe", Script::Batch, dir, "x & y.cmd"), ("cmd.exe".into(), "\"x & y.cmd\"".into()));
        assert_eq!(
            script_run("cmd.exe", Script::PowerShell, dir, "a b.ps1").1,
            "powershell -NoProfile -ExecutionPolicy Bypass -File \"a b.ps1\""
        );
        // cmd can't be in a UNC folder: the full path.
        let unc = Path::new(r"\\server\share\tools");
        assert_eq!(script_run("cmd", Script::Batch, unc, "go.bat").1, r#""\\server\share\tools\go.bat""#);
        // Another shell: one that runs the script.
        assert_eq!(script_run("bash -l", Script::Batch, dir, "go.bat"), ("cmd.exe".into(), "\"go.bat\"".into()));
        let (shell, line) = script_run("bash -l", Script::PowerShell, dir, "go.ps1");
        assert_eq!(shell, format!("{} -ExecutionPolicy Bypass", crate::config::DEFAULT_BUILTIN_SHELL));
        assert_eq!(line, ".\\go.ps1");
    }

    #[test]
    fn external_script_commands() {
        let dir = Path::new(r"C:\Scripts");
        assert_eq!(
            external_script_command(Script::PowerShell, dir, "a b.ps1"),
            ("powershell.exe", "-NoExit -ExecutionPolicy Bypass -File \"a b.ps1\"".to_owned())
        );
        assert_eq!(external_script_command(Script::Batch, dir, "x&y.cmd"), ("cmd.exe", "/k \"\"x&y.cmd\"\"".to_owned()));
        let unc = Path::new(r"\\srv\s");
        assert_eq!(external_script_command(Script::Batch, unc, "a.bat").1, r#"/k ""\\srv\s\a.bat"""#);
    }

    #[test]
    fn powershell_quoting() {
        assert_eq!(ps_quote("a"), "'a'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote("\u{2018}x\u{201b}"), "'\u{2018}\u{2018}x\u{201b}\u{201b}'");
        assert_eq!(ps_quote("$a `b\""), "'$a `b\"'");
    }

    #[test]
    fn cd_lines() {
        let dir = Path::new(r"C:\My Files");
        let ps = r"Set-Location -LiteralPath 'C:\My Files'";
        assert_eq!(cd_line(ShellFamily::WindowsPowerShell, dir), ps);
        assert_eq!(cd_line(ShellFamily::Pwsh, dir), ps);
        assert_eq!(cd_line(ShellFamily::WindowsPowerShell, Path::new(r"C:\it's")), r"Set-Location -LiteralPath 'C:\it''s'");
        assert_eq!(cd_line(ShellFamily::Cmd, dir), r#"cd /d "C:\My Files""#);
        assert_eq!(cd_line(ShellFamily::Cmd, Path::new(r"C:\a&b\100%")), r#"cd /d "C:\a&b\100%""#);
        assert_eq!(cd_line(ShellFamily::Cmd, Path::new(r"\\srv\share\x")), r#"pushd "\\srv\share\x""#);
        assert_eq!(cd_line(ShellFamily::Other, Path::new("/home/a b")), "cd -- '/home/a b'");
        assert_eq!(cd_line(ShellFamily::Other, Path::new("/home/it's")), r"cd -- '/home/it'\''s'");
        assert_eq!(cd_line(ShellFamily::Other, Path::new("/srv/ünïcode")), "cd -- '/srv/ünïcode'");
    }

    fn named(n: Named, m: M) -> Option<Vec<u8>> {
        key_bytes(&Key::Named(n), m, None, false)
    }

    fn ch(c: &str, m: M, text: Option<&str>) -> Option<Vec<u8>> {
        key_bytes(&Key::Character(c.into()), m, text, false)
    }

    #[test]
    fn key_tip_lists_bound_keys() {
        let k = |s: &str| Some(s.to_owned());
        let same = [k("Alt+Shift+Left"), k("Alt+Shift+Right"), k("Alt+Shift+Up"), k("Alt+Shift+Down")];
        assert_eq!(arrow_keys(same), ["Alt+Shift+arrows"]);
        assert_eq!(arrow_keys([k("Ctrl+Left"), None, k("Alt+Up"), None]), ["Ctrl+Left", "Alt+Up"]);
        let keymap = keymap::Keymap::new(&Default::default()).unwrap();
        let bindings = keymap.bindings();
        let key = |cmd| keymap::Keymap::keys_for(&bindings, cmd).first().map(|k| k.to_string());
        let tip = term_key_tip(&key);
        let find = |action: &str| tip.iter().find(|h| h.action == action).map(|h| h.keys.clone());
        assert_eq!(find("move the pane (swap, or to that edge)").unwrap(), ["Ctrl+Alt+Shift+arrows"]);
        assert_eq!(find("find text in the scrollback").unwrap(), ["Ctrl+Shift+F"]);
        assert!(find("back to the panels").is_some());
    }

    #[test]
    fn scrollback_search() {
        assert_eq!(regex_escape("a.b*(c)"), "a\\.b\\*\\(c\\)");
        // 30 lines on a 5-line grid: most are in the history.
        let text: String = (0..30).map(|i| format!("line {i} x.y\r\n")).collect();
        let mut term = small_term(20, 5, &text);
        let found = |term: &Term<Proxy>| {
            let r = term.selection.as_ref().and_then(|s| s.to_range(term)).unwrap();
            (r.start.line.0, r.start.column.0, r.end.column.0)
        };
        // The newest match first; the last line written is the empty prompt line.
        assert!(search(&mut term, "x.y", None));
        let newest = found(&term);
        assert_eq!((newest.1, newest.2), (8, 10));
        // Older goes up a line, into the history, and scrolls there.
        assert!(search(&mut term, "x.y", Some(true)));
        assert_eq!(found(&term).0, newest.0 - 1);
        // Case-insensitive for lower-case text; refining keeps the match.
        term.selection = None;
        assert!(search(&mut term, "line 1", None));
        let (line, col, _) = found(&term);
        assert_eq!(col, 0);
        assert!(search(&mut term, "line 1", Some(true)));
        assert!(found(&term).0 < line);
        assert!(search(&mut term, "line 1", Some(false)));
        assert_eq!(found(&term).0, line);
        assert!(term.grid().display_offset() > 0);
        // A literal dot: "x.y" matches, "xzy" doesn't; capitals make it case-sensitive.
        assert!(!search(&mut term, "xzy", None));
        assert!(!search(&mut term, "X.Y", None));
        assert!(search(&mut term, "x.y", None));
    }

    fn small_term(cols: usize, lines: usize, input: &str) -> Term<Proxy> {
        let (tx, _rx) = std_mpsc::channel();
        let proxy = Proxy { tx, wakeup: Arc::new(AtomicBool::new(false)) };
        let mut term = Term::new(TermConfig::default(), &TermSize::new(cols, lines), proxy);
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        parser.advance(&mut term, input.as_bytes());
        term
    }

    #[test]
    fn split_tabs_track_focus_and_zoom() {
        let mut tab = TermTab::new(3);
        assert_eq!((tab.ids(), tab.focused()), (vec![3], 3));
        let (pane, _) = tab.panes.split(pane_grid::Axis::Vertical, tab.focus, 8).unwrap();
        tab.focus = pane;
        assert_eq!((tab.ids(), tab.focused(), tab.drawn()), (vec![3, 8], 8, vec![3, 8]));
        assert_eq!(tab.panes.adjacent(tab.focus, pane_grid::Direction::Left).and_then(|p| tab.panes.get(p)), Some(&3));
        tab.panes.maximize(tab.focus);
        assert_eq!(tab.drawn(), vec![8]);
        let (_, sibling) = tab.panes.close(pane).unwrap();
        assert_eq!((tab.panes.get(sibling), tab.drawn()), (Some(&3), vec![3]));
        assert_eq!(tab.pane(8), None);
    }

    #[test]
    fn split_panes_move() {
        use pane_grid::{Axis, Direction, Node};
        let at = |tab: &TermTab, d| tab.panes.adjacent(tab.focus, d).and_then(|p| tab.panes.get(p)).copied();
        let mut tab = TermTab::new(3);
        assert!(!tab.move_focused(Direction::Left));
        // 3 | 8, focus on 8.
        let (pane, _) = tab.panes.split(Axis::Vertical, tab.focus, 8).unwrap();
        tab.focus = pane;
        // Swap with the left neighbour: 8 | 3, focus stays on 8.
        assert!(tab.move_focused(Direction::Left));
        assert_eq!((tab.focused(), at(&tab, Direction::Right)), (8, Some(3)));
        // Already alone on the left edge: nothing to do.
        assert!(!tab.move_focused(Direction::Left));
        // Nothing below: to the bottom edge, 3 over 8.
        assert!(tab.move_focused(Direction::Down));
        assert_eq!((tab.focused(), at(&tab, Direction::Up)), (8, Some(3)));
        assert!(matches!(tab.panes.layout(), Node::Split { axis: Axis::Horizontal, .. }));
        assert!(!tab.move_focused(Direction::Down));
        // Back up: 8 over 3, then to the right edge: 3 | 8.
        assert!(tab.move_focused(Direction::Up));
        assert_eq!(at(&tab, Direction::Down), Some(3));
        assert!(tab.move_focused(Direction::Right));
        assert_eq!((tab.focused(), at(&tab, Direction::Left)), (8, Some(3)));
        assert_eq!(tab.ids(), vec![3, 8]);
    }

    #[test]
    fn ctrl_tab_cycles_terminals_only() {
        assert_eq!(next_terminal(&[5], 5, 1), None);
        assert_eq!(next_terminal(&[], 5, 1), None);
        assert_eq!(next_terminal(&[5, 7, 9], 5, 1), Some(7));
        assert_eq!(next_terminal(&[5, 7, 9], 9, 1), Some(5));
        assert_eq!(next_terminal(&[5, 7, 9], 5, -1), Some(9));
        assert_eq!(next_terminal(&[5, 7, 9], 7, -1), Some(5));
    }

    #[test]
    fn paste_without_bracketed_mode() {
        assert_eq!(paste_bytes("a\r\nb\nc\rd", false), b"a\rb\rc\rd");
        assert_eq!(paste_bytes("x\x1by", false), b"x\x1by");
    }

    #[test]
    fn dropped_paths_are_quoted_when_needed() {
        let paths = [PathBuf::from(r"C:\img\shot.png"), PathBuf::from(r"C:\My Files\a b.txt"), PathBuf::from(r"D:\x&y")];
        assert_eq!(drop_text(&paths), r#"C:\img\shot.png "C:\My Files\a b.txt" "D:\x&y""#);
        assert_eq!(drop_text(&[]), "");
    }

    #[test]
    fn paste_in_bracketed_mode() {
        assert_eq!(paste_bytes("ls\r\npwd", true), b"\x1b[200~ls\rpwd\x1b[201~");
        // An ESC can't end the bracket early.
        assert_eq!(paste_bytes("a\x1b[201~b", true), b"\x1b[200~a[201~b\x1b[201~");
    }

    #[test]
    fn minimized_window_doesnt_resize_the_grid() {
        // A minimized window lays the grid out at 0x0: no resize is sent.
        let m = Metrics::new(Font::MONOSPACE, 14.0);
        assert!(!m.fits_grid(Size::ZERO));
        assert!(!m.fits_grid(Size::new(800.0, 0.0)));
        assert!(!m.fits_grid(Size::new(0.0, 600.0)));
        assert!(m.fits_grid(Size::new(800.0, 600.0)));
        // The smallest area that fits gives the smallest grid, 2x1.
        let side = 2.0 * PAD + crate::scrollbar::WIDTH;
        let min = Size::new(side + 2.0 * m.cell_w + 0.5, 2.0 * PAD + m.cell_h + 0.5);
        assert!(m.fits_grid(min));
        assert_eq!(m.grid_size(min), (2, 1));
    }

    #[test]
    fn wheel_in_full_screen_programs_sends_arrows() {
        assert_eq!(wheel_arrows(false, 3), b"\x1b[A\x1b[A\x1b[A");
        assert_eq!(wheel_arrows(true, -2), b"\x1bOB\x1bOB");
    }

    #[test]
    fn wheel_reports_for_mouse_tracking_programs() {
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let none = M::empty();
        assert_eq!(wheel_reports(sgr, 2, 4, 9, none), b"\x1b[<64;10;5M\x1b[<64;10;5M");
        assert_eq!(wheel_reports(sgr, -1, 0, 0, none), b"\x1b[<65;1;1M");
        assert_eq!(wheel_reports(sgr, 0, 0, 0, none), b"");
        assert_eq!(wheel_reports(sgr, 1, 0, 0, M::CTRL), b"\x1b[<80;1;1M");
        let x10 = TermMode::MOUSE_DRAG;
        assert_eq!(wheel_reports(x10, 1, 0, 300, none), [0x1b, b'[', b'M', 96, 255, 33]);
    }

    #[test]
    fn mouse_reports_sgr_and_x10() {
        let none = M::empty();
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        // Press and release, left/middle/right; columns then rows, from 1.
        assert_eq!(mouse_report(sgr, 0, true, 4, 9, none), b"\x1b[<0;10;5M");
        assert_eq!(mouse_report(sgr, 0, false, 4, 9, none), b"\x1b[<0;10;5m");
        assert_eq!(mouse_report(sgr, 1, true, 0, 0, none), b"\x1b[<1;1;1M");
        assert_eq!(mouse_report(sgr, 2, false, 0, 0, none), b"\x1b[<2;1;1m");
        // Drag (+32), a move with no button (35), modifiers.
        assert_eq!(mouse_report(sgr, 32, true, 1, 2, none), b"\x1b[<32;3;2M");
        assert_eq!(mouse_report(sgr, 35, true, 1, 2, none), b"\x1b[<35;3;2M");
        assert_eq!(mouse_report(sgr, 0, true, 0, 0, M::SHIFT), b"\x1b[<4;1;1M");
        assert_eq!(mouse_report(sgr, 0, true, 0, 0, M::ALT | M::CTRL), b"\x1b[<24;1;1M");
        // SGR doesn't clamp.
        assert_eq!(mouse_report(sgr, 0, true, 0, 499, none), b"\x1b[<0;500;1M");
        // X10: 32 + button, 32 + 1 + cell; a release is button 3.
        let x10 = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(mouse_report(x10, 0, true, 4, 9, none), [0x1b, b'[', b'M', 32, 42, 37]);
        assert_eq!(mouse_report(x10, 2, false, 4, 9, none), [0x1b, b'[', b'M', 35, 42, 37]);
        assert_eq!(mouse_report(x10, 32, true, 0, 0, M::CTRL), [0x1b, b'[', b'M', 80, 33, 33]);
        assert_eq!(mouse_report(x10, 0, true, 300, 300, none), [0x1b, b'[', b'M', 32, 255, 255]);
        // UTF-8 (1005): cells past 94 as two-byte characters (300 -> U+014D).
        let utf8 = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        assert_eq!(mouse_report(utf8, 0, true, 0, 300, none), [0x1b, b'[', b'M', 32, 0xc5, 0x8d, 33]);
    }

    #[test]
    fn shift_keeps_the_mouse_ours_while_a_program_tracks_it() {
        for tracking in [TermMode::MOUSE_REPORT_CLICK, TermMode::MOUSE_DRAG, TermMode::MOUSE_MOTION] {
            let mode = tracking | TermMode::SGR_MOUSE | TermMode::ALT_SCREEN;
            assert!(reports_mouse(mode, M::empty()));
            assert!(reports_mouse(mode, M::CTRL | M::ALT));
            assert!(!reports_mouse(mode, M::SHIFT));
        }
        // No tracking: always ours.
        let plain = TermMode::SGR_MOUSE | TermMode::ALT_SCREEN;
        assert!(!reports_mouse(plain, M::empty()));
        assert!(!reports_mouse(TermMode::empty(), M::empty()));
    }

    #[test]
    fn selection_text_and_cells() {
        let mut term = small_term(20, 3, "hello world\r\nsecond line");
        let at = |line, col| GridPoint::new(Line(line), GridColumn(col));
        let mut selection = Selection::new(SelectionType::Simple, at(0, 6), Side::Left);
        selection.update(at(0, 10), Side::Right);
        term.selection = Some(selection);
        assert_eq!(term.selection_to_string().as_deref(), Some("world"));
        let range = term.selection.as_ref().and_then(|s| s.to_range(&term)).unwrap();
        assert_eq!(selection_cells(&range, 0, 20, 3), vec![(0, 6, 5)]);
        // A word (double-click) and a line (triple-click).
        term.selection = Some(Selection::new(SelectionType::Semantic, at(0, 7), Side::Left));
        assert_eq!(term.selection_to_string().as_deref(), Some("world"));
        term.selection = Some(Selection::new(SelectionType::Lines, at(1, 3), Side::Left));
        assert_eq!(term.selection_to_string().as_deref(), Some("second line\n"));
        // A click without a drag selects nothing.
        let click = Selection::new(SelectionType::Simple, at(0, 2), Side::Left);
        assert!(click.is_empty());
    }

    #[test]
    fn copy_joins_lines_filled_to_the_edge() {
        let at = |line, col| GridPoint::new(Line(line), GridColumn(col));
        let select = |term: &mut Term<Proxy>, from: (i32, usize), to: (i32, usize)| {
            let mut selection = Selection::new(SelectionType::Simple, at(from.0, from.1), Side::Left);
            selection.update(at(to.0, to.1), Side::Right);
            term.selection = Some(selection);
            selection_text(term).unwrap()
        };
        // A hard break after a full line (as ConPTY sends a soft wrap) joins;
        // a short line keeps its break, without its trailing spaces.
        let mut term = small_term(10, 4, "C:\\Users\\a\r\nb\\c   \r\nnext");
        assert_eq!(select(&mut term, (0, 0), (2, 3)), "C:\\Users\\ab\\c\nnext");
        // A real soft wrap joins too (alacritty marks it).
        let mut term = small_term(5, 3, "abcdefgh");
        assert_eq!(select(&mut term, (0, 0), (1, 4)), "abcdefgh");
        // A rule across the window ends its line.
        let mut term = small_term(4, 3, "\u{2500}\u{2500}\u{2500}\u{2500}\r\nx");
        assert_eq!(select(&mut term, (0, 0), (1, 0)), "\u{2500}\u{2500}\u{2500}\u{2500}\nx");
        // A whole-line selection keeps its final break.
        let mut term = small_term(10, 3, "one\r\ntwo");
        term.selection = Some(Selection::new(SelectionType::Lines, at(1, 0), Side::Left));
        assert_eq!(selection_text(&term).as_deref(), Some("two\n"));
    }

    #[test]
    fn copy_as_one_line_drops_breaks_and_indentation() {
        assert_eq!(join_lines("C:\\Users\\a\\AppData\\  \n  Local\\Temp\n"), "C:\\Users\\a\\AppData\\Local\\Temp");
        assert_eq!(join_lines("  cargo build\r\n    --release"), "cargo build--release");
        assert_eq!(join_lines("one"), "one");
        let mut term = small_term(12, 3, "path: C:\\a\\\r\n  b\\c.txt");
        let mut selection = Selection::new(SelectionType::Simple, GridPoint::new(Line(0), GridColumn(6)), Side::Left);
        selection.update(GridPoint::new(Line(1), GridColumn(8)), Side::Right);
        term.selection = Some(selection);
        assert_eq!(join_lines(&selection_text(&term).unwrap()), "C:\\a\\b\\c.txt");
    }

    #[test]
    fn styles_keep_the_cell_grid() {
        // Whatever face each style ends up with, it has the regular advance
        // and baseline (else bold/italic text drifts off the cells). The
        // bundled font has real bold and italic faces.
        load_fonts();
        for base in [Font::MONOSPACE, BUNDLED_FONT] {
            let m = Metrics::new(base, 14.0);
            let regular = measure(base, 14.0, 18.0);
            for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
                let (font, synthetic) = m.style_font(bold, italic);
                let (w, baseline) = measure(font, 14.0, 18.0);
                assert!((w - regular.0).abs() < 0.05, "{bold} {italic}: {w} vs {}", regular.0);
                assert!((baseline - regular.1).abs() < 0.5, "{bold} {italic}: {baseline} vs {}", regular.1);
                assert!(!synthetic || bold);
                if base == BUNDLED_FONT {
                    assert!(!synthetic, "{bold} {italic}: synthetic bold");
                    assert_eq!((font.weight == Weight::Bold, font.style == FontStyle::Italic), (bold, italic));
                }
            }
            assert!((m.cell_w - regular.0).abs() < 0.001);
            assert_eq!(m.cell_h, 18.0);
        }
    }

    #[test]
    fn bundled_faces_are_the_ones_drawn() {
        // Each style of the bundled font shapes with its own face, not with
        // an installed Cascadia Mono or a fallback.
        use iced::advanced::graphics::text::cosmic_text::fontdb::Source;
        use iced::advanced::graphics::text::{font_system, Paragraph};
        use iced::advanced::text::Paragraph as _;
        load_fonts();
        let faces = [
            ("CascadiaMono-Regular", false, false),
            ("CascadiaMono-Bold", true, false),
            ("CascadiaMono-Italic", false, true),
            ("CascadiaMono-BoldItalic", true, true),
        ];
        for (name, bold, italic) in faces {
            let font = Font {
                weight: if bold { Weight::Bold } else { Weight::Normal },
                style: if italic { FontStyle::Italic } else { FontStyle::Normal },
                ..BUNDLED_FONT
            };
            let p = Paragraph::with_text(iced::advanced::Text {
                content: "0",
                bounds: Size::INFINITY,
                size: Pixels(14.0),
                line_height: LineHeight::Absolute(Pixels(18.0)),
                font,
                horizontal_alignment: alignment::Horizontal::Left,
                vertical_alignment: alignment::Vertical::Top,
                shaping: Shaping::Basic,
                wrapping: Wrapping::None,
            });
            let id = p.buffer().layout_runs().next().and_then(|run| run.glyphs.first()).map(|g| g.font_id).unwrap();
            let mut fs = font_system().write().unwrap();
            let face = fs.raw().db().face(id).unwrap();
            assert_eq!(face.post_script_name, name);
            assert!(matches!(face.source, Source::Binary(_)), "{name}");
        }
    }

    #[test]
    fn links_from_osc8_and_text() {
        let pal = Palette::new(Color::BLACK, Color::WHITE);
        let term = small_term(20, 3, "\x1b]8;;https://x.y\x1b\\text\x1b]8;;\x1b\\ C:\\a.rs:3\r\nftp://no \x1b]8;;javascript:x\x1b\\js\x1b]8;;\x1b\\");
        let frame = Frame::build(&term, &pal);
        let url = LinkTarget::Url("https://x.y".to_owned());
        let path = LinkTarget::Path { path: PathBuf::from(r"C:\a.rs"), line: Some(3) };
        assert_eq!(frame.links, [Link { target: url, spans: vec![(0, 0, 4)] }, Link { target: path, spans: vec![(0, 5, 9)] }]);
        assert!(frame.link_at(0, 3).is_some() && frame.link_at(0, 4).is_none() && frame.link_at(1, 9).is_none());
        // A path that runs to the edge goes on in the next line.
        let term = small_term(10, 3, "> C:\\Users\\me\\x.txt is it");
        let frame = Frame::build(&term, &pal);
        let path = LinkTarget::Path { path: PathBuf::from(r"C:\Users\me\x.txt"), line: None };
        assert_eq!(frame.links, [Link { target: path, spans: vec![(0, 2, 8), (1, 0, 9)] }]);
    }

    #[test]
    fn bold_is_bright_only_when_asked() {
        // Bold red, bold indexed green, bold bright red, bold RGB, plain red.
        let term = small_term(8, 1, "\x1b[1;31ma\x1b[38;5;2mb\x1b[38;5;9mc\x1b[38;2;1;2;3md\x1b[0;31me");
        let colors = |pal: &Palette| -> Vec<Color> {
            let frame = Frame::build(&term, pal);
            frame.runs.iter().flat_map(|r| r.text.chars().map(move |_| r.color)).collect()
        };
        let pal = Palette::new(Color::BLACK, Color::WHITE);
        let rgb = Color::from_rgb8(1, 2, 3);
        assert_eq!(colors(&pal), [pal.ansi[1], pal.ansi[2], pal.ansi[9], rgb, pal.ansi[1]]);
        let bright = Palette { bold_bright: true, ..pal };
        assert_eq!(colors(&bright), [pal.ansi[9], pal.ansi[10], pal.ansi[9], rgb, pal.ansi[1]]);
        assert_eq!(bold_color(AnsiColor::Named(NamedColor::Foreground)), AnsiColor::Named(NamedColor::Foreground));
        assert_eq!(bold_color(AnsiColor::Named(NamedColor::White)), AnsiColor::Named(NamedColor::BrightWhite));
        assert_eq!(bold_color(AnsiColor::Indexed(7)), AnsiColor::Indexed(15));
        assert_eq!(bold_color(AnsiColor::Indexed(8)), AnsiColor::Indexed(8));
    }

    #[test]
    fn metrics_follow_the_font_size() {
        assert_eq!(line_height(14.0), 18.0);
        assert_eq!(line_height(8.0), 10.0);
        assert_eq!(line_height(20.0), 26.0);
        assert_eq!(line_height(32.0), 41.0);
        let area = Size::new(1000.0, 600.0);
        let mut last = (usize::MAX, usize::MAX);
        for size in [8.0, 12.0, 14.0, 20.0, 32.0] {
            let m = Metrics::new(Font::MONOSPACE, size);
            assert_eq!(m.cell_h, line_height(size));
            // About 0.55 em wide, whatever the monospace font is.
            assert!(m.cell_w > size * 0.4 && m.cell_w < size * 0.7, "{size}: {}", m.cell_w);
            let (cols, lines) = m.grid_size(area);
            assert_eq!(lines, ((600.0 - 2.0 * PAD) / m.cell_h).floor() as usize, "{size}");
            assert!(cols < last.0 && lines < last.1, "{size}: {cols}x{lines} after {last:?}");
            last = (cols, lines);
            let ws = m.window_size(cols, lines);
            assert_eq!((ws.num_cols as usize, ws.num_lines as usize, ws.cell_height), (cols, lines, m.cell_h as u16));
        }
    }

    #[test]
    fn zoom_steps_stay_in_range() {
        assert_eq!(zoom_after(14, 0, Some(1)), 1);
        assert_eq!(zoom_after(14, 3, Some(-1)), 2);
        assert_eq!(zoom_after(14, 5, None), 0);
        assert_eq!(zoomed_size(14, 4), 18);
        // At the ends of 8..=32 a step does nothing.
        assert_eq!(zoom_after(14, 18, Some(1)), 18);
        assert_eq!(zoom_after(14, -6, Some(-1)), -6);
        // A zoom beyond the range (the setting changed since) comes back in one step.
        assert_eq!(zoomed_size(30, 10), 32);
        assert_eq!(zoom_after(30, 10, Some(-1)), 1);
        assert_eq!(zoomed_size(14, i32::MAX), 32);
        assert_eq!(zoomed_size(14, i32::MIN), 8);
    }

    #[test]
    fn font_names_are_kept_once() {
        assert_eq!(font_named(""), BUNDLED_FONT);
        assert_eq!(font_named("  "), BUNDLED_FONT);
        let a = font_named(" Cascadia Mono ");
        assert_eq!(a, Font::with_name("Cascadia Mono"));
        let (iced::font::Family::Name(x), iced::font::Family::Name(y)) = (a.family, font_named("Cascadia Mono").family)
        else {
            panic!("not named")
        };
        assert!(std::ptr::eq(x, y));
    }

    #[test]
    fn blocks_and_lines_are_shapes_not_text() {
        let term = small_term(10, 2, "ab\u{2588}\u{2588}c\u{2502}\u{2504}");
        let frame = Frame::build(&term, &Palette::new(Color::BLACK, Color::WHITE));
        let shapes: Vec<_> = frame.shapes.iter().map(|&(l, c, ch, _)| (l, c, ch)).collect();
        assert_eq!(shapes, vec![(0, 2, '\u{2588}'), (0, 3, '\u{2588}'), (0, 5, '\u{2502}')]);
        // Dashed lines stay text.
        let runs: Vec<_> = frame.runs.iter().map(|r| (r.col, r.text.as_str())).collect();
        assert_eq!(runs, vec![(0, "ab"), (4, "c"), (6, "\u{2504}")]);
    }

    #[test]
    fn multi_line_selection_cells() {
        let range = SelectionRange::new(GridPoint::new(Line(0), GridColumn(5)), GridPoint::new(Line(2), GridColumn(3)), false);
        assert_eq!(selection_cells(&range, 0, 10, 4), vec![(0, 5, 5), (1, 0, 10), (2, 0, 4)]);
        // Scrolled back by one line, everything moves down.
        assert_eq!(selection_cells(&range, 1, 10, 4), vec![(1, 5, 5), (2, 0, 10), (3, 0, 4)]);
    }

    #[test]
    fn bracketed_paste_mode_is_tracked() {
        let mut term = small_term(10, 2, "");
        assert!(!term.mode().contains(TermMode::BRACKETED_PASTE));
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        parser.advance(&mut term, b"\x1b[?2004h");
        assert!(term.mode().contains(TermMode::BRACKETED_PASTE));
    }

    #[test]
    fn closing_asks_only_while_a_program_runs() {
        assert!(!close_needs_confirm(None));
        assert!(close_needs_confirm(Some("ping")));
        assert_eq!(quit_question(0), None);
        assert_eq!(quit_question(1).as_deref(), Some("1 terminal is still running. Quit anyway?"));
        assert_eq!(quit_question(3).as_deref(), Some("3 terminals are still running. Quit anyway?"));
    }

    #[test]
    fn cells_under_the_mouse() {
        let m = Metrics::new(Font::MONOSPACE, 14.0);
        let at = |x: f32, y: f32| m.cell_at(Point::new(PAD + x * m.cell_w, PAD + y * m.cell_h), 10, 5);
        assert_eq!(at(0.2, 0.5), (0, 0, false));
        assert_eq!(at(3.7, 2.5), (2, 3, true));
        // Outside: the nearest cell.
        assert_eq!(at(-4.0, -2.0), (0, 0, false));
        assert_eq!(at(40.0, 40.0), (4, 9, true));
    }

    #[test]
    fn sidebar_cycles_around_files_and_terminals() {
        // Files + 2 terminals = 3 entries.
        assert_eq!(cycle(0, 3, 1), 1);
        assert_eq!(cycle(2, 3, 1), 0);
        assert_eq!(cycle(0, 3, -1), 2);
        assert_eq!(cycle(1, 3, -1), 0);
        assert_eq!(cycle(0, 1, 1), 0);
        assert_eq!(cycle(0, 0, -1), 0);
    }

    #[test]
    fn closing_falls_back_to_the_entry_before() {
        assert_eq!(fallback_after_close(&[4, 7, 9], 9), Some(7));
        assert_eq!(fallback_after_close(&[4, 7, 9], 7), Some(4));
        assert_eq!(fallback_after_close(&[4, 7, 9], 4), None);
        assert_eq!(fallback_after_close(&[4], 4), None);
        assert_eq!(fallback_after_close(&[4], 5), None);
    }

    #[test]
    fn named_keys() {
        let none = M::empty();
        assert_eq!(named(Named::Enter, none).unwrap(), b"\r");
        assert_eq!(named(Named::Escape, none).unwrap(), b"\x1b");
        assert_eq!(named(Named::Tab, none).unwrap(), b"\t");
        assert_eq!(named(Named::Tab, M::SHIFT).unwrap(), b"\x1b[Z");
        assert_eq!(named(Named::Backspace, none).unwrap(), b"\x7f");
        assert_eq!(named(Named::Backspace, M::CTRL).unwrap(), b"\x08");
        assert_eq!(named(Named::Backspace, M::ALT).unwrap(), b"\x1b\x7f");
        assert_eq!(named(Named::ArrowUp, none).unwrap(), b"\x1b[A");
        assert_eq!(named(Named::ArrowLeft, M::CTRL).unwrap(), b"\x1b[1;5D");
        assert_eq!(named(Named::End, M::SHIFT).unwrap(), b"\x1b[1;2F");
        assert_eq!(key_bytes(&Key::Named(Named::ArrowUp), none, None, true).unwrap(), b"\x1bOA");
        assert_eq!(key_bytes(&Key::Named(Named::Home), none, None, true).unwrap(), b"\x1bOH");
        // Modifiers win over application cursor mode.
        assert_eq!(key_bytes(&Key::Named(Named::ArrowUp), M::ALT, None, true).unwrap(), b"\x1b[1;3A");
        assert_eq!(named(Named::Delete, none).unwrap(), b"\x1b[3~");
        assert_eq!(named(Named::Insert, none).unwrap(), b"\x1b[2~");
        assert_eq!(named(Named::PageDown, M::CTRL).unwrap(), b"\x1b[6;5~");
        assert_eq!(named(Named::F1, none).unwrap(), b"\x1bOP");
        assert_eq!(named(Named::F4, M::SHIFT).unwrap(), b"\x1b[1;2S");
        assert_eq!(named(Named::F5, none).unwrap(), b"\x1b[15~");
        assert_eq!(named(Named::F9, none).unwrap(), b"\x1b[20~");
        assert_eq!(named(Named::F12, M::CTRL | M::SHIFT).unwrap(), b"\x1b[24;6~");
        assert_eq!(named(Named::Space, M::CTRL).unwrap(), b"\0");
        assert_eq!(named(Named::Shift, none), None);
    }

    #[test]
    fn character_keys() {
        let none = M::empty();
        assert_eq!(ch("a", none, Some("a")).unwrap(), b"a");
        assert_eq!(ch("A", M::SHIFT, Some("A")).unwrap(), b"A");
        assert_eq!(ch("č", none, Some("č")).unwrap(), "č".as_bytes());
        assert_eq!(ch("c", M::CTRL, None).unwrap(), b"\x03");
        assert_eq!(ch("c", M::CTRL, Some("\u{3}")).unwrap(), b"\x03");
        assert_eq!(ch("C", M::CTRL | M::SHIFT, None).unwrap(), b"\x03");
        assert_eq!(ch("z", M::CTRL, None).unwrap(), b"\x1a");
        assert_eq!(ch("[", M::CTRL, None).unwrap(), b"\x1b");
        assert_eq!(ch("x", M::ALT, Some("x")).unwrap(), b"\x1bx");
        assert_eq!(ch("x", M::CTRL | M::ALT, None).unwrap(), b"\x1b\x18");
        // AltGr (Ctrl+Alt) types `@` on many layouts.
        assert_eq!(ch("v", M::CTRL | M::ALT, Some("@")).unwrap(), b"@");
    }

    #[test]
    fn terminal_colors_config() {
        let map = |pairs: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let rgb8 = |c: Color| c.into_rgba8();
        let o = theme::term_colors(&map(&[("Red", "#102030"), ("background", "#ffffff"), ("cursor", "00ff00")])).unwrap();
        let pal = Palette::new(Color::BLACK, Color::WHITE).with(&o);
        assert_eq!(rgb8(pal.color(1)), [0x10, 0x20, 0x30, 0xff]);
        // A light background picks the light set for the others.
        assert_eq!(rgb8(pal.color(2)), [0x50, 0xa1, 0x4f, 0xff]);
        assert_eq!(rgb8(pal.bg), [0xff, 0xff, 0xff, 0xff]);
        assert_eq!(rgb8(pal.cursor), [0, 0xff, 0, 0xff]);
        assert_eq!(pal.fg, Color::WHITE);
        // A theme's own set is kept under overrides that don't flip dark / light.
        let gruvbox = theme::preset("gruvbox").unwrap();
        let themed = Palette { ansi: gruvbox.ansi.map(hex), ..Palette::new(gruvbox.colors.panel, gruvbox.colors.text) };
        let o = theme::term_colors(&map(&[("background", "#101010"), ("blue", "#0000ff")])).unwrap();
        let pal = themed.with(&o);
        assert_eq!(rgb8(pal.color(1)), [0xcc, 0x24, 0x1d, 0xff]);
        assert_eq!(rgb8(pal.color(4)), [0, 0, 0xff, 0xff]);
        assert!(theme::PRESETS.iter().all(|p| p.ansi[0] != p.ansi[15]));
        // Nothing set: the defaults.
        let plain = Palette::new(Color::BLACK, Color::WHITE).with(&[None; 19]);
        assert_eq!(rgb8(plain.color(1)), [0xc5, 0x0f, 0x1f, 0xff]);
        assert!(theme::term_colors(&map(&[("pink", "#000000")])).unwrap_err().contains("pink"));
        assert!(theme::term_colors(&map(&[("red", "red")])).unwrap_err().contains("red"));
    }

    #[test]
    fn colours() {
        let pal = Palette::new(Color::BLACK, Color::WHITE);
        let rgb8 = |c: Color| c.into_rgba8();
        assert_eq!(rgb8(pal.color(1)), [0xc5, 0x0f, 0x1f, 0xff]);
        assert_eq!(rgb8(pal.color(16)), [0, 0, 0, 0xff]);
        assert_eq!(rgb8(pal.color(196)), [0xff, 0, 0, 0xff]);
        assert_eq!(rgb8(pal.color(208)), [0xff, 0x87, 0, 0xff]);
        assert_eq!(rgb8(pal.color(231)), [0xff, 0xff, 0xff, 0xff]);
        assert_eq!(rgb8(pal.color(232)), [8, 8, 8, 0xff]);
        assert_eq!(rgb8(pal.color(255)), [238, 238, 238, 0xff]);
        assert_eq!(pal.color(NamedColor::Foreground as usize), Color::WHITE);
        assert_eq!(pal.color(NamedColor::Background as usize), Color::BLACK);
        let overrides = TermColors::default();
        assert_eq!(pal.resolve(AnsiColor::Named(NamedColor::Red), &overrides), pal.color(1));
        assert_eq!(pal.resolve(AnsiColor::Indexed(9), &overrides), pal.color(9));
        let spec = AnsiColor::Spec(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(rgb8(pal.resolve(spec, &overrides)), [1, 2, 3, 0xff]);
        let mut overrides = TermColors::default();
        overrides[1] = Some(Rgb { r: 9, g: 9, b: 9 });
        assert_eq!(rgb8(pal.resolve(AnsiColor::Named(NamedColor::Red), &overrides)), [9, 9, 9, 0xff]);
        // A light background gets the light set.
        let light = Palette::new(Color::WHITE, Color::BLACK);
        assert_eq!(rgb8(light.color(0)), [0x38, 0x3a, 0x42, 0xff]);
    }

    #[test]
    fn sidebar_labels() {
        let docs = Path::new(r"C:\Users\me\Documents");
        assert_eq!(term_label(docs, None, 18), "Documents");
        assert_eq!(term_label(docs, Some("ping"), 18), "Documents \u{b7} ping");
        assert_eq!(term_label(Path::new(r"C:\"), None, 18), r"C:\");
        // The folder is cut first, the program keeps its name.
        let long = Path::new(r"C:\Projects\A very long folder name");
        assert_eq!(term_label(long, None, 18), "A very long folde\u{2026}");
        assert_eq!(term_label(long, Some("vim"), 18), "A very long\u{2026} \u{b7} vim");
        let label = term_label(long, Some("a-very-long-program-name"), 18);
        assert_eq!(label, "A ver\u{2026} \u{b7} a-very-l\u{2026}");
        assert!(label.chars().count() <= 18);
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("abcd", 3), "ab\u{2026}");
    }

    #[test]
    fn titles_that_are_exe_paths_are_ignored() {
        assert!(!meaningful_title(r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe"));
        assert!(!meaningful_title(r"C:\WINDOWS\system32\cmd.exe - ping  -n 5 127.0.0.1"));
        assert!(!meaningful_title(r"Administrator: C:\Program Files\PowerShell\7\pwsh.EXE"));
        assert!(!meaningful_title("  "));
        assert!(meaningful_title(r"notes.txt (C:\Users\me) - VIM"));
    }

    #[test]
    fn switching_tip() {
        let s = |k: &str| Some(k.to_owned());
        assert_eq!(
            switch_tip(s("Ctrl+Shift+Up"), s("Ctrl+Shift+Down"), s("Ctrl+Shift+T")),
            "Ctrl+Shift+Up/Down: switch \u{b7} Ctrl+Shift+T: new"
        );
        assert_eq!(switch_tip(s("Alt+Up"), s("Ctrl+Down"), None), "Alt+Up / Ctrl+Down: switch");
        assert_eq!(switch_tip(None, s("F2"), None), "F2: next");
        assert_eq!(switch_tip(None, None, s("Ctrl+T")), "Sidebar: switch \u{b7} Ctrl+T: new");
        assert_eq!(switch_tip(None, None, None), "The sidebar on the right switches terminals");
    }

    #[test]
    fn prompts() {
        let p = |s: &str| parse_prompt(s).map(|p| p.display().to_string());
        assert_eq!(p(r"PS C:\Users\me> ").as_deref(), Some(r"C:\Users\me"));
        assert_eq!(p(r"PS C:\Users\me>> ").as_deref(), Some(r"C:\Users\me"));
        assert_eq!(p(r"C:\Users\me>").as_deref(), Some(r"C:\Users\me"));
        assert_eq!(p(r"C:\>").as_deref(), Some(r"C:\"));
        assert_eq!(p(r"PS Microsoft.PowerShell.Core\FileSystem::\\srv\share> ").as_deref(), Some(r"\\srv\share"));
        // Typing after the prompt, other providers, plain output.
        assert_eq!(p(r"PS C:\Users\me> dir"), None);
        assert_eq!(p(r"PS HKLM:\Software> "), None);
        assert_eq!(p("<html>"), None);
        assert_eq!(p(""), None);
    }

    #[test]
    fn shell_command_lines() {
        assert_eq!(shell_command("powershell.exe -NoLogo"), Some(("powershell.exe".into(), vec!["-NoLogo".into()])));
        assert_eq!(shell_command("  cmd.exe  "), Some(("cmd.exe".into(), vec![])));
        assert_eq!(shell_command(""), None);
        let (program, args) = shell_command(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo -c "a b""#).unwrap();
        let path = r"C:\Program Files\PowerShell\7\pwsh.exe";
        let quoted = if cfg!(windows) { format!("\"{path}\"") } else { path.to_owned() };
        assert_eq!(program, quoted);
        assert_eq!(args, ["-NoLogo", "-c", "a b"]);
    }

    /// A real cmd's folder, from its prompt and from its process, after a
    /// `cd`; and the program it runs.
    #[cfg(windows)]
    #[test]
    fn shell_folder_and_program_are_found() {
        let dir = std::env::temp_dir().join(format!("rc-term-cwd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let s = TermSession::spawn(2, "cmd.exe /q /k", &dir, &Metrics::new(Font::MONOSPACE, 14.0), 120, 24).unwrap();
        let rx = s.events.lock().unwrap().take().unwrap();
        let same = |a: &Option<PathBuf>, b: &Path| a.as_ref().is_some_and(|a| a.as_os_str().eq_ignore_ascii_case(b));
        let wait = |what: &dyn Fn() -> bool| {
            let end = Instant::now() + Duration::from_secs(15);
            while Instant::now() < end {
                while rx.recv_timeout(Duration::from_millis(50)).is_ok() {
                    s.wakeup.store(false, Ordering::Release);
                }
                if what() {
                    return true;
                }
            }
            false
        };
        let pid = s.pid.expect("shell pid");
        assert!(wait(&|| same(&prompt_dir(&s.term.lock()), &dir)), "{:?}", prompt_dir(&s.term.lock()));
        assert!(same(&rc_platform::process_cwd(pid), &dir), "{:?}", rc_platform::process_cwd(pid));
        s.write(b"cd sub\r".to_vec());
        let sub = dir.join("sub");
        assert!(wait(&|| same(&prompt_dir(&s.term.lock()), &sub)), "{:?}", prompt_dir(&s.term.lock()));
        assert!(same(&rc_platform::process_cwd(pid), &sub));
        assert_eq!(rc_platform::foreground_child(pid), None);
        s.write(b"ping -n 6 127.0.0.1\r".to_vec());
        assert!(wait(&|| rc_platform::foreground_child(pid).is_some_and(|p| p.eq_ignore_ascii_case("ping"))));
        drop(rx);
        drop(s);
        // The shell may still hold the folder for a moment.
        let end = Instant::now() + Duration::from_secs(5);
        while std::fs::remove_dir_all(&dir).is_err() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Runs a real shell in a temporary folder: output reaches the grid, the
    /// frame has the text, and dropping the session ends the shell process.
    #[cfg(windows)]
    #[test]
    fn shell_runs_and_ends_with_the_session() {
        let dir = std::env::temp_dir().join(format!("rc-term-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = Metrics::new(Font::MONOSPACE, 14.0);
        let mut s = TermSession::spawn(1, "cmd.exe /q /k prompt $G", &dir, &m, 80, 24).unwrap();
        let rx = s.events.lock().unwrap().take().unwrap();
        let screen = |s: &TermSession| -> String {
            let term = s.term.lock();
            let grid = term.grid();
            (0..grid.screen_lines())
                .map(|l| {
                    let row = &grid[alacritty_terminal::index::Line(l as i32)];
                    (0..grid.columns()).map(|c| row[alacritty_terminal::index::Column(c)].c).collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let wait = |s: &TermSession, what: &str| {
            let end = Instant::now() + Duration::from_secs(15);
            while Instant::now() < end {
                while let Ok(e) = rx.recv_timeout(Duration::from_millis(50)) {
                    if let TermEvent::Wakeup = e {
                        s.wakeup.store(false, Ordering::Release);
                    }
                }
                if screen(s).contains(what) {
                    return true;
                }
            }
            false
        };
        s.write(b"echo hello-from-term\r".to_vec());
        assert!(wait(&s, "\nhello-from-term"), "{}", screen(&s));
        s.refresh();
        assert!(s.frame.runs.iter().any(|r| r.text == "hello-from-term"), "{:?}", s.frame.runs);
        assert!(s.frame.cursor.is_some());
        s.resize(&m, 100, 30);
        let size = {
            let term = s.term.lock();
            (term.columns(), term.screen_lines())
        };
        assert_eq!(size, (100, 30));

        let pid = s.pid.expect("shell pid");
        assert!(process_alive(pid));
        drop(rx);
        drop(s);
        let end = Instant::now() + Duration::from_secs(10);
        while process_alive(pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_alive(pid), "the shell outlived its terminal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    fn process_alive(pid: u32) -> bool {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }
}

#[cfg(all(test, windows))]
mod ctrl_c_tests {
    use super::*;

    fn alive(pid: u32) -> bool {
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }

    /// Even when we were started ignoring Ctrl+C, Ctrl+C stops a running command.
    #[test]
    fn ctrl_c_stops_a_command_even_if_we_ignore_it() {
        // SAFETY: sets this test process's ignore flag (what some launchers hand down).
        unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 1) };
        let m = Metrics::new(Font::MONOSPACE, 14.0);
        let s = TermSession::spawn(1, "cmd.exe /d /c ping -t 127.0.0.1", &std::env::temp_dir(), &m, 80, 24).unwrap();
        let pid = s.pid.unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert!(alive(pid), "ping should be running");
        s.write(vec![3]);
        let stopped = (0..30).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            !alive(pid)
        });
        assert!(stopped, "ping still runs after Ctrl+C");
    }
}
