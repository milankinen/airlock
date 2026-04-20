# Fail middleware compilation on unresolved `${VAR}`

`network.http.middleware::compile` previously resolved each declared
`env.*` template with `vault.subst(template).ok()` and set unresolved
entries to `nil` in the Lua `env` table. That made it possible to run
a middleware whose security-critical inputs (API tokens, allowlisted
origins) were silently missing — the attached script would see `nil`
and, depending on how carefully it checked, either 500 on a concat or
forward an empty `Authorization` header.

## What changed

One-line fix: swap `.ok()` for `.with_context(|| format!("resolve
middleware env.{key}"))?`. Any unresolved template aborts `compile`,
which is already propagated via `?` from the single caller in
`network/rules.rs`, so the sandbox refuses to start with a clear
message like:

```
resolve middleware env.TOKEN: no such variable: MY_API_KEY
```

The surrounding doc comments were updated to reflect the new contract:
scripts may treat every declared entry as present.

## Why this over a guard inside the script

The manual previously suggested a `if not env.TOKEN then req:deny()`
guard. That pushes the check into every middleware author's head and
moves detection from config-load time to first-request time, when the
sandbox is already running and the user has already given it
credentials. Fail-closed at compile matches how `[env]` works in
`vm::resolve_env` (same `subst` call, same `?`), so the two env
surfaces now behave consistently.

## Docs

`docs/manual/src/advanced/network-scripting.md` — removed the `if not
env.TOKEN then req:deny()` snippet (now impossible to reach) and
rewrote the surrounding paragraph to state the new contract and point
at the vault chapter for the fallback source.
