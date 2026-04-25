# Directory masks

Add a `[mask.<name>]` config section that hides chosen subdirectories of
the project from the sandbox by bind-mounting an empty directory over
each declared path. Built-in `.airlock/` masking moves into the same
mechanism, so user masks and the always-on sandbox-internals mask share
the same code path.

Motivating case: cordoning off parts of a monorepo from an AI agent
(`secrets/`, an unrelated app, a vendor tree). The agent sees an empty
directory; the host file is untouched.

### Scope

- Per-mask: `enabled`, `paths` (list of project-relative dirs).
- Path validation on the host: rejects empty entries, leading `/`,
  leading `~`, and any `..` segment. Rejection is a hard config error
  before the VM starts.
- Per-mask source dirs live under `/mnt/disk/mask/project/<name>` so
  each block is isolated. The whole tree is wiped and recreated on every
  VM start — host config is the source of truth, no per-VM mask state.
- `.airlock/` masking is applied unconditionally via the same code path
  (with name `.airlock` reserved).

### Design choices

- **Bind-mount inside the VM, not host-side filter.** The project is
  shared into the VM via virtiofs as a single tree; pre-filtering at
  the host would mean either constructing a shadow tree (expensive
  with large trees and breaks identity-based tools like git) or
  carving up the virtiofs share. Bind-mounting inside the VM keeps
  the host mount intact and applies the hide at the cheapest layer
  possible.
- **Invisibility, not security boundary.** A cooperative agent won't
  see masked content; a process that actively wants to defeat the
  mask and has enough privilege to call `umount` can still reach the
  underlying virtiofs share. The manual page is explicit about this —
  hard separation requires putting paths in a different project.
- **Empty source dir per mask, not per path.** All paths inside one
  mask share the same source — the contents are always empty so there
  is nothing to differ on.
- **One unified loop in `overlay.rs`.** Built-in `.airlock` and user
  masks go through the same `mkdir + bind` sequence after the overlay
  rootfs is mounted. Keeping them in one place avoids two parallel
  copies of the mask-application logic.
- **Host validation, not guest.** Path checks (`/`, `~`, `..`) run in
  the CLI before the start RPC fires. The supervisor trusts what the
  host hands it, matching how dirs/caches/daemons already work.

### git_worktree_skip detour (removed)

The first iteration also offered `git_worktree_skip = true` per mask:
read `<project>/.git/index` via `gix-index`, set `SKIP_WORKTREE` on
entries under masked paths, write the rewritten index to
`/mnt/disk/mask/git-index`, bind-mount it over `.git/index`. Goal: stop
`git status` from reporting the masked files as deleted.

Two gix-index 0.50 quirks made it fragile:

1. `Entry::write_to` only emits the extended-flags word when
   `Flags::EXTENDED` is set on the entry, and `to_storage()` truncates
   to `u16` — so flipping `SKIP_WORKTREE` (bit 30) without also
   setting `EXTENDED` was silently dropped on write, with the index
   downgraded to v2.
2. `detect_required_version` likewise only bumps to v3 when an entry
   has `EXTENDED`. Same root cause.

Both fixable by inserting `SKIP_WORKTREE | EXTENDED` together. But the
real blocker was the bind-mount itself: git writes the index by
creating `.git/index.lock` and `rename(2)`-ing it over `.git/index`,
which fails with `EBUSY` over a mountpoint. So `git add` broke as soon
as the user tried to use it, and the only ways to make it work properly
(overlay-mount the entire `.git/`, or mutate the host's index in place)
both moved the complexity well past what the feature was worth.

Removed the option entirely. The manual page calls out the `git status`
side effect as known and points users at running git from outside the
sandbox if it bothers them.

### Module layout

- `airlock-cli/src/masking.rs` — `build_specs` (config → wire format
  with validation) and `print_verbose` (the `--verbose` mask block).
  Mirrors the `daemon.rs` split.
- `airlock-common/schema/supervisor.capnp` — `MaskSpec` struct and
  `masks :List(MaskSpec)` field on `Supervisor.start`.
- `airlockd/src/init.rs` — `MaskConfig` carried inside `MountConfig`.
- `airlockd/src/init/linux/overlay.rs` — applies masks after the
  overlay rootfs and dir/cache binds are in place.

### Out of scope

- File-level masks (paths to a single file).
- Pattern-based masks (globs).
- Make hard: virtiofs-source filtering or a separate project share.
- Hiding from `git status` (see git_worktree_skip detour above).
