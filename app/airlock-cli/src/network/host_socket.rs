//! Host sockets of the socket forwards.
//!
//! The user picks the host socket of each socket forward. Its symlinks are
//! resolved one time, at the start. A socket path that goes into a
//! directory that the sandbox can write must stay in that directory.
//!
//! Each connection checks the path again, so a symlink that appears later
//! is refused. A socket in a directory that the sandbox can write is also
//! pinned to its identity. Then a connection to an other socket at that
//! path is refused, so the sandbox cannot swap the socket. Sockets in
//! other directories are not pinned: a host service that restarts can make
//! its socket again.

use std::cell::Cell;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::messages;
use crate::util::{VerifiedPath, vfs};
use crate::vm::mount;

/// Device and inode number of a socket.
type Id = (u64, u64);

/// A host socket that the guest can connect to.
#[derive(Debug)]
pub(crate) struct HostSocket {
    path: VerifiedPath,
    // True if the socket is in a directory that the sandbox can write.
    pinned: bool,
    // `None` until the socket exists. A pinned socket that is missing at
    // the start is pinned at the first connection.
    id: Cell<Option<Id>>,
    // The user sees the refusal one time. The log gets all refusals.
    warned: Cell<bool>,
}

impl HostSocket {
    /// Resolve the host socket path of a socket forward.
    /// Args:
    ///  - `given`: Host socket path from the config. `~` expands from
    ///    `host_home`, a relative path resolves from `cwd`.
    ///  - `host_home`: Host home directory
    ///  - `cwd`: Host project directory
    ///  - `writable`: Directories that the sandbox can write
    ///
    /// Returns:
    ///   The socket, or error if the path cannot be resolved, or if it goes
    ///   into a directory of `writable` and a symlink there points out of
    ///   it.
    pub(crate) fn resolve(
        given: &str,
        host_home: &Path,
        cwd: &Path,
        writable: &[VerifiedPath],
    ) -> anyhow::Result<Self> {
        let path = mount::absolute(given, host_home, cwd);
        let resolved = VerifiedPath::new_follow(&path, None)?;
        mount::check_stays_in(
            messages::network::SOCKET_KIND_HOST,
            &path,
            &resolved,
            writable,
        )?;
        let target = resolved.to_entry_path()?;
        let pinned = writable
            .iter()
            .any(|dir| dir.to_entry_path().is_ok_and(|dir| target.starts_with(dir)));
        let socket = Self {
            path: resolved,
            pinned,
            id: Cell::new(None),
            warned: Cell::new(false),
        };
        // A missing socket is no error here: the host service can start
        // later. A hard-linked socket is refused at the first connection.
        if pinned {
            socket.id.set(
                socket
                    .current_id()
                    .ok()
                    .filter(|(_, nlink)| *nlink == 1)
                    .map(|(id, _)| id),
            );
        }
        Ok(socket)
    }

    /// Get the resolved path of the socket.
    pub(crate) fn path(&self) -> &VerifiedPath {
        &self.path
    }

    /// Connect to the socket.
    /// Returns:
    ///   The stream, or error if the connect fails, if the path has a
    ///   symlink now, or if a pinned socket is not the same socket as at
    ///   the start.
    pub(crate) async fn connect(&self) -> io::Result<tokio::net::UnixStream> {
        if self.pinned {
            let (id, nlink) = self.current_id()?;
            // The sandbox can make a hard link to an other host socket in
            // its directory. A socket with more than one link is refused.
            if nlink > 1 {
                return Err(self.refuse());
            }
            match self.id.get() {
                None => self.id.set(Some(id)),
                Some(pinned) if pinned != id => return Err(self.refuse()),
                Some(_) => {}
            }
        }
        vfs::connect_unix_async(&self.path).await
    }

    /// Get the identity and the link count of the socket that is now at the
    /// path.
    fn current_id(&self) -> io::Result<(Id, u64)> {
        let md = vfs::metadata(&self.path)?;
        Ok(((md.dev(), md.ino()), md.nlink()))
    }

    /// Log the refusal of a replaced socket and make its error.
    fn refuse(&self) -> io::Error {
        let path = self.path.to_entry_path().unwrap_or(self.path.given());
        let text = messages::network::host_socket_replaced(path.display());
        tracing::warn!("{text}");
        if !self.warned.replace(true) {
            crate::cli::log!("{} {}", crate::cli::bullet(), crate::cli::yellow(&text));
        }
        io::Error::new(io::ErrorKind::PermissionDenied, text)
    }
}
