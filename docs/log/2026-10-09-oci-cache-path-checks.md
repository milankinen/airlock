# OCI cache path checks

Review finding #4 (`FINDINGS.md` in faa033a). Present in v2026.9.6.

## Problem

- Layer extraction turns `.wh.<name>` into an overlayfs whiteout. It
  first deletes `<dir>/<name>`. The entry `dir/.wh...` gives the name
  `..`, and `.wh.` gives the empty name. The delete then ran
  `remove_dir_all` on the staging dir or its parent, which is
  `<data>/oci/layers/`. One hostile image layer emptied the shared layer
  cache, also the layers of running VMs.
- Digests from a registry manifest and from `docker save` manifests went
  into cache paths with no check. `digest_name` keeps the part after the
  last `:`, so `sha256:<hex>/../../..` named a path out of the cache.

## Change

- `layer.rs`: refuse the layer if the whiteout target is empty, `.`, `..`
  or contains `/`. `safe_join` already checks the parent path, but not
  the final name.
- `cache::check_digest`: requires `sha256:` and 64 lowercase hex digits
  (OCI spec form). `registry::resolve` checks the image digest and each
  layer digest before any cache path is made.
- `docker.rs`: a manifest layer must be a content-addressed blob name
  (`blob_hex`). Before, other names fell through as raw digests. They
  could never have a staged blob, so the start failed later anyway. Now
  it fails early with a clear error.
- `cache::image_path` / `cache::layer_dir`: defense in depth. The name
  must be one plain path component. This check is loose on purpose
  (not a full digest check), because local engine image IDs and test
  digests are not always `sha256:<hex>`.

## Tests

- `test_layer_cache.rs`: `.wh.`, `.wh..` and `.wh...` refuse the layer
  and a cached layer stays.
- `test_docker_save.rs`: a manifest layer `../../../etc/passwd` is
  refused.
