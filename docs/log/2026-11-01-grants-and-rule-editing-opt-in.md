# Grants and rule editing are opt-in

The agent tools for temporary network access are now off by default:

* `network.grants.enabled` defaults to `false` (was `true`).
* `network.rule_editing` defaults to `"none"` (was `"on-demand"`).

Reason: the features are new. Users must enable them on purpose until
they are stable.

Implementation notes:

* `Grants::is_default` now checks `!enabled`. Configs that do not set
  `[network.grants]` still serialize with no `grants` table.
* The grant tests set `network.grants.enabled = true` and
  `rule_editing = "on-demand"` where they use the features. The shared
  test config is now `OPTED_IN`.
* The install phase still forces `grants.enabled = false`. This has no
  effect on the new default, but it keeps the install boot safe if a
  user enables grants.
