# Remove project sandboxes

The audit of `io-hardening` found host escapes that exist only when the
sandbox dir is `<project>/.airlock/sandbox`. The guest writes that dir.
All sandboxes now live in `<data>/boxes/<id>`.

## Reason

* vsock symlink: the guest gets any host Unix socket.
* Hard links of read-only file mounts: the guest can write those host
  files.
* The `image` hard link: the guest can change the shared image cache.

## Design

* One location. `security.sandbox_type`, the `keep-in-project` marker and
  the project branches of `start`, `rm`, `info`, `exec` and `sandboxes`
  are removed. An old `settings.toml` with the key still loads.
* The guest mask of `.airlock` is removed. It was the last boot param.
  airlockd is in the embedded initramfs, so both sides changed together.
* Import of an old `.airlock/sandbox` at `airlock start`:
  * One question: Continue or Abort. `--yes` moves. No terminal and no
    `--yes` gives an error and a hint.
  * airlock takes only `disk.id`, `disk.img`, `installs.json`, `ca.json`
    and the image digest, as regular files without a link follow, with
    the user as owner. `image` may have more than one link, because
    airlock reads only a digest.
  * airlock drops `run.json` and the `image` file. It links the image
    from the cache. Else the marker `image.imported` keeps the old
    digest. The next start asks the image change question if the digest
    is different or unknown. An unknown old image with a disk gives
    exit 2, also with `--yes`.
  * airlock marks the install records (`ran_session = true`, `failed`
    for records that are not `installed` or `kept`).
  * After an error, airlock removes the new box and puts back moved
    files. If the disk cannot go back, airlock keeps the new box.
* Config change check fails closed. A sandbox with a disk and no stored
  config gets the question. Without a terminal, or with `--yes`, and
  without `--allow-configuration-changes`, the start gives exit 2. An import without a
  disk gets no question.

## Decisions

* Remove the project location. Do not harden it. The guest must not write
  sandbox data.
* Do not trust the old dir. A cloned repo can ship `.airlock/sandbox`.
* Take a short allowlist. Drop what the guest can use to attack the host.
* Fail closed on a missing config copy, because the import has no stored
  config.

## Behavior changes

* `security.sandbox_type` has no effect.
* `airlock start` offers to import `.airlock/sandbox`, also with a
  `keep-in-project` marker.
* If the import took a disk, the first start asks the config change
  question. A data-dir sandbox from an older version without a stored
  config gets this question one time.
* `airlock rm` uses only the registry. It never touches `.airlock/`.
* `airlock exec` finds sandboxes only through the registry.

## Residual risks

* The guest can plant `.airlock/sandbox`.
* After `airlock rm`, the next start offers to import it.
* The import rules and the fail-closed config check limit the damage. A
  planted CA key harms only that guest.

## Tests

Rust tests, lint and the CLI bats tests ran on Linux. The host had no
KVM. One bats run used an empty `/dev/kvm` file, so that the start tests
ran up to the image step. The VM bats tests, a manual start with a real
VM and the macOS build did not run. CI runs only `tests/vm`, not
`tests/cli`.
