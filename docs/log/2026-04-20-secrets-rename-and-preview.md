# Rename CLI commands and add masked preview column

Three related CLI polish changes landed together:

## 1. Rename `secret`/`ls`/`rm` → `secrets`/`list`/`remove`

All aliased back to the short forms, so nothing existing breaks:

- `airlock rm` → `airlock remove` (alias: `rm`)
- `airlock secret` → `airlock secrets` (alias: `secret`)
- `airlock secrets ls` → `airlock secrets list` (alias: `ls`)
- `airlock secrets rm` → `airlock secrets remove` (alias: `rm`)

### Why

Long names read better as canonical forms — they describe the action
rather than being terminal-history-optimized abbreviations. `ls` / `rm`
are shell-idiom abbreviations that look natural in isolation but feel
inconsistent alongside `airlock start` / `airlock show` / `airlock
exec`. The plural `secrets` fits since the command operates over a
collection; `secret add FOO` reads fine because the object is named
right after.

Aliases exist so the rename is zero-friction — existing user scripts,
muscle memory, and bats tests (`airlock rm --force`) continue to work.

## 2. Add `VALUE` preview column to `airlock secrets list`

`list` now prints three columns: `NAME`, `VALUE`, `SAVED AT`. The
`VALUE` column is a `****`-prefixed masked suffix:

| Value length | Shown             |
| ------------ | ----------------- |
| ≥ 16 chars   | `****` + last 4   |
| 8–15 chars   | `****` + last 2   |
| < 8 chars    | `****` (no tail)  |

### Why

Users frequently accumulate multiple similar-sounding tokens
(`STAGING_API_TOKEN`, `PROD_API_TOKEN`, `STAGING_API_TOKEN_OLD`) and
need to tell which one holds which rotated value without a full
"show value" command. A suffix preview answers "is this the one I
just rotated?" without exposing enough entropy to help an attacker:

- 4 chars of a ~30-char token is ~13% of the material.
- 2 chars of an 8-char password is ~25% of the material — exposed
  only for values that were already short enough to be weak.
- Under 8 chars we show nothing, because two leaked chars of a
  4–6-char password is a meaningful fraction of the value and a
  plausible shoulder-surfing aid.

### Safety argument for `list`

The `list` command already has to clear the backend's auth gate
(keychain unlock, passphrase prompt, or disabled → refused) before
it can read any metadata, so the preview is gated by the same bar
that protects the full value. No new attack surface: an attacker
who can `list` already has vault access and could call `get_secret`
directly. The `file` backend is a no-op outlier — it's plaintext
on disk anyway.

Always prefixing `****` keeps the total rendered length constant
(don't leak value length beyond the three bucket tiers).

## 3. Sort `list` output alphabetically by name

`list_secrets` is backed by a `BTreeMap`, so iteration is already
key-sorted today — but the `list` display layer now calls an
explicit `sort_by(&name)` so the invariant is pinned at the UI
boundary. If the vault internals ever migrate to `HashMap` (for
example to stabilize insertion-order or add indexing), the list
output won't suddenly become non-deterministic.

## Unrelated wording tweak bundled in

`cmd_rm.rs` / `cmd_show.rs` had been carrying pending reword from
"project data" → "sandbox" (reflects the terminology the manual
already uses). Bundled in here rather than landing separately.
