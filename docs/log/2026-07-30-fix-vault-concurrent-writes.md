# Serialize vault writes so concurrent changes aren't lost

## Symptom

Vault mutations could silently erase each other's data:

- A long-running process (e.g. `airlock up`) opened the vault once, caching the
  snapshot. If the user ran `airlock secrets add NEW` in another terminal and
  the long-running process later wrote the vault (e.g. storing registry creds
  after a 401), it flushed its **stale** snapshot and `NEW` was gone.
- Two `atomic_write`s racing shared one fixed `…​.tmp` filename, so one process
  could rename a file the other was mid-writing.

## Fix

Three parts, in `vault.rs` and the file-backed storage backends:

1. **Locked read-modify-write.** Mutations (`set_secret`, `remove_secret`,
   `set_registry`) now go through a `mutate` helper that takes a cross-process
   `flock` on a sidecar lock file, **re-reads the current on-disk state**,
   applies the change, writes, and refreshes the in-memory cache. Concurrent
   changes are merged onto rather than clobbered — a stale cache can no longer
   overwrite a newer write.
2. **Unique temp name.** `atomic_write` stages through `…​.<pid>.tmp` instead of
   a fixed `…​.tmp`, so concurrent writers can't rename each other's partial
   files into place.
3. **Cheap re-read for encrypted vaults.** `EncryptedFileStorage::load` reuses
   the in-memory key when the vault was already unlocked this process and the
   on-disk salt still matches, so the extra read the `mutate` cycle performs
   doesn't prompt for the passphrase again.

The `lock_path` is exposed via a new default-`None` method on the `Storage`
trait; only the file and encrypted backends return a path. Keyring, disabled,
and the in-memory test doubles need no file lock.

## Tests

`concurrent_writers_do_not_lose_secrets` drives two independent vault handles
on one file (as two processes would): one caches an empty snapshot, the other
adds a secret, the first adds a different one, and both survive — which failed
before the merge.
