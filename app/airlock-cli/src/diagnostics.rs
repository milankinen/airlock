//! Crash / panic instrumentation for the CLI.
//!
//! Installed first in `main.rs` so that any failure after this point
//! is *visible*:
//!
//! - [`install_panic_hook`] chains a logging hook in front of the
//!   default Rust panic hook. Every panic — including those in
//!   `spawn_local` background tasks that tokio would otherwise
//!   swallow silently — is written to `airlock.log` via `tracing`.
//! - [`install_fatal_signal_handlers`] catches `SIGSEGV` / `SIGBUS` /
//!   `SIGILL` (which bypass Rust's panic machinery entirely — the
//!   prime candidates when an in-process FFI dep, e.g. Apple's
//!   Virtualization.framework on macOS, crashes). Writes a terse
//!   marker line to stderr and re-raises the default handler so the
//!   OS still delivers the original signal (coredump / shell exit
//!   status intact).
//!
//! Neither attempts terminal restoration — `Drop` on the raw-mode
//! guard handles the unwind case, and signal handlers can't safely
//! call `tcsetattr`. If the process dies abnormally the user's
//! terminal may still need `stty sane`, but they'll at least see
//! *why* in the logs.

/// Chain a logging hook in front of the default Rust panic hook.
///
/// The default hook already prints the panic + backtrace to stderr;
/// we additionally send it through `tracing::error` so it lands in
/// `airlock.log` (once logging is initialised). Panics in background
/// `spawn_local` tasks, which tokio otherwise swallows, now show up
/// in the log with the task's panic message.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info.location().map_or_else(
            || "<unknown>".to_string(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("Box<dyn Any>");
        tracing::error!("panic at {loc}: {payload}");
        default(info);
    }));
}

/// Install a minimal signal handler for the three synchronous "your
/// program is broken" signals. Writes `"[airlock] fatal signal N\n"`
/// to stderr (via `write(2)`, the only async-signal-safe stdio
/// primitive) and re-raises the default disposition so the process
/// still dies with the correct signal.
pub fn install_fatal_signal_handlers() {
    for sig in [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGABRT] {
        // SAFETY: `fatal_signal_handler` is async-signal-safe (only
        // calls write/signal/raise). Installing a handler once per
        // signal is safe; we never un-install.
        unsafe {
            libc::signal(sig, fatal_signal_handler as *const () as libc::sighandler_t);
        }
    }
}

extern "C" fn fatal_signal_handler(sig: libc::c_int) {
    const PREFIX: &[u8] = b"[airlock] fatal signal ";
    // Format the signal number into a 4-byte buffer (digits only, no
    // NUL). 4 bytes is enough for any Unix signal number.
    let (num, len) = {
        let mut tmp = [0u8; 4];
        let mut n = if sig < 0 { 0_u32 } else { sig as u32 };
        if n == 0 {
            tmp[0] = b'0';
            (tmp, 1)
        } else {
            let mut i = 0;
            while n > 0 && i < tmp.len() {
                tmp[i] = (n % 10) as u8 + b'0';
                n /= 10;
                i += 1;
            }
            // tmp holds digits in reverse; flip in place.
            let mut out = [0u8; 4];
            for j in 0..i {
                out[j] = tmp[i - 1 - j];
            }
            (out, i)
        }
    };
    // SAFETY: write(2) is async-signal-safe. fd 2 is stderr, always
    // open in any supported deployment. Ignoring the return value is
    // fine — we're about to die anyway.
    unsafe {
        libc::write(2, PREFIX.as_ptr().cast(), PREFIX.len());
        libc::write(2, num.as_ptr().cast(), len);
        libc::write(2, b"\n".as_ptr().cast(), 1);
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}
