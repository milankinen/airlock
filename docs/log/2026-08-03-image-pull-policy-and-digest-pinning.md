# Add `vm.image.pull-policy` and digest-pinned image references

## Motivation

Image resolution had exactly one behaviour, and it was invisible: if a ready
image was already cached under the configured name, `prepare` returned it
without contacting the registry at all. A moved `:latest` therefore stayed
unnoticed indefinitely — the only ways to pick up a new image were to change
the name string or wipe the cache. That is the right default for a fast
`airlock start`, but there was no way to opt out of it.

There was also no way to say "this exact image". The name string was passed
verbatim to `oci_client::Reference`, whose grammar already accepts
`repo[:tag][@algo:hex]`, but nothing in airlock ever read `.digest()`, so a
pin was neither honoured on the Docker path nor verified on the registry path.

## Change

`[vm.image]` grows a `pull-policy` field:

- `if-not-present` (default) — the existing name-keyed fast path, unchanged.
- `if-changed` — skip the fast path, re-resolve the reference to a digest on
  every start, and reuse the cached image only while the digest matches.

When the digest *has* moved, `if-changed` funnels into the existing
`prompt_image_changed()` flow (re-create / keep old / cancel) rather than
recreating unattended. Recreating discards the sandbox overlay, i.e. the
sandbox's persistent state, so silently reloading on a tag push would be a
data-loss footgun; the prompt already existed for exactly this decision.

Image names now accept a pinned digest, with or without a tag alongside it
(`ubuntu@sha256:…`, `ubuntu:24.04@sha256:…`), matching Docker/Compose. A pin
short-circuits `pull-policy` in both directions: a digest is immutable, so a
name match already implies a digest match and there is nothing to re-check.

## Notes on the implementation

**The pin is verified, not assumed.** A local Docker tag can point somewhere
entirely different from the same tag in the registry, so `resolve_via_docker`
strips the `@sha256:…` suffix before querying (`docker images` matches on
repo:tag and knows nothing about digests) and then checks the daemon's
recorded `RepoDigests` via the new `docker::repo_digests`. A mismatch demotes
Docker to "present but unusable", which falls through to the registry under
`auto` and is fatal under `docker`. Locally built images have no
`RepoDigests` entry at all and so can never satisfy a pin.

**A pin may name the index or the manifest.** `pull_manifest_and_config`
returns the digest of the *platform-specific* manifest it selected, but what a
registry advertises for a tag — and therefore what users copy into a config —
is usually the multi-platform *index* digest. Comparing a pin against only the
former would reject correct pins on every multi-arch image. `registry::resolve`
switched to `pull_manifest_and_config_and_list_digest` and now carries
`list_digest` on `RegistryImage`; the pin check accepts either. The plain
variant upstream just delegates to the list-digest one and drops the field, so
the `digest` we store as `image_id` is byte-identical to before — no existing
cache is invalidated by the switch.

**Resolution failure under `if-changed`.** The round-trip is a liveness check,
not a prerequisite: a registry that is down shouldn't strand a sandbox whose
image is already complete on disk. On a non-auth resolution error with a usable
cached image, airlock asks `image resolution failed (<error>): do you want to
continue with cached image?`. It defaults to no and returns `Ok(false)`
immediately when non-interactive, so CI fails loudly rather than running a
quietly stale image. The fallback is suppressed when `cli::is_interrupted()` —
a Ctrl+C during auth should not be answered with another prompt.

The fallback can only trigger under `if-changed` in practice: it requires a
ready cached image whose stored name matches the config, which is precisely the
condition the `if-not-present` fast path would have returned on before any
resolution was attempted.

**Refactors carried along.** The fast path and the failure fallback both need
to heal the GC hardlink, create the overlay dir, and report "environment
ready", so that tail moved into `use_cached_image`. The anonymous → vault →
prompt credential retry loop moved into `resolve_with_auth`, which returns the
auth that worked, so that resolution has a single failure point to hang the
fallback prompt off. `resolve_image` now takes `&ImageRef` instead of three
unpacked fields.

**Key spelling.** The user-facing key is `pull-policy`, but every other config
section in this repo uses snake_case keys and `ImageRef`'s untagged
`Deserialize` helper silently ignores unknown fields — so `pull_policy` would
have parsed to the default instead of erroring. Added as a serde alias.

## Tests

`config::tests::test_image_ref` — string vs object form defaults, both key
spellings, an unknown policy value failing closed, digest pins recognised in
all three name shapes (bare, tag+digest, registry-with-port), unpinned names
yielding `None`, and malformed digests (no separator, too short, non-hex,
empty name) not being mistaken for pins. That last case matters: an empty name
prefix would otherwise strip to `""` and make the Docker branch run
`docker images ""`, which lists every image and would resolve to an arbitrary
one.
