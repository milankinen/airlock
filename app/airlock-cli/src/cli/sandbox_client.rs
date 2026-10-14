//! Client for a running sandbox.
//!
//! Finds the sandbox that serves a directory and opens an RPC connection
//! to its `airlock start` process. Commands that act on a running sandbox
//! share this code. Never follows symlinks while it looks for the sandbox,
//! because code in a sandbox can write its project mount.

use airlock_common::cli_capnp::cli_service;
use futures::AsyncReadExt;

use crate::context::Context;
use crate::util::verified_path::VerifiedPath;
use crate::{messages, sandboxes};

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
        let lookup_dir = sandboxes::project_dir(&context.work)?;
        let sock_path = find_cli_sock(context, &lookup_dir, follow_git_worktree)
            .await?
            .ok_or_else(|| anyhow::anyhow!(messages::cli::no_running_sandbox(&context.work)))?;
        Self::connect_sock(&sock_path).await
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

/// Find the CLI socket of the nearest running sandbox at or above `start`.
/// Sandboxes in the data directory and in the project both count. With
/// `follow_git_worktree`, the search goes on from the root of a git
/// worktree to the root of its main repository.
///
/// Returns:
///   The first socket path that exists, or `None`. Error for a sandbox
///   directory or a socket that is a symlink.
// A sandbox without a socket does not run. The search then goes on to the
// parent directories. The socket path is `cli.sock` in the sandbox
// directory. If that path is longer than the `AF_UNIX` limit, the path is a
// hash-keyed fallback under `<data>/sock/`.
//
// Code in a sandbox can write its project mount. A symlink there at
// `.airlock/sandbox` or `cli.sock` could send the command, the env values
// and the terminal to a different sandbox. Thus never follow a symlink.
pub(super) async fn find_cli_sock(
    context: &Context,
    start: &VerifiedPath,
    follow_git_worktree: bool,
) -> anyhow::Result<Option<VerifiedPath>> {
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
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}
