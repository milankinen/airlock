# Authoring packs

You can write your own packs. A pack that you write has the same format
as a built-in pack. Put it in one of these places:

- A user pack directory, for your personal packs. The default directory
  is `~/.airlock/packs`. See [Pack sources](../packs.md#pack-sources).
- A directory in the project, for a pack that the team shares through
  the repository.

## Directory layout

A pack is one directory with these files:

```
my-tools/
  pack.toml        # label, description, kind and args (required)
  config.toml      # pack config (optional)
  setup.sh         # install script for the sandbox (optional)
```

## Pack definition

`pack.toml` defines the pack. It gives the label and the description that
the setup wizard shows, and the args that the user can set in the
`[packs]` entry of the pack:

```toml
label = "My tools"
description = "Shell tools and an editor"
kind = "tool"

[[args]]
key = "editor"
type = "choice"
description = "editor"
default = "vim"
values = ["vim", "nano"]

[[args]]
key = "allow-github"
type = "bool"
description = "allow GitHub"
default = true
```

| Field         | Description                                                    |
|---------------|----------------------------------------------------------------|
| `label`       | Name that the setup wizard and the messages show (required)    |
| `description` | One-line description. It cannot be empty (required)            |
| `kind`        | `distro`, `agent`, `tool` or `other`. Default `other`          |
| `[[args]]`    | Pack options. Each entry is one arg                            |

Each `[[args]]` entry has these fields:

| Field         | Description                                                    |
|---------------|----------------------------------------------------------------|
| `key`         | Arg name. Lowercase letters, digits and `-`, unique. Cannot be `version`, `enabled` or `args` (required) |
| `type`        | `bool` or `choice` (required)                                  |
| `description` | Short text that the setup wizard shows (required)              |
| `default`     | Default value. A boolean for `bool`, one of `values` for `choice` (required) |
| `values`      | Only for `choice` (required). The valid values, not empty, no duplicates |
| `other`       | Only for `choice`. If `true`, any non-empty string is also valid. Default `false` |

An unknown field is an error.

## Configuration

The config of the pack adds normal airlock configuration to the
sandbox, for example network rules, environment variables and mounts.
It uses the same keys as the [configuration files](../configuration.md)
except it cannot set `packs` or `presets`. For example:

```toml
[env]
EDITOR = "vim"
```

The config file can be `config.toml`, `config.json`, `config.yaml` or
`config.yml`. A pack can also have a
[`config.lua` script](#dynamic-lua-configuration) for a dynamic
configuration. A pack can have only one config file.

### Dynamic Lua configuration

Use `config.lua` when the config depends on the args or on the host.
The script runs on the host each time airlock resolves the config, for
example on `airlock start` and `airlock info`. It has the full Lua
standard library. Thus it can read host files and run host commands.

In addition to the standard Lua imports, the script gets these inputs:

| Input            | Description                                               |
|------------------|-----------------------------------------------------------|
| `pack.name`      | Pack id, for example `~my-tools` or `./packs/my-tools`   |
| `pack.args`      | Arg values, with the defaults. A `bool` arg is a boolean, a `choice` arg is a string |
| `pack.directory` | Host directory of the pack. airlock makes it before the script runs. All projects with the pack share it. Use it for the host side of mounts |
| `fail(message)`  | Function that stops with the config error `pack <name>: <message>` |

The script writes its outputs to these tables. Both are empty when the
script starts:

| Output      | Description                                                    |
|-------------|----------------------------------------------------------------|
| `config`    | Config values of the pack                                      |
| `setup.env` | Values for `setup.sh` (see [Setup values](#setup-values))      |

Use `fail` for arg values that the pack cannot use. Then the user sees
the error before the sandbox starts.

Many built-in packs of airlock use Lua configuration. See the
[airlock repository](https://github.com/milankinen/airlock/tree/main/packs)
for examples.

## Tooling setup

`setup.sh` installs software into the sandbox. Without it, the pack adds
only config. The script runs:

- In the guest, as `root`, with `/` as the working directory.
- In a separate install boot before the session, when airlock creates
  the sandbox disk or when a pack is new or changed.
- With an open network to public addresses, with no TLS interception.
- With no project directory, no mounts, no secrets and no daemons. Use
  [setup values](#setup-values) to give host values to the script.

Currently, airlock setup scripts support Debian, Ubuntu and Alpine distros.

### Variables

The script gets these variables:

| Variable                  | Description                                      |
|---------------------------|--------------------------------------------------|
| `AIRLOCK_PACK_ARG_<KEY>`  | Arg value. The key is in upper case, with `-` changed to `_`. A `bool` arg is `true` or `false` |
| `AIRLOCK_PACK_ENV_<KEY>`  | Setup value from `config.lua` (see [Setup values](#setup-values)) |
| `AIRLOCK_PACK_ID`         | Pack id                                          |
| `AIRLOCK_PACK_API`        | Version of the script API, now `1`               |
| `DISTRO`                  | `alpine` or `debian`. Ubuntu is `debian`         |
| `ARCH`                    | `x86_64` or `aarch64`                            |
| `LIBC`                    | `glibc` or `musl`                                |
| `AIRLOCK_PACK_TMPDIR`     | Temporary directory on the sandbox disk. airlock removes it at the end. `TMPDIR` points to it |

For example, the arg `allow-github` is `AIRLOCK_PACK_ARG_ALLOW_GITHUB`.

### Functions

The script can use these functions:

| Function                                  | Description                       |
|-------------------------------------------|-----------------------------------|
| `airlock_steps <n>`                       | Set the number of steps (1 to 99) to show in the sandbox startup. Call it one time, before the first step |
| `airlock_status <text>`                   | Start the next step and show `<text>` in the sandbox startup |
| `airlock_log <text>`                      | Write `<text>` to the install log |
| `airlock_fail <code> <text>`              | Write the error to the log and exit with `<code>` |
| `airlock_detect_distro`                   | Print `alpine` or `debian`        |
| `airlock_detect_arch`                     | Print `x86_64` or `aarch64`       |
| `airlock_detect_libc`                     | Print `glibc` or `musl`           |
| `airlock_pkg_is_installed <pkg>`          | Return 0 if the package is installed |
| `airlock_pkg_available <pkg>`             | Return 0 if the package index has the package. Alpine always returns 0 |
| `airlock_pkg_install <pkg>...`            | Install the packages that are not installed yet, with `apk` or `apt-get` |
| `airlock_fetch <url> <file>`              | Download a file over HTTPS        |

`airlock_status` gives the step count only if you call it one time for
each step. Call it also for a step that has no work to do.

Names that start with `airlock_internal__` or `_` are internal. They can
change. Do not use them. Do not define names that start with `airlock_`
or `_airlock_`.

### Exit codes

Use these exit codes for errors. airlock shows a message for each code:

| Code | Meaning                                                        |
|------|----------------------------------------------------------------|
| `10` | The distro or the CPU architecture is not supported            |
| `11` | A package install failed                                       |
| `12` | A download or a vendor installer failed                        |
| `13` | The script cannot use an arg value                             |

Other non-zero codes are also failures. A failed pack does not stop the
installs of the other packs. The next `airlock start` runs the failed
pack again.

### Idempotency

Write the script so that you can run it again and again. A new run after
a complete, interrupted or failed run must do only the work that is
still missing. airlock runs the script again after a failure, and the
"Reinstall on top" answer runs it on a disk that has the old install.
For example, `airlock_pkg_install` skips the installed packages.

## Setup values

`config.lua` can give values to `setup.sh` through `setup.env`:

```lua
setup.env.API_TOKEN = os.getenv("MY_TOOLS_TOKEN") or ""
```

The script gets each value as `AIRLOCK_PACK_ENV_<KEY>`, for example
`AIRLOCK_PACK_ENV_API_TOKEN`. Rules:

- A key starts with an uppercase letter and has only uppercase letters,
  digits and `_`.
- A value is a string with no NUL character.
- A pack can have 64 keys.
- One variable `AIRLOCK_PACK_ENV_<KEY>=<value>` can have 128 KiB.
- All variables of one pack can have 1 MiB.
- A pack that sets `setup.env` must have a `setup.sh`.

airlock handles the values as secrets. It does not show, log or store
them. It shows only the names of the changed keys. The script output
goes to `installs.log`. Thus do not print the values in the script.

A changed value is a change of the pack. Then `airlock start` asks what
to do with the sandbox disk (see [Approval and changes](#approval-and-changes)).

## Approval and changes

Built-in packs need no approval. For a user pack or a project pack,
airlock keeps an approval of the `config.lua` text:

- A pack with a new or changed `config.lua` needs approval before the
  script runs. This applies also to your own changes.
- A pack with no `config.lua`, or with a static config file, needs no
  approval.
- A pack that you select in the setup wizard needs no other approval.
- `airlock info` and `airlock config --resolved` do not ask. They stop
  with an error until `airlock start` gets the approval.

A change of any pack file, or of a setup value, makes the pack
"changed" on the sandbox disk. Then `airlock start` asks what to do. You
can re-create the sandbox, install the pack again on top of the disk, or
keep the old install.

## Restrictions

- airlock reads only `pack.toml`, the config file and `setup.sh`. It
  ignores all other files and directories, for example a `README.md`.
- Each file must be a regular file. Symlinks are not allowed.
- Files can be max 256 KiB. `setup.sh` can be max 100 KiB.
- `setup.sh` must be a POSIX `sh` script. The sandbox image must run as
  `root`.
- Pack directory name can contain only lowercase letters, digits
  and `-`, and starts with a letter.
- A project pack must be inside the project root. Its path cannot
  contain symlinks.
- A user pack directory can have 64 packs.
- A config can name 32 project packs.
- A project pack path (`./path`) can be max 256 characters.
- The same user pack name in two user pack directories is an error.

## Example: editor pack

This example pack installs Vim or Emacs into the sandbox. It also copies
your editor config from the host into the sandbox.

`pack.toml`:

```toml
label = "Editor"
description = "Vim or Emacs with your own config"
kind = "tool"

[[args]]
key = "editor"
type = "choice"
description = "editor"
default = "vim"
values = ["vim", "emacs"]
```

`config.lua`:

```lua
-- The editor of the arg is the default editor in the sandbox.
local editors = {
  vim = { pkg = "vim", file = ".vimrc" },
  emacs = { pkg = "emacs-nox", file = ".emacs.d/init.el" },
}
local editor = editors[pack.args["editor"]]
config.env = { EDITOR = pack.args["editor"] }

-- Give the package, the config file name and the config of the host
-- user to the setup script.
setup.env.EDITOR_PKG = editor.pkg
setup.env.EDITOR_FILE = editor.file
local file = io.open(os.getenv("HOME") .. "/" .. editor.file, "r")
if file then
  setup.env.EDITOR_CONFIG = file:read("a")
  file:close()
end
```

`setup.sh`:

```sh
# Install the editor and write the editor config of the host user.
airlock_steps 2

airlock_status "installing $AIRLOCK_PACK_ENV_EDITOR_PKG"
airlock_pkg_install "$AIRLOCK_PACK_ENV_EDITOR_PKG"

airlock_status "writing ~/$AIRLOCK_PACK_ENV_EDITOR_FILE"
if [ -n "${AIRLOCK_PACK_ENV_EDITOR_CONFIG:-}" ]; then
    mkdir -p "$(dirname "$HOME/$AIRLOCK_PACK_ENV_EDITOR_FILE")"
    printf '%s' "$AIRLOCK_PACK_ENV_EDITOR_CONFIG" >"$HOME/$AIRLOCK_PACK_ENV_EDITOR_FILE"
else
    airlock_log "no editor config on the host"
fi
```

> [!TIP]
> A read-only [mount](../configuration/mounts.md) of the config file is
> another way to do this. The mount shows the changes of the host file
> immediately, with no new install.

To use the pack as a user pack, put the files in `~/.airlock/packs/editor/`.
The setup wizard shows the pack as `Editor (local)`. You can also add it
to the config yourself, for example in `airlock.local.toml`:

```toml
[packs]
"~editor" = { args = { editor = "emacs" } }
```

To use the pack as a project pack, put the files in `packs/editor/` in
the project. Then add it to `airlock.toml` and commit both:

```toml
[packs]
"./packs/editor" = {}
```

## Tips

- See the [built-in packs](https://github.com/milankinen/airlock/tree/main/packs)
  for complete examples of `config.lua` and `setup.sh`.
- Read `installs.log` in the sandbox directory when a setup script
  fails.
- When you change `setup.sh`, select "Reinstall on top" to test the
  change on the existing disk. Select "Re-create sandbox" to test a
  clean install.
- Keep host access in `config.lua` small. The approval question shows
  the script, and a short script is easy to check.
