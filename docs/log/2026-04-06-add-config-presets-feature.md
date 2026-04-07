# Add config presets feature

Presets allow reusable configuration templates that pre-populate config
values. A user config with `presets = ["claude-code"]` applies the named
preset as a base layer before the user's own settings.

### Loading algorithm

1. Create empty base config
2. Merge all user config files (hierarchical TOML)
3. Extract `presets` array from merged config
4. For each preset, recursively resolve its own presets, merge onto base
5. Merge user config on top (user always wins)

### Design choices

- **Presets are compiled-in TOML files** under `cli/src/config/presets/`,
  embedded via `include_dir!`. No runtime file loading — keeps deployment
  simple, presets are versioned with the binary.
- **`presets` is a meta-field** stripped before smart-config parsing, not
  part of the `Config` struct.
- **Cycle detection** via call chain tracking; **diamond dependencies**
  handled by "already applied" set (each preset applied at most once).
- **Null-safe merging**: `merge_json` skips null overlays so serialized
  `Option::None` values don't clobber existing config.
- **Config structs now derive `Serialize`** so presets can be modeled as
  Rust objects and converted to Values. `ByteSize` uses a custom
  `serialize_with` helper (upstream doesn't impl Serialize).
- **Preset resolver is injectable** (`&dyn Fn`) — tests use fixture
  presets from `tests/fixtures/`, production uses `presets::get`.

### Bundled presets

- `claude-code`: Anthropic API + claude.ai hosts, mounts `~/.claude` and
  `~/.claude.json` (global state file needed for onboarding/terms).
- `copilot-cli`: GitHub API + githubusercontent hosts, mounts `~/.config/gh`.
- `codex`: OpenAI API hosts, mounts `~/.codex`.

### Test coverage

- Merge: recursive objects, array concatenation, null skip, primitive
  override.
- Presets: single, multiple, nested, circular detection, diamond
  deduplication, unknown error, key stripping, full parse.
- `all_bundled_presets_are_valid`: iterates every non-test preset,
  resolves and parses — catches invalid TOML or schema mismatches.
