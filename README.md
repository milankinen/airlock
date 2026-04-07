# ezpez

> Easy peasy.

`ezpez` is a command-line tool for running VM-sandboxed environments that
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

```bash
ez go                          # start interactive VM shell
ez go -- ls /usr               # one-off command
echo hello | ez go -- cat      # pipe mode
ez exec bash                   # attach to running VM (alias: ez x)

ez help                        # see all available commands and options
```


## Configuration

`ez.toml` in the project root; `ez.local.toml` for untracked local overrides.
Files are loaded in order (later wins): `~/.ezpez/config.toml`, `~/.ez.toml`,
`ez.toml`, `ez.local.toml`.

```toml
image = "ubuntu:24.04"
cpus = 4
memory = "4 GB"
presets = ["rust"]             # built-in preset bundles
```

**Presets** (`alpine`, `debian`, `fedora`, `arch`, `suse`, `rust`, `python`,
`nodejs`, `claude-code`, `copilot-cli`, `codex`) supply network rules and
cache settings for common ecosystems. Your config always overrides presets.


### Disk and cache

ezpez creates a VM disk image that persists container writes outside of
mounted host directories (for example, system package installs). The image
is 10 GB sparse by default and can be enlarged later. Note that changing the
project image resets the disk contents. To preserve state across image
changes, use named cache mounts:

```toml
[disk]
size = "20 GB"

[disk.cache.cargo]             # persists across image rebuilds
path = "~/.cargo/registry"

[disk.cache.target]
path = "target"                # relative = inside project dir
```


### Mounts

ezpez can expose host directories into the VM as two-way synced VirtioFS
mounts:

```toml
[mounts.ssh-config]
enable    = true               # enable/disable mount (default: true)
source    = "~/.ssh/config"    # path on the host
target    = "~/.ssh/config"    # path in the container
read_only = true
missing   = "warn"             # fail | warn | ignore | create
```

The project directory is always mounted at its exact host path.


### Network rules

By default all outbound traffic is blocked. Define rules to allow specific
hosts:

```toml
[network.rules.my-registry]
allow = [
    "*.prod.example.com",           # any subdomain of prod.example.com
    "registry.example.com:443",     # specific host, port 443 only
    "http:packages.example.com",    # HTTP only (no TLS interception)
    "*:80",                         # any host on port 80
]
```


#### Network middleware

Rules can include Lua middleware to intercept and modify traffic:

```toml
[network.rules.my-api]
allow = ["api.example.com:443"]

[network.rules.my-api.middleware."api.example.com"]
script = '''
if req.path ~= "/telemetry" then
  req:deny()
end
req:header("Authorization", "Bearer " .. os.getenv("MY_API_KEY"))
'''
```
