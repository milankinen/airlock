# Replace version trailer with patched rodata slot

## Problem

The release action used to record the version by appending
`<utf8 version><len:u32le>` to the signed binary. On macOS this makes
`codesign --verify --deep --strict` fail with
`main executable failed strict validation`, because bytes past the
Mach-O structure are not allowed by the strict Mach-O layout check.
The signature itself was valid, but `--strict` is more pedantic than
Gatekeeper and rejected the layout.

Dropping `--strict` would have worked, but we want the signed release
to pass the strongest available local verification so regressions
show up in the release run rather than on a user's Mac.

## Approach

Embed the version inside the binary, before signing, in a way that
keeps the edit inside the signed region.

1. `airlock-cli` declares a 80-byte static `AIRLOCK_VERSION_SLOT`
   composed of a 16-byte sentinel (`AIRLK-VER-f3a7c2`) followed by a
   64-byte zero-padded version slot.
2. The release action (post-build, pre-sign) locates the sentinel
   with a small patcher script and overwrites the trailing 64 bytes
   with the release version. Because the static lives in rodata,
   the edit stays inside the Mach-O / ELF structure, and the
   subsequent `codesign` pass hashes the final bytes — `--strict`
   passes.
3. `release_version()` reads the static at runtime via
   `read_volatile` and scans up to the first null byte. Unpatched
   binaries read an all-zero slot and fall back to
   `env!("CARGO_PKG_VERSION")`.

## Design notes

- **`#[used]` + `#[unsafe(no_mangle)]`.** Without external linkage,
  the linker's dead-strip pass removed the static even though the
  sole reader (`release_version`) referenced it. Giving the symbol a
  public name forces the linker to preserve it across all three
  targets (macOS aarch64, Linux x86_64, Linux aarch64) without a
  per-platform `#[link_section]` dance.
- **`read_volatile`.** Stops LLVM from folding the slot's
  compile-time initializer into the call site at LTO time, which
  would eliminate the runtime read and make patching pointless.
- **Exactly-one sentinel.** The patcher aborts if the sentinel
  appears zero or more-than-one times. A second occurrence would
  mean rustc emitted the initializer bytes somewhere besides the
  static; we want to know about that rather than silently patch the
  wrong copy.
- **Fixed-size slot.** 64 bytes is plenty for versions like
  `v2026.4.3` (far under the limit) and keeps the on-disk layout
  predictable for the patcher.

## Alternatives considered

- **Drop `--strict` from the verify step.** Simplest fix, but ships
  a binary whose strictest local check does not pass.
- **Rebuild on the macOS release runner with `option_env!`.**
  Requires installing the full toolchain on the signing runner and
  fetching VM assets built by CI (kernel, initramfs,
  cloud-hypervisor, virtiofsd). Large CI surface change for a
  one-line version string.

## Files

- `app/airlock-cli/src/cli.rs` — slot definition and reader.
- `mise/tasks/release/patch-version` — self-contained Python
  patcher; also invokable from the release workflow directly.
- `.github/workflows/release.yml` — calls the patcher before
  `codesign` for macOS binaries and during Linux packaging, and
  restores `codesign --verify --deep --strict`.
