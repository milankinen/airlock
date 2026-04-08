# Fix copilot-cli preset middleware syntax

The `copilot-cli` preset used `[[network.rules.copilot-cli.middleware."hostname"]]`
TOML syntax attempting per-host middleware, but `NetworkRule.middleware` is a flat
`Vec<NetworkMiddleware>` — keyed tables are not valid there. This caused the
`all_bundled_presets_are_valid` test to fail with "invalid type: map, expected array".

Fixed by collapsing all per-host scripts into a single middleware entry that branches
on `req.host`, which the Lua runtime exposes alongside `req.path`.
