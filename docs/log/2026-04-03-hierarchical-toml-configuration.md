# Hierarchical TOML configuration

### What

Replaced hardcoded `Config::default()` with hierarchical TOML config
file loading via smart-config. CLI flags (`--verbose`, `-- args`)
remain as runtime overrides.

### Config file loading

Files loaded in order (later merges over former):

1. `~/.ezpez/config.toml` — global defaults
2. `~/.ez.toml` — user-level shorthand
3. `<project-root>/ez.toml` — project config (committed)
4. `<project-root>/ez.local.toml` — local overrides (gitignored)

Merge rules: arrays concatenate, objects merge recursively,
primitives and type mismatches override with the latter value.

### smart-config integration

Config structs use `DescribeConfig` + `DeserializeConfig` derives for
schema-driven validation with rich error messages. Custom `Nested<T>`
deserializer bridges `DeserializeConfig` types inside `Vec<T>` by
deserializing the raw JSON object first, then feeding it into a fresh
ConfigSchema for full error collection. Thread-local nesting depth
tracks indentation for nested error formatting.

### Defaults

- `cpus`: all available cores (`std::thread::available_parallelism`)
- `memory_mb`: half of system RAM (via `sysinfo` crate)
- `image`: `alpine:latest`
- `verbose` moved to CLI-only flag (not in config files)

### CLI refactoring

- `assets/mod.rs` → `assets.rs` (no sub-modules)
- Fixed `Rc` import in `network/server.rs` (`std::rc::Rc`)
- `verbose` removed from Config, passed directly to `vm.start()`
