# Masks

Masks hide subdirectories of the project from the sandbox. Processes
inside the VM see the listed paths as present but empty directories.
Masking does not touch the host files — airlock applies it per VM
start, on top of the project mount.

The typical use case is to hide parts of a monorepo from an AI
agent: `secrets/`, an unrelated app, or a vendor tree the agent has no
reason to read.

## Defining a mask

Each mask is a named entry under `[mask.<name>]`:

```toml
[mask.secrets]
paths = ["secrets"]
```

Inside the sandbox, `secrets/` now appears as an empty directory. The
real contents on the host stay untouched and visible from outside.

## Multiple paths per mask

A single mask block can hide several paths. They share the same empty
source directory, which is fine since the contents are always empty:

```toml
[mask.private]
paths = ["apps/admin", "internal/notes", "vendor/closed-source"]
```

## Path rules

Paths are project-relative. The host validates them before the sandbox
starts and rejects the following:

- absolute paths (starting with `/`)
- home-relative paths (starting with `~`)
- any segment equal to `..`

If a listed path doesn't exist in the project, the guest creates it (as
an empty directory) before applying the mask, so order of `mkdir` and
`mask` doesn't matter.

## Disabling a mask

You can disable a mask without removing the entry — useful when a pack
defines one you don't need:

```toml
[mask.secrets]
enabled = false
paths = ["secrets"]
```

## Notes

- airlock recreates masks on every VM start, so the host config is the
  source of truth — there is no per-VM state to clean up.
- Masking is **invisibility, not a security boundary**. The hide is a
  bind-mount *inside* the VM, on top of the project mount. The masked
  files are still shared into the VM — an empty directory only
  shadows them at their path. A cooperative agent
  won't see them, which is the point. A process that *actively* wants
  to defeat the mask (and has enough privilege to call `umount` or
  walk the underlying mount) can still reach the contents. If you need
  a hard boundary, keep those paths in a separate project entirely.
- airlock always masks the sandbox's own `.airlock/` directory
  unconditionally, so processes in the VM can't reach the CA keys,
  disk image, or lock file.
- `git status` will report masked files as deleted (the worktree copy
  is gone from the sandbox's view, but the index still references them).
  This is expected. If it bothers you, run git from outside the sandbox
  for those paths.
