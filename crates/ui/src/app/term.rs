//! Built-in terminal (F9): a shell in a ConPTY, drawn over the whole window
//! like the other full-window tools. `alacritty_terminal` parses the output
//! into a grid on its own I/O thread (`EventLoop`); we draw that grid
//! (`Grid` widget) and turn key presses into the bytes a terminal sends.
//!
//! Terminals live in `App::terms`, apart from the screen: `Screen::Term(id)`
//! only says which one is shown, so a hidden terminal keeps running (and
//! several can exist; the sidebar to switch between them comes later). Each
//! has its own event subscription keyed by its id (`TermSession::events`),
//! running whether or not it is shown. The one key the terminal keeps for
//! itself is the `ToggleTerminal` command (Ctrl+Shift+F9): back to the
//! panels. When the shell exits, its terminal is removed.

use super::*;
use crate::theme::filled;
use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors as TermColors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Pty, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Rgb};
use iced::advanced::layout::{self, Layout};
use iced::advanced::text::Renderer as _;
use iced::advanced::widget::{Tree, Widget};
use iced::advanced::{renderer, Clipboard, Renderer as _, Shell as IcedShell};
use iced::alignment;
use iced::font::{Style as FontStyle, Weight};
use iced::widget::text::{LineHeight, Shaping, Wrapping};
use iced::widget::{column, container, row, text, Space};
use iced::{Background, Color, Element, Font, Length, Pixels, Rectangle};
use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;
use std::thread::JoinHandle;

/// Font size and line height of the grid.
const FONT_SIZE: f32 = 14.0;
const CELL_H: f32 = 18.0;
/// Space around the grid.
const PAD: f32 = 6.0;
/// Height of the line under the grid (title, how to get back).
const FOOTER_H: f32 = 22.0;
/// Scrollback lines kept per terminal.
const HISTORY: usize = 10_000;
/// Redraws happen at most this often (output arriving faster is coalesced).
const FRAME: Duration = Duration::from_millis(16);

/// Width of a cell: a monospace character at `FONT_SIZE`, measured once with
/// the font the system resolves `Font::MONOSPACE` to.
fn cell_w() -> f32 {
    use iced::advanced::text::Paragraph as _;
    static W: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *W.get_or_init(|| {
        const N: usize = 100;
        let sample = "0".repeat(N);
        let paragraph = iced::advanced::graphics::text::Paragraph::with_text(iced::advanced::Text {
            content: sample.as_str(),
            bounds: Size::INFINITY,
            size: Pixels(FONT_SIZE),
            line_height: LineHeight::Absolute(Pixels(CELL_H)),
            font: Font::MONOSPACE,
            horizontal_alignment: alignment::Horizontal::Left,
            vertical_alignment: alignment::Vertical::Top,
            shaping: Shaping::Basic,
            wrapping: Wrapping::None,
        });
        let w = paragraph.min_width() / N as f32;
        if w.is_finite() && w > 1.0 { w } else { 8.4 }
    })
}

/// Columns and lines that fit in an area of `size` pixels (the grid widget).
fn grid_size(size: Size) -> (usize, usize) {
    let cols = ((size.width - 2.0 * PAD) / cell_w()).floor().max(2.0) as usize;
    let lines = ((size.height - 2.0 * PAD) / CELL_H).floor().max(1.0) as usize;
    (cols.min(u16::MAX as usize), lines.min(u16::MAX as usize))
}

fn window_size(cols: usize, lines: usize) -> WindowSize {
    WindowSize {
        num_lines: lines as u16,
        num_cols: cols as u16,
        cell_width: cell_w().round() as u16,
        cell_height: CELL_H as u16,
    }
}

#[derive(Debug, Clone)]
pub enum TermMsg {
    /// An event from terminal `id`'s I/O thread.
    Event(u64, TermEvent),
    /// The grid widget of terminal `id` has room for (columns, lines).
    Resize(u64, usize, usize),
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
    /// Window title the shell set (OSC 0/2), else the folder.
    pub(super) title: String,
    /// Folder the shell started in.
    pub(super) dir: PathBuf,
    /// Process id of the shell (Windows; read by the tests for now).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) pid: Option<u32>,
    /// Exit code once the shell has exited.
    exit_code: Option<i32>,
    /// What the widget draws; rebuilt on output while shown.
    frame: Frame,
}

