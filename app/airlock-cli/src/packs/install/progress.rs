//! Install output on the host: one spinner line per pack with its status
//! text (`<label>: <status> [<step>/<steps>]`), the full output in
//! `.airlock/sandbox/installs.log`, and the last lines kept for the
//! failure report. With `--verbose`, the last [`WINDOW`] log lines show
//! under the spinner, updated in place.
//!
//! The exec's stdout is the status channel, its stderr the log. Protocol
//! v1 has two kinds of lines; everything else is ignored:
//! - `status <text>`: the status text;
//! - `steps <n>`: the script has `<n>` numbered steps (1 to
//!   [`MAX_STEPS`], digits only; only the first valid such line counts).
//!   The status lines after it are steps 1 to `<n>`, numbered on the host.
//!   Without it, no numbering.
//!
//! Guest output is untrusted: every line goes through
//! [`util::strip_controls`], status text is capped at [`MAX_STATUS`]
//! characters, the step number never goes past the declared count, and
//! the label next to it comes from the host.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::rc::Rc;
use std::time::Instant;

use indicatif::ProgressBar;

use crate::runtime::OutputSink;
use crate::{cli, util};

/// The install log, in the sandbox directory.
pub const INSTALLS_LOG: &str = "installs.log";
/// Lines kept per pack for the failure report.
const TAIL_LINES: usize = 40;
/// Log lines shown with `--verbose`.
const WINDOW: usize = 5;
/// Longest status text shown (characters).
const MAX_STATUS: usize = 200;
/// Most steps that a script can declare.
const MAX_STEPS: u32 = 99;
/// Longest log line shown in the verbose window (characters).
const WINDOW_WIDTH: usize = 100;
/// Largest log written; the rest is dropped.
const LOG_CAP: u64 = 8 * 1024 * 1024;
/// A longer "line" (no newline in sight) is split.
const MAX_LINE: usize = 8 * 1024;

/// An [`OutputSink`] for the install execs, one pack at a time.
pub struct InstallProgress {
    spinner: ProgressBar,
    verbose: bool,
    id: String,
    label: String,
    status: Option<String>,
    /// The step count that the script declared, if it did.
    steps: Option<u32>,
    /// The current step: the status lines since the declaration, at
    /// most `steps`.
    step: u32,
    out_partial: Vec<u8>,
    err_partial: Vec<u8>,
    tail: VecDeque<String>,
    window: VecDeque<String>,
    log: Option<File>,
    log_written: u64,
    log_cap: u64,
    /// When the guest last wrote anything (for the idle timeout).
    activity: Rc<Cell<Instant>>,
}

impl InstallProgress {
    /// Progress shown on `spinner` (no text until the first pack
    /// [`begins`](Self::begin)), logged to `log`.
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

    /// Start the output of pack `id` (shown as `label`).
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

    /// End the output of the current pack: flush its last lines and print
    /// its result line.
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

    /// Print the tail of the current pack's log, for a failure. Always
    /// shown, also with `--quiet`: it is the failure report.
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

    /// The spinner message: `<label>: <status>`, with ` [<step>/<steps>]`
    /// when the status is a step, and with `--verbose` the last log lines
    /// under it.
    fn message(&self) -> String {
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

    fn render(&self) {
        self.spinner.set_message(self.message());
    }

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

    /// A line of the status channel (see the module docs).
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
    use super::*;
    use crate::test_support::TempDir;

    fn progress(log: Option<File>, verbose: bool) -> InstallProgress {
        let mut p = InstallProgress::new(ProgressBar::hidden(), log, verbose);
        p.begin("python", "Python");
        p
    }

    #[test]
    fn status_lines_are_parsed_across_chunks() {
        let mut p = progress(None, false);
        p.stdout(b"status down");
        assert_eq!(p.status(), None);
        p.stdout(b"loading\nstat");
        assert_eq!(p.status(), Some("downloading"));
        assert_eq!(p.message(), "Python: downloading");
        p.stdout(b"us \x1b[31minstalling\x1b[0m\n");
        assert_eq!(p.status(), Some("installing"));
        // Status text is not log text.
        assert_eq!(p.tail().count(), 0);
    }

    #[test]
    fn non_status_stdout_is_ignored() {
        let mut p = progress(None, true);
        p.stdout(b"hello\nstatus\nstatus   \nSTATUS x\n");
        assert_eq!(p.status(), None);
        assert_eq!(p.tail().count(), 0);
        assert_eq!(p.message(), "Python");
    }

    #[test]
    fn status_is_capped() {
        let mut p = progress(None, false);
        let long = format!("status {}\n", "x".repeat(1000));
        p.stdout(long.as_bytes());
        assert_eq!(p.status().unwrap().chars().count(), MAX_STATUS);
    }

    #[test]
    fn stderr_is_stripped_and_the_tail_is_bounded() {
        let mut p = progress(None, false);
        p.stderr(b"\x1b[31mred\x1b[0m\r50%\r100%\n");
        assert_eq!(p.tail().collect::<Vec<_>>(), ["red", "50%", "100%"]);
        for i in 0..100 {
            p.stderr(format!("line {i}\n").as_bytes());
        }
        p.stderr(b"unterminated");
        p.end("", false);
        let tail: Vec<_> = p.tail().collect();
        assert_eq!(tail.len(), TAIL_LINES);
        assert_eq!(tail.last(), Some(&"unterminated"));
        assert_eq!(tail[0], "line 61");
        // The next pack starts with an empty tail.
        p.begin("rust", "Rust");
        assert_eq!(p.tail().count(), 0);
    }

    #[test]
    fn verbose_shows_a_rolling_window() {
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
        // Long lines are cut.
        p.stderr(format!("{}\n", "y".repeat(500)).as_bytes());
        assert!(p.message().lines().last().unwrap().len() < 200);
        // Without --verbose, no window.
        let mut q = progress(None, false);
        q.stderr(b"one\ntwo\n");
        assert_eq!(q.message(), "Python");
    }

    #[test]
    fn log_is_prefixed_and_capped() {
        let tmp = TempDir::new("packs-progress-log");
        let path = tmp.path().join(INSTALLS_LOG);
        let mut p = progress(Some(File::create(&path).unwrap()), false);
        p.stderr(b"first\n");
        p.stdout(b"status not logged\n");
        p.log_cap = 100;
        for _ in 0..50 {
            p.stderr(b"0123456789\n");
        }
        p.finish();
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.starts_with("[python] first\n"), "{log}");
        assert!(!log.contains("not logged"));
        assert!(log.len() < 200, "{}", log.len());
        assert!(log.ends_with("[airlock: log truncated]\n"));
    }
}
