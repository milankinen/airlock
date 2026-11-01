# Build zbus without its tokio feature

## Symptom

`airlock start` panicked at once with "Cannot start a runtime from within a
runtime". The panic happened when the config had `[env]` entries that need
the vault, for example `${SECRET}`. A config without vault access worked.

## Cause

The desktop notification library was built with `z-with-tokio`. Cargo
joins the features of all crates in the workspace. Thus zbus got its `tokio`
feature in the whole build, also for the keyring.

On Linux the keyring uses the Secret Service over zbus, with the blocking
API. With the `tokio` feature, the blocking API calls `block_on` on a tokio
runtime. The CLI runs on a `current_thread` runtime, and the vault read
happens on that thread. So `block_on` panicked.

## Fix

Build the notification library with the `z` feature. zbus then uses its
own `async-io` executor thread. This works with `show_async()` under tokio,
and the blocking keyring calls work on a runtime thread.

## Notes

* Do not add a zbus `tokio` feature again, directly or through another
  crate, while the keyring reads the vault on a runtime thread.
* A real fix for the keyring reads is to run them on `spawn_blocking`.
  This needs a larger change, because `Vault` has a synchronous API.
