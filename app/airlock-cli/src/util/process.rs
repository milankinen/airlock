//! Host child processes that live only as long as airlock.
//!
//! Puts a child process in its own process group, so that terminal signals
//! do not reach it and airlock can stop the full group. On Linux, the
//! kernel also stops the child if airlock dies without a clean stop. Can
//! also find out if a child stopped, before airlock reaps it.

use std::process::Command;

/// Start `cmd` in its own process group, and stop it when airlock stops.
///  * The SIGINT of the terminal (Ctrl+C in cooked mode, e.g. during the
///    boot) goes only to airlock, not to the process. Airlock then stops
///    its children in the correct order (for the VM, the guest sync first).
///  * The process is the leader of a new group. Thus [`kill_group`] also
///    stops the processes that it starts.
///  * Linux only: `PR_SET_PDEATHSIG` kills the process if airlock dies
///    without a stop (SIGKILL, crash). The signal fires when the *thread*
///    that started the process exits. Airlock starts its children on the
///    main thread (the current-thread runtime). The `getppid` check covers
///    the case where airlock dies before the `prctl`. The signal goes only
///    to the process itself. The processes that it starts continue to run
///    if airlock dies without a stop.
pub(crate) fn tie_to_airlock(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    {
        let parent = libc::pid_t::try_from(std::process::id()).expect("pid fits pid_t");
        // SAFETY: only async-signal-safe calls (two syscalls) after fork.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
}

/// Send SIGKILL to all processes in the process group of `leader`. The
/// leader must be a process that [`tie_to_airlock`] started and that is
/// not reaped yet, so that the group ID is still in use. An error (for
/// example no process left in the group) is only logged.
pub(crate) fn kill_group(leader: u32) {
    let Ok(pgid) = libc::pid_t::try_from(leader) else {
        return;
    };
    // SAFETY: plain syscall without memory arguments.
    if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
        tracing::debug!("killpg {pgid}: {}", std::io::Error::last_os_error());
    }
}

/// Check if the child process `pid` stopped. Does not reap it. Thus the
/// process stays a zombie, and its process ID and group ID cannot go to a
/// new process until airlock reaps it.
/// Returns:
///   `true` if the process stopped, or if the check fails (for example
///   the process is not a child of airlock).
pub(crate) fn exited_unreaped(pid: u32) -> bool {
    // `id_t` is `u32` on Linux and macOS.
    let id: libc::id_t = pid;
    // SAFETY: an all-zero `siginfo_t` is valid. `waitid` writes only into
    // it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, writable `siginfo_t`.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        tracing::debug!("waitid {pid}: {}", std::io::Error::last_os_error());
        return true;
    }
    // With `WNOHANG`, a process that still runs leaves `info` all zero.
    #[cfg(target_os = "linux")]
    // SAFETY: `waitid` filled `info` for a child event, or left it zero.
    let found = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let found = info.si_pid;
    found != 0
}
