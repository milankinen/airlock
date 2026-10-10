# Open-network bootstrap

A `deny-by-default` policy is what makes the sandbox safe. But a fresh
sandbox typically first needs to fetch its tooling — `mise install`,
`npm ci`, and the like — from registries and CDNs nobody wants to list
as network rules.

Instead of loosening `airlock.toml`, run the bootstrap as a one-off session
with the policy overridden on the command line:

```bash
airlock start --network=allow-always --login -- ./init.sh
```

`--network` replaces the `[network] policy` value for that run only. Rules,
middleware, port forwards, and socket forwards still apply, and airlock
writes nothing back to the config. The sandbox exits when `init.sh` finishes, so
the open network lives exactly as long as the script.

Then start the normal session, which uses the policy from the config again:

```bash
airlock start --monitor --login
```

Both runs share the same project [disk](../configuration/disk.md), so
whatever `init.sh` installed is there for the second session.

## Wrapping it in mise tasks

With mise's `sources` / `outputs` tracking, the open-network bootstrap
re-runs only when the tooling definition changes:

```toml
[tasks."sandbox:init"]
description = "Fetch project tooling inside the sandbox (network open)"
sources = ["mise.toml", "init.sh"]
outputs = [".sandbox-init-done"]
run = """
airlock start --network=allow-always --login -- ./init.sh
touch .sandbox-init-done
"""

[tasks.sandbox]
description = "Start the sandbox"
depends = ["sandbox:init"]
raw = true
run = "exec airlock start --monitor --login"
```

Now `mise sandbox` is the only command to remember, and `sandbox:init` is
the only thing that ever runs with an open network.

Add `.sandbox-init-done` to `.gitignore`. `airlock rm` does not remove
this file. Remove it after `airlock rm`, so that `sandbox:init` runs again
for the new sandbox disk.
