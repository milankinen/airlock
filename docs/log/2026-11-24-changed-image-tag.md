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

The pin now applies only if the name of the last start is the configured
name. Otherwise airlock resolves the tag, and a different digest gives the
image change question.

An import marker has only a digest. It keeps the pin. Without the pin, a
moved tag is an image change, and `--yes` removes the imported disk.

## Stored name

The image file of a sandbox is a hardlink to the cache entry of a digest,
and that entry has one name. Another sandbox with the same digest and a
different tag writes its name there, and the next start links the first
sandbox to that entry. The name check then fails, with these effects:

* A start resolves the tag. If the tag moved, an unchanged config gives an
  image change, and `--yes` removes the disk.
* `airlock exec` on the stopped sandbox finds no image. v2026.11.5 also has
  this problem.
* After the answer "Continue with current sandbox", the old entry has the
  new tag. Another sandbox on that entry that changes to the same tag keeps
  the old image with no question. v2026.11.5 has the same code.

Each sandbox thus has its own image record in the database. See
`2026-11-27-image-records.md`.

## Known limits

* A sandbox that kept its old image on an older airlock version has the new
  name in its image file, so it stays on the old image. Remove the sandbox
  to get the new image.

## Tests

* `test_image_cache::stored_digest_of_another_tag_is_not_pinned`
* `test_image_cache::sandboxes_with_different_names_for_one_image_keep_own_names`
* `tests/vm/image.bats` changes a tag and checks the guest version and the
  disk. It fails without the fix.
* Manual check with a local registry, for an unchanged name and a moved
  tag: a missing layer, a `v2` image file and an import marker each keep
  the old image and the disk.
* Manual check with two sandboxes on one digest: an unchanged config with a
  moved tag, `--yes`, `airlock exec`, and a kept old image.
