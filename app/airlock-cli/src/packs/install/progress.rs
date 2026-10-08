//! Pack install progress output.
//!
//! Shows the progress of the pack install scripts on the host. Each pack
//! gets a spinner line with the status text that its script reports. The
//! full script output goes to an install log, and the last lines show in
//! the failure report. Install scripts report their status with a small
//! line protocol.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::rc::Rc;
use std::time::Instant;

use indicatif::ProgressBar;

use crate::runtime::OutputSink;
use crate::{cli, util};

/// File name of the install log in the sandbox directory.
pub const INSTALLS_LOG: &str = "installs.log";
/// Number of lines kept for each pack for the failure report.
const TAIL_LINES: usize = 40;
/// Number of log lines shown with `--verbose`.
const WINDOW: usize = 5;
/// Maximum length of the status text shown, in characters.
const MAX_STATUS: usize = 200;
/// Maximum number of steps that a script can declare.
const MAX_STEPS: u32 = 99;
/// Maximum length of a log line in the verbose window, in characters.
const WINDOW_WIDTH: usize = 100;
/// Maximum size of the log file. Output after this limit is dropped.
const LOG_CAP: u64 = 8 * 1024 * 1024;
/// Maximum line length in bytes. A longer "line" (no newline) is split.
const MAX_LINE: usize = 8 * 1024;

/// An [`OutputSink`] for the install execs, one pack at a time.
///
/// The stdout of the exec is the status channel, and its stderr is the
/// log. The install log is `.airlock/sandbox/installs.log` (see
/// [`INSTALLS_LOG`]). With `--verbose`, the last log lines show under the
/// spinner.
///
/// Status protocol v1 (`AIRLOCK_PACK_API=1`) has two kinds of lines. Other
/// lines are ignored:
///  - `status <text>`: The status text.
///  - `steps <n>`: The script has `<n>` numbered steps (1 to
///    [`MAX_STEPS`], digits only). Only the first valid line of this
///    kind counts. The host numbers the status lines after it as steps
///    1 to `<n>`. Without it, there is no numbering.
///
/// Pack scripts write these lines with the `airlock_steps` and
/// `airlock_status` helpers of `lib.sh` (see [`super::compose`]).
///
/// Guest output is untrusted. Thus:
///  * each line goes through [`util::strip_controls`]
///  * status text is at most [`MAX_STATUS`] characters
///  * the step number never goes past the declared count
///  * the label next to the status comes from the host
pub struct InstallProgress {
    spinner: ProgressBar,
    verbose: bool,
    /// Name of the current pack.
    id: String,
    /// Human-readable name of the current pack.
    label: String,
    /// Status text of the current pack.
    status: Option<String>,
    /// Step count that the script declared, if any.
    steps: Option<u32>,
    /// Current step: the number of status lines after the declaration, at
    /// most `steps`.
    step: u32,
    out_partial: Vec<u8>,
    err_partial: Vec<u8>,
    tail: VecDeque<String>,
    window: VecDeque<String>,
    log: Option<File>,
    log_written: u64,
    log_cap: u64,
    /// Time of the last guest output (for the idle timeout).
    activity: Rc<Cell<Instant>>,
}

impl InstallProgress {
    /// Create the progress output.
    /// Args:
    ///  - `spinner`: Spinner to show the progress on. It has no text until
    ///    the first pack [`begins`](Self::begin).
    ///  - `log`: Install log file, if any
    ///  - `verbose`: Show the last log lines under the spinner
    pub fn new(spinner: ProgressBar, log: Option<File>, verbose: bool) -> Self {
        Self {
            spinner,
            verbose,
            id: String::new(),
            label: String::new(),
            status: None,
            steps: None,
            step: 0,
            out_partial: Vec::new(),
            err_partial: Vec::new(),
            tail: VecDeque::with_capacity(TAIL_LINES),
            window: VecDeque::with_capacity(WINDOW),
            log,
            log_written: 0,
            log_cap: LOG_CAP,
            activity: Rc::new(Cell::new(Instant::now())),
        }
    }

