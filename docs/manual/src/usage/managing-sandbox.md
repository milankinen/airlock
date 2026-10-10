# Managing sandboxes

## View current sandbox

The `airlock info` command shows the sandbox and the configuration 
of the current project. It is a shortcut for `airlock sandbox info` 
without an id:

```bash
airlock info
airlock info --json   # Sandbox details as JSON, for scripts
```

The output includes the image name, CPU and memory allocation, disk usage,
the packs and their install status, configured mounts, network rules, the
sign-ins of the network services, and whether the sandbox is currently
running. Use it to see your configuration without opening the TOML
file.

Example output:

```
Path:     /Users/me/my-project
ID:       k3x7q2ma
Status:   running
Image:    debian:stable-slim
CPUs:     4
Memory:   2.0 GB
Last run: 2 minutes ago

Sandbox:  /Users/me/Library/Application Support/airlock/boxes/k3x7q2ma
Disk:     1.2 GB / 10.0 GB

Packs:
  debian 1 — config only
  claude 1 — installed
  rust 1 (toolchain = nightly) — installed

Mounts:
  claude-dir: /Users/me/Library/Application Support/airlock/share/all/packs/claude/claude → ~/.claude

Network policy: deny-by-default
Network rules:
  claude-code: allow 2 deny 0
  debian-packages: allow 7 deny 0
  rust-lang: allow 2 deny 0
  rust-packages: allow 3 deny 0
Services:
  (sign-ins are the user's: all sandboxes share them)
  anthropic:
    me@example.com (5 scopes), saved 2 days ago
```

The pack status is one of these:

| Status                           | Meaning                                         |
|----------------------------------|-------------------------------------------------|
| `installed`                      | The pack is on the sandbox disk                 |
| `pending`                        | The next `airlock start` installs the pack      |
| `install failed`                 | The last install failed. The next start tries again |
| `install not confirmed`          | The last install did not complete correctly. The next start tries again |
| `config only`                    | The pack installs nothing, it adds only config  |
| `installed, removed from config` | The pack is still on the disk, but not in the config |
| `kept, removed from config`      | You selected "continue with current" after you removed the pack |

## Listing sandboxes

The `airlock sandbox` command shows the sandboxes in the airlock data
directory (see [Where airlock keeps sandbox data](#where-airlock-keeps-sandbox-data)):

```bash
airlock sandbox list              # All sandboxes in the data directory (alias: ls)
airlock sandbox info k3x7q2ma     # Details of one sandbox (as `airlock info`)
```

The list shows the id, the status, the last run, the disk use and the
project directory of each sandbox. If the project directory no longer
exists, the list shows `(missing)` next to it.

The list does not show sandboxes in a project directory. `airlock info`
shows them when you run it in their project. If the project directory of
a sandbox is gone, `airlock sandbox info` shows only the sandbox.

## Removing sandbox

The `airlock remove` command (alias `airlock rm`) removes the sandbox of
the current project. It is a shortcut for `airlock sandbox remove` without
an id. This removes the disk image, the CA certificate,
the install records, the local project config and the other runtime
state:

```bash
airlock remove
```

The `airlock sandbox remove` command (alias `airlock sandbox rm`) removes
any sandbox in the data directory by its id (see `airlock sandbox list`).
Use it for the sandbox of a project directory that no longer exists:

```bash
airlock sandbox remove k3x7q2ma
airlock sandbox remove k3x7q2ma x5bq7d2c   # Remove more than one
```

Both commands ask you to confirm before they remove anything. To skip the
confirmation prompt (for example in scripts), pass `--force`. airlock
refuses to remove a running sandbox.

After removal, `airlock start` makes a new sandbox: a new disk, a new CA
certificate, and a new image pull if necessary. Removal does not change
the project configuration files (`airlock.toml`, `airlock.local.toml`).
If the project has no other config, `airlock start` opens the setup
wizard again.

In your home directory, `.airlock/` also holds your user files: user
config, vault and settings. There, `airlock rm` removes only
`.airlock/sandbox/`. If `.airlock` is a symbolic link, `airlock rm`
removes only the link.

## Where airlock keeps sandbox data

airlock stores the sandbox data in the application data directory:

- macOS: `~/Library/Application Support/airlock`
- Linux: `$XDG_DATA_HOME/airlock` or `~/.local/share/airlock`

To use a different directory, set `data_dir` in `~/.airlock/settings.toml`:

```toml
data_dir = "~/airlock-data"
```

The data directory also holds the image cache. Keep it on one file
system: airlock links each sandbox to its cached image with a hard link.

Older airlock versions kept the image cache in `~/.cache/airlock`. If the
data directory does not exist, airlock moves this cache to the data
directory when it starts. If the data directory is on a different file
system, airlock shows a warning and uses `~/.cache/airlock` as the data
directory.

## Project-colocated sandboxes

Older airlock versions kept the sandbox in `.airlock/sandbox` in the
project directory. `airlock start` offers to move such a sandbox into the
data directory.

To keep new sandboxes in the project directory, set `sandbox_type` in
`~/.airlock/settings.toml`:

```toml
[security]
sandbox_type = "project-owned"   # default: "managed"
```

The guest hides the `.airlock` directory of a project-owned sandbox.

> [!WARNING]
> A sandbox in the project directory is less secure. The sandbox guest
> can read the project directory, so the sandbox data (for example the CA
> private key) is open to code in the sandbox. Keep the default and use
> the application data directory.
