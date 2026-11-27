# Image records in the database

## Reason

The `image` file of a sandbox or a template was a hard link to the cache
entry `images/<digest>`. A link count above 1 told the cache cleanup that
the image was in use. This design is from the time when sandboxes were in
the project directories and airlock had no list of them. Now all sandboxes
are in the data directory and in the registry.

The cache entry of a digest also had one `name` for all sandboxes of that
digest. See `2026-11-24-changed-image-tag.md` for the problems that this
caused.

## Design

The database `images` has one record for each owner. The key is
`sandbox/<id>`, `template/<name>` or `template-build/<name>`. The value is
JSON with `digest` and `name`. An imported sandbox has no `name`, and an
empty `digest` if its old image is not known.

* `prepare` reads the record for the fast path, the digest pin and the
  image change check. It writes the record after the image change decision
  and before the download. Thus a sweep of another process keeps the image
  while it downloads. The answer "Continue with current sandbox" writes the
  old digest with the configured name.
* The fast path needs a record with the configured name. A configured name
  with a pinned digest needs a record with that digest, or no record. A new
  sandbox from a template has no record, so the template seed copies no
  image file.
* The cache entry has no `name`. An older entry with a `name` still parses.
* `airlock exec` reads the record of the sandbox.
* The sweep keeps the digests that the records name. It removes the records
  of owners whose directory is gone. If the records cannot be read, the
  sweep removes no image.
* `airlock rm`, `airlock sandboxes rm` and a failed first start remove the
  record of the sandbox. A template build writes the build record, and a
  successful build gives it to the template. A failed build removes it.

## Migration

A record read with no record takes the files of an older version into a new
record: the `image` file gives the digest and the name, else the
`image.imported` marker gives the digest. Then the files go. Before the
sweep, this runs for all sandboxes and templates. Thus a sandbox that did
not start since the update keeps its image.

The import of a sandbox from a project directory writes only the import
marker. It no longer links the cached image.

## Known limits

* A project sandbox that is not imported yet no longer keeps its cached
  image. The import keeps the digest, so the first start downloads the same
  image again.
* An imported sandbox has no name. Its first start resolves the image,
  also when the image is in the cache.
* An older airlock version reads a cache entry without `name` as a cache
  miss and pulls the image again. Its sweep removes the images that only
  records keep.
* If the download fails after the answer "Re-create sandbox", the next
  start sees no image change and keeps the disk. The old hard link design
  did the same, because it removed the link before the download.

## Tests

* `test_image_cache::gc_sweep_keeps_only_images_named_by_records_and_their_layers`
* `test_image_cache::image_files_of_older_version_become_records_and_keep_images`
* `test_image_cache::planted_image_file_symlink_is_not_followed`
* `test_image_cache::sandboxes_with_different_names_for_one_image_keep_own_names`
* `test_template_sandbox::sandbox_from_template_installs_only_extra_packs`
  checks the template record.
