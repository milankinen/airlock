# Managing sandboxes

## View current sandbox

The `airlock info` command shows the sandbox and the configuration 
of the current project. It is a shortcut for `airlock sandboxes info` 
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
| `kept, removed from config`      | You selected "Continue with current sandbox" after you removed the pack |

## Listing sandboxes

The `airlock sandboxes` command (alias `airlock sandbox`) shows the
sandboxes in the airlock data directory (see [Where airlock keeps sandbox data](#where-airlock-keeps-sandbox-data)):

```bash
airlock sandboxes list            # All sandboxes in the data directory (alias: ls)
airlock sandboxes info k3x7q2ma   # Details of one sandbox (as `airlock info`)
```

The list shows the id, the status, the last run, the disk use and the
project directory of each sandbox. If the project directory no longer
exists, the list shows `(missing)` next to it.

The list does not show sandboxes in a project directory. `airlock info`
shows them when you run it in their project. If the project directory of
a sandbox is gone, `airlock sandboxes info` shows only the sandbox.

## Export the config

In case you want to view or export the sandbox config, use `airlock config`:

```bash
airlock config                        # Show current sandbox config
airlock sandboxes config k3x7q2ma     # Show specific sandbox config
airlock config > stored.toml          # Export current sandbox config to file
airlock config --resolved             # Show fully resolved sandbox config
airlock config --format json          # Show config in JSON format
```

## Removing sandbox

The `airlock remove` command (alias `airlock rm`) removes the sandbox of
the current project. It is a shortcut for `airlock sandboxes remove` without
an id. This removes the disk image, the CA certificate,
the install records, the local project config and the other runtime
state:

```bash
airlock remove
```

The `airlock sandboxes remove` command (alias `airlock sandboxes rm`) removes
any sandbox in the data directory by its id (see `airlock sandboxes list`).
Use it for the sandbox of a project directory that no longer exists:

```bash
airlock sandboxes remove k3x7q2ma
airlock sandboxes remove k3x7q2ma x5bq7d2c   # Remove more than one
```

Both commands ask you to confirm before they remove anything. To skip the
confirmation prompt (for example in scripts), pass `--force`. airlock
refuses to remove a running sandbox.

After removal, `airlock start` makes a new sandbox: a new disk, a new CA
certificate, and a new image pull if necessary. Removal does not change
the project configuration files (`airlock.toml`, `airlock.local.toml`).
If the project has no other config, `airlock start` opens the setup
wizard again.

## Where airlock keeps sandbox data

airlock stores the sandbox data in the application data directory:

- macOS: `~/Library/Application Support/airlock`
- Linux: `$XDG_DATA_HOME/airlock` or `~/.local/share/airlock`

To use a different directory, set `data_dir` in `~/.airlock/settings.toml`:

```toml
data_dir = "~/airlock-data"
```

On Linux, the path of the data directory must be 70 bytes or shorter.
The VM uses Unix sockets in the data directory, and the length of a
socket path has a limit. If the path is longer, `airlock start` stops
with an error.

The data directory also holds the image cache. Keep it on one file
system: airlock links each sandbox to its cached image with a hard link.

Older airlock versions kept the image cache in `~/.cache/airlock`. If the
data directory does not exist, airlock moves this cache to the data
directory when it starts. If the data directory is on a different file
system, airlock shows a warning and uses `~/.cache/airlock` as the data
directory.

## Sandboxes of older versions

Older airlock versions kept the sandbox in `.airlock/sandbox` in the
project directory. The sandbox guest can write this directory, so airlock
no longer uses it. All sandboxes live in the data directory.

When `airlock start` finds `.airlock/sandbox` in a project that has no
registered sandbox, it asks one question: `Continue` or `Abort`.
`Continue` moves the sandbox data to the data directory. A registered
sandbox wins. airlock leaves a stale `.airlock/sandbox` next to it
untouched.

* With `--yes`, airlock moves the data without a question.
* Without a terminal and without `--yes`, `airlock start` stops with a
  hint. Run it in a terminal, or pass `--yes`.
* If `.airlock` holds other files than `.gitignore`, `airlock.log`,
  `sandbox` and the local config, airlock refuses the move. Move or
  remove the files that the message lists.

The move takes only these items:

* the disk (`disk.img`) and its id (`disk.id`)
* the install records. airlock marks them so that the question "a session
  ran on this disk" stays on.
* the CA (`ca.json`)

airlock does not take the other files. This includes `run.json`, logs
and sockets. airlock removes `.airlock/` after the move.

The image link comes from the image cache. If the cache has the image,
airlock links it at once. Else the next start compares the old image with
the configured image. A different image gives the image change question.
If the old image is unknown, each image counts as a change. Then `--yes`
stops the start with exit code 2, and the disk stays.

If airlock took a disk, the first start after the move asks the config
change question. The stored config of the old sandbox is not taken.
Without a terminal, or with `--yes`, and without
`--allow-configuration-changes`, the start exits with code 2.

A sandbox in the data directory that an older version made can have no
stored config. It gets the same question one time.
