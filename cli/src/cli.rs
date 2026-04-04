//! CLI argument parsing, console output, and interruption handling.
//!
//! Use `cli::log!("message")` for status messages,
//! `cli::progress_bar()` / `cli::spinner()` for progress,
//! and `cli::interrupted()` for Ctrl+C cancellation.

use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::signal::unix::SignalKind;
use tokio::sync::watch;

// -- CLI argument parsing --

/// Lightweight VM sandbox for running untrusted code
#[derive(Parser, Debug)]
#[command(name = "ez", version, about)]
pub struct CliArgs {
    /// Suppress ez cli output
    #[arg(short, long, default_value_t = false)]
    pub quiet: bool,

    /// Debug log file path
    #[arg(long, env = "EZ_LOG_LEVEL", default_value = "warn")]
    pub log_level: LogLevel,

    /// Arguments passed to the entrypoint (e.g. ez -- -c 'echo hi')
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

// -- Console output and interruption --

static SILENT: AtomicBool = AtomicBool::new(false);
static IS_TTY: AtomicBool = AtomicBool::new(false);

static INTERRUPTED: std::sync::LazyLock<(watch::Sender<bool>, watch::Receiver<bool>)> =
    std::sync::LazyLock::new(|| watch::channel(false));

/// Green checkmark for completed steps.
pub fn check() -> String {
    console::style("\u{2714}").green().to_string()
}

/// Dim bullet for detail lines.
pub fn bullet() -> String {
    "\u{2022}".to_string()
}

/// Format a value as dim/grey text.
pub fn dim(s: &str) -> String {
    console::style(s).dim().to_string()
}

/// Format a value as red text (for errors).
pub fn red(s: &str) -> String {
    console::style(s).red().to_string()
}

/// Initialize the console; call at the very beginning of the program.
pub fn initialize() -> CliArgs {
    let args = CliArgs::parse();
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    SILENT.store(args.quiet, Ordering::Relaxed);
    IS_TTY.store(is_tty, Ordering::Relaxed);

    let tx = INTERRUPTED.0.clone();
    let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt())
        .expect("failed to register SIGINT handler");
    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())
        .expect("failed to register SIGTERM handler");
    tokio::task::spawn(async move {
        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {}
        }
        let _ = tx.send(true);
    });
    args
}

pub fn is_silent() -> bool {
    SILENT.load(Ordering::Relaxed)
}

/// Returns true if the user has pressed Ctrl+C / SIGTERM.
pub fn is_interrupted() -> bool {
    *INTERRUPTED.1.borrow()
}

/// Returns a future that resolves when the user interrupts.
pub async fn interrupted() {
    let mut rx = INTERRUPTED.1.clone();
    let _ = rx.wait_for(|&v| v).await;
}

/// Return true if cli has started in interactive mode.
pub fn is_interactive() -> bool {
    IS_TTY.load(Ordering::Relaxed)
}

/// Print a status message to stderr (unless silent).
/// Uses `\r\n` so it works correctly in raw terminal mode.
macro_rules! _log {
    ($($arg:tt)*) => {
        if !$crate::cli::is_silent() {
            eprint!("{}\r\n", format_args!($($arg)*));
        }
    };
}

macro_rules! _error {
    ($($arg:tt)*) => {
        eprint!("{}\r\n", $crate::cli::red(&format!("{}", format_args!($($arg)*))))
    };
}

pub(crate) use {_error as error, _log as log};

/// Create a progress bar for downloading (unless silent).
pub fn progress_bar(total: u64, prefix: &str) -> ProgressBar {
    if is_silent() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("  {prefix} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec}")
            .unwrap()
            .progress_chars("=> "),
    );
    pb.set_prefix(prefix.to_string());
    pb
}

/// Create a spinner for indeterminate progress (unless silent).
pub fn spinner(msg: &str) -> ProgressBar {
    if is_silent() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::with_template("  {spinner} {msg}").unwrap());
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

impl CliArgs {
    pub fn log_filter(&self) -> &str {
        match self.log_level {
            LogLevel::Trace => "info,ez=trace,ezpez_supervisor=trace",
            LogLevel::Debug => "warn,ez=debug,ezpez_supervisor=trace",
            LogLevel::Info => "warn,ez=info,ezpez_supervisor=trace",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}
