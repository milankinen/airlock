# Honour `[env].HOME` in container-side `~` expansion

When a user sets `HOME` in their `[env]` block, the in-container
shell reports the override (existing behaviour — `vm::resolve_env`
already overlays `[env]` over the image env, replacing the image's
`HOME=/root` line). But the host-side tilde expansion of mount
`target`s, cache paths, and socket-forward `host.target`s only
consulted the OCI image's user record. So a config like:

```toml
[env]
HOME = "/home/dev"

[mounts.ssh-config]
source = "~/.ssh/config"
target = "~/.ssh/config"
```

…would mount the file at `/root/.ssh/config` inside the sandbox
while the shell's `$HOME` reported `/home/dev` — `~` and `$HOME`
disagree, and tools that re-tilde the path of a mount source land
somewhere different from where it was actually mounted.

### Fix

`oci::effective_container_home(project, image)` returns the
override-aware home: `[env].HOME` (run through the existing vault
`${VAR}` substitution) if set, otherwise the image's user-record
home. Computed once in `cmd_start::main` after `oci::prepare`, then
threaded into `network::setup` and `vm::start` (which forwards to
`assemble_mounts` and `disk::prepare`). The `OciImage.container_home`
field stays as the raw image-derived value to keep the type honest;
its docstring points readers at `effective_container_home` for the
guest-path expansion case.

### Why a helper, not a method on Project

The project has the env map and the vault but not the OCI image; the
image has the fallback home but doesn't know about the env. A free
function next to `OciImage` takes both — small, no need to plumb
either type through into the other.

### Out of scope

- Host-side `~` expansion still uses `dirs::home_dir()` (no override
  knob). The host's `HOME` is what the user's shell already uses, so
  there's nothing to be wrong about.
- Re-substituting the resolved home back into `vm::resolve_env`'s
  output to keep them rigorously identical. They already are: the
  helper reads the same `[env].HOME` template through the same vault
  subst, and `resolve_env` overlays the same `[env]` on top of the
  image env. The two paths produce the same bytes.
