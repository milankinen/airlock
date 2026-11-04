//! Tests for the sandbox lookup: the nearest registered sandbox in one
//! directory tree wins. The lookup can also go from a git worktree to its
//! main repository, but not through a forged worktree link. A connect
//! refuses a symlink that code in a sandbox made. A host dir in the
//! project maps to the dir in the guest.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::context::Context;
use crate::sandbox::{self, FindOpts};
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::sandboxes::data_dir_sandbox;
use crate::test_cfg::{TempDir, block_on_local, temp_dir, test_context};
use crate::util::VerifiedPath;
use crate::vault::{Vault, VaultStorageType};

/// Find the sandbox of `start` and its parents, with or without the git
/// worktree link.
/// Returns:
///   The sandbox directory, or `None` if there is no sandbox.
fn lookup_with(context: &Context, start: &Path, follow: bool) -> Option<PathBuf> {
    let opts = FindOpts {
        parents: true,
        follow_git_worktree: follow,
    };
    block_on_local(sandbox::find(context, &verified_dir(start), opts))
        .unwrap()
        .map(|found| found.dir().to_path().unwrap().to_path_buf())
}

/// Find the sandbox of `start` and its parents (see [`lookup_with`]).
fn lookup(context: &Context, start: &Path) -> Option<PathBuf> {
    lookup_with(context, start, false)
}

