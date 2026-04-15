# Managing project data

## Viewing sandbox status

The `airlock show` command displays the current sandbox configuration and
status for the project:

```bash
airlock show
```

The output includes the image name, CPU and memory allocation, disk usage,
configured mounts, network rules, and whether the sandbox is currently
running. This is a quick way to verify your configuration without opening
the TOML file.

Example output:

```
Path:     /Users/me/my-project
Status:   running
Image:    ubuntu:24.04
CPUs:     4
Memory:   2.0 GB
Last run: 2 minutes ago

Sandbox:  /Users/me/my-project/.airlock/sandbox
Disk:     1.2 GB / 10.0 GB

Mounts:
  ssh-config: ~/.ssh/config → ~/.ssh/config

Network rules (default: deny):
  my-api: allow 2, 1 middleware
```

## Removing sandbox state

The `airlock rm` command removes the sandbox state for the current project.
This deletes the `.airlock/sandbox/` directory, which includes the disk
image, CA certificate, overlay data, and run logs:

```bash
airlock rm
```

You'll be asked to confirm before anything is deleted. To skip the
confirmation prompt (useful in scripts), pass `--force`:

```bash
airlock rm --force
```

After removal, running `airlock start` again creates a fresh sandbox from
scratch — new disk, new CA cert, fresh image pull if needed. The project
configuration files (`airlock.toml`, `airlock.local.toml`) are not affected.

## The `.airlock/` directory

Each project that uses airlock has a `.airlock/` directory at its root. This
directory is automatically excluded from version control (it contains a
`.gitignore` with `*`). Inside it, the `sandbox/` subdirectory holds all
runtime state:

| File / Directory | Purpose |
|-----------------|---------|
| `lock`          | PID lock file preventing concurrent sandbox instances |
| `ca.json`       | Per-project CA certificate and private key for TLS interception |
| `disk.img`      | Sparse ext4 disk image for persistent VM storage |
| `image`         | Reference to the cached OCI image |
| `run.json`      | Metadata from the last run (timestamp, working directory) |
| `run.log`       | Supervisor log output |
| `overlay/`      | Overlayfs upper directory for writable mount layers |
| `ca/`           | CA certificate overlay injected into the VM |

You should never need to touch these files directly. If something goes wrong,
`airlock rm` and a fresh `airlock start` is the cleanest recovery path.
