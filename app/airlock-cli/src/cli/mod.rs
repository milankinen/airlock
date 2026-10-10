//! Command-line interface.
//!
//! Contains the airlock CLI commands and the console output that they share:
//!  * status, verbose and error messages
//!  * progress bars and spinners
//!  * text styles and colors
//!  * Ctrl+C and SIGTERM handling, so that long steps can stop cleanly

pub mod cmd_exec;
pub mod cmd_info;
pub mod cmd_rm;
pub mod cmd_sandbox;
pub mod cmd_secret;
pub mod cmd_start;
pub mod logging;
pub mod prompt;

use std::sync::atomic::{AtomicBool, Ordering};

use clap::ValueEnum;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use tokio::signal::unix::SignalKind;
use tokio::sync::watch;

// -- CLI argument parsing --

/// The "airlock" logo. The setup wizard and the top-level help show it.
pub const LOGO: [&str; 4] = [
    "   ▗    ▜       ▌",
    "▝▀▖▄ ▙▀▖▐ ▞▀▖▞▀▖▌▗▘",
    "▞▀▌▐ ▌  ▐ ▌ ▌▌ ▖▛▚",
    "▝▀▘▀▘▘   ▘▝▀ ▝▀ ▘ ▘",
];

/// Return the help template for the top-level help. It shows an empty
/// line and the logo in bold text above the description.
pub fn help_template() -> String {
    let logo = LOGO
        .iter()
        .map(|line| console::style(line).bold().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    // Same as the default template, but with the logo in place of the
    // "before help" text. The "before help" text always has an empty line
    // after it, and the description must come directly below the logo.
    // Clap removes the first line of the help when it is empty, thus the
    // template starts with two line breaks to keep one empty line.
    format!(
        "\n\n{logo}\n{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{{all-args}}{{after-help}}"
    )
}

/// Return a "Status" section with the KVM access state for help output.
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

/// Return an empty status section. Other platforms have no status to show.
#[cfg(not(target_os = "linux"))]
pub fn platform_status() -> String {
    String::new()
}

/// Log verbosity level. See [`LogLevel::filter`] for the `tracing` filter.
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

/// Bullet for detail lines.
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

/// Format a value as yellow text (for warnings).
pub fn yellow(s: &str) -> String {
    console::style(s).yellow().to_string()
}

/// Build the version string that `-V` shows.
/// Args:
///  - `include_hash`: Add the git commit hash to the end of the string
///
/// Returns:
///   The release version from [`AIRLOCK_VERSION_SLOT`], or the Cargo package
///   version in dev builds. Distroless builds add a `[distroless]` tag.
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

/// Version slot that the release action patches in the binary.
///
/// Layout: a 16-byte sentinel, then a 64-byte version slot. The release action
/// finds the sentinel and writes the version into the slot before code signing.
/// The bytes are in the binary's rodata, so the signature stays valid under
/// `codesign --strict`.
// `#[no_mangle]` and `#[used]` force external linkage. Thus the linker's
// dead-strip pass cannot remove the static.
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
    // `read_volatile` prevents LTO from putting the compile-time initializer
    // of the slot into the call site.
    let bytes = unsafe { std::ptr::read_volatile(&raw const AIRLOCK_VERSION_SLOT) };
    let tail = &bytes[SENTINEL_LEN..];
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    let s = std::str::from_utf8(&tail[..end]).ok()?;
    (!s.is_empty()).then(|| s.to_string())
}

/// Initialize the console. Call this at the start of the program.
/// Args:
///  - `quiet`: Do not print status messages and progress
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

/// Return true if the user gave `--quiet`.
pub fn is_silent() -> bool {
    SILENT.load(Ordering::Relaxed)
}

/// Return true if the user gave `--verbose`.
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Enable or disable verbose output for the current command.
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

/// Format a `SystemTime` as local time in `YYYY-MM-DD HH:MM:SS` format.
// The TUI network log uses the same `localtime_r` conversion, but a different
// format. The helper is a copy, so the CLI does not need the TUI crate for one
// function.
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

/// Return true if the user pressed Ctrl+C or the process got SIGTERM.
pub fn is_interrupted() -> bool {
    *INTERRUPTED.1.borrow()
}

/// Return a future that completes when the user interrupts the program.
pub async fn interrupted() {
    let mut rx = INTERRUPTED.1.clone();
    let _ = rx.wait_for(|&v| v).await;
}

/// Return true if the CLI started in interactive mode (stdin is a TTY).
pub fn is_interactive() -> bool {
    IS_TTY.load(Ordering::Relaxed)
}

/// Print a status message to stderr, unless silent.
// `\r\n` makes the output correct in raw terminal mode.
macro_rules! _log {
    ($($arg:tt)*) => {
        if !$crate::cli::is_silent() {
            eprint!("{}\r\n", format_args!($($arg)*));
        }
    };
}

/// Print an error message in red to stderr. Silent mode does not apply.
macro_rules! _error {
    ($($arg:tt)*) => {
        eprint!("{}\r\n", $crate::cli::red(&format!("{}", format_args!($($arg)*))))
    };
}

/// Print a status message to stderr in verbose mode, unless silent.
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

/// Create a `MultiProgress` container that shows many bars at the same time.
/// In silent mode, the container is hidden.
pub fn multi_progress() -> MultiProgress {
    let mp = MultiProgress::new();
    if is_silent() {
        mp.set_draw_target(ProgressDrawTarget::hidden());
    }
    mp
}

/// Create a progress bar for one image layer inside `mp`.
/// Args:
///  - `mp`: Container that shows the bar
///  - `total`: Total size of the layer in bytes
///
/// Returns:
///   A progress bar. Its message is the phase label: `downloading`,
///   `extracting`, `ready` or `cached`. Callers use the same bar for all
///   phases, so each layer uses exactly one line.
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

/// Add an empty spacer line as the last line of `mp`.
///
/// The spacer puts a blank line between the bars and the next terminal output.
/// The returned bar lives as long as `mp`. The `mp.clear()` call that removes
/// the other bars also removes the spacer.
pub fn progress_spacer(mp: &MultiProgress) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(1));
    pb.set_style(ProgressStyle::with_template("").unwrap());
    pb
}

/// Create a spinner for progress of unknown length. In silent mode, the
/// spinner is hidden.
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
    /// Return the `tracing` filter directive for this log level. The host log
    /// file and the guest supervisor both use it.
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
mod tests;
