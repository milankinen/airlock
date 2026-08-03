# Fail closed when an existing config file can't be read

## Symptom

An `airlock.toml` (or `.json`/`.yaml`) that existed but could not be read —
wrong permissions, an IO error, or a directory in its place — was silently
skipped. The loader fell through to the next extension and, finding nothing,
started the sandbox with built-in defaults. That includes the permissive
default network policy, so the user's deny rules and masks were dropped with no
warning: a security-relevant fail-open.

## Root cause

`load_first` read each candidate file with
`let Ok(content) = std::fs::read_to_string(&path) else { continue };`, which
discards *every* error identically. A "file absent" outcome and a
"file present but unreadable" outcome were indistinguishable, so a real read
failure was treated as "no config here" and the loader returned `None`.

## Fix

`load_first` now inspects the error kind: only `ErrorKind::NotFound` continues
to the next extension. Any other read error is returned as
`read config file <path>: <err>`, so a config that exists but can't be read
aborts the launch instead of quietly reverting to defaults. This matches the
already-fail-closed behavior of the sibling settings loader.

## Tests

`config::tests::test_load` covers both directions: a truly absent path still
yields `None`, and an unreadable path (a directory standing in for the config
file, which fails with a non-`NotFound` error for every user including root)
now returns an error rather than being skipped.
