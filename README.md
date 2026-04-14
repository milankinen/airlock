# airlock

`airlock` is a command-line tool for running VM-sandboxed environments that
start within seconds and require no additional installers. It's primarily
meant for running AI agents in yolo mode, but works for any untrusted code.

Sandbox definitions live in config files so they can be shared across a team
or organization via version control.

Features:

- VM-level isolation — hardware boundary, not just namespaces
- Environment-agnostic — load the runtime from any OCI-compatible image
- Full network isolation with built-in connection interception and
  Lua-scriptable rules (see [design doc](./docs/DESIGN.md))
- Selective host filesystem sharing via VirtioFS mounts

## Quick start

**Install** (macOS / Linux):

```bash
curl -fsSL https://github.com/milankinen/airlock/releases/latest/download/install.sh | sh
```

This installs the `all` variant which bundles the VM kernel and initramfs.
For the `distroless` variant (bring your own kernel and initramfs via `airlock.toml`):

```bash
curl -fsSL https://github.com/milankinen/airlock/releases/latest/download/install.sh | sh -s -- --distroless
```

**Run:**

```bash
airlock start                       # Start interactive VM shell
airlock start -- ls /usr            # One-off command
airlock exec bash                   # Attach to running VM (alias: airlock x)
airlock show                        # Show sandbox status and config
airlock rm                          # Remove sandbox state

airlock help                        # See all available commands and options
```

## CLI reference

```
airlock start [options] [-- cmd args...]
```

Start the project VM from the current directory. Extra arguments after `--`
are passed to the container command.

| Option               | Description                                                                       |
|----------------------|-----------------------------------------------------------------------------------|
| `--sandbox-cwd PATH` | Working directory inside the container (defaults to host cwd)                     |
| `-l, --login`        | Run the container command in a login shell (sources `/etc/profile`, `~/.profile`) |
| `-v, --verbose`      | Show mounts and network rules during startup                                      |
| `--log-level LEVEL`  | Supervisor log verbosity: `trace`/`debug`/`info`/`warn`/`error` (default: `info`) |

---

```
airlock exec <cmd> [args...] [options]
airlock x <cmd> [args...]
```

Execute a command inside the running VM container.

| Option              | Description                              |
|---------------------|------------------------------------------|
| `-w, --cwd PATH`    | Working directory inside the container   |
| `-e, --env KEY=VAL` | Set an environment variable (repeatable) |
| `-l, --login`       | Run in a login shell                     |

---

```
airlock show
```

Show sandbox status, image, resource config, disk usage, mounts, and network
rules for the current project.

---

```
airlock rm [-f]
```

Remove the sandbox state (`.airlock/` directory). `-f`/`--force` skips the
confirmation prompt. Run `airlock start` again to reinitialise.

## Configuration

`airlock.toml` in the project root; `airlock.local.toml` for untracked local
overrides. Files are loaded in order (later wins): `~/.cache/airlock/config.toml`,
`~/.airlock.toml`, `airlock.toml`, `airlock.local.toml`. JSON and YAML are also
accepted alongside TOML.

Sandbox state (overlay, disk image, CA cert, lock) is stored in `.airlock/`
inside the project directory and is excluded from version control automatically.
`airlock rm` removes this directory entirely.

```toml
presets = ["rust"]             # built-in preset bundles

[vm]
image = "ubuntu:24.04"
cpus = 4
memory = "4 GB"
```

**Presets** (`alpine`, `debian`, `fedora`, `arch`, `suse`, `rust`, `python`,
`nodejs`, `claude-code`, `copilot-cli`, `codex`) supply network rules and
cache settings for common ecosystems. Your config always overrides presets.

### VM options

```toml
[vm]
image = "ubuntu:24.04"   # OCI image (string or object form, see below)
cpus = 4                # virtual CPUs (default: all host CPUs)
memory = "4 GB"           # VM memory (default: half of host RAM)
harden = true             # namespace isolation + no-new-privileges (default: true)
kvm = false            # nested KVM virtualisation, Linux only (default: false)

# distroless builds only:
kernel = "/path/to/vmlinux"
initramfs = "/path/to/initramfs.cpio.gz"
```

The `image` field accepts both a plain string and a config object:

