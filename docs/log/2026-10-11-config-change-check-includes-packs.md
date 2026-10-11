# Config change check includes packs

## Problem

The VM bats test `alpine: packs install, stay, run again and need --yes to
remove` failed on CI. The test removes `mise` from `[packs]` and expects the
config change question first. It got the tool question:

```
Tools changed in the sandbox (removed: mise). Run in a terminal or pass --yes.
```

The config change check compares the final config values of this run and
the last run. The pack list was not in these values. A pack contributes
only its config values (mounts, env, daemons, ...). The `mise` pack has no
config, so its removal did not change the values. The check saw no change,
and the tool check came next.

## Fix

`final_values` in `start/config_change.rs` now adds a `packs` object: the
enabled packs by name, each with its version and all arg values (defaults
included). A pack add, remove, version change or arg change is now a
config change, also for a pack without config values. The diff of the
question shows the `[packs]` lines too.

Defaults are included on purpose: the comparison resolves both configs
with the same pack definitions, so a new arg default gives the same value
on both sides.

The CLI bats test for the removed pack has no stored config copy, so it
still gets the tool question.
