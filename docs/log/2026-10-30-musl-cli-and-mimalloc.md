# Static musl CLI on Linux and mimalloc

The Linux release CLI linked to glibc. Distros without glibc (Alpine,
for example) could not run it. CI now builds the Linux CLI artifacts
(normal and distroless) for `<arch>-unknown-linux-musl`. The Rust musl
target links statically by default, so the binary has no dynamic
section and no loader dependency. The artifact names stay the same, so
the release workflow, `install.sh` and self-update need no change.

The C parts of the build (vendored Lua, aws-lc, LMDB, mimalloc) compile
with `musl-gcc` from `musl-tools`. The CLI has no glibc-only code: no
NSS user lookup, no `dlopen`. TLS uses rustls, and the keyring uses the
pure-Rust Secret Service client.

Local dev builds stay on the host target (gnu). To find musl-only
problems early, `mise lint` also runs clippy on the musl target of the
CLI when that target is installed. The `lint-linux` CI job installs it.
The first finding: libc deprecates `time_t` on musl for a future type
change. The two `localtime_r` helpers allow the warning. `time_t` is
64-bit on the 64-bit targets already.

The musl allocator is slow under many small allocations, and the
network stack makes many of them. Thus both the CLI and airlockd use
mimalloc as the global allocator. airlockd was a musl build already,
so it gets the same gain. The fork paths in airlockd (`pre_exec`, the
sandbox namespace fork) use only raw syscalls, so the allocator change
has no effect on fork safety.