impl TermSession {
    /// Starts `command` (program and arguments) in `dir` with a grid of
    /// `cols` x `lines`.
    pub(super) fn spawn(id: u64, command: &str, dir: &Path, cols: usize, lines: usize) -> io::Result<TermSession> {
        let (program, args) = shell_command(command)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the builtin_shell setting is empty"))?;
        safe_dll_search();
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
        let pty = tty::new(&options, window_size(cols, lines), id)?;
        #[cfg(windows)]
        let pid = pty.child_watcher().pid().map(|p| p.get());
        #[cfg(not(windows))]
        let pid = None;
        let config = TermConfig { scrolling_history: HISTORY, ..TermConfig::default() };
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
            title: folder_title(dir),
            dir: dir.to_owned(),
            pid,
            exit_code: None,
            frame: Frame::default(),
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
        }
        let _ = self.chan.send(Msg::Input(bytes.into()));
    }

    /// Resizes the grid and the ConPTY when the size changed.
    fn resize(&mut self, cols: usize, lines: usize) {
        let mut term = self.term.lock();
        if (term.columns(), term.screen_lines()) == (cols, lines) {
            return;
        }
        term.resize(TermSize::new(cols, lines));
        drop(term);
        let _ = self.chan.send(Msg::Resize(window_size(cols, lines)));
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

fn folder_title(dir: &Path) -> String {
    dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The colours a terminal draws with: 16 ANSI colours (light or dark set,
/// picked by the theme's background), and the theme's text and background.
#[derive(Debug, Clone, Copy)]
struct Palette {
    ansi: [Color; 16],
    fg: Color,
    bg: Color,
    cursor: Color,
}

/// Windows Terminal's "Campbell" scheme.
const DARK_ANSI: [u32; 16] = [
    0x0c0c0c, 0xc50f1f, 0x13a10e, 0xc19c00, 0x0037da, 0x881798, 0x3a96dd, 0xcccccc, 0x767676, 0xe74856, 0x16c60c,
    0xf9f1a5, 0x3b78ff, 0xb4009e, 0x61d6d6, 0xf2f2f2,
];
/// "One Half Light".
const LIGHT_ANSI: [u32; 16] = [
    0x383a42, 0xe45649, 0x50a14f, 0xc18401, 0x0184bc, 0xa626a4, 0x0997b3, 0xfafafa, 0x4f525d, 0xdf6c75, 0x98c379,
    0xe4c07a, 0x61afef, 0xc577dd, 0x56b5c1, 0xffffff,
];

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
        Palette { ansi: set.map(hex), fg, bg, cursor: fg }
    }

    fn from_theme() -> Palette {
        let c = theme::c();
        Palette::new(c.panel, c.text)
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
    cursor: Option<Cursor>,
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

impl Run {
    fn font(&self) -> Font {
        Font {
            weight: if self.bold { Weight::Bold } else { Weight::Normal },
            style: if self.italic { FontStyle::Italic } else { FontStyle::Normal },
            ..Font::MONOSPACE
        }
    }
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

impl Frame {
    fn build(term: &Term<Proxy>, pal: &Palette) -> Frame {
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let overrides = content.colors;
        let mut frame = Frame {
            cols: term.columns(),
            lines: term.screen_lines(),
            bg: pal.resolve(AnsiColor::Named(NamedColor::Background), overrides),
            ..Frame::default()
        };
        let cursor_point = content.cursor.point;
        let cursor_shape = content.cursor.shape;
        let mut cursor_cell = None;
        // The open run and the spaces seen after it (added once more text
        // of its style follows).
        let mut open: Option<Run> = None;
        let mut spaces = 0u16;
        for item in content.display_iter {
            let flags = item.flags;
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let line = (item.point.line.0 + offset) as u16;
            let col = item.point.column.0 as u16;
            let cells = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            let mut fg = pal.resolve(item.fg, overrides);
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
}

/// Draws a `Frame` and reports the size that fits (`TermMsg::Resize`).
struct Grid<'a> {
    id: u64,
    frame: &'a Frame,
    /// The window has focus (else the cursor is drawn hollow).
    focused: bool,
}

impl Grid<'_> {
    fn draw_run(&self, renderer: &mut iced::Renderer, run: &Run, origin: Point, clip: Rectangle) {
        let cw = cell_w();
        let x = origin.x + run.col as f32 * cw;
        let y = origin.y + run.line as f32 * CELL_H;
        renderer.fill_text(
            iced::advanced::Text {
                content: run.text.clone(),
                bounds: Size::new((run.cells + 1) as f32 * cw, CELL_H),
                size: Pixels(FONT_SIZE),
                line_height: LineHeight::Absolute(Pixels(CELL_H)),
                font: run.font(),
                horizontal_alignment: alignment::Horizontal::Left,
                vertical_alignment: alignment::Vertical::Top,
                shaping: if run.complex { Shaping::Advanced } else { Shaping::Basic },
                wrapping: Wrapping::None,
            },
            Point::new(x, y),
            run.color,
            clip,
        );
        let width = run.cells as f32 * cw;
        if run.underline {
            fill(renderer, Rectangle { x, y: y + CELL_H - 2.0, width, height: 1.0 }, run.color);
        }
        if run.strike {
            fill(renderer, Rectangle { x, y: y + (CELL_H / 2.0).round(), width, height: 1.0 }, run.color);
        }
    }
}

fn fill(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    renderer.fill_quad(renderer::Quad { bounds, ..renderer::Quad::default() }, Background::Color(color));
}

/// The pixel rectangle of `cells` cells at (line, col); edges rounded so
/// neighbouring fills meet without seams.
fn cell_rect(origin: Point, line: u16, col: u16, cells: u16) -> Rectangle {
    let cw = cell_w();
    let x0 = (origin.x + col as f32 * cw).round();
    let x1 = (origin.x + (col + cells) as f32 * cw).round();
    Rectangle { x: x0, y: origin.y + line as f32 * CELL_H, width: x1 - x0, height: CELL_H }
}

impl Widget<Message, Theme, iced::Renderer> for Grid<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&self, _tree: &mut Tree, _renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn on_event(
        &mut self,
        _tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut IcedShell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        // Checked before every frame, so a window resize resizes the grid.
        if let Event::Window(window::Event::RedrawRequested(_)) = event {
            let (cols, lines) = grid_size(layout.bounds().size());
            if (cols, lines) != (self.frame.cols, self.frame.lines) {
                shell.publish(Message::Term(TermMsg::Resize(self.id, cols, lines)));
            }
        }
        event::Status::Ignored
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let f = self.frame;
        fill(renderer, bounds, f.bg);
        let origin = Point::new(bounds.x + PAD, bounds.y + PAD);
        for &(line, col, cells, color) in &f.fills {
            fill(renderer, cell_rect(origin, line, col, cells), color);
        }
        for run in &f.runs {
            self.draw_run(renderer, run, origin, bounds);
        }
        let Some(c) = &f.cursor else { return };
        let r = cell_rect(origin, c.line, c.col, c.cells);
        match c.shape {
            CursorShape::Block if self.focused => {
                fill(renderer, r, c.color);
                if let Some(run) = &c.text {
                    self.draw_run(renderer, run, origin, bounds);
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
        ("Every other key", "Goes to the shell (Esc, Tab, Ctrl+C, arrows, F1-F12, ...)"),
        ("exit", "Ends the shell and closes the terminal"),
    ];

    fn term(&self, id: u64) -> Option<&TermSession> {
        self.terms.iter().find(|t| t.id == id)
    }

    fn term_mut(&mut self, id: u64) -> Option<&mut TermSession> {
        self.terms.iter_mut().find(|t| t.id == id)
    }

    /// F9: shows the built-in terminal, starting one in the active panel's
    /// folder if none is running.
    pub(super) fn open_term(&mut self) -> Task<Message> {
        self.leave_edits();
        if let Some(t) = self.terms.first_mut() {
            t.set_visible(true);
            self.screen = Some(Screen::Term(t.id));
            return Task::none();
        }
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
        let id = self.next_term_id;
        self.next_term_id += 1;
        let (cols, lines) = grid_size(Size::new(self.window_width, self.window_height - FOOTER_H));
        match TermSession::spawn(id, &self.config.builtin_shell(), &dir, cols, lines) {
            Ok(session) => {
                self.terms.push(session);
                self.screen = Some(Screen::Term(id));
            }
            Err(e) => self.error(format!("Can't start the shell \"{}\": {e}", self.config.builtin_shell())),
        }
        Task::none()
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
    fn hide_term(&mut self) {
        if let Some(&id) = self.screen.term() {
            if let Some(t) = self.term_mut(id) {
                t.set_visible(false);
            }
            self.screen = None;
            self.info("The terminal keeps running: F9 shows it again");
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
    }

    pub(super) fn term_update(&mut self, msg: TermMsg) -> Task<Message> {
        let shown = self.screen.term().copied();
        match msg {
            TermMsg::Resize(id, cols, lines) => {
                if let Some(t) = self.term_mut(id) {
                    t.resize(cols, lines);
                    t.refresh();
                }
            }
            TermMsg::Event(id, event) => {
                let Some(t) = self.term_mut(id) else { return Task::none() };
                match event {
                    // (A late one for a terminal just hidden is dropped: showing
                    // it rebuilds the frame anyway.)
                    TermEvent::Wakeup => {
                        if shown == Some(id) {
                            t.refresh();
                        }
                    }
                    TermEvent::Title(title) => t.title = title,
                    TermEvent::ResetTitle => t.title = folder_title(&t.dir),
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
                            window_size(term.columns(), term.screen_lines())
                        };
                        let _ = t.chan.send(Msg::Input(format(size).into_bytes().into()));
                    }
                    TermEvent::ClipboardStore(_, text) => return iced::clipboard::write(text),
                    TermEvent::ChildExit(status) => t.exit_code = status.code(),
                    TermEvent::Exit => {
                        let code = t.exit_code;
                        self.terms.retain(|t| t.id != id);
                        if shown == Some(id) {
                            self.screen = None;
                        }
                        match code {
                            Some(0) | None => self.info("The terminal's shell has exited"),
                            Some(code) => self.info(format!("The terminal's shell has exited (exit code {code})")),
                        }
                    }
                    // Pasting the clipboard on a program's request is left out
                    // on purpose; the rest needs nothing here.
                    TermEvent::ClipboardLoad(..)
                    | TermEvent::MouseCursorDirty
                    | TermEvent::CursorBlinkingChange
                    | TermEvent::Bell => {}
                }
            }
        }
        Task::none()
    }

    /// Every key goes to the shell except the `ToggleTerminal` key.
    pub(super) fn term_key(
        &mut self,
        key: &keyboard::Key,
        mods: keyboard::Modifiers,
        text: Option<&str>,
    ) -> Task<Message> {
        let Some(&id) = self.screen.term() else { return Task::none() };
        if self.keymap.command(key, mods) == Some(Command::ToggleTerminal) {
            self.hide_term();
            return Task::none();
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
        let bindings = self.keymap.bindings();
        let back = keymap::Keymap::keys_for(&bindings, Command::ToggleTerminal).first().map(|k| k.to_string());
        let back = match back {
            Some(key) => format!("{key}: back to the panels (the terminal keeps running)"),
            None => "Bind cm_ToggleTerminal to a key to get back to the panels".to_owned(),
        };
        let footer = row![
            text(t.title.as_str()).size(12).color(colors.muted).wrapping(Wrapping::None),
            Space::with_width(Length::Fill),
            text(back).size(12).color(colors.muted).wrapping(Wrapping::None),
        ]
        .spacing(12)
        .padding([3, 10])
        .height(Length::Fixed(FOOTER_H));
        let grid = Element::new(Grid { id, frame: &t.frame, focused: self.focused });
        container(column![grid, footer])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(filled(t.frame.bg))
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::Key;
    use iced::keyboard::Modifiers as M;

    fn named(n: Named, m: M) -> Option<Vec<u8>> {
        key_bytes(&Key::Named(n), m, None, false)
    }

    fn ch(c: &str, m: M, text: Option<&str>) -> Option<Vec<u8>> {
        key_bytes(&Key::Character(c.into()), m, text, false)
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

    /// Runs a real shell in a temporary folder: output reaches the grid, the
    /// frame has the text, and dropping the session ends the shell process.
    #[cfg(windows)]
    #[test]
    fn shell_runs_and_ends_with_the_session() {
        let dir = std::env::temp_dir().join(format!("rc-term-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = TermSession::spawn(1, "cmd.exe /q /k prompt $G", &dir, 80, 24).unwrap();
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
        s.resize(100, 30);
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