    /// Shared time of the last output.
    pub fn activity(&self) -> Rc<Cell<Instant>> {
        self.activity.clone()
    }

    /// Start the output of a pack.
    /// Args:
    ///  - `id`: Pack name, written in the log
    ///  - `label`: Human-readable pack name, shown on the spinner
    pub fn begin(&mut self, id: &str, label: &str) {
        self.id = id.to_string();
        self.label = label.to_string();
        self.status = None;
        self.steps = None;
        self.step = 0;
        self.tail.clear();
        self.window.clear();
        self.activity.set(Instant::now());
        self.render();
    }

    /// End the output of the current pack. Flush its last lines and print
    /// its result line.
    /// Args:
    ///  - `result`: Text after the label in the result line
    ///  - `ok`: True if the install succeeded
    pub fn end(&mut self, result: &str, ok: bool) {
        for stdout in [true, false] {
            let partial = if stdout {
                std::mem::take(&mut self.out_partial)
            } else {
                std::mem::take(&mut self.err_partial)
            };
            if !partial.is_empty() {
                self.line(&partial, stdout);
            }
        }
        let mark = if ok { cli::check() } else { cli::red("✗") };
        self.spinner
            .println(format!("  {mark} {}{result}", self.label));
        self.window.clear();
        self.status = None;
    }

    /// Print the last log lines of the current pack, for a failure. The
    /// lines show also with `--quiet`, because they are the failure report.
    pub fn print_tail(&self) {
        let tail: Vec<&str> = self.tail().collect();
        if tail.is_empty() {
            return;
        }
        self.spinner.suspend(|| {
            cli::error!("Last output of {}:", self.label);
            for line in tail {
                eprint!("  {}\r\n", cli::dim(line));
            }
        });
    }

    /// The status text of the current pack.
    #[cfg(test)]
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// The last [`TAIL_LINES`] log lines of the current pack.
    pub fn tail(&self) -> impl Iterator<Item = &str> {
        self.tail.iter().map(String::as_str)
    }

    /// Flush the log and remove the spinner.
    pub fn finish(&mut self) {
        if let Some(log) = &mut self.log {
            let _ = log.flush();
        }
        self.spinner.finish_and_clear();
    }

    /// Make the spinner message: `<label>: <status>`. Adds
    /// ` [<step>/<steps>]` if the status is a step. With `--verbose`, adds
    /// the last log lines under it.
    pub(crate) fn message(&self) -> String {
        let label = &self.label;
        let mut msg = match (&self.status, self.steps.filter(|_| self.step > 0)) {
            (None, _) => label.clone(),
            (Some(status), None) => format!("{label}: {status}"),
            (Some(status), Some(steps)) => format!("{label}: {status} [{}/{steps}]", self.step),
        };
        if self.verbose {
            for line in &self.window {
                msg.push_str("\n    ");
                msg.push_str(&cli::dim(line));
            }
        }
        msg
    }

    /// Show the current message on the spinner.
    fn render(&self) {
        self.spinner.set_message(self.message());
    }

    /// Split guest output into lines and handle each complete line.
    fn feed(&mut self, bytes: &[u8], stdout: bool) {
        self.activity.set(Instant::now());
        for &b in bytes {
            let partial = if stdout {
                &mut self.out_partial
            } else {
                &mut self.err_partial
            };
            if b == b'\n' || b == b'\r' {
                let line = std::mem::take(partial);
                self.line(&line, stdout);
            } else {
                partial.push(b);
                if partial.len() >= MAX_LINE {
                    let line = std::mem::take(partial);
                    self.line(&line, stdout);
                }
            }
        }
    }

