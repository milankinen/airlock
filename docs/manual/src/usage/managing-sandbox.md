# Managing sandbox data

## Viewing sandbox status

The `airlock show` command displays the current sandbox configuration and
status for the project:

```bash
airlock show
```

The output includes the image name, CPU and memory allocation, disk usage,
the packs and their install status, configured mounts, network rules, the
sign-ins of the network services, and whether the sandbox is currently
running. This is a quick way to verify your configuration without opening
the TOML file.

Example output:

```
Path:     /Users/me/my-project
Status:   running
Image:    debian:stable-slim
CPUs:     4
Memory:   2.0 GB
Last run: 2 minutes ago

Sandbox:  /Users/me/my-project/.airlock/sandbox
Disk:     1.2 GB / 10.0 GB

Packs:
  debian 1 — config only
  claude 1 — installed
  rust 1 (toolchain = nightly) — installed

Mounts:
  claude-dir: /Users/me/.cache/airlock/packs/mounts/claude/claude → ~/.claude

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

## Removing sandbox state

The `airlock remove` command removes the `.airlock/` directory of the
current project. This includes the disk image, the CA certificate, the
logs and the local project config `.airlock/airlock.toml`:

```bash
airlock remove
```

airlock asks you to confirm before it removes anything. To skip the
confirmation prompt (useful in scripts), pass `--force`:

```bash
airlock remove --force
```

The short alias `airlock rm` also works.

After removal, running `airlock start` again creates a fresh sandbox from
scratch — new disk, new CA certificate, fresh image pull if needed. Removal does
not affect the project configuration files (`airlock.toml`,
`airlock.local.toml`). If the project has no other config, `airlock start`
opens the setup wizard again.

In your home directory, `.airlock/` also holds your user files: user
config, vault and sign-ins. There, `airlock rm` removes only
`.airlock/sandbox/`. If `.airlock` is a symbolic link, `airlock rm`
removes only the link.

## The `.airlock/` directory

Each project that uses airlock has a `.airlock/` directory at its root.
Sandbox state lives inside the project (rather than in a global location
like `~/.airlock/`) so that each checkout gets its own isolated sandbox.
Work on two branches in parallel, clone the same repo twice, or
`airlock rm` a feature branch's state — none of these touches anything
else. The directory contains a `.gitignore` with `*`, which excludes it
from version control automatically. Inside it, the `sandbox/`
subdirectory holds all runtime state. The local project config
`.airlock/airlock.toml` is next to it.

| File / Directory | Purpose                                                         |
|------------------|-----------------------------------------------------------------|
| `lock`           | PID lock file preventing concurrent sandbox instances           |
| `ca.json`        | Per-project CA certificate and private key for TLS interception |
| `disk.img`       | Sparse ext4 disk image for persistent VM storage                |
| `image`          | Link to the cached OCI image                                    |
| `cli.sock`       | Unix socket `airlock exec` connects to                          |
| `run.json`       | Metadata from the last run (timestamp, working directory)       |
| `installs.json`  | Install status of the packs                                     |
| `installs.log`   | Output of the pack installs                                     |
| `overlay/`       | Internal staging directory for file mounts                      |

The `tracing` log lives one level up, at `.airlock/airlock.log`.

You should never need to touch these files directly. If something goes wrong,
`airlock rm` and a fresh `airlock start` is the cleanest recovery path.
