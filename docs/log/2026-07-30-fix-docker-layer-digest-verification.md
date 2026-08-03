# Fix docker-loaded layers cached without digest verification

## Symptom

Image layers imported from the local Docker daemon (`docker image save`) were
written into the shared OCI layer cache using only the digest advertised in the
tar member name `blobs/sha256/<hex>`. The bytes were never hashed. A subsequent
registry pull whose manifest referenced the same `sha256:<hex>` found the layer
already extracted and reused it as if it had passed the registry's own SHA-256
check — so content from an untrusted or tampered Docker image could satisfy a
later, supposedly verified, registry pull. Registry downloads have always
verified size and SHA-256 (`registry::pull_layer`); the docker import path did
not, leaving a cross-source cache-poisoning gap.

## Root cause

`save_from_stream` in `oci/docker.rs` copied each `blobs/sha256/<hex>` blob
straight to `<key>.download.tmp` with `std::io::copy` and keyed the cache purely
on the `<hex>` from the tar entry name. Nothing recomputed the SHA-256 of the
content, so the on-disk cache key was never proven to match the bytes. Because
the layer cache is shared across sources and keyed only by digest, an unverified
docker blob at `layers/2.<hex>/` short-circuited any future pull for that digest.

## Fix

Hash the blob while it is being staged and reject it before it is renamed into
the cache. `save_from_stream` now streams each blob through a new `copy_hashing`
helper (reusing `sha2::Sha256`, already used by the registry path) and compares
the computed hex against the claimed `<hex>`. On mismatch the staged
`.download.tmp` is removed and the import fails with a `docker layer digest
mismatch` error, so poisoned content never reaches `layers/2.<hex>/`. The check
covers config and layer blobs alike, since docker-save names every blob by the
SHA-256 of its content. No new dependencies; this mirrors the existing registry
verification.

## Tests

- `copy_hashing_matches_known_vectors` — the helper reproduces the standard
  SHA-256 vectors for `"abc"` and the empty input and passes the bytes through
  unchanged.
- `save_from_stream_rejects_blob_with_mismatched_digest` — feeds an in-memory
  docker-save tar whose blob is named `sha256:aaaa…` but whose content hashes to
  something else, and asserts the import errors with a digest mismatch and leaves
  no staged `.download.tmp` behind. Runs without a Docker daemon
  (`save_from_stream` now accepts any `Read`).
