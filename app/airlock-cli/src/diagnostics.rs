//! Crash and panic diagnostics for the CLI.
//!
//! Makes failures visible:
//!  * logs panics, also panics in background tasks
//!  * reports fatal signals, for example crashes in native libraries
//!
//! Release builds do not print backtraces, also when the user's
//! environment asks for them.
//!
//! The CLI installs these handlers when it starts, so they cover all later
//! failures.

/// Disable panic and error backtraces in release builds.
///
/// Removes `RUST_BACKTRACE` and `RUST_LIB_BACKTRACE` from the process
/// environment. Call this first in `main`, before any other thread starts.
/// Child processes also do not get these variables.
pub fn disable_release_backtraces() {
    #[cfg(not(debug_assertions))]
    // SAFETY: The caller runs this first in `main`. The current-thread tokio
    // runtime has no worker threads, and its blocking pool starts threads
    // only on first use. So no other thread reads the environment now.
    // std and anyhow read these variables on first use and then keep the
    // value, so the removal applies to all later panics and errors.
    unsafe {
        std::env::remove_var("RUST_BACKTRACE");
        std::env::remove_var("RUST_LIB_BACKTRACE");
    }
}

/// Add a logging hook in front of the default Rust panic hook.
///
/// The hook writes each panic to `airlock.log` (after logging starts). This
/// includes panics in background `spawn_local` tasks, which tokio otherwise
/// hides. The default hook still prints the panic to stderr. It prints the
/// backtrace only in debug builds, if the environment asks for it.
///
/// The hook does not restore the terminal. The `Drop` of the raw mode guard
/// does this during unwind.
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

/// Install a minimal handler for the fatal signals `SIGSEGV`, `SIGBUS`,
/// `SIGILL` and `SIGABRT`.
///
/// These signals bypass the Rust panic machinery. They are the most likely
/// result when an in-process FFI dependency crashes (for example Apple's
/// Virtualization.framework on macOS). The handler writes
/// `"[airlock] fatal signal N\n"` to stderr. Then it raises the signal again
/// with the default disposition, so the process still dies with the correct
/// signal (core dump and shell exit status stay the same).
///
/// The handler does not restore the terminal, because a signal handler cannot
/// safely call `tcsetattr`. After such an exit, the user's terminal may still
/// need `stty sane`, but the logs show the cause.
pub fn install_fatal_signal_handlers() {
    for sig in [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGABRT] {
        // SAFETY: `fatal_signal_handler` is async-signal-safe (it calls only
        // write/signal/raise). Installing a handler once per signal is safe.
        // The handler is never removed.
        unsafe {
            libc::signal(sig, fatal_signal_handler as *const () as libc::sighandler_t);
        }
    }
}

/// Signal handler for [`install_fatal_signal_handlers`].
extern "C" fn fatal_signal_handler(sig: libc::c_int) {
    const PREFIX: &[u8] = b"[airlock] fatal signal ";
    // Format the signal number into a 4-byte buffer (digits only, no NUL).
    // 4 bytes is sufficient for all Unix signal numbers.
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
            // tmp has the digits in reverse order. Reverse them.
            let mut out = [0u8; 4];
            for j in 0..i {
                out[j] = tmp[i - 1 - j];
            }
            (out, i)
        }
    };
    // SAFETY: write(2) is async-signal-safe. It is the only stdio primitive
    // that is async-signal-safe. fd 2 is stderr, which is always
    // open in all supported deployments. The return value is ignored because
    // the process dies next.
    unsafe {
        libc::write(2, PREFIX.as_ptr().cast(), PREFIX.len());
        libc::write(2, num.as_ptr().cast(), len);
        libc::write(2, b"\n".as_ptr().cast(), 1);
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}
