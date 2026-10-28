//! Client for a running sandbox.
//!
//! Finds the sandbox that serves a directory and opens an RPC connection
//! to its `airlock start` process. Also gives the guest directory of a
//! host directory in the project. Commands that act on a running sandbox
//! share this code. Never follows symlinks while it looks for the sandbox,
//! because code in a sandbox can write its project mount.

use airlock_common::cli_capnp::cli_service;
use futures::AsyncReadExt;

use crate::context::Context;
use crate::sandboxes::Found;
use crate::util::verified_path::VerifiedPath;
use crate::{messages, project, sandboxes};

/// A running sandbox that the lookup found.
#[derive(Debug)]
pub struct RunningSandbox {
    /// CLI socket of the sandbox.
    pub sock: VerifiedPath,
    /// The sandbox and its project dir.
    pub found: Found,
    /// Project dir in the guest.
    guest_root: String,
}

impl RunningSandbox {
    /// Get the guest dir of the host dir `host`.
    /// Returns:
    ///   For a dir in the project, the same dir below the project dir of
    ///   the guest. For other dirs, the host path with its symlinks
    ///   resolved. Error if a part of `host` changed since the check.
    // The project mount of the guest is at the guest cwd of `airlock
    // start`, which can differ from the host path (`--sandbox-cwd`). A dir
    // out of the project, for example a git worktree of the project, has
    // no project mount. A user mount at the same path can have it.
    pub fn guest_dir(&self, host: &VerifiedPath) -> anyhow::Result<String> {
        let host = sandboxes::project_dir(host)?;
        Ok(match host.relative_to(&self.found.project) {
            Some(rel) if rel.is_empty() => self.guest_root.clone(),
            Some(rel) => format!("{}/{rel}", self.guest_root.trim_end_matches('/')),
            None => host.to_path()?.to_string_lossy().into_owned(),
        })
    }
}

/// Find the nearest running sandbox at or above `work`.
/// Args:
///  - `context`: Context with the sandbox registry
///  - `work`: Directory to start from
///  - `follow_git_worktree`: Also look for the sandbox of the main
///    repository of a git worktree
///
/// Returns:
///   The running sandbox, or error if no sandbox runs.
pub async fn locate(
    context: &Context,
    work: &VerifiedPath,
    follow_git_worktree: bool,
) -> anyhow::Result<RunningSandbox> {
    let lookup_dir = sandboxes::project_dir(work)?;
    find_running(context, &lookup_dir, follow_git_worktree)
        .await?
        .ok_or_else(|| anyhow::anyhow!(messages::cli::no_running_sandbox(work)))
}

/// Open connection to the CLI service of a running sandbox.
pub struct SandboxClient {
    /// Client of the CLI service.
    pub service: cli_service::Client,
    /// RPC connection driver. The caller must run it, for example with
    /// `spawn_local`. It ends when the sandbox stops.
    pub rpc_system: capnp_rpc::RpcSystem<capnp_rpc::rpc_twoparty_capnp::Side>,
}

impl SandboxClient {
    /// Connect to the nearest running sandbox at or above the current
    /// directory.
    /// Args:
    ///  - `context`: Context with the sandbox registry
    ///  - `follow_git_worktree`: Also look for the sandbox of the main
    ///    repository of a git worktree
    ///
    /// Returns:
    ///   Connected client, or error if no sandbox runs or the connection fails.
    pub async fn connect(context: &Context, follow_git_worktree: bool) -> anyhow::Result<Self> {
        let running = locate(context, &context.work, follow_git_worktree).await?;
        Self::connect_sock(&running.sock).await
    }

    /// Connect to the CLI socket at `sock_path`.
    /// Returns:
    ///   Connected client, or error if the connection fails.
    pub async fn connect_sock(sock_path: &VerifiedPath) -> anyhow::Result<Self> {
        let stream = sock_path.connect_unix_async().await.map_err(|e| {
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                anyhow::anyhow!(messages::cli::stale_cli_sock(sock_path))
            } else {
                anyhow::anyhow!(messages::cli::sandbox_connect_failed(sock_path, e))
            }
        })?;

        let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
        let network = capnp_rpc::twoparty::VatNetwork::new(
            reader,
            writer,
            capnp_rpc::rpc_twoparty_capnp::Side::Client,
            capnp::message::ReaderOptions::default(),
        );
        let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
        let service: cli_service::Client =
            rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
        Ok(Self {
            service,
            rpc_system,
        })
    }
}

/// Find the nearest running sandbox at or above `start`. With
/// `follow_git_worktree`, the search goes on from the root of a git
/// worktree to the root of its main repository.
///
/// Returns:
///   The first sandbox whose CLI socket exists, or `None`. Error for a
///   sandbox directory or a socket that is a symlink.
// A sandbox without a socket does not run. The search then goes on to the
// parent directories. The socket path is `cli.sock` in the sandbox
// directory. If that path is longer than the `AF_UNIX` limit, the path is a
// hash-keyed fallback under `<data>/sock/`.
//
// A symlink at the sandbox directory or at `cli.sock` could send the
// command, the env values and the terminal to a different sandbox. Thus
// never follow a symlink.
pub(super) async fn find_running(
    context: &Context,
    start: &VerifiedPath,
    follow_git_worktree: bool,
) -> anyhow::Result<Option<RunningSandbox>> {
    for found in sandboxes::candidates(context, start, true, follow_git_worktree).await? {
        let Ok(meta) = found.dir.symlink_metadata() else {
            continue;
        };
        // A symlink here gives the path check error.
        found.dir.to_path()?;
        if !meta.is_dir() {
            continue;
        }
        let Ok(candidate) = crate::cache::cli_sock_path(&context.data, &found.dir) else {
            continue;
        };
        if candidate.symlink_metadata().is_ok() {
            candidate.to_path()?;
            // A sandbox out of the start directory and its parents comes
            // from a worktree link. Tell the user which sandbox it is.
            if !start.starts_with(&found.project) {
                crate::cli::log!(
                    "{} {}",
                    crate::cli::bullet(),
                    messages::cli::attached_to_sandbox(&found.project)
                );
            }
            // `airlock start` records the guest cwd. A sandbox of an older
            // airlock has no record and uses the host path.
            let guest_root = project::read_run_meta(&found.dir)
                .guest_cwd
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| found.project.to_string());
            return Ok(Some(RunningSandbox {
                sock: candidate,
                found,
                guest_root,
            }));
        }
    }
    Ok(None)
}
