# Verify OCI layer digests on pull

A code review flagged that `registry::pull_layer` compared the
downloaded blob against `layer.size` but not against `layer.digest`.
Whether the inner `oci_client::Client::pull_blob` verifies digests
itself depends on the crate version and configuration; airlock should
not rely on that transitively. The practical risk is a compromised or
MITM'd registry serving a same-size but different blob, which would
then be extracted into the image cache and eventually executed inside
the sandbox. The VM contains blast radius, but cached images are
long-lived and re-used across runs, so a one-time poisoning persists.

## Change

`pull_layer` now streams the downloaded bytes through a `HashingWriter`
that accumulates a SHA-256 as it forwards to the existing
`ProgressWriter → tokio::fs::File` chain. The hasher only ingests the
bytes actually accepted by the inner writer (i.e. those that made it
to disk after short-write handling) so the digest stays consistent
with the on-disk content.

After the pull completes, the size check remains and a digest check is
added: format the accumulated hash as `sha256:<hex>` and compare
case-insensitively against `layer.digest` from the manifest. On
either mismatch, the staged `.download.tmp` file is removed before
bailing so a failed download doesn't linger and doesn't poison the
atomic rename step.

The three writer wrappers compose as
`HashingWriter → ProgressWriter → File` with the hasher outermost;
this order matters because the progress bars should only advance for
bytes actually hashed and flushed to disk, which they already do by
sitting directly above the file.

## Why SHA-256 only

OCI descriptors carry `digest` strings prefixed by the algorithm
(`sha256:…`, `sha512:…`). All images airlock has pulled in practice
use `sha256`, and the spec designates it as the mandatory-to-implement
algorithm. Supporting `sha512` would double the writer types without
adding practical value; keep it simple and reject anything else on
mismatch (since the expected prefix won't match `sha256:<our hash>`).

## Size check kept

The size check runs before the digest check so a truncated download
(short read from the registry) fails with a clear message instead of
as a generic digest mismatch. A correct-digest-but-wrong-size case is
not reachable in practice (hash collisions aside) but the check is
cheap.

## Error behavior

`pull_blob` errors, flush errors, size mismatches, and digest
mismatches all remove the staged file before returning. The caller
(`layer::ensure_layer_cached`) relies on a clean slate for its
`.download.tmp → .download → .tmp → <digest>/` staging sequence, so
leaving a partial file would wedge the next attempt.

## Dependency

`sha2 = { workspace = true }` added to `airlock-cli`; the workspace
already pinned `sha2 = "0.10"` for other crates. `hex` was already a
direct dependency so no new transitive deps.
