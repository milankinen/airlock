# Use BTreeMap for composable config

Changed config collections from `Vec` to `BTreeMap<String, T>` so presets
can compose cleanly and users can selectively disable inherited entries by
key.

## Changes

- `network.rules`: `Vec<NetworkRule>` → `BTreeMap<String, NetworkRule>`
- `mounts`: `Vec<Mount>` → `BTreeMap<String, Mount>`
- `network.rules[].middleware`: `HashMap` → `BTreeMap`
- `cache: Option<Cache>` → `disk: Disk` (always present, default 10GB)
- `Disk.cache`: `Vec<String>` → `BTreeMap<String, CacheMount>`
- Added `enabled: bool` on `NetworkRule`, `NetworkMiddleware`, `Mount`, `CacheMount`
- Removed `name` from `NetworkRule` — map key IS the name

## Why

With `Vec`, presets that define rules get concatenated on merge. There's no
way to override or disable a specific rule from a preset. With `BTreeMap`,
a user config can override a preset rule by key, or disable it:

```toml
presets = ["copilot-cli"]

# Disable copilot telemetry rule from the preset
[network.rules.copilot-cli]
enabled = false
```

TOML table syntax (`[network.rules.key]`) replaces array-of-tables
(`[[network.rules]]`). All presets updated accordingly.
