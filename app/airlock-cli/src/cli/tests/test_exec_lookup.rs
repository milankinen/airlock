//! Tests for the sandbox lookup of `airlock exec`: the nearest running
//! sandbox wins, with sandboxes of both kinds in one directory tree. The
//! lookup can also go from a git worktree to its main repository, but not
//! through a forged worktree link or a symlink that code in a sandbox made.

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;

use crate::cli::cmd_exec::find_cli_sock;
use crate::context::Context;
use crate::test_cfg::sandboxes::{data_dir_sandbox, legacy_sandbox};
use crate::test_cfg::{TempDir, block_on_local, temp_dir, test_context};
use crate::vault::{Vault, VaultStorageType};

/// Run the lookup of `airlock exec` from `start`.
fn lookup(context: &Context, start: &Path) -> Option<std::path::PathBuf> {
    block_on_local(find_cli_sock(context, start, false)).unwrap()
}

/// Make the socket of a running sandbox at `path`.
fn listen(path: &Path) {
    // The socket file stays after the listener drops.
    UnixListener::bind(path).unwrap();
}

/// A test context with its home directory.
fn context() -> (TempDir, Context) {
    let home = temp_dir();
    let context = test_context(
        home.path(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    (home, context)
}

/// Run `git` with `args` in `dir` and panic if it fails.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
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

        listen(&parent_sock);
        assert_eq!(lookup(&context, &child), Some(parent_sock.clone()));

        listen(&child_sock);
        assert_eq!(lookup(&context, &child), Some(child_sock));
        assert_eq!(lookup(&context, &parent), Some(parent_sock));
    }
}

/// Test that exec goes from a git worktree to the running sandbox of the
/// main repository only with the worktree option. The worktree is not
/// below the main repository, thus the parent search cannot find it.
///   1. Make a git repository with a running sandbox and a worktree of it
///      in a different directory
///   2. Check that a lookup from a subdirectory of the worktree finds no
///      sandbox without the option
///   3. Check that the same lookup finds the socket of the main repository
///      with the option
///   4. Change the worktree links to relative paths and check the lookup
///      again
#[test]
fn exec_from_git_worktree_finds_main_repository_sandbox_with_option() {
    let (_home, context) = context();
    let tree = temp_dir();
    let root = std::fs::canonicalize(tree.path()).unwrap();
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&main, &["worktree", "add", "-q", "../wt"]);
    let sub = root.join("wt/sub");
    std::fs::create_dir_all(&sub).unwrap();

    let main_dir = data_dir_sandbox(&context, &main).1;
    let main_sock = crate::cache::cli_sock_path(&context.data_dir, &main_dir).unwrap();
    listen(&main_sock);

    assert_eq!(lookup(&context, &sub), None);
    assert_eq!(
        block_on_local(find_cli_sock(&context, &sub, true)).unwrap(),
        Some(main_sock.clone())
    );

    // Git 2.48+ with `worktree.useRelativePaths` writes relative links.
    std::fs::write(root.join("wt/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
    std::fs::write(
        main.join(".git/worktrees/wt/gitdir"),
        "../../../../wt/.git\n",
    )
    .unwrap();
    assert_eq!(
        block_on_local(find_cli_sock(&context, &sub, true)).unwrap(),
        Some(main_sock)
    );
}

/// Test that a forged worktree link does not send exec to the sandbox of a
/// different repository. Code in sandbox A can write the `.git` file in its
/// project, but not the link back in the other repository.
///   1. Make project A with a running sandbox, and repository B with a
///      running sandbox and a real worktree
///   2. In a subdirectory of A, write a `.git` file that points to the git
///      directory of the worktree of B
///   3. Check that a lookup with the worktree option from there finds the
///      sandbox of A
///   4. Make the `.git` file a symlink to the `.git` file of the worktree
///      of B and check the lookup again
#[test]
fn forged_worktree_link_does_not_reach_other_repository() {
    let (_home, context) = context();
    let tree = temp_dir();
    let root = std::fs::canonicalize(tree.path()).unwrap();
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    git(&b, &["init", "-q"]);
    git(&b, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&b, &["worktree", "add", "-q", "../wt"]);
    let a_sock = crate::cache::cli_sock_path(&context.data_dir, &legacy_sandbox(&a)).unwrap();
    let b_dir = data_dir_sandbox(&context, &b).1;
    let b_sock = crate::cache::cli_sock_path(&context.data_dir, &b_dir).unwrap();
    listen(&a_sock);
    listen(&b_sock);

    let sub = a.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let forged = format!("gitdir: {}\n", b.join(".git/worktrees/wt").display());
    std::fs::write(sub.join(".git"), forged).unwrap();
    assert_eq!(
        block_on_local(find_cli_sock(&context, &sub, true)).unwrap(),
        Some(a_sock.clone())
    );

    // The link back of the worktree names the `.git` file of the worktree.
    // A symlink to that file must not pass as the same file.
    std::fs::remove_file(sub.join(".git")).unwrap();
    std::os::unix::fs::symlink(root.join("wt/.git"), sub.join(".git")).unwrap();
    assert_eq!(
        block_on_local(find_cli_sock(&context, &sub, true)).unwrap(),
        Some(a_sock)
    );
}

/// Test that exec refuses a symlinked project sandbox directory and a
/// symlinked socket. Code in a sandbox can make them in its project to send
/// exec to a different sandbox.
///   1. Make project B with a running sandbox in the project
///   2. In project A, make `.airlock/sandbox` a symlink to the sandbox of B
///      and check that the lookup fails and names the symlink
///   3. Make `.airlock/sandbox` a real directory with a `cli.sock` symlink
///      to the socket of B and check that the lookup fails
#[test]
fn symlinked_sandbox_or_socket_is_refused() {
    let (_home, context) = context();
    let tree = temp_dir();
    let root = std::fs::canonicalize(tree.path()).unwrap();
    let a = root.join("a");
    let b = root.join("b");
    let b_dir = legacy_sandbox(&b);
    let b_sock = crate::cache::cli_sock_path(&context.data_dir, &b_dir).unwrap();
    listen(&b_sock);

    let a_dir = a.join(".airlock/sandbox");
    std::fs::create_dir_all(a_dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&b_dir, &a_dir).unwrap();
    let error = block_on_local(find_cli_sock(&context, &a, false)).unwrap_err();
    assert!(error.to_string().contains("is a symlink"), "{error}");

    std::fs::remove_file(&a_dir).unwrap();
    let a_dir = legacy_sandbox(&a);
    let a_sock = crate::cache::cli_sock_path(&context.data_dir, &a_dir).unwrap();
    std::os::unix::fs::symlink(&b_sock, &a_sock).unwrap();
    let error = block_on_local(find_cli_sock(&context, &a, false)).unwrap_err();
    assert!(error.to_string().contains("is a symlink"), "{error}");
}
