//! Open descriptors that go from a parent process to its child.
//!
//! Makes connected sockets for a parent and its child, lets a child process
//! keep given descriptors, and lets the child take over an inherited
//! socket. The child processes of the child do not get the descriptors.
//! Also lets a process leave its terminal.

use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;

/// Make a connected pair of Unix stream sockets. Both ends close on exec
/// until [`inherit_fds`] keeps one of them for a child.
/// Returns:
///   The end of the parent and the end of the child.
pub(crate) fn socket_pair() -> std::io::Result<(UnixStream, UnixStream)> {
    UnixStream::pair()
}

/// Let the child process of `cmd` keep the open descriptors `fds` with the
/// same numbers. Only the child keeps them: this process does not change
/// the flags of its own copies.
/// Args:
///  - `cmd`: Command of the child
///  - `fds`: Open descriptors of this process. They must stay open until
///    the spawn of `cmd`
pub(crate) fn inherit_fds(cmd: &mut tokio::process::Command, fds: Vec<RawFd>) {
    // SAFETY: The closure runs in the child before exec. It calls only the
    // async-signal-safe function fcntl.
    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            for &fd in &fds {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Take over the inherited socket descriptor `fd`. Its child processes do
/// not get it.
/// Returns:
///   The socket, or error if `fd` is a standard stream, is not open, or is
///   not a Unix stream socket.
pub(crate) fn take_socket(fd: RawFd) -> std::io::Result<UnixStream> {
    let refused = || std::io::Error::other(crate::messages::sandbox_process::INIT_FD_NOT_SOCKET);
    if fd <= 2 {
        return Err(refused());
    }
    // SAFETY: an all-zero `stat` is valid. `fstat` writes only into it.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a valid, writable `stat`.
    if unsafe { libc::fstat(fd, &raw mut stat) } == -1 {
        return Err(refused());
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(refused());
    }
    // SAFETY: The descriptor is an open socket (see above). This process
    // does not use it in another place.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: `stream` owns the open descriptor.
    if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stream)
}

/// Make this process the leader of a new session with no terminal. Then
/// the signals of the old terminal do not reach it, and it stays when the
/// terminal closes.
/// Returns:
///   Error if the call fails, for example for a process group leader.
pub(crate) fn new_session() -> std::io::Result<()> {
    // SAFETY: setsid has no memory arguments.
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
