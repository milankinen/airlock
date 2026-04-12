# OCI registry auth, image config object form, local dev registry

## Registry credential prompting and storage

`registry::resolve` and `registry::pull_layer` were hardcoded to
`RegistryAuth::Anonymous`. Pulls from private registries would fail with
an OCI `UNAUTHORIZED` error and no recovery path.

### Flow

1. Load stored credentials for the registry hostname (Keychain on macOS,
   `~/.ezpez/registry-credentials.json` on Linux with 0600 permissions);
   fall back to anonymous if none.
2. Try `resolve_image`. On `AuthenticationFailure` / `UnauthorizedError`:
   prompt interactively (username + password via `dialoguer`), retry.
3. If the retry also fails with an auth error, print "authentication failed,
   try again" and loop back to the prompt — repeating until either the
   resolve succeeds or the user interrupts (Ctrl+C).
4. On success after a prompt, save credentials to the platform store.
5. Layer downloads (`ensure_image`) reuse the same auth — no separate retry
   needed there.

Auth errors are detected by downcasting `anyhow::Error` to
`oci_client::errors::OciDistributionError` and matching
`AuthenticationFailure` or `UnauthorizedError`.

### Credential storage

- **macOS**: `security_framework::passwords::{get,set}_generic_password`
  with service `"ezpez-registry"` and account = registry hostname.
  Credentials are stored as JSON bytes `{"username":"…","password":"…"}`.
  `security-framework` is already a transitive dep (via `rustls-native-certs`);
  added as an explicit dep in `crates/ez/Cargo.toml` for macOS.
- **Linux/other**: `~/.ezpez/registry-credentials.json` (0600), a flat
  JSON map from registry hostname to `{username, password}`.

## `vm.image` config: string or object form

The `image` field was `String`. It now accepts both forms:

```toml
# string (backwards compatible)
image = "alpine:latest"

# object
[vm.image]
name = "localhost:5005/alpine:3"
resolution = "registry"   # "auto" (default) | "docker" | "registry"
insecure = true           # use plain HTTP (default false)
```

`ImageRef` implements `serde::Deserialize` with an untagged helper enum
(string → `ImageRef::auto(name)`, object → full struct). Its `WellKnown`
impl uses `Serde<{ BasicTypes::STRING.or(BasicTypes::OBJECT).raw() }>` so
smart-config accepts both TOML value kinds.

`Resolution` controls whether Docker daemon or the OCI registry is tried:
- `auto` — try Docker first (by image ID/arch), fall back to registry
- `docker` — Docker only; bail if not found locally
- `registry` — skip Docker, go straight to the registry

`insecure = true` makes `make_client` use `ClientProtocol::Http` instead
of `ClientProtocol::Https`, fixing the `InvalidContentType` TLS error that
occurred when pointing at a plain-HTTP local registry.

## Local dev registry (docker compose)

`dev/registry/` provides a two-registry compose setup for testing:

- `localhost:5005` — `testuser` / `testpass`
- `localhost:5006` — `testuser` / `testpass2`

Same username, different passwords — exercises independent per-hostname
credential entries. `setup.sh` generates both htpasswd files, starts the
registries, and pushes a test image to each. Accessible via
`mise run dev:setup-registry`.
