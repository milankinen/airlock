# Add mount `missing` action and resolve relative paths

Mount source paths are now resolved against the project's cwd before
checking existence, fixing `./target`-style relative paths. Added a
configurable `missing` field to mount config controlling behavior when
the source doesn't exist:

- `fail` (default) — error out
- `warn` — skip with a warning message
- `ignore` — skip silently
- `create` — create the directory and mount it

18 tests covering: absolute/relative/tilde paths, all missing actions,
nested create, mixed mounts, file vs dir detection, read_only flag.
