# Mounts

airlock can share host files and directories into the VM using VirtioFS
mounts. The project directory is always mounted automatically at its exact
host path, but you can add additional mounts for things like SSH config,
credential files, or shared caches.

## Defining a mount

Each mount is a named entry under `[mounts]`:

```toml
[mounts.ssh-config]
source = "~/.ssh/config"
target = "~/.ssh/config"
read_only = true
```

The `source` is a path on the host and `target` is the path inside the
container. Both support `~` expansion to the respective home directory.

## Read-only mounts

Setting `read_only = true` prevents the container from writing to the mount.
This is a good default for configuration files and credentials that the
sandbox should be able to read but not modify:

```toml
[mounts.aws-credentials]
source = "~/.aws/credentials"
target = "~/.aws/credentials"
read_only = true
```

## Handling missing sources

By default, airlock fails if a mount's source path doesn't exist on the host.
The `missing` field controls this behaviour:

```toml
[mounts.optional-config]
source = "~/.config/myapp/settings.json"
target = "~/.config/myapp/settings.json"
missing = "warn"
```

The available options are:

- `fail` (default) — stop with an error if the source doesn't exist
- `warn` — skip the mount and print a warning
- `ignore` — skip the mount silently
- `create-dir` — create the source as a directory and mount it
- `create-file` — create the source as a file and mount it

When using `create-file`, you can provide initial content for the new file:

```toml
[mounts.git-config]
source = "~/.airlock/gitconfig"
target = "~/.gitconfig"
missing = "create-file"
file_content = "[user]\n\tname = Dev\n\temail = dev@example.com\n"
```

## Disabling a mount

A mount can be temporarily disabled without removing it from the config. This
is useful when a preset defines a mount that you don't need:

```toml
[mounts.ssh-config]
enabled = false
source = "~/.ssh/config"
target = "~/.ssh/config"
```
