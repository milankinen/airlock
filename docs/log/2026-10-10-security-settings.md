# Sandbox type in the security settings

The user settings now have one `[security]` table for the settings that
make the sandbox less isolated if they change:

```toml
[security]
sandbox_type = "managed"    # or "project-owned"
insecure_mounts = false
```

## Change

- `sandbox_location = "cache-dir" | "project-dir"` (top level) is now
  `security.sandbox_type = "managed" | "project-owned"`. The behavior is
  the same: a managed sandbox is in `<data>/boxes/<id>/`, a project-owned
  sandbox is in `<project>/.airlock/sandbox/` and the guest hides
  `<project>/.airlock`.
- No alias for the old key. It was never in a release
  (`2026-10-08-sandbox-data-dir.md`).
- `security.insecure_mounts` came with the home mount check
  (`2026-10-10-refuse-home-mounts.md`).
- `airlock sandbox info --json` keeps its `location` values
  (`data-dir`, `project-dir`). They tell where a sandbox is, not the
  setting.

## Tests

- `settings/mod.rs`: defaults (`managed`, `insecure_mounts = false`), a
  TOML file that sets both, and a bad `sandbox_type` value.
- `start/tests/test_sandbox_location.rs`: the project-owned setting.
