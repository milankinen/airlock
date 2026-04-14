# Split mount missing=create into create-dir and create-file

The `missing` field on mount configs previously only supported `"create"`
which always created a directory. This was insufficient for file mounts
like `~/.claude.json` where the source might not exist yet.

## Changes

Split `MissingAction::Create` into two variants:

- `create-dir` — creates the directory tree (was `create`)
- `create-file` — creates parent dirs, then writes the file

Two new optional fields on `Mount`:

- `create_mode` — Unix permissions (octal). Defaults to 755 for dirs,
  644 for files. Always applied (not just when non-default).
- `file_content` — initial content for `create-file`. Defaults to empty
  string.

Serde renamed from `snake_case` to `kebab-case` to match the new
hyphenated variant names (`create-dir`, `create-file`). The other
variants (`fail`, `warn`, `ignore`) are single words so unaffected.

## Example

```toml
[mounts.config]
source = "~/.config/app/config.json"
target = "~/.config/app/config.json"
missing = "create-file"
file_content = "{}"
create_mode = 0644
```
