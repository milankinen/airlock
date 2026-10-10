//! Tests for the sandbox lookup of `airlock exec`: the nearest running
//! sandbox wins, with sandboxes of both kinds in one directory tree.

use std::path::Path;

use crate::cli::cmd_exec::find_cli_sock;
use crate::context::Context;
use crate::test_cfg::sandboxes::{data_dir_sandbox, legacy_sandbox};
use crate::test_cfg::{block_on_local, temp_dir, test_context};
use crate::vault::{Vault, VaultStorageType};

/// Run the lookup of `airlock exec` from `start`.
fn lookup(context: &Context, start: &Path) -> Option<std::path::PathBuf> {
    block_on_local(find_cli_sock(context, start)).unwrap()
}

/// Test that exec finds the nearest running sandbox from a directory and
/// its parents, for each order of a sandbox in the data directory and a
/// sandbox in the project directory.
///   1. Make a parent project and a child project, one with a sandbox in
///      the data directory and one with a sandbox in the project
///   2. Check that no socket gives no sandbox
///   3. Add the socket of the parent and check that the child finds it
///   4. Add the socket of the child and check that the child finds its own
///      socket and the parent still finds the parent socket
#[test]
fn exec_finds_nearest_running_sandbox_of_either_kind() {
    for data_dir_parent in [true, false] {
        // The socket path can be in the data directory, if the default path
        // is too long (macOS temporary directories).
        let home = temp_dir();
        let context = test_context(
            home.path(),
            Vault::for_storage_type(VaultStorageType::Disabled),
        );
        let tree = temp_dir();
        let parent = std::fs::canonicalize(tree.path()).unwrap();
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let (parent_dir, child_dir) = if data_dir_parent {
            (
                data_dir_sandbox(&context, &parent).1,
                legacy_sandbox(&child),
            )
        } else {
            (
                legacy_sandbox(&parent),
                data_dir_sandbox(&context, &child).1,
            )
        };
        let parent_sock = crate::cache::cli_sock_path(&context.data_dir, &parent_dir).unwrap();
        let child_sock = crate::cache::cli_sock_path(&context.data_dir, &child_dir).unwrap();

        assert_eq!(lookup(&context, &child), None);

        std::fs::write(&parent_sock, "").unwrap();
        assert_eq!(lookup(&context, &child), Some(parent_sock.clone()));

        std::fs::write(&child_sock, "").unwrap();
        assert_eq!(lookup(&context, &child), Some(child_sock));
        assert_eq!(lookup(&context, &parent), Some(parent_sock));
    }
}
