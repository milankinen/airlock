//! The terminal of a prompt: raw mode, and the prompt's view drawn in
//! place on stderr below the output before it (no alternate screen).
//!
//! [`Screen::draw`] replaces the last drawn [`Frame`] with the next one.
//! Every line is narrower than the terminal, so none wraps and the
//! screen knows how many rows it drew. A frame taller than the terminal
//! shows the part of its lines with the focused row ([`scroll`]). After a
//! resize that made the terminal narrower than a drawn line (the terminal wrapped or
//! cut it, so its rows are unknown), the next frame starts over at the
//! top of a cleared screen. [`Screen`] restores the terminal when it is
//! dropped, also on a panic. Logs go to the log file, not to the
//! terminal ([`crate::cli::logging`]), so nothing writes between the
//! frames.

use std::io::Write;
use std::ops::Range;
use std::time::Duration;

use console::{measure_text_width, truncate_str};
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::{cursor, queue, terminal};

use crate::cli;
use crate::cli::prompt::PromptError;

/// How often [`next_input`] and [`read_input`] look for a key or an
/// interrupt.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What the view shows.
pub struct Frame {
    /// The lines, each fitting in the columns that the frame was made
    /// for (longer ones are cut).
    pub lines: Vec<String>,
    /// The lines of the focused row, which stay on the screen.
    pub focus: Range<usize>,
    /// Where the text cursor shows (line, column), if it shows.
    pub cursor: Option<(usize, usize)>,
}

/// What came from the terminal.
pub enum Input {
    Key(KeyEvent),
    /// The terminal changed its size.
    Resized,
    /// Ctrl+C or SIGTERM from outside (a signal, not a key).
    Interrupted,
}

/// The terminal in raw mode, with the last drawn frame.
pub struct Screen {
    /// Whether the terminal is in raw mode (see [`Screen::suspend`]).
    raw: bool,
    /// The rows from the top of the drawn frame to the cursor.
    cursor_row: usize,
    /// The width of the widest drawn line.
    drawn_width: usize,
    /// The first line of the frame that shows.
    scroll: usize,
}

impl Screen {
    /// Put the terminal in raw mode, with the cursor hidden.
    pub fn open() -> Result<Self, PromptError> {
        let mut screen = Self {
            raw: false,
            cursor_row: 0,
            drawn_width: 0,
            scroll: 0,
        };
        screen.resume()?;
        Ok(screen)
    }

    /// Replace the drawn frame with the frame that `make` makes for the
    /// columns of text that the terminal has (one less than its width,
    /// so that no line wraps).
    pub fn draw(&mut self, make: impl FnOnce(usize) -> Frame) -> Result<(), PromptError> {
        // Some ptys report 0x0: draw for a common size then.
        let (columns, rows) = terminal::size()
            .ok()
            .filter(|&(columns, rows)| columns > 0 && rows > 0)
            .unwrap_or((80, 24));
        let room = usize::from(columns).saturating_sub(1).max(1);
        let height = usize::from(rows).max(1);
        let frame = make(room);
        let mut out = Vec::new();
        if self.drawn_width > room {
            queue!(
                out,
                cursor::MoveTo(0, 0),
                terminal::Clear(terminal::ClearType::All)
            )
            .map_err(PromptError::Io)?;
        } else {
            self.queue_erase(&mut out)?;
        }
        self.scroll = scroll(self.scroll, &frame.focus, frame.lines.len(), height);
        let end = frame.lines.len().min(self.scroll + height);
        let shown: Vec<_> = frame.lines[self.scroll..end]
            .iter()
            .map(|line| truncate_str(line, room, "…"))
            .collect();
        for (i, line) in shown.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(line.as_bytes());
        }
        self.drawn_width = shown
            .iter()
            .map(|line| measure_text_width(line))
            .max()
            .unwrap_or(0);
        let last = shown.len().saturating_sub(1);
        let text_cursor = frame
            .cursor
            .filter(|(line, _)| (self.scroll..end).contains(line));
        if let Some((line, column)) = text_cursor {
            let row = line - self.scroll;
            if last > row {
                queue!(out, cursor::MoveUp(count(last - row))).map_err(PromptError::Io)?;
            }
            queue!(
                out,
                cursor::MoveToColumn(count(column.min(room))),
                cursor::Show
            )
            .map_err(PromptError::Io)?;
            self.cursor_row = row;
        } else {
            queue!(out, cursor::Hide).map_err(PromptError::Io)?;
            self.cursor_row = last;
        }
        write_stderr(&out)
    }

    /// Erase the drawn frame and leave raw mode, for output in the
    /// normal mode (it goes where the frame was), until
    /// [`Screen::resume`].
    pub fn suspend(&mut self) -> Result<(), PromptError> {
        if !self.raw {
            return Ok(());
        }
        let mut out = Vec::new();
        self.queue_erase(&mut out)?;
        queue!(out, cursor::Show).map_err(PromptError::Io)?;
        // Leave raw mode even when the erase cannot be written: a tty left
        // in raw mode is worse than a stale frame.
        let written = write_stderr(&out);
        self.raw = false;
        let restored = terminal::disable_raw_mode().map_err(PromptError::Io);
        written.and(restored)
    }

    /// Back to raw mode after [`Screen::suspend`]; the next frame goes
    /// below the output in between.
    pub fn resume(&mut self) -> Result<(), PromptError> {
        terminal::enable_raw_mode().map_err(PromptError::Io)?;
        self.raw = true;
        self.cursor_row = 0;
        self.drawn_width = 0;
        let mut out = Vec::new();
        queue!(out, cursor::Hide).map_err(PromptError::Io)?;
        write_stderr(&out)
    }

    /// Erase the drawn frame and restore the terminal.
    pub fn close(mut self) -> Result<(), PromptError> {
        self.suspend()
    }

    /// Go to the top of the drawn frame and erase it to the end of the
    /// screen.
    fn queue_erase(&self, out: &mut Vec<u8>) -> Result<(), PromptError> {
        queue!(out, cursor::MoveToColumn(0)).map_err(PromptError::Io)?;
        if self.cursor_row > 0 {
            queue!(out, cursor::MoveUp(count(self.cursor_row))).map_err(PromptError::Io)?;
        }
        queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown)).map_err(PromptError::Io)
    }
}

