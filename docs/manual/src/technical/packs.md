# Packs

This page tells how airlock loads, resolves and installs packs. See
[Authoring packs](../packs/authoring.md) for the file format, the script
API and the limits for pack authors.

## Sources and ids

A pack is one folder with `pack.toml`, an optional config and an
optional `setup.sh`. The `[packs]` key of a pack is its id:

| Source   | Id       | Files                                                  |
|----------|----------|--------------------------------------------------------|
| Built-in | `name`   | `packs/<name>@<version>/` in the repository, in the binary |
| User     | `~name`  | `<dir>/<name>/` in a `packs.dirs` directory            |
| Project  | `./path` | `<project root>/<path>/`                               |

Only built-in packs have versions. The host directories of the pack
mounts (`<data>/share/all/packs/`) are `<name>`, `local.<name>` and
`project.<hash>`. The hash comes from the resolved project root and the
path. airlock sorts packs by kind (distro, agent, tool, other), then by
name.

## Loading

- Built-in packs come from the binary.
- User packs load at the start of each command. The default
  `~/.airlock/packs` is optional. Each other `packs.dirs` entry must be
  an absolute path.
- Project packs load when the config resolves, from the project root of
  `airlock start`. Template builds refuse project packs. User-level
  config files refuse all packs.

Each pack from the disk gets a content hash: SHA-256 over its files in
name order.

## Config resolve

The resolve has three steps:

1. Select: read the config layers and load the pack files. No Lua runs.
2. Approve: check the `config.lua` of each user pack and project pack
   (see [Approval of config scripts](#approval-of-config-scripts)).
3. Evaluate: run `config.lua` on the host and merge the config.

The packs merge into the config layers in this order:

1. The legacy `presets` lists, below all files
2. For each file, lowest first: the configs of the packs that the file
   enables, then the file

Thus a pack overrides the files below its entry, and the file with the
entry overrides the pack. Two enabled packs that set one scalar path to
different values give a config error.

## Install state

`installs.json` in the [sandbox directory](./project-layout.md#sandbox-directory)
has the disk ID, the image ID and one record per pack:

| Status        | Meaning                                                 |
|---------------|---------------------------------------------------------|
| `unconfirmed` | The script exited with 0. The guest disk sync is not certain yet |
| `installed`   | The shutdown synced the guest disk                      |
| `failed`      | The script failed                                       |
| `kept`        | Removed from the config, but kept on the disk           |

Each record has a fingerprint, a SHA-256 of these values:

| Pack                 | Fingerprint input                                     |
|----------------------|-------------------------------------------------------|
| Built-in             | id, version, non-default args, env MAC (if set)       |
| User or project pack | id, non-default args, content hash, env MAC (if set)  |

A new arg with a default does not change the fingerprint. The env MAC
comes from the [setup values](#setup-values).

The plan compares the config with the records:

- A different disk or image resets the records.
- An `unconfirmed` or `failed` record with the same fingerprint is a
  retry with no question. If a session ran on the disk after the
  install, airlock asks the added-tools question.
- A different fingerprint is a changed pack. airlock asks the
  changed-packs question:

| Answer              | Plan                                                     |
|---------------------|----------------------------------------------------------|
| `Re-create sandbox` | New disk, all packs install (default, `--yes`)           |
| `Reinstall on top`  | The changed packs install again on the old disk          |
| `Keep old`          | The records get the new fingerprints and keep their status |

Without a terminal and without `--yes`, the start exits with code 2.

## Install boot

The install runs in its own VM boot before the session boot:

- One `spawn` per pack, in pack order. A failed pack does not stop the
  next pack. airlock saves the state after each pack.
- The idle timeout is 20 minutes.
- The network is open to public addresses, with no TLS interception.
  The host resolves each name and connects only to its public
  addresses. Thus DNS rebinding cannot reach a local address.
- No project share, mounts, secrets, middleware, ports, sockets,
  daemons or clipboard.
- The image must run as uid 0, with Alpine, Debian or Ubuntu. airlock
  checks this on the host before the boot.

The guest runs a wrapper, `lib.sh` and `setup.sh` as one `sh -c`
argument. The host values come only from the env, not from the script
text. Thus no value can inject code. The script output goes to
`installs.log` in the sandbox directory.

## Setup values

`config.lua` can set string values in `setup.env` for `setup.sh`.
airlock checks the keys and the sizes after the script ran (see
[Setup values](../packs/authoring.md#setup-values)). The size limits
come from Linux: one env string has 128 KiB or less, and one exec has
about 2 MiB for args and env. A changed value changes the fingerprint.

## Security measures

### Approval of config scripts

The `config.lua` of a user pack or a project pack runs on the host with
the full Lua standard library. The guest can change the project, and a
pull can change a pack. Thus a new or changed script needs approval
before it runs:

- The approval covers only the `config.lua` text. The other files only
  set values or run in the sandbox.
- The Lua entry point needs a token that only the approval code can
  make. Thus no code path can run `config.lua` without the check.
- The approved hash is of the text in memory that runs later. airlock
  does not read the pack files again in the run. Thus a file change
  after the approval has no effect.

| Mode    | Used by                                                       |
|---------|---------------------------------------------------------------|
| Prompt  | The first resolve of `airlock start` with a terminal          |
| Auto    | `airlock start --auto-approve-packs`. Prints the approved ids |
| Refuse  | `--yes`, no terminal, `airlock info`, `airlock config --resolved`, the later resolves of `start`, template builds |

The setup wizard records an approval for each pack that the user
selects, before any form Lua runs.

The store is `pack-approvals.json` in the data directory, with mode
0600 and an atomic write. The key is the source kind and the resolved
pack directory. A record has the SHA-256 of `config.lua` and a copy of
the text for the diff of the next question. The store keeps 128 records.
If the store is too large or not valid, all packs need approval again.

### Pack files

The guest can write to the project directory and make links:

- The loader reads only the known file names and ignores all other
  entries.
- Each file must be a regular file, not a link, with 256 KiB or less.
- The project path joins one name at a time, and each join refuses a
  link. The last name must be a real directory.
- `setup.sh` must fit in one exec argument. The loader checks this
  before the boot.

Pack text can have terminal escapes or bidi characters that hide code.
airlock shows these characters as visible escapes in the approval diff,
the change diff, the labels and the setup value keys. The progress lines
lose their control characters and have a length limit.

### Setup value secrecy

The setup values can be secrets:

- The type that holds them has no serialization. Its debug output and
  the spawn tracing show only the keys.
- airlock does not store the values. The records, the snapshots and the
  diff have only the keys and the MACs.
- The env MAC is HMAC-SHA256 of the sorted values. The key is random,
  in `<data>/pack-env.key` with mode 0600. The MAC shows a change, but
  does not give the value.
- The values go to the guest only in the env of the install exec. A
  script that prints a value puts it in `installs.log`.

### Change detection

`pack-snapshots.json` in the sandbox directory keeps a copy of each
installed pack, by fingerprint. The file has mode 0600 and a read limit
of 32 MiB. An entry has the id, the version, the args, the HMAC of each
setup value and, for a user pack or a project pack, the SHA-256 and the
text of each file.

The changed-packs question shows the diff between the snapshot and the
current pack. If the file is too large, airlock removes the texts and
keeps the hashes. `Keep old` gives the old copy to the new fingerprint,
so the next diff is against the real install.

Templates copy `pack-snapshots.json` with the disk. Thus a changed pack
in a sandbox from a template gets the changed-packs question.
