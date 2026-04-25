# Masks

Masks hide subdirectories of the project from the sandbox. Each mask
bind-mounts an empty directory over the listed paths, so processes inside
the VM see those directories as present but empty. The host files are not
touched — masking is applied per-VM-start, on top of the project mount.

The typical use case is cordoning off parts of a monorepo from an AI
agent: `secrets/`, an unrelated app, or a vendor tree the agent has no
reason to read.

## Defining a mask

Each mask is a named entry under `[mask.<name>]`:

```toml
[mask.secrets]
paths = ["secrets"]
```

Inside the sandbox, `secrets/` now appears as an empty directory; the
real contents on the host stay untouched and visible from outside.

## Multiple paths per mask

A single mask block can hide several paths. They share the same empty
source directory, which is fine since the contents are always empty:

```toml
[mask.private]
paths = ["apps/admin", "internal/notes", "vendor/closed-source"]
```

## Path rules

Paths are project-relative and validated by the host before the sandbox
starts. The following are rejected:

- absolute paths (starting with `/`)
- home-relative paths (starting with `~`)
- any segment equal to `..`

If a listed path doesn't exist in the project, the guest creates it (as
an empty directory) before applying the mask, so order of `mkdir` and
`mask` doesn't matter.

## Disabling a mask

A mask can be disabled without removing the entry — useful when a preset
defines one you don't need:

```toml
[mask.secrets]
enabled = false
paths = ["secrets"]
```

## Notes

- Masks are recreated on every VM start, so the host config is the source
  of truth — there is no per-VM state to clean up.
- Masking is **invisibility, not a security boundary**. The hide is
  applied as a bind-mount *inside* the VM, on top of the project mount
  — the masked files are still shared into the VM via virtiofs, just
  shadowed by an empty directory at their path. A cooperative agent
  won't see them, which is the point. A process that *actively* wants
  to defeat the mask (and has enough privilege to call `umount` or
  walk the underlying mount) can still reach the contents. If you need
  a hard boundary, keep those paths in a separate project entirely.
- The sandbox's own `.airlock/` directory is always masked
  unconditionally, so processes in the VM can't reach back into the
  CA keys, disk image, or lock file.
- `git status` will report masked files as deleted (the worktree copy
  is gone from the sandbox's view, but the index still references them).
  This is expected; if it bothers you, run git from outside the sandbox
  for those paths.