    /// Handle one line of stdout (status) or stderr (log).
    fn line(&mut self, raw: &[u8], stdout: bool) {
        let text = util::strip_controls(raw);
        let text = text.trim_end();
        if text.is_empty() {
            return;
        }
        if stdout {
            self.status_line(text);
            return;
        }
        self.write_log(text);
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(text.to_string());
        if self.verbose {
            if self.window.len() == WINDOW {
                self.window.pop_front();
            }
            self.window
                .push_back(text.chars().take(WINDOW_WIDTH).collect());
            self.render();
        }
    }

    /// Handle a line of the status channel (protocol v1, see
    /// [`InstallProgress`]).
    fn status_line(&mut self, text: &str) {
        if let Some(status) = text.strip_prefix("status ") {
            let status: String = status.trim().chars().take(MAX_STATUS).collect();
            if !status.is_empty() {
                if let Some(steps) = self.steps {
                    self.step = (self.step + 1).min(steps);
                }
                self.status = Some(status);
                self.render();
            }
        } else if let Some(count) = text.strip_prefix("steps ")
            && self.steps.is_none()
            && count.bytes().all(|b| b.is_ascii_digit())
        {
            self.steps = count.parse().ok().filter(|n| (1..=MAX_STEPS).contains(n));
        }
    }

    /// Write a line to the install log, up to the size limit.
    fn write_log(&mut self, text: &str) {
        let Some(log) = &mut self.log else {
            return;
        };
        if self.log_written >= self.log_cap {
            return;
        }
        let line = format!("[{}] {text}\n", self.id);
        self.log_written += line.len() as u64;
        let _ = log.write_all(line.as_bytes());
        if self.log_written >= self.log_cap {
            let _ = log.write_all(b"[airlock: log truncated]\n");
        }
    }
}

impl OutputSink for InstallProgress {
    fn stdout(&mut self, bytes: &[u8]) {
        self.feed(bytes, true);
    }

