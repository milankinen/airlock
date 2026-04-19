# Move project CA injection from a host overlay to a guest RPC field

The project CA used to be materialized on the host as five pre-edited CA
bundle files under `sandbox/ca/`, exposed as its own `ca` virtiofs share
and stacked as the highest-priority overlayfs lowerdir in the guest. That
layout was a workaround for the host having to compose the bundles
against the image's rootfs tree — which is the same host-side reason
`images/<d>/rootfs/` still exists.

Now that the guest assembles rootfs from the per-layer cache, the image's
CA bundles live on lowerdirs the host never reads. Pushing the CA
through the start RPC and appending to the bundles inside the guest,
after overlayfs is mounted, is both simpler and removes one of the last
two reasons the host needs a merged image rootfs (the other, home-dir
lookup, goes next).

## What changed

- `supervisor.capnp`: `start` gains `caCert :Data`. Empty when the
  project has no CA (vault disabled / TLS interception off).
- Guest (`airlockd`):
  - `MountConfig` gains `ca_cert: Vec<u8>`.
  - `init/linux.rs` drops `mount_virtiofs("ca")` and the `/mnt/ca`
    lowerdir entry. After the overlayfs rootfs is mounted, a new
    `inject_ca_cert` walks the same five well-known bundle paths
    (`etc/ssl/certs/ca-certificates.crt`, …). For each that exists in
    the lower stack, it reads, appends the CA, and writes back —
    overlayfs copy-up lands the edit on the upperdir. If none of the
    known bundles are present (e.g. a minimal distroless image), it
    writes a standalone bundle at the Debian/Ubuntu path so
    `SSL_CERT_FILE` can point at a stable location.
- Host CLI:
  - `rpc/supervisor.rs` sends `project.ca_cert.as_bytes()` in the
    `start` request.
  - `project::install_ca_cert` and the `sandbox_dir/ca/` overlay are
    deleted. The Recreate path still cleans up that legacy directory.
  - `vm::prepare_shares` drops the `ca` virtiofs share.
  - `OciImage` no longer carries `rootfs`; nothing on the host reads
    a merged image rootfs after this change.

## Why copy-up is safe here

The upperdir lives on the sandbox's ext4 disk, which backs the overlayfs
upper + work dirs. That disk is already per-sandbox, so the copy-up's
write doesn't leak into any shared cache. The lowerdir layer trees stay
untouched and still dedup across sandboxes, exactly as before — the only
difference is that the CA-edited version of `ca-certificates.crt` now
lives on the upper instead of being synthesized on the host.

## Fallback path

The previous behavior always wrote all five bundle paths, even when the
image didn't ship them. The guest version only writes the path that
actually exists in the lower stack; if none do, it drops the bundle at
the Debian/Ubuntu path. This matches the typical pattern: containers
that need HTTPS usually ship the standard bundle, and minimal images
without any bundle need the CA written *somewhere* for
`SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt` to work.

## Out of scope

- Moving home-dir lookup to the guest — next step, after which
  `images/<d>/rootfs/` extraction can be removed entirely.
- Per-layer CA injection (writing into each layer's upperdir separately
  so the CA is visible even when a lower layer is masked by an opaque
  upper). Not needed: the edit happens on the composed upperdir, which
  every readdir() through the overlay sees.