```toml
# string (default)
[vm]
image = "alpine:latest"

# object — useful for private or local registries
[vm.image]
name = "localhost:5005/myimage:latest"
resolution = "registry"   # "auto" (default) | "docker" | "registry"
insecure = true         # plain HTTP instead of HTTPS (default: false)
```

`resolution` controls where the image is resolved from:

- `auto` — try local Docker daemon first, fall back to registry
- `docker` — local Docker only; error if not found
- `registry` — skip Docker, always pull from the OCI registry

### Disk and cache

airlock creates a VM disk image that persists container writes outside of
mounted host directories (for example, system package installs). The image
is 10 GB sparse by default and can be enlarged later. Note that changing the
project image resets the disk contents. To preserve state across image
changes, use named cache mounts:

```toml
[disk]
size = "20 GB"

[disk.cache.cargo]             # persists across image rebuilds
paths = ["~/.cargo/registry"]

[disk.cache.target]
paths = ["target"]             # relative = inside project dir
```

### Environment variables

```toml
[env]
MY_VAR = "value"
API_TOKEN = "${HOST_TOKEN}"    # expanded from host environment; empty string if unset
```

### Mounts

airlock can expose host directories into the VM as two-way synced VirtioFS
mounts:

```toml
[mounts.ssh-config]
enabled = true               # enable/disable mount (default: true)
source = "~/.ssh/config"    # path on the host
target = "~/.ssh/config"    # path in the container
read_only = true
missing = "warn"             # fail (default) | warn | ignore | create-dir | create-file
# file_content = "..."       # initial content when missing = "create-file"
```

The project directory is always mounted at its exact host path.

### Network rules

By default all outbound connections are allowed (passthrough, no inspection).
Use rules to restrict hosts, block specific destinations, or attach middleware:

```toml
[network]
default_mode = "deny"              # "allow" (default) or "deny"

[network.rules.my-registry]
allow = [
    "*.prod.example.com", # any subdomain of prod.example.com
    "registry.example.com:443", # specific host, port 443 only
    "*:80", # any host on port 80
]
deny = [
    "bad.example.com", # blocked unconditionally (deny wins)
]
```

`deny` patterns are checked first and win unconditionally over `allow`.

#### Network middleware

Rules can include Lua middleware to intercept TLS and modify HTTP traffic.
Add one or more `[[network.rules.<name>.middleware]]` entries:

```toml
[network.rules.my-api]
allow = ["api.example.com:443"]

[[network.rules.my-api.middleware]]
env.TOKEN = "${MY_API_KEY}"        # expanded from host environment; nil if unset
script = '''
if not env.TOKEN then
    req:deny()
end
req:setHeader("Authorization", "Bearer " .. env.TOKEN)
'''
```

The `env` table is populated at startup from the host environment using
`${VAR}` substitution. Values are available as `env.VAR` in the script;
any variable not set on the host is nil.

A per-project CA certificate is automatically installed in the container so
intercepted TLS is transparent to the containerized process.

### Unix socket forwarding

Forward a host Unix socket into the guest container:

```toml
[network.sockets.docker]
host = "/var/run/docker.sock"    # path on the host
guest = "/var/run/docker.sock"    # path in the container
```

## License

### Rust source code

All Rust source code in this repository (`crates` directory) is dual-licensed
under **MIT OR Apache-2.0** at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).

### Bundled Linux kernel (non-distroless builds)

The default build embeds a Linux kernel and initramfs image directly into the
`airlock` binary as data blobs that are extracted at runtime and loaded by the
hypervisor. The Linux kernel is distributed under the **GNU General Public
License version 2** (GPLv2). Distributing a binary that contains the bundled
kernel therefore requires GPLv2 compliance for the kernel component —
in particular, you must make the corresponding kernel source available to
recipients upon request. The Rust-authored code in the same binary remains
**MIT OR Apache-2.0**; it does not constitute a combined work with the kernel
because the two components execute in separate protection domains (host process
vs. guest VM) and do not link to or call each other directly.

Releases also include a pre-built `distroless` variant **without the embedded
Linux kernel or initramfs**, licensed entirely under MIT OR Apache-2.0.
When using the distroless build, you must supply your own kernel that has the
capabilities required to work correctly with the `airlockd` supervisor.
See [kernel configs](vm/kernel) for details.
