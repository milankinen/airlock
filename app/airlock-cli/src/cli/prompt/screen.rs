//! Terminal control for the prompts.
//!
//! Draws a prompt view in place on stderr, below the earlier output. It does not
//! use an alternate screen. Also reads the key presses of the user.

use std::io::Write;
use std::ops::Range;
use std::time::Duration;

use console::{measure_text_width, truncate_str};
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::{cursor, queue, terminal};

use crate::cli;
use crate::cli::prompt::PromptError;

/// Interval at which [`next_input`] and [`read_input`] check for a key or an
/// interrupt.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Content of the prompt view.
pub struct Frame {
    /// The lines. Each line must fit in the columns that the frame was made
    /// for. Longer lines are cut.
    pub lines: Vec<String>,
    /// The lines of the focused row, which stay on the screen.
    pub focus: Range<usize>,
    /// Position of the text cursor (line, column), if it shows.
    pub cursor: Option<(usize, usize)>,
}

/// Input from the terminal.
pub enum Input {
    /// A key press.
    Key(KeyEvent),
    /// The terminal changed its size.
    Resized,
    /// Ctrl+C or SIGTERM from outside (a signal, not a key press).
    Interrupted,
}

/// The terminal in raw mode, with the last drawn frame.
///
/// [`Screen`] restores the terminal when dropped, also on a panic. Logs go to
/// the log file and not to the terminal (see [`crate::cli::logging`]). Thus
/// no other output comes between the frames.
pub struct Screen {
    /// Whether the terminal is in raw mode (see [`Screen::suspend`]).
    raw: bool,
    /// Number of rows from the top of the drawn frame to the cursor.
    cursor_row: usize,
    /// The width of the widest drawn line.
    drawn_width: usize,
    /// Index of the first frame line that shows.
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

    /// Replace the drawn frame with a new frame.
    /// Args:
    ///  - `make`: Makes the frame for the given number of text columns
    ///
    /// The number of text columns is one less than the terminal width, so
    /// that no line wraps. Thus the screen knows how many rows it drew. If the
    /// frame is taller than the terminal, only the part with the focused row
    /// shows (see [`scroll`]).
    // If a resize made the terminal narrower than a drawn line, the terminal
    // wrapped or cut that line. Then the drawn row count is unknown, so the
    // next frame starts at the top of a cleared screen.
    pub fn draw(&mut self, make: impl FnOnce(usize) -> Frame) -> Result<(), PromptError> {
        // Some ptys report 0x0. Then use a common size.
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

    /// Erase the drawn frame and leave raw mode, until [`Screen::resume`].
    ///
    /// Use this to write output in normal mode. The output goes where the
    /// frame was.
    pub fn suspend(&mut self) -> Result<(), PromptError> {
        if !self.raw {
            return Ok(());
        }
        let mut out = Vec::new();
        self.queue_erase(&mut out)?;
        queue!(out, cursor::Show).map_err(PromptError::Io)?;
        // Leave raw mode even if the erase fails. A tty left in raw mode is
        // worse than an old frame on the screen.
        let written = write_stderr(&out);
        self.raw = false;
        let restored = terminal::disable_raw_mode().map_err(PromptError::Io);
        written.and(restored)
    }

    /// Go back to raw mode after [`Screen::suspend`]. The next frame goes
    /// below the output that came after the suspend.
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

    /// Queue commands that move to the top of the drawn frame and erase to the
    /// end of the screen.
    fn queue_erase(&self, out: &mut Vec<u8>) -> Result<(), PromptError> {
        queue!(out, cursor::MoveToColumn(0)).map_err(PromptError::Io)?;
        if self.cursor_row > 0 {
            queue!(out, cursor::MoveUp(count(self.cursor_row))).map_err(PromptError::Io)?;
        }
        queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown)).map_err(PromptError::Io)
    }
}

/// Restore the terminal.
///
/// Erase the frame, show the cursor and leave raw mode. After a panic, do not
/// erase. Move below the frame instead, because the panic message is already
/// on the screen.
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

/// Wait for the next key press, resize or interrupt.
// Waits on the runtime between the checks, so that the signal handler of
// [`crate::cli::initialize`] can run.
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

/// Wait for the next key press, resize or interrupt, and block the thread.
///
/// For callers that cannot wait on the runtime. Because the thread blocks,
/// the signal handler of [`crate::cli::initialize`] records a signal only
/// after the prompt. In raw mode, Ctrl+C is a key press.
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

/// Read the waiting event. Return it as an input if it is a key press or a
/// resize.
fn read_event() -> Result<Option<Input>, PromptError> {
    Ok(match event::read().map_err(PromptError::Io)? {
        Event::Key(key) if key.kind != KeyEventKind::Release => Some(Input::Key(key)),
        Event::Resize(..) => Some(Input::Resized),
        _ => None,
    })
}

/// Return the first line to show.
/// Args:
///  - `scroll`: First line that the last drawn frame showed
///  - `focus`: Lines of the focused row
///  - `lines`: Total number of lines
///  - `height`: Terminal height in rows
///
/// Returns:
///   The first line after the smallest change that shows the `focus` lines.
///   If they do not fit, the start of `focus` shows.
fn scroll(scroll: usize, focus: &Range<usize>, lines: usize, height: usize) -> usize {
    let scroll = scroll.min(lines.saturating_sub(height));
    let scroll = scroll.max(focus.end.saturating_sub(height));
    scroll.min(focus.start)
}

/// Convert `n` to a cursor move count.
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
