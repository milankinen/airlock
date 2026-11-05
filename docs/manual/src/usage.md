# Installation and usage

## Install

To install airlock on macOS or Linux, run:

```bash
curl -fsSL https://github.com/milankinen/airlock/releases/latest/download/install.sh | sh
```

This installs the **bundled** variant, which includes the VM kernel and
initramfs — everything you need in a single binary. The installer places
the binary in `~/.local/bin` by default. Check that it is on your `PATH`:

```bash
export PATH="$PATH:$HOME/.local/bin"
```

If you prefer a smaller binary and want to supply your own kernel and
initramfs, install the **distroless** variant instead:

```bash
curl -fsSL https://github.com/milankinen/airlock/releases/latest/download/install.sh | sh -s -- --distroless
```

Set the `AIRLOCK_INSTALL_DIR` environment variable to change the install
directory. Set `AIRLOCK_VERSION` to pin a specific version.

## Update

To update airlock to the latest release, run:

```bash
airlock self-update           # Install the latest release
airlock self-update --check   # Only check for a newer release
```

The update keeps your variant (bundled or distroless).

## Shell completions

Airlock can complete commands, options, template names and sandbox ids
in bash, zsh and fish. To add the completions to the startup file of
your shell, run:

```bash
airlock completions install        # The shell in $SHELL
airlock completions install zsh    # A specific shell
```

The command adds one line to the startup file:

| Shell | Startup file                                 |
|-------|----------------------------------------------|
| bash  | `~/.bashrc`                                  |
| zsh   | `$ZDOTDIR/.zshrc`, or `~/.zshrc`             |
| fish  | `$XDG_CONFIG_HOME/fish/completions/airlock.fish`, or `~/.config/fish/completions/airlock.fish` |

The install is idempotent. Open a new shell to use the completions.

To set up the completions yourself, load the completion script in the
startup file:

```bash
eval "$(airlock completions script bash)"    # ~/.bashrc
eval "$(airlock completions script zsh)"     # ~/.zshrc
airlock completions script fish | source     # fish
```

## Quick overview

After installation, the basic workflow is:

```bash
airlock start                       # Start a sandbox VM and open a shell
airlock start -- ls /usr            # Run a one-off command in the VM
airlock exec bash                   # Attach to a running VM
airlock info                        # Show sandbox status and config
airlock remove                      # Remove sandbox state
airlock sandboxes list              # List sandboxes
airlock self-update                 # Update airlock to the latest release
```

The first time you run `airlock start` in a project directory, a setup
wizard asks for the base image, the coding agents and the tools. After
that, each subsequent `start` reuses the existing configuration and
sandbox state.

The following sections cover each of these commands in detail.
