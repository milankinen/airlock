# Rust

The `rust` pack installs Rust with rustup, and a C toolchain for linking.
It always allows the Rust project downloads, so rustup can install
toolchains and components, also for a `rust-toolchain.toml`.

```toml
[packs]
rust = { version = "1", args = { toolchain = "nightly" } }
```

| Arg           | Default  | Description                                       |
|---------------|----------|---------------------------------------------------|
| `toolchain`   | `stable` | Toolchain: `stable`, `beta`, `nightly`, `none` (rustup only), or a rustup toolchain name, for example `1.90` |
| `allow-cargo` | `true`   | Allow crates.io                                   |
