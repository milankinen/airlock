# Sweep-based OCI cache GC; fatal hardlink failure

The previous per-image GC (`gc_unused_image`) ran only on the
`ImageChangeAction::Recreate` path and only consulted the single
image about to be superseded. After the recent
`~/.cache/airlock/` → `~/.cache/airlock/oci/` cache migration, old
sandboxes' hardlinks at `.airlock/sandbox/image` dangle at the old
path. A sibling sandbox that pulled the same image at the new path
created `meta.json` with `nlink == 1`; the next `Recreate` then
deleted an image that other sandboxes still depend on via their
dangling old-path refs.

## Change

`oci::gc::sweep()` replaces `gc_unused_image`. It walks the full
`images/` tree, removes any image whose `meta.json` has `nlink <= 1`
(no sandbox hardlink), collects the union of `layers` from every
surviving `meta.json`, and then prunes `layers/` against that set.
Stray staging entries (`.download`, `.download.tmp`, `.tmp`) are
always removed — they're only valid mid-pull.

Sweep triggers:

- `ImageChangeAction::Recreate` in `oci::prepare`, after removing the
  sandbox's own hardlink (so the replaced image is a candidate).
- `airlock rm`, after the sandbox's cache dir is deleted (its
  hardlink went with it).

Sweep is deliberately **not** wired into every `prepare()`: doing so
would race against a sibling sandbox that's in the middle of starting
up but hasn't yet created its hardlink. Binding the sweep to
user-initiated removals preserves the invariant that "about to start"
sandboxes still have their refs in place by the time anyone sweeps.

## Hardlink failure is now fatal

`prepare()` previously did:

```rust
if let Err(e) = std::fs::hard_link(&meta_path, &sandbox_image) {
    tracing::debug!("image ref hard-link failed (cross-device?): {e}");
}
```

That path is the entire GC-safety guarantee. Silently continuing
leaves the sandbox with `nlink == 1` on the image; the next sweep
deletes the image while the sandbox is still running. Both paths
live under `$HOME`, so a cross-device failure should not happen; if
it does, the user needs a loud error rather than a silent degradation.
The call now returns `anyhow::Result`, with an error message pointing
at the two paths and the filesystem requirement.

## Tests

`oci::gc::tests` covers:

- An image with `nlink > 1` (sandbox ref present) survives the sweep;
  its layers are retained.
- An image with `nlink == 1` is collected; layers exclusive to it are
  collected too.
- A layer referenced by both a live and an orphan image is kept
  (because the live image still lists it).
- Stray `.download`, `.download.tmp`, `.tmp` entries under `layers/`
  are always pruned.

The `HOME_LOCK` mutex used to serialize tests that mutate the
process-wide `HOME` env var moved from per-module to a single
`cache::HOME_LOCK`, so `oci::gc::tests` and `oci::layer::tests` can't
step on each other when run concurrently under `cargo test`.

## Out of scope

The unified docker/registry pull pipeline, dropping `images/<d>/rootfs/`,
and the staged `.download` / `.tmp` flow land in follow-up steps.
