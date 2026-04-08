# Distroless feature, custom kernel/initramfs config, and OCI wrapper scripts

## Custom kernel/initramfs config (`vm.kernel`, `vm.initramfs`)

Added optional `vm.kernel` and `vm.initramfs` string fields to the `VirtualMachine`
config struct. When present, `Assets::init` validates that the paths exist (expanding
`~` and resolving relative to `host_cwd`) and uses them instead of the bundled assets.
This enables users to test with custom kernels without rebuilding the binary.

## `distroless` Cargo feature

Added `distroless = []` feature that gates out the `include_bytes!` calls for the
bundled kernel (`sandbox/out/Image`) and initramfs (`sandbox/out/initramfs.gz`).
In a distroless build, `Assets::init` returns an error at runtime if `vm.kernel` or
`vm.initramfs` are not configured — the binary carries no kernel at all.

The `build.rs` rerun-if-changed and hashing logic is also skipped when
`CARGO_FEATURE_DISTROLESS` is set, so distroless builds don't need the kernel/rootfs
artifacts to be present.

### CI and release changes

Added three new CI jobs that build distroless variants (macOS aarch64, Linux x86_64,
Linux aarch64) using `--features distroless`. These jobs skip the kernel/rootfs build
steps entirely. The release workflow packages and publishes them as
`ez-${VERSION}-*-distroless.tar.gz` alongside the standard binaries.

## `/ez/oci-run` and `/ez/oci-exec` wrapper scripts

Replaced direct `crun run` and `crun exec` invocations in the Rust supervisor RPC code
with thin shell wrapper scripts bundled inside the rootfs:

- `/ez/oci-run` — wraps `crun run --no-pivot --bundle /mnt/overlay ezpez0`
- `/ez/oci-exec` — wraps `crun exec "$@"`

This decouples the host-side Rust code from the exact `crun` invocation flags, making
it easier to swap the runtime without recompiling the host binary. The scripts are
built into the rootfs via `sandbox/rootfs/build.sh` (copied from
`sandbox/rootfs/ez/oci-{run,exec}`) and tracked as mise task sources so the rootfs
rebuild is triggered when they change.

In dev mode (`EZ_DEV_NO_CRUN=true`), the Rust code still bypasses the scripts and
runs the shell or user command directly (unchanged behavior).
