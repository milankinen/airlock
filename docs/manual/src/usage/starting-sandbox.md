# Starting a sandbox

The `airlock start` command starts a sandbox VM in the current project
directory.

```bash
airlock start
```

On the first start, airlock shows a setup wizard. The wizard lets you
customize the sandbox with [packs](../packs.md). To keep the config in
version control, select `Create airlock.toml` in the wizard. Then airlock
creates `airlock.toml`, which you can commit to version control.

## Sandbox changes

When the image or the [packs](../packs.md) of an existing sandbox change,
`airlock start` asks what to do. The default answer re-creates the
sandbox: airlock removes the sandbox disk and installs all packs into a
new disk.

For an added pack, you can also select "install anyways". Then airlock
installs the pack into the existing disk. Code that is already on the
disk can run during the install, with an open network. Use it only if you
trust the sandbox contents.

Pass `--yes` (`-y`) to select the default answer for all questions, for
example in scripts.

## Configuration basics

If you selected `Create airlock.toml` in the wizard, airlock creates
`airlock.toml` at the project root. You can put it in version control.

Each user can also configure airlock with `airlock.local.toml`. The values
in this file override the values from `airlock.toml`. Do not commit this
file.

A minimal config that uses Ubuntu:

```toml
[vm]
image = "ubuntu:24.04"
```

All configuration options are covered in the [Configuration](../configuration.md)
chapter. For now, the most important thing to know is that the `[vm]` section
controls the image and resource allocation.

## Running commands

By default, `airlock start` opens an interactive shell inside the VM. You can
also pass a command after `--` to run it directly:

```bash
airlock start -- python3 -c "print('hello from the sandbox')"
```

The command runs inside the container. When it finishes, airlock exits
with the command's exit code.

## Login shell

The `--login` flag (or `-l`) starts a login shell that sources `/etc/profile`
and `~/.profile` before running the command. This is useful when the image
defines environment variables or PATH entries in profile scripts:

```bash
airlock start --login
```

## Project directory and working directory

airlock automatically mounts the host project directory into the VM at the
same path. The working directory inside the container defaults to the host's
current directory, so files are right where you'd expect them.

To override the working directory inside the sandbox, use `--sandbox-cwd`:

```bash
airlock start --sandbox-cwd /tmp
```

## Image pulling and caching

airlock pulls OCI images and caches them locally under `oci/` in the
airlock data directory (see [Managing sandboxes](./managing-sandbox.md#where-airlock-keeps-sandbox-data)).
Image metadata lives in `oci/images/<digest>` (one JSON file per image) and
the underlying layer trees in a shared `oci/layers/` cache that
deduplicates across images. On subsequent runs, airlock reuses the cached
image without contacting the registry at all — a tag that has since moved
goes unnoticed. Set `pull-policy = "if-changed"` (see [VM
options](../configuration/vm.md)) to check for a newer image on every start.

By default, airlock searches Docker first, then Podman, and then pulls from
the OCI registry. Control this with the `resolution` field in the config:

```toml
# Always pull from the registry, skip Docker
[vm.image]
name = "ubuntu:24.04"
resolution = "registry"
```

The resolution modes are:

- `auto` — search Docker, then Podman, then the registry (default)
- `docker` — use local Docker images only, and fail if the image isn't found
- `podman` — use local Podman images only, and fail if the image isn't found
- `registry` — always pull from the registry, ignore local images

For private registries, airlock prompts for a username and password the first
time it sees a `401 Unauthorized` response. When the vault is enabled (see
[Secrets management](../secrets.md)), airlock saves credentials keyed by
registry host so subsequent pulls reuse them. With the vault disabled,
airlock prompts on every pull that requires auth.

For development registries served over plain HTTP, set `insecure = true`:

```toml
[vm.image]
name = "localhost:5005/my-dev-image:latest"
resolution = "registry"
insecure = true
```

## Network policy override

The `--network` flag replaces the `[network] policy` value from the config
for a single run. It accepts the same four values as the config field:
`allow-always`, `deny-always`, `allow-by-default`, and `deny-by-default`.

```bash
airlock start --network=allow-always -- ./init.sh
```

The flag overrides only the policy — rules, middleware, port forwards, and
socket forwards from the config still apply. airlock writes nothing back to
`airlock.toml`, and the next `airlock start` without the flag uses the
configured policy again. With `--verbose`, the network rules summary shows
the effective policy. airlock also records the override in
`airlock.log` in the sandbox directory.

This is mainly useful for one-off bootstrap commands that need broader
network access than the day-to-day session. See
[Open-network bootstrap](../tips/init-with-open-network.md)
for a worked example.

## Monitor dashboard

Pass `--monitor` (`-m`) to open a tabbed TUI control panel alongside the
sandbox shell, with live network, CPU, and memory views. See the
[Monitor dashboard](./monitor.md) chapter for details.

## Verbose output

The `--verbose` flag (or `-v`) shows mounts and network rules during startup,
which is helpful for verifying your configuration:

```bash
airlock start --verbose
```

## Supervisor logging

For debugging VM-level issues, you can increase the VM log verbosity
with `--log-level`:

```bash
airlock start --log-level debug
```

Log levels are `trace`, `debug`, `info` (default), `warn`, and `error`.
airlock writes logs to `airlock.log` in the sandbox directory.
`airlock sandboxes info` shows the sandbox directory.

## Quiet mode

The `-q` / `--quiet` flag suppresses airlock's own output. This is useful
in scripts or CI pipelines where only the command output matters:

```bash
airlock start -q -- echo "only this is printed"
```

