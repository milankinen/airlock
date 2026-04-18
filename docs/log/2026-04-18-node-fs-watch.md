# Node.js fs.watch + TLS in the sandbox

Two small changes to make common Node dev loops work out of the box
in the guest VM:

## Enable inotify on arm64

The guest kernel config on arm64 had `CONFIG_INOTIFY_USER` and
`CONFIG_FANOTIFY` turned off. That made every Linux program that
depends on `fs.watch` fail with `ENOSYS` — Node's stdlib, chokidar,
nodemon, tsx watch, vite, etc. The x86_64 config already enables
both, so this was a pure arm64 oversight.

Fix: flip both to `=y` in `vm/kernel/config-arm64` alongside the
existing `# CONFIG_DNOTIFY is not set` line. DNOTIFY stays off —
it's the pre-inotify API, unused by modern tooling.

Requires rebuilding the kernel (`mise run build:kernel`).

## NODE_EXTRA_CA_CERTS in the node preset

We MITM all allowed HTTPS inside the sandbox (see
`2026-04-…-mitm-all-tls.md`), so guest-side clients need to trust
our CA bundle. Most tools read `/etc/ssl/certs/ca-certificates.crt`
automatically; Node doesn't — it ships its own compiled-in root
list and only reads a bundle when `NODE_EXTRA_CA_CERTS` is set.

Setting the var in the `node` preset means `npm install`, `fetch`,
`undici`, and anything else using Node's TLS stack picks up our
intercept cert without each project needing to configure it. The
file is guaranteed to exist in the guest rootfs.
