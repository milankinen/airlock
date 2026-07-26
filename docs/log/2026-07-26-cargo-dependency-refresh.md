# Cargo dependency refresh

Routine sweep to bring every direct dependency to its latest release.
Most of it was a lockfile refresh plus version-string edits; four
crates needed real migration work, and one is deliberately held back.

## keyring 3 → 4 (the interesting one)

keyring 4 was restructured from a single crate with per-platform
feature flags into `keyring-core` plus one crate per credential store.
The old `default-features = false` spelling no longer builds at all:
`lib.rs` has a `compile_error!` unless one of `v1` or `cli` is enabled.

`v1` is the compatibility surface — it re-exports an `Entry` wrapper
with the same `new` / `get_password` / `set_password` / `Error::NoEntry`
API we were already using, so `vault/keyring.rs` needed no changes. It
also hardcodes the store per OS in `set_credential_store()`: Keychain
Services on macOS, Secret Service on other unixes.

Two consequences worth recording:

- **The per-target keyring entries in `app/airlock-cli/Cargo.toml` are
  gone.** All the store crates are target-gated inside keyring's own
  manifest, so one workspace-level entry now covers every platform.
  Previously we hand-picked `sync-secret-service` + `crypto-rust` +
  `vendored` for Linux and `apple-native` for macOS.
- **libdbus is no longer in the build.** `v1` selects
  `zbus-secret-service-keyring-store`, which talks D-Bus over pure-Rust
  zbus. The `dbus` and `dbus-secret-service` crates dropped out of the
  lockfile entirely, and with them the `vendored` feature that existed
  only to avoid depending on a system libdbus.

The Secret Service calls are still synchronous from our side, so the
blocking semantics of `Storage::load` / `Storage::store` are unchanged.
zbus's blocking wrappers resolve to `async-io`, not tokio — checked
because a tokio-backed `block_on` nested inside our runtime would
panic. `async-io`'s does not; it just parks the calling thread, which
is what the libdbus path did anyway.

## rand 0.9 → 0.10

Two renames, both in `vault/encrypted.rs`: the `TryRngCore` trait is
now `TryRng`, and `rngs::OsRng` is now `rngs::SysRng` (re-exported from
`getrandom`, behind the default-on `sys_rng` feature).

## scc 2 → 3

The blocking methods grew a `_sync` suffix — `get` → `get_sync`,
`insert` → `insert_sync` — freeing the unsuffixed names for the async
variants. Mechanical change in `net/dns.rs`; same locking behaviour.

## sha2 0.10 → 0.11, chacha20poly1305 0.10 → 0.11

The RustCrypto bump moves fixed-size buffers to `hybrid_array`, which
deprecates `Array::from_slice` in favour of conversions. `Key` and
`Nonce` are concrete aliases (`Array<u8, U32>` / `Array<u8, U12>`) and
our inputs are already exact-size arrays, so `<&Key>::from(&key)` is
the zero-copy, compile-time-checked replacement — no runtime length
check, no copy of the key material.

Algorithms and parameters are untouched, so the on-disk encrypted
vault format is unchanged; `encrypted_file_storage_roundtrip` and
`encrypted_file_storage_rejects_wrong_passphrase` cover that.

## Drop-in

base64 0.23, capnp/capnp-rpc/capnpc 0.26, mlua 0.12, notify 8,
oci-client 0.17, quick_cache 0.7, simple-dns 0.11 — no source changes.
notify 6 → 8 crossing two majors without a code change is a little
surprising; the `RecommendedWatcher` + `Config` + `Event` surface we
touch in `vm/file_sync.rs` didn't move.

## Held back

- **sysinfo 0.38 → 0.39** needs Rust 1.95; `rust-toolchain.toml` pins
  1.94. Bump together with the toolchain — done immediately after, see
  `2026-07-26-rust-1.97-toolchain.md`.
- **matchit 0.8.4 → 0.8.6** is pinned exactly (`=0.8.4`) by axum 0.8.
- **generic-array 0.14.7 → 0.14.9** is pinned exactly by
  `crypto-common` 0.1.7, reached via argon2 0.5 and secret-service.

The last two are transitive-only and resolve themselves whenever those
upstreams relax their pins.
