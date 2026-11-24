# A changed image tag uses the new image

## Symptom

After a change of `vm.image` from `debian:bookworm-slim` to
`debian:trixie-slim`, the next start kept the bookworm image and the old
disk. The output showed the trixie tag with the bookworm digest. This
occurs with `if-not-present` on v2026.11.5. The code is in v2026.11.3 and
later. v2026.11.3 and v2026.11.4 were not tested.

## Cause

With `if-not-present`, a sandbox with no ready cache entry pins the stored
digest (see `2026-11-06-layer-owner-xattr.md`). The cache lookup also needs
the configured name, so a changed tag got the same pin:
`debian:trixie-slim@<bookworm digest>`. The registry resolves such a
reference by the digest, and the old digest exists in the same repository.

## Fix

The stored image file has the configured name of the last start, also in
the `v2` schema. The pin now applies only if that name is the configured
name. Otherwise airlock resolves the tag, and a different digest gives the
image change question.

An import marker has only a digest. It keeps the pin. Without the pin, a
moved tag is an image change, and `--yes` removes the imported disk.

## Known limits

* A sandbox that already kept its old image has the new name in its image
  file, so it stays on the old image. Remove the sandbox to get the new
  image.
* Sandboxes with different tags for one digest share a cache entry, which
  has the name of only one of them. A start links the image file of the
  sandbox to that entry, so the next start can find an other name and
  resolve the tag. If the tag moved, this gives the image change question,
  as before v2026.11.3.

## Tests

* `test_image_cache::stored_digest_of_another_tag_is_not_pinned`
* `tests/vm/image.bats` changes a tag and checks the guest version and the
  disk. It fails without the fix.
* Manual check with a local registry, for an unchanged name and a moved
  tag: a missing layer, a `v2` image file and an import marker each keep
  the old image and the disk.
