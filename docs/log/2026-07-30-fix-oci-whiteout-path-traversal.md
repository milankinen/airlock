# Fix path traversal in OCI layer whiteout handling

## Symptom

Pulling a crafted (malicious) container image could create or delete files
**outside** the layer cache, anywhere the invoking user can write — during a
plain `airlock` image pull, before any code runs in the sandbox. For a tool
whose premise is safe handling of untrusted images this is a host-side
breakout.

## Root cause

Layer extraction unpacks normal tar entries with `entry.unpack_in(&tmp)`,
which contains them inside the extraction root (sanitizing traversal and
symlink targets). But whiteout entries (`.wh.<name>` and `.wh..wh..opq`) are
handled with our *own* filesystem calls — `create_dir_all`, `remove_file`,
`remove_dir_all`, `File::create`, `xattr::set` — against
`tmp.join(parent_rel)`, with no containment. Two vectors:

1. **Symlink traversal.** A layer can order a symlink entry first, e.g.
   `etc -> ../../../../home/user`; `unpack_in` creates it verbatim (symlink
   targets are not validated at creation). A following `etc/.wh.passwd` then
   makes `create_dir_all(tmp/etc)` follow the symlink out of the root and
   `remove_file`/`File::create` operate on host paths.
2. **Absolute path.** The pre-existing guard only rejected `..` components,
   not a leading `/`. An entry `/etc/.wh.passwd` passed the guard, and
   `tmp.join("/etc")` evaluates to `/etc` (Rust's `Path::join` discards the
   base when the argument is absolute), so the whiteout targeted `/etc`
   directly.

## Fix

Replaced `tmp.join(parent_rel)` in both whiteout branches with a `safe_join`
helper that rebuilds the path one component at a time from the extraction
root:

- accepts only `Normal` components (rejecting absolute prefixes and any
  leftover `..`/other components), and
- refuses if any existing component along the path is a symlink.

The extraction loop is single-threaded and only `safe_join` and `unpack_in`
write under the root, so a symlink component can only have come from an
earlier entry in the same layer — exactly the attack. On any violation the
whole layer is refused (error), which is the fail-closed choice for a
security product; a legitimate image does not whiteout through a symlinked or
absolute parent.

Chosen over an `openat(O_NOFOLLOW)`-based walk (as `vm/file_sync.rs` uses)
because the whiteout code is otherwise straightforward `std::fs`, and a
component-wise symlink check gives the same guarantee here without threading
raw fds through the extraction loop.

## Tests

Added to `oci/layer.rs`:

- `safe_join_accepts_normal_relative_path`, `safe_join_rejects_absolute_path`,
  `safe_join_rejects_symlink_component` — direct, deterministic coverage of
  the three cases.
- `ensure_layer_cached_wont_delete_outside_root_via_whiteout` — end-to-end:
  builds a layer that plants `esc -> <outside>` then `esc/.wh.victim`, and
  asserts the sentinel host file outside the root survives.
