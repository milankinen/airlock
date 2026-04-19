//! CLI argument parsing, console output, and interruption handling.
//!
//! Use `cli::log!("message")` for status messages,
//! `cli::progress_bar()` / `cli::spinner()` for progress,
//! and `cli::interrupted()` for Ctrl+C cancellation.

pub mod cmd_exec;
pub mod cmd_rm;
pub mod cmd_secret;
pub mod cmd_show;
pub mod cmd_start;

use std::sync::atomic::{AtomicBool, Ordering};

use clap::ValueEnum;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::signal::unix::SignalKind;
use tokio::sync::watch;

// -- CLI argument parsing --

#[cfg(target_os = "linux")]
pub fn platform_status() -> String {
    use crate::vm::{KvmStatus, kvm_status};
    let kvm_line = match kvm_status() {
        KvmStatus::Available => format!("{} kvm access granted", check()),
        KvmStatus::NotFound => format!("{} kvm not available", red("!")),
        KvmStatus::NoPermission => format!("{} kvm permission denied", red("!")),
    };
    format!("{}:\n  {kvm_line}\n", console::style("Status").underlined())
}

#[cfg(not(target_os = "linux"))]
pub fn platform_status() -> String {
    String::new()
}

/// Runtime arguments for the `up` command. Constructed from parsed CLI args
/// plus any extra arguments that appeared after `--`.
pub struct CliArgs {
    pub log_level: LogLevel,
    pub args: Vec<String>,
    pub login: bool,
}

impl CliArgs {
    pub fn new(log_level: LogLevel, extra_args: Vec<String>, login: bool) -> Self {
        Self {
            log_level,
            args: extra_args,
            login,
        }
    }
}

/// Supervisor log verbosity level, mapped to `tracing` filter strings.
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
static VERBOSE: AtomicBool = AtomicBool::new(false);

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

/// Build the version string shown by `-V`.
///
/// In release builds the release action appends `<version><len:u32le>` to
/// the binary; we detect that here and show it alongside the git hash.
/// Falls back to the Cargo package version in dev builds.
pub fn version_string() -> String {
    let git_hash = env!("GIT_HASH");
    let distroless = cfg!(feature = "distroless");
    match release_version() {
        Some(v) if distroless => format!("{v} [distroless] ({git_hash})"),
        Some(v) => format!("{v} ({git_hash})"),
        None if distroless => format!("{} [distroless] ({git_hash})", env!("CARGO_PKG_VERSION")),
        None => format!("{} ({git_hash})", env!("CARGO_PKG_VERSION")),
    }
}

/// Read the release version appended to the binary by the release action.
/// Format: `<utf8 version string><length as u32 little-endian>`.
fn release_version() -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
    let file_len = f.seek(SeekFrom::End(0)).ok()?;
    if file_len < 4 {
        return None;
    }
    f.seek(SeekFrom::End(-4)).ok()?;
    let mut len_bytes = [0u8; 4];
    f.read_exact(&mut len_bytes).ok()?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len == 0 || len >= 20 || (file_len as usize) < len + 4 {
        return None;
    }
    f.seek(SeekFrom::End(-(len as i64 + 4))).ok()?;
    let mut ver_bytes = vec![0u8; len];
    f.read_exact(&mut ver_bytes).ok()?;
    Some(String::from_utf8_lossy(&ver_bytes).into_owned())
}

/// Initialize the console; call at the very beginning of the program.
pub fn initialize(quiet: bool) {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    SILENT.store(quiet, Ordering::Relaxed);
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
}

/// Returns true if `--quiet` was passed.
pub fn is_silent() -> bool {
    SILENT.load(Ordering::Relaxed)
}

/// Returns true if `--verbose` was passed.
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Enable verbose output for the current command.
pub fn set_verbose(value: bool) {
    VERBOSE.store(value, Ordering::Relaxed);
}

/// Format a byte count as a human-readable string (GB/MB/KB).
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
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

macro_rules! _verbose {
    ($($arg:tt)*) => {
        if $crate::cli::is_verbose() && !$crate::cli::is_silent() {
            eprint!("{}\r\n", format_args!($($arg)*));
        }
    };
}

pub(crate) use _error as error;
pub(crate) use _log as log;
pub(crate) use _verbose as verbose;

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
    /// Map the user-facing log level to a `tracing` filter directive.
    pub fn log_filter(&self) -> &str {
        Self::log_filter_for(self.log_level)
    }

    /// Map a log level to a `tracing` filter directive (static version).
    pub fn log_filter_for(level: LogLevel) -> &'static str {
        match level {
            LogLevel::Trace => "info,airlock=trace,airlockd=trace",
            LogLevel::Debug => "warn,airlock=debug,airlockd=trace",
            LogLevel::Info => "warn,airlock=info,airlockd=info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}
