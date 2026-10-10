# Default network policy is deny-by-default

The default `[network] policy` was `allow-always`. A config without a
`policy` value gave the sandbox full network access. This is not safe for
untrusted code.

The default is now `deny-by-default`. Without a `policy` value, airlock
allows only connections that match an `allow` rule.

Effect on existing configs: a config that does not set `policy` now denies
the hosts that no rule allows. To get the old behavior, set
`policy = "allow-always"` or use `airlock start --network=allow-always`.
The legacy presets golden file changes only in the `policy` value.
