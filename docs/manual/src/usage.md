# Installation and usage

## Install

To install airlock on macOS or Linux, run:

```bash
curl -fsSL https://github.com/milankinen/airlock/releases/latest/download/install.sh | sh
```

This installs the **bundled** variant, which includes the VM kernel and
initramfs — everything you need in a single binary. The binary is placed in
`~/.local/bin` by default. Make sure it's on your `PATH`:

```bash
export PATH="$PATH:$HOME/.local/bin"
```

If you prefer a smaller binary and want to supply your own kernel and
initramfs, install the **distroless** variant instead:

```bash
curl -fsSL https://github.com/milankinen/airlock/releases/latest/download/install.sh | sh -s -- --distroless
```

The install directory can be changed with the `AIRLOCK_INSTALL_DIR` environment
variable, and a specific version can be pinned with `AIRLOCK_VERSION`.

## Quick overview

Once installed, the basic workflow is straightforward:

```bash
airlock start                       # Boot a sandbox VM and open a shell
airlock start -- ls /usr            # Run a one-off command in the VM
airlock exec bash                   # Attach to a running VM
airlock show                        # Show sandbox status and config
airlock remove                      # Remove sandbox state
```

The first time you run `airlock start` in a project directory, airlock will
ask whether to create a default `airlock.toml`. After that, each subsequent
`start` reuses the existing configuration and sandbox state.

The following sections cover each of these commands in detail.