/// Get the socket path of the sandbox directory `dir`.
fn sock_of(context: &Context, dir: &Path) -> PathBuf {
    let dir = VerifiedPath::new_follow(dir, None).unwrap();
    let sock = crate::cache::cli_sock_path(&context.data, &dir).unwrap();
    sock.to_path().unwrap().to_path_buf()
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

/// Test that the lookup finds the nearest registered sandbox of a
/// directory and its parents, also a sandbox that never started or that
/// boots for its first start. The commands then tell the state of that
/// sandbox, and never act on the sandbox of a parent project.
///   1. Make a parent project and a child project, each with a sandbox.
///      The parent sandbox runs. A process holds the startup lock of the
///      child sandbox, as in a first start
///   2. Check that a dir in the child finds the child sandbox, and the
///      parent finds the parent sandbox
///   3. Check that a lookup with no parents from the dir in the child
///      finds no sandbox
#[test]
fn nearest_registered_sandbox_wins() {
    let (_home, context) = context();
    let tree = temp_dir();
    let parent = std::fs::canonicalize(tree.path()).unwrap();
    let child = parent.join("child");
    let sub = child.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let parent_dir = data_dir_sandbox(&context, &parent).1;
    listen(&sock_of(&context, &parent_dir));
    let child_dir = data_dir_sandbox(&context, &child).1;
    let _startup = crate::sandbox::lock::try_lock(
        &verified_dir(&child_dir),
        crate::sandbox::lock::LockKind::Startup,
    )
    .unwrap()
    .unwrap();

    assert_eq!(lookup(&context, &sub), Some(child_dir));
    assert_eq!(lookup(&context, &parent), Some(parent_dir));
    let found = block_on_local(sandbox::find(
        &context,
        &verified_dir(&sub),
        FindOpts::default(),
    ));
    assert_eq!(found.unwrap(), None);
}

/// Test that the lookup goes from a git worktree to the sandbox of the main
/// repository only with the worktree option. The worktree is not
/// below the main repository, thus the parent search cannot find it.
///   1. Make a git repository with a sandbox and a worktree of it
///      in a different directory
///   2. Check that a lookup from a subdirectory of the worktree finds no
///      sandbox without the option
///   3. Check that the same lookup finds the sandbox of the main repository
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

    assert_eq!(lookup(&context, &sub), None);
    assert_eq!(lookup_with(&context, &sub, true), Some(main_dir.clone()));

    // Git 2.48+ with `worktree.useRelativePaths` writes relative links.
    std::fs::write(root.join("wt/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
    std::fs::write(
        main.join(".git/worktrees/wt/gitdir"),
        "../../../../wt/.git\n",
    )
    .unwrap();
    assert_eq!(lookup_with(&context, &sub, true), Some(main_dir));
}

/// Test that a forged worktree link does not send a command to the sandbox
/// of a different repository. Code in sandbox A can write the `.git` file in its
/// project, but not the link back in the other repository.
///   1. Make project A with a sandbox, and repository B with a sandbox and
///      a real worktree
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
    let a_dir = data_dir_sandbox(&context, &a).1;
    data_dir_sandbox(&context, &b);

    let sub = a.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let forged = format!("gitdir: {}\n", b.join(".git/worktrees/wt").display());
    std::fs::write(sub.join(".git"), forged).unwrap();
    assert_eq!(lookup_with(&context, &sub, true), Some(a_dir.clone()));

    // The link back of the worktree names the `.git` file of the worktree.
    // A symlink to that file must not pass as the same file.
    std::fs::remove_file(sub.join(".git")).unwrap();
    std::os::unix::fs::symlink(root.join("wt/.git"), sub.join(".git")).unwrap();
    assert_eq!(lookup_with(&context, &sub, true), Some(a_dir));
}

/// Test that a connect refuses a symlinked sandbox directory and a
/// symlinked socket. A link must not send a command to a different
/// sandbox.
///   1. Make project B with a running sandbox
///   2. Make the sandbox directory of project A a symlink to the sandbox
///      of B and check that the connect fails and names the symlink
///   3. Make the sandbox directory of A a real directory with a `cli.sock`
///      symlink to the socket of B and check that the connect fails
#[test]
fn symlinked_sandbox_or_socket_is_refused() {
    let (_home, context) = context();
    let tree = temp_dir();
    let root = std::fs::canonicalize(tree.path()).unwrap();
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let b_dir = data_dir_sandbox(&context, &b).1;
    let b_sock = sock_of(&context, &b_dir);
    listen(&b_sock);
    let connect = || {
        block_on_local(async {
            let found = sandbox::find(&context, &verified_dir(&a), FindOpts::default())
                .await
                .unwrap()
                .unwrap();
            found.connect(&context).await.err().unwrap().to_string()
        })
    };

    let a_dir = data_dir_sandbox(&context, &a).1;
    std::fs::remove_dir_all(&a_dir).unwrap();
    std::os::unix::fs::symlink(&b_dir, &a_dir).unwrap();
    let error = connect();
    assert!(error.contains("is a symlink"), "{error}");

    std::fs::remove_file(&a_dir).unwrap();
    std::fs::create_dir(&a_dir).unwrap();
    let a_sock = sock_of(&context, &a_dir);
    std::os::unix::fs::symlink(&b_sock, &a_sock).unwrap();
    let error = connect();
    assert!(error.contains("is a symlink"), "{error}");
}

/// Test that a host dir maps to the guest dir below the project dir of
/// the guest. The project dir of the guest can differ from the host path
/// (`--sandbox-cwd`), and the host path can go through a symlink.
///   1. Make a sandbox with the guest project dir `/work`
///   2. Check the guest dir of the project dir, of a sub dir, and of the
///      sub dir through a symlink to the project
///   3. Remove the guest project dir record and check that the host path
///      is the guest dir
#[test]
fn host_dir_maps_to_guest_project_dir() {
    let (_home, context) = context();
    let tree = temp_dir();
    let root = std::fs::canonicalize(tree.path()).unwrap();
    let project = root.join("project");
    std::fs::create_dir_all(project.join("sub")).unwrap();
    std::os::unix::fs::symlink(&project, root.join("link")).unwrap();
    let dir = data_dir_sandbox(&context, &project).1;
    std::fs::write(dir.join("run.json"), r#"{"guest_cwd":"/work"}"#).unwrap();

    let guest_dir = |host: &Path| {
        let host = verified_dir(host);
        let opts = FindOpts {
            parents: true,
            follow_git_worktree: false,
        };
        let found = block_on_local(sandbox::find(&context, &host, opts));
        found.unwrap().unwrap().guest_dir(&host).unwrap()
    };
    assert_eq!(guest_dir(&project), "/work");
    assert_eq!(guest_dir(&project.join("sub")), "/work/sub");
    assert_eq!(guest_dir(&root.join("link/sub")), "/work/sub");

    std::fs::remove_file(dir.join("run.json")).unwrap();
    let sub = project.join("sub");
    assert_eq!(guest_dir(&sub), sub.display().to_string());
}