    fn stderr(&mut self, bytes: &[u8]) {
        self.feed(bytes, false);
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the install progress: the status protocol, the log, the
    //! failure tail and the verbose window.

    use super::*;
    use crate::test_cfg::temp_dir;

    /// A progress with a hidden spinner where the pack `python` began.
    fn progress(log: Option<File>, verbose: bool) -> InstallProgress {
        let mut p = InstallProgress::new(ProgressBar::hidden(), log, verbose);
        p.begin("python", "Python");
        p
    }

    /// Test that the progress joins status lines across output chunks,
    /// removes control characters and limits the length. Guest output is
    /// not trusted.
    ///   1. Send lines that are not valid status lines and a status line
    ///      that continues in the next chunk
    ///   2. Check that only the joined status shows
    ///   3. Send a status with color codes and check that they are removed
    ///   4. Send a very long status and check that it is cut to the limit
    #[test]
    fn status_lines_split_across_chunks_are_sanitized_and_capped() {
        let mut p = progress(None, true);
        // A status with no text and the upper case keyword are ignored.
        // "status down" continues in the next chunk.
        p.stdout(b"hello\nstatus\nstatus   \nSTATUS x\nstatus down");
        assert_eq!(p.message(), "Python");
        p.stdout(b"loading\nstat");
        assert_eq!(p.message(), "Python: downloading");
        p.stdout(b"us \x1b[31minstalling\x1b[0m\n");
        assert_eq!(p.status(), Some("installing"));
        p.stdout(format!("status {}\n", "x".repeat(1000)).as_bytes());
        assert_eq!(p.status().unwrap().chars().count(), MAX_STATUS);
        // Stdout is the status channel only. It does not go to the tail.
        assert_eq!(p.tail().count(), 0);
    }

    /// Test that a valid step count numbers the status lines, and that the
    /// step number never goes past the count.
    ///   1. Send step counts that are not valid and check that the status
    ///      has no number
    ///   2. Begin again, send two step counts and check that only the
    ///      first counts
    ///   3. Send more status lines than steps and check that the number
    ///      stops at the count
    #[test]
    fn declared_steps_number_status_lines_up_to_count() {
        let mut p = progress(None, false);
        // The count must be digits only, from 1 to 99.
        p.stdout(b"steps 0\nsteps 100\nsteps 2x\nstatus first\n");
        assert_eq!(p.message(), "Python: first");
        p.begin("python", "Python");
        p.stdout(b"steps 2\nsteps 5\nstatus one\n");
        assert_eq!(p.message(), "Python: one [1/2]");
        p.stdout(b"status two\nstatus three\n");
        assert_eq!(p.message(), "Python: three [2/2]");
    }

    /// Test that log lines lose their control characters and that the tail
    /// keeps only the last lines of the current pack.
    ///   1. Send a line with color codes and carriage returns and check the
    ///      split, clean lines
    ///   2. Send 100 lines and an unterminated line, then end the pack
    ///   3. Check that the tail has the last lines, with the unterminated
    ///      line last
    ///   4. Begin the next pack and check that the tail is empty
    #[test]
    fn stderr_lines_are_sanitized_and_tail_keeps_last_lines() {
        let mut p = progress(None, false);
        // A carriage return also ends a line, as in progress bars.
        p.stderr(b"\x1b[31mred\x1b[0m\r50%\r100%\n");
        assert_eq!(p.tail().collect::<Vec<_>>(), ["red", "50%", "100%"]);
        for i in 0..100 {
            p.stderr(format!("line {i}\n").as_bytes());
        }
        // The end of the pack flushes the unterminated line.
        p.stderr(b"unterminated");
        p.end("", false);
        let tail: Vec<_> = p.tail().collect();
        assert_eq!(tail.len(), TAIL_LINES);
        assert_eq!(tail.last(), Some(&"unterminated"));
        assert_eq!(tail[0], "line 61");
        p.begin("rust", "Rust");
        assert_eq!(p.tail().count(), 0);
    }

    /// Test that the verbose window shows the last log lines under the
    /// label, cut to a maximum width.
    ///   1. Send eight log lines in verbose mode
    ///   2. Check that the message has the label and the last five lines
    ///   3. Send a very long line and check that it is cut
    ///   4. Check that without verbose mode only the label shows
    #[test]
    fn verbose_window_shows_last_lines_cut_to_width() {
        let mut p = progress(None, true);
        for i in 0..8 {
            p.stderr(format!("line {i}\n").as_bytes());
        }
        let msg = p.message();
        let lines: Vec<&str> = msg.lines().collect();
        assert_eq!(lines.len(), 1 + WINDOW, "{msg}");
        assert_eq!(lines[0], "Python");
        assert!(lines[1].contains("line 3"), "{msg}");
        assert!(lines[WINDOW].contains("line 7"), "{msg}");
        p.stderr(format!("{}\n", "y".repeat(500)).as_bytes());
        // The cut is 100 characters. The dim style adds some bytes.
        assert!(p.message().lines().last().unwrap().len() < 200);
        let mut quiet = progress(None, false);
        quiet.stderr(b"one\ntwo\n");
        assert_eq!(quiet.message(), "Python");
    }

    /// Test that the install log stops at its size limit and ends with a
    /// marker. A script must not fill the host disk.
    ///   1. Write one line, then set a small limit
    ///   2. Write many lines
    ///   3. Check that the log has the first line, stays small and ends
    ///      with the marker
    #[test]
    fn log_stops_at_cap_with_truncation_marker() {
        let tmp = temp_dir();
        let path = tmp.path().join(INSTALLS_LOG);
        let mut p = progress(Some(File::create(&path).unwrap()), false);
        p.stderr(b"first\n");
        p.log_cap = 100;
        for _ in 0..50 {
            p.stderr(b"0123456789\n");
        }
        p.finish();
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.starts_with("[python] first\n"), "{log}");
        assert!(log.len() < 200, "{}", log.len());
        assert!(log.ends_with("[airlock: log truncated]\n"));
    }
}
