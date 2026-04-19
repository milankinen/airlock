# Heal the `sandbox/image` hardlink invariant on every prepare

Sweep GC considers an image live via `nlink > 1` on its cache file at
`images/<digest>`. That's only sound if every sandbox referencing the
image keeps `<sandbox>/image` as a hardlink to the canonical entry.
Two code paths left that invariant in a broken state:

1. The fast path in `oci::prepare` read `sandbox/image`, matched on
   name, and returned without any inode check. If the link was
   severed (by the earlier `~/.cache/airlock/` → `~/.cache/airlock/oci/`
   cache migration, a manual cache wipe, or any code path that ever
   rewrote `sandbox/image` as a fresh file), the sandbox kept working
   but its `images/<digest>` stayed at `nlink == 1`.
2. The post-pull hardlink creation in the slow path was gated on
   `digest_changed`. A run where the digest hadn't changed never
   re-established the link, so a once-broken link stayed broken.

With the link broken, any sibling sandbox's `Recreate` or `airlock rm`
would run `sweep()` and happily delete a live image — observed as the
user's "dev project" image disappearing when GC ran in another project.

## Change

New helper `ensure_image_hardlink(sandbox_image, image_path, &image)`:

- Compare `dev`/`ino` on both paths. No-op when the inodes match.
- If the canonical `images/<digest>` is missing (cache wipe or schema
  migration), write the sandbox's own copy back to it first.
- Remove `sandbox/image` if present and re-link to `images/<digest>`.
- Fail hard on link error: both paths live under `$HOME`, so the only
  plausible cause is a cross-filesystem config problem the user wants
  surfaced immediately.

Called unconditionally on both the fast path and after `ensure_image`
completes — the invariant is re-asserted on every `prepare()`, so a
single successful run is enough to heal a previously broken link.

`digest_changed` survives only to decide whether to prompt the user
about the tag moving; it no longer influences hardlink creation.

## Why heal instead of refuse

A strict alternative is to bail when the link is broken and tell the
user to wipe `.airlock/sandbox`. In practice the cache migration and
old code paths mean broken links exist in the wild today, and the
data needed to rebuild the link is entirely local: we have the
`OciImage` in hand and the canonical file is deterministic given the
digest. Healing on demand is zero-cost on the happy path (one
`metadata()` pair) and makes the invariant self-repairing.
