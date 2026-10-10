# Packs

## Format

Each pack is a folder `packs/<name>@<version>/` in the repository. The
airlock binary contains all packs. A folder has these files:

- `pack.toml` (required): `label`, `description`, `kind` and `[[args]]`.
  `kind` is `distro`, `agent` or `tool`. An arg is `bool` or `choice`,
  with a typed default. A `choice` arg with `other = true` also accepts
  any non-empty string.
- A config (optional): a static `config.{toml,json,yaml,yml}` or a
  `config.lua`. A pack config cannot set `packs` or `presets`.
- `setup.sh` (optional): a POSIX `sh` script that installs the software.
  It must be idempotent. Without it, the pack is "config only".

Packs are ordered by kind (distro, agent, tool), then by name. The
installs, the wizard and `airlock show` use this order.

## Lua configs

`config.lua` runs on the host each time airlock resolves the config
(`airlock start`, `airlock show`). Pack code ships with airlock, so it is
trusted. It gets the full Lua standard library, unlike the sandboxed
network middleware. Globals:

- `config`: the table that the script fills
- `pack.name`, `pack.version`, `pack.args` (with defaults)
- `pack.directory`: `<data>/share/all/packs/<name>/`, the host
  side of the pack mounts. All projects with the pack share it.
- `fail(msg)`: stops with the config error `pack <name>: <msg>`

## Config merge

The packs merge into the config layers in this order:

1. The legacy `presets` lists, below all files
2. For each file, lowest first: the configs of the packs that this file
   enables (its entry is the highest entry of the pack), then the file

Thus a pack overrides the files below its entry, and the file with the
entry overrides the pack. The `[packs]` entries merge per pack. Per key,
the highest file wins. Args come only from files whose entry has the
final version or no version.

The configs of the enabled packs must not set one scalar path to
different values. Objects merge per key and arrays concatenate, as in
config files. A conflict is a config error.

## Install state

`.airlock/sandbox/installs.json` holds the install records of the disk:
the disk ID (random for each new disk), the image ID, and one record per
pack. A record has a status, a fingerprint and a time:

| Status        | Meaning                                                |
|---------------|--------------------------------------------------------|
| `unconfirmed` | The script exited with 0. The disk sync is not confirmed yet |
| `installed`   | The shutdown confirmed the guest disk sync             |
| `failed`      | The script failed                                      |
| `kept`        | Removed from the config, but kept on the disk          |

The fingerprint is SHA-256 of the pack name, the version and the arg
values that differ from the defaults. A new arg with a default thus does
not change the fingerprint. The scripts are not part of it: a pack
version never changes its files.

The plan compares the config with the records. A different fingerprint
is a changed pack. An `unconfirmed` or `failed` record with the same
fingerprint is a retry, which runs without a question. If a normal
session ran on the disk after that install, the retry asks the
added-tools question, because the session can have left code on the
disk.

## Install boot

The install runs in its own VM boot before the session boot:

- One `spawn` per pack, in pack order. A failed pack does not stop the
  next pack. airlock saves the state after each pack.
- The idle timeout is 20 minutes.
- The network policy is `allow-always`, with passthrough (no TLS
  interception). Only public addresses are allowed. airlock denies
  `localhost`, `*.localhost` and non-public IP literals. The host
  resolves each name and connects only to its public addresses. Thus
  DNS rebinding cannot reach a local address.
- No project share, mounts, secrets, masked env, middleware, services,
  ports, sockets, daemons, masks or clipboard.

Before the install, airlock checks the image layers on the host. The
image must run as uid 0 and have an `/etc/os-release` with `alpine`,
`debian` or `ubuntu` in `ID` or `ID_LIKE`.

## Setup scripts

The install process runs a wrapper, then the shared `lib.sh`, then
`setup.sh`. The args come only from the env:
`AIRLOCK_PACK_ARG_<KEY>` (upper case, `-` changed to `_`, bools as
`true`/`false`). A script with host values in it is not necessary, so
there is no injection risk.

Progress protocol: stdout (fd 3 in the script) carries only
`steps <n>` and `status <text>` lines. airlock removes control
characters and limits the length. stderr goes to
`.airlock/sandbox/installs.log`, with a `[<pack>]` prefix on each line.

`lib.sh` detects the distro, the CPU and the libc. It installs packages
with apk or apt and downloads with https and TLS 1.2 or later. Exit
codes: 10 unsupported distro or CPU, 11 package error, 12 download error,
13 bad arg value.

The scripts use the official vendor installers or release builds.
Agents go to `/usr/local/bin`, which the sandbox user cannot change. The
pack configs turn off self-updates.

The `acp` arg of `claude` and `codex` compiles the npm ACP adapter into
one executable with Bun. airlock removes Bun after the build. The
adapter runs the agent binary of the pack. With `acp = false`, the
adapter command is a stub that exits with an error.
