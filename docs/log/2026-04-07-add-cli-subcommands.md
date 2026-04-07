# Add CLI subcommands

Restructured the flat CLI into subcommands: `ez go`, `ez project info`,
`ez project list`, `ez project remove`.

## Arg parsing

`--` is split from argv manually before clap sees it. Extra args after
`--` are passed to the `go` command only — other commands reject them.
Default (no subcommand) is `go` for backward compat.

## Structure

```
cli/src/
  main.rs                    entrypoint: split --, dispatch
  cli/mod.rs                 clap definitions, console helpers
  cli/cmd_go.rs              VM launch (moved from main.rs)
  cli/cmd_project_info.rs    show project config and paths
  cli/cmd_project_list.rs    list all projects with running status
  cli/cmd_project_remove.rs  remove project cache (fail if running)
  project/mod.rs             Project struct, lock(), project_hash()
  project/meta.rs            metadata helpers: save/read image, last_run,
                             resolve_project_dir, project_id, abbrev IDs
```

`CliArgs` kept as a plain struct (not a Parser) for compat with
`rpc/supervisor.rs`, `vm.rs`, `oci.rs`. Constructed from `GlobalArgs`
+ extra args.

Project `cwd` is now written to a file in the cache dir during lock,
so `project list` can display it.

## project info output

Shows: ID (full hash), project path, running status, image, CPUs,
memory, last run (timeago), disk path, disk cache mounts, host mounts,
network rules. Errors if the project cache dir doesn't exist yet
(i.e. `ez go` has never been run for this directory).

## project remove

Prompts for confirmation before deleting the project cache dir.
`-y`/`--yes` flag bypasses the prompt.

## Project IDs and abbreviated hashes

Each project is identified by its SHA-256 hash (hex, 32 chars).
`project list` computes the minimum prefix length (≥7, git-style)
such that all listed hashes are unique, and shows only that prefix.

`project info` and `project remove` accept a path, full hash, or
abbreviated hash as their argument. Abbreviated hash lookup scans
the projects dir for a unique prefix match; errors on 0 or >1 matches.
