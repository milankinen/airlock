# Update libkrun to the newest main commit

## Why

The new `main` has the `LinuxSimplifiedFixedOwner` virtio-fs semantics.
airlock uses it for the user data shares, see
`2026-11-17-libkrun-fixed-owner-shares.md`. Other changes that matter on
macOS arm64:

* Transparent huge pages for guest RAM. A clean `cargo check` in the
  guest went from about 4.0 s to 1.4 s.
* The balloon maps guest RAM in 2 MiB blocks and gives free pages back
  one block at a time (free page reporting in Replace mode).
* hvf masks SME on all vCPUs and handles 2-byte MMIO writes.
* Exit codes stay the same over pause and resume.

## FFI change

The pin moves from `a0375321` to `1156ba7f`. The ffier 0.3 bindings
change the layout of objects that the caller implements: the handle has
no vtable size, and each vtable starts with a slot count and two
reserved fields. airlock implements only the string writer for libkrun
error texts, so only that changed. The functions and constants that
airlock uses did not change.

## Testing

* The new libkrun passes `cargo check` for `aarch64-apple-darwin`.
* The writer layout is checked against the new header and the C
  examples. A wrong layout shows only when libkrun gives an error.
* Manual: the sandbox booted on macOS with the new library.
