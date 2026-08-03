# Fix "keep old environment" writing the new image under the old digest

## Symptom

Answering "Continue using old environment" at the image-changed prompt could
silently start the **new** image instead — and, worse, persist it to the cache
under the **old** image's digest. Every sandbox on the host that referenced
that digest then got the new image too, and the `image_id` the supervisor uses
for change detection described neither image.

Only reachable when the old image's layers had been swept while its metadata
survived, so it stayed rare enough not to be noticed.

## Root cause

The `KeepOld` branch tested reusability with a bare existence check:

```rust
let old_image_path = crate::cache::image_path(old_digest.trim())?;
if old_image_path.exists() {
    image.digest = old_digest.trim().to_string();
}
```

Image metadata (`oci/images/<digest>`) and layer trees (`oci/layers/<key>`) are
collected independently, so `images/<old>` routinely outlives the layers it
points at. `exists()` cannot tell that state apart from a complete image, which
is exactly what `read_ready_image` exists to do.

On that path the branch swapped `image.digest` to the old digest while leaving
`image.source` and `image.config` describing the newly resolved image — a
combination `ResolvedImage` was never meant to hold. `ensure_image` then missed
its `read_ready_image(&image_path)` cache hit (the layers were gone, which is
how we got here), fell through to the pull path, downloaded the *new* image's
layers, and committed:

```rust
let image = build_oci_image(resolved.digest.clone(), /* old */
                            image_name.to_string(),
                            ordered_layers,          /* new */
                            &resolved.config)?;      /* new */
write_cached_image(&image_path, &image)?;            /* → images/<old> */
```

## Fix

`KeepOld` now gates on `read_ready_image` and returns directly with the old
image rather than editing a digest into an otherwise-new `ResolvedImage`:

```rust
if let Some(mut old) = read_ready_image(&old_image_path) {
    if old.name != *image_name {
        old.name.clone_from(image_name);
        write_cached_image(&old_image_path, &old)?;
    }
    return use_cached_image(project, &sandbox_image, old);
}
```

Returning early is the substantive part. Merely upgrading `exists()` to a
readiness check would still leave a window — a concurrent sweep between the
check and `ensure_image`'s own lookup reopens the identical failure. With the
early return, no digest reaches `ensure_image` unless it came from the same
resolution as the source beside it, so the mismatch is unrepresentable rather
than just unlikely. `ResolvedImage` now documents that pairing as an invariant.

When the old image is *not* reusable, the fall-through to the new image is what
the original comment already intended; it just says so out loud now
("old environment is incomplete — using the new image") instead of silently
producing a corrupt cache entry.

The name-stamping is preserved deliberately. `ensure_image` used to relabel the
kept image with the configured name on its way through, which is what stops the
name-keyed fast path from re-resolving and re-prompting on every subsequent
start. Short-circuiting past `ensure_image` would have dropped that, so the
early return does it explicitly.

## Tests

`oci::tests::cached_image_missing_layers_is_not_ready` — writes an image JSON
whose layer tree is absent, asserts it is readable via `read_cached_image` but
*not* ready via `read_ready_image` (the exact distinction the old code missed),
then restores the layer and asserts the same entry becomes ready again.

`oci::tests::cached_image_without_layers_is_not_ready` — a layerless entry is
never ready, since it can't be composed into a rootfs.

Both redirect `HOME` under `cache::HOME_LOCK`, following the `oci::gc` tests.
