# Starting a sandbox

The `airlock start` command boots a sandbox VM in the current project
directory. If no `airlock.toml` exists yet, airlock will offer to create one
with sensible defaults.

```bash
airlock start
```

On first run, airlock pulls the configured OCI image (Alpine by default),
generates a per-project CA certificate, and boots the VM. Subsequent runs
reuse the cached image and existing sandbox state, so startup is near-instant.

## Configuration basics

Sandbox configuration lives in two files at the project root:

- `airlock.toml` — the main config, meant to be committed to version control
- `airlock.local.toml` — local overrides, typically gitignored

A minimal config that uses Ubuntu instead of the default Alpine:

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

The command runs inside the container and airlock exits when it finishes,
returning the command's exit code.

## Login shell

The `--login` flag (or `-l`) starts a login shell that sources `/etc/profile`
and `~/.profile` before running the command. This is useful when the image
sets up environment variables or PATH entries through profile scripts:

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

airlock pulls OCI images and caches them locally at `~/.cache/airlock/images/`.
On subsequent runs, the cached image is reused unless the remote tag has
changed.

By default, airlock tries the local Docker daemon first and falls back to
pulling from the OCI registry. This can be controlled with the `resolution`
field in the config:

```toml
# Always pull from the registry, skip Docker
[vm.image]
name = "ubuntu:24.04"
resolution = "registry"
```

The three resolution modes are:

- `auto` — try Docker daemon first, fall back to the registry (default)
- `docker` — use the local Docker daemon only; fail if the image isn't found
- `registry` — always pull from the registry, ignore Docker

For private registries, airlock supports standard OCI registry authentication
through the Docker credential store (keychain on macOS, credential helpers on
Linux). If you can `docker pull` an image, airlock can pull it too.

For development registries served over plain HTTP, set `insecure = true`:

```toml
[vm.image]
name = "localhost:5005/my-dev-image:latest"
resolution = "registry"
insecure = true
```

## Verbose output

The `--verbose` flag (or `-v`) shows mounts and network rules during startup,
which is helpful for verifying your configuration:

```bash
airlock start --verbose
```

## Supervisor logging

For debugging VM-level issues, you can increase the supervisor log verbosity
with `--log-level`:

```bash
airlock start --log-level debug
```

Log levels are `trace`, `debug`, `info` (default), `warn`, and `error`.
Supervisor logs are also written to `.airlock/sandbox/run.log`.

## Quiet mode

The `-q` / `--quiet` flag suppresses airlock's own output, which is useful
when running airlock in scripts or CI pipelines where only the command output
matters:

```bash
airlock start -q -- echo "only this is printed"
```

