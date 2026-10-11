# `airlock exec --follow-git-worktree`

`airlock exec` finds the running sandbox in the current directory or the
nearest parent directory. A git worktree outside of its main repository
(for example `~/worktrees/my-feature` for `~/my-project`) has no parent
with a sandbox, so `exec` failed there. The new flag lets the search go
from a worktree to the sandbox of its main repository.

## Change

- `sandboxes::candidates` and `resolve_sandbox` have a new
  `follow_git_worktree` argument. Only `exec` sets it. All other callers
  (`rm`, `info`, `start`, the local config lookup) pass `false`, so
  destructive commands cannot go to the sandbox of a different project.
- `search_dirs` makes the search order. It starts at `start`. At each
  directory, if the flag is on and the directory is the root of a linked
  worktree, the next directory is the main repository root. Otherwise it
  is the parent (if `parents`). Thus the directories of the worktree come
  first, and a sandbox of the worktree itself still wins. A visited set
  stops cycles.
- `git_worktree_main` reads the git files and does not run `git`. It
  reads them with `PinnedDir::read`: a symlink, a FIFO or a file that is
  not regular is not followed. A symlink to the `.git` file of a real
  worktree would otherwise pass the link back check.
  - `<dir>/.git` is a file with `gitdir: <common>/worktrees/<name>`. The
    canonical `<gitdir>` must be in `<common>/worktrees/`, and `<common>`
    must be named `.git`. Its parent is the main repository root. A bare
    main repository has no root and is not followed.
  - `<gitdir>/commondir` (usually `../..`) must name the same `<common>`.
  - `<gitdir>/gitdir` must point back to `<dir>/.git`, as git checks it.
    A relative path (git 2.48+, `worktree.useRelativePaths`) is relative
    to `<gitdir>`.
  - A submodule also has a `.git` file, but its git directory is not in
    `worktrees/`, so it is not followed.
  - If a check fails, the search goes on to the parent directory.
- `exec` prints `attached to the sandbox of <project>` when the sandbox
  is not in the start directory or its parents, that is after a worktree
  jump.
- `find_cli_sock` refuses a symlinked sandbox directory and a symlinked
  `cli.sock` with an error. It uses `symlink_metadata` and never follows
  a symlink.
- Manual: `usage/attaching-to-running-sandbox.md` has a "Git worktrees"
  section. It tells the user to mount the worktrees directory, because
  airlock mounts only the project directory. If the worktree is not in
  the sandbox, the command runs in `/` (the supervisor falls back when
  `chdir` fails).

## Security

The project directory is shared read-write, so the guest can plant
`.git` and `commondir` files. Without checks, they could point the
search to the running sandbox of a different host repository, and the
command, the `-e` values and the terminal input would go there. The link
back in `<gitdir>/gitdir` is in the main repository, out of reach of the
guest. Thus a forged `.git` file does not get past the check.

A symlinked `.airlock/sandbox` or `cli.sock` in the project could do the
same redirect, also without the flag. `exec` now refuses both.

Limit: if the main repository is also in the project mount (for example
a monorepo), the guest can also write the link back. Then the guest
already controls the files of that project. `info` still follows a
symlinked sandbox directory, but it only shows data.

## Tests

- `test_exec_lookup.rs`: `exec_from_git_worktree_finds_main_repository_sandbox_with_option`
  makes a real repository and a worktree beside it with `git worktree
  add`. A lookup from a worktree subdirectory finds the main repository
  socket only with the flag, also with relative links.
- `forged_worktree_link_does_not_reach_other_repository`: a `.git` file
  in project A that points to the git directory of a real worktree of
  repository B finds the sandbox of A, not B. The same for a `.git`
  symlink to the `.git` file of that worktree.
- `symlinked_sandbox_or_socket_is_refused`: a symlinked
  `.airlock/sandbox` and a symlinked `cli.sock` give an error.
