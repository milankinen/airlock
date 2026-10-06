//! CLI argument parsing, console output, and interruption handling.
//!
//! Use `cli::log!("message")` for status messages,
//! `cli::layer_progress_bar()` / `cli::spinner()` for progress,
//! and `cli::interrupted()` for Ctrl+C cancellation.

pub mod cmd_exec;
pub mod cmd_rm;
pub mod cmd_secret;
pub mod cmd_show;
pub mod cmd_start;
pub mod logging;
pub mod prompt;

use std::sync::atomic::{AtomicBool, Ordering};

use clap::ValueEnum;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
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
        KvmStatus::Unavailable(err) => format!("{} kvm unavailable: {err}", red("!")),
    };
    format!("{}:\n  {kvm_line}\n", console::style("Status").underlined())
}

#[cfg(not(target_os = "linux"))]
pub fn platform_status() -> String {
    String::new()
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

/// Format a value as yellow text (for warnings)
pub fn yellow(s: &str) -> String {
    console::style(s).yellow().to_string()
}

/// Build the version string shown by `-V`.
///
/// In release builds the release action patches `AIRLOCK_VERSION_SLOT` in
/// the binary with the version bytes; we read them here. Falls back to the
/// Cargo package version in dev builds.
pub fn version_string(include_hash: bool) -> String {
    let git_hash = env!("GIT_HASH");
    let distroless = cfg!(feature = "distroless");
    let hash_suffix = if include_hash {
        format!(" ({git_hash})")
    } else {
        String::new()
    };
    match release_version() {
        Some(v) if distroless => format!("{v} [distroless]{hash_suffix}"),
        Some(v) => format!("{v}{hash_suffix}"),
        None if distroless => format!("{} [distroless]{hash_suffix}", env!("CARGO_PKG_VERSION")),
        None => format!("{}{hash_suffix}", env!("CARGO_PKG_VERSION")),
    }
}

// 16-byte sentinel + 64-byte version slot. The release action locates the
// sentinel and overwrites the slot bytes before code signing. Bytes live
// inside the binary's rodata, so the signature stays valid under
// `codesign --strict`. `#[no_mangle]` + `#[used]` force external linkage so
// the linker's dead-strip pass cannot remove the static.
#[used]
#[unsafe(no_mangle)]
pub static AIRLOCK_VERSION_SLOT: [u8; 80] = {
    let mut buf = [0u8; 80];
    let sentinel = *b"AIRLK-VER-f3a7c2";
    let mut i = 0;
    while i < sentinel.len() {
        buf[i] = sentinel[i];
        i += 1;
    }
    buf
};

fn release_version() -> Option<String> {
    const SENTINEL_LEN: usize = 16;
    // `read_volatile` prevents LTO from folding the slot's compile-time
    // initializer into the call site.
    let bytes = unsafe { std::ptr::read_volatile(&raw const AIRLOCK_VERSION_SLOT) };
    let tail = &bytes[SENTINEL_LEN..];
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    let s = std::str::from_utf8(&tail[..end]).ok()?;
    (!s.is_empty()).then(|| s.to_string())
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

/// Format a `SystemTime` as local `YYYY-MM-DD HH:MM:SS` via
/// `libc::localtime_r`. Matches the style used in the TUI's network
/// log — but kept inline here so the CLI doesn't pull in the TUI crate
/// for one helper.
pub fn format_local_time(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let tt = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&raw const tt, &raw mut tm).is_null() };
    if !ok {
        return "----/--/-- --:--:--".to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
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

/// Create a `MultiProgress` container for composing several bars in parallel.
/// Hidden in silent mode so nothing renders.
pub fn multi_progress() -> MultiProgress {
    let mp = MultiProgress::new();
    if is_silent() {
        mp.set_draw_target(ProgressDrawTarget::hidden());
    }
    mp
}

/// Create a per-layer progress bar registered inside a `MultiProgress`.
///
/// The leading `{msg}` doubles as a phase label — callers set it to
/// `downloading`, `extracting`, `ready`, or `cached` as the layer moves
/// through the pipeline. The same bar is reused across phases so each image
/// layer occupies exactly one line.
pub fn layer_progress_bar(mp: &MultiProgress, total: u64) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(total));
    pb.set_style(
        ProgressStyle::with_template("  {msg:<11} [{bar:25.240}] {bytes:>10}/{total_bytes:<10}")
            .unwrap()
            .progress_chars("━╸ "),
    );
    pb.set_message("downloading");
    pb
}

/// Append a zero-height spacer as the last line of a `MultiProgress` so
/// there's a blank line between the bars and whatever the terminal prints
/// next. Returned bar lives as long as the `MultiProgress` and is cleared
/// by the same `mp.clear()` that removes the real bars.
pub fn progress_spacer(mp: &MultiProgress) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(1));
    pb.set_style(ProgressStyle::with_template("").unwrap());
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

impl LogLevel {
    /// Map the user-facing log level to a `tracing` filter directive, used
    /// for both the host log file and the guest supervisor.
    pub fn filter(self) -> &'static str {
        match self {
            LogLevel::Trace => "info,airlock=trace,airlockd=trace",
            LogLevel::Debug => "warn,airlock=debug,airlockd=trace",
            LogLevel::Info => "warn,airlock=info,airlockd=info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_filter_keeps_the_airlock_targets() {
        assert_eq!(
            LogLevel::Trace.filter(),
            "info,airlock=trace,airlockd=trace"
        );
        assert_eq!(
            LogLevel::Debug.filter(),
            "warn,airlock=debug,airlockd=trace"
        );
        assert_eq!(LogLevel::Info.filter(), "warn,airlock=info,airlockd=info");
        assert_eq!(LogLevel::Warn.filter(), "warn");
        assert_eq!(LogLevel::Error.filter(), "error");
        for level in LogLevel::value_variants() {
            assert!(
                tracing_subscriber::EnvFilter::try_new(level.filter()).is_ok(),
                "{level:?}"
            );
        }
    }
}