/// Restore the terminal: erase the frame, or after a panic (its message
/// is on the screen already) go below it; show the cursor; leave raw
/// mode.
impl Drop for Screen {
    fn drop(&mut self) {
        if !self.raw {
            return;
        }
        if std::thread::panicking() {
            let _ = write_stderr(b"\r\n");
            let mut out = Vec::new();
            let _ = queue!(out, cursor::Show);
            let _ = write_stderr(&out);
            let _ = terminal::disable_raw_mode();
        } else {
            let _ = self.suspend();
        }
    }
}

/// The next key or resize, or an interrupt. Waits on the runtime between
/// the looks, so that the signal handler of [`crate::cli::initialize`]
/// runs.
pub async fn next_input() -> Result<Input, PromptError> {
    loop {
        if cli::is_interrupted() {
            return Ok(Input::Interrupted);
        }
        if !event::poll(Duration::ZERO).map_err(PromptError::Io)? {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        if let Some(input) = read_event()? {
            return Ok(input);
        }
    }
}

/// The next key or resize, or an interrupt, for a caller that cannot
/// wait on the runtime: it blocks the thread, so the signal handler of
/// [`crate::cli::initialize`] latches a signal only after the prompt
/// (Ctrl+C is a key in raw mode).
pub fn read_input() -> Result<Input, PromptError> {
    loop {
        if cli::is_interrupted() {
            return Ok(Input::Interrupted);
        }
        if event::poll(POLL_INTERVAL).map_err(PromptError::Io)?
            && let Some(input) = read_event()?
        {
            return Ok(input);
        }
    }
}

/// The waiting event as an input, if it is one (a key press or a
/// resize).
fn read_event() -> Result<Option<Input>, PromptError> {
    Ok(match event::read().map_err(PromptError::Io)? {
        Event::Key(key) if key.kind != KeyEventKind::Release => Some(Input::Key(key)),
        Event::Resize(..) => Some(Input::Resized),
        _ => None,
    })
}

/// The first line to show of `lines` lines on a terminal `height` rows
/// high, from the last one (`scroll`): the least change that shows the
/// lines of `focus` (their start, if they do not fit).
fn scroll(scroll: usize, focus: &Range<usize>, lines: usize, height: usize) -> usize {
    let scroll = scroll.min(lines.saturating_sub(height));
    let scroll = scroll.max(focus.end.saturating_sub(height));
    scroll.min(focus.start)
}

/// `n` as a cursor move count.
fn count(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn write_stderr(bytes: &[u8]) -> Result<(), PromptError> {
    let mut stderr = std::io::stderr().lock();
    stderr
        .write_all(bytes)
        .and_then(|()| stderr.flush())
        .map_err(PromptError::Io)
}
