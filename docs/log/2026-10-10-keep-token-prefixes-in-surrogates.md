# Keep public token prefixes in masked `[env]` surrogates

## Problem

With the `copilot` pack and `COPILOT_GITHUB_TOKEN` set on the host,
Copilot CLI still said that the user must log in. The monitor showed no
HTTP request at all, so the token never reached the proxy.

The cause: Copilot classifies the token by its prefix before it uses it.
The auth code in its native runtime addon accepts `github_pat_`, `gho_`
and `ghu_`, rejects `ghp_` with a dedicated "classic PAT" error, and
treats anything else as "no authentication information". The masked
surrogate was pure `[A-Za-z0-9]`, so the prefix was gone.

Checked against Copilot CLI 1.0.95 on the host with fake tokens and a
dead `HTTPS_PROXY`:

| Token shape                         | Result                                   |
|-------------------------------------|------------------------------------------|
| 93 random alphanumerics             | "No authentication information found"    |
| `github_pat_` + 82 random           | tries `api.github.com/copilot_internal/user` |
| `github_pat_` + 22 + `_` + 59       | same                                     |
| `gho_` + 36 random                  | same                                     |
| `ghp_` + 36 random                  | "Classic Personal Access Tokens ... not supported" |

So only the prefix matters, not the inner `_` of the fine-grained PAT
format.

## Options considered

**Generic heuristic** (keep the leading lowercase/separator run, such as
`sk-ant-oat01-`). Rejected. For a masked password or URL it copies real
secret text into the guest (`my-secret-pw` would keep `my-secret-`).

**Built-in list of well-known prefixes.** Works without config, but
airlock would have to track every vendor's format, and adding `sk-` style
prefixes would change the surrogates of existing Codex and Claude
entries. Codex stores its surrogate in a shared auth file, so that
breaks it once.

**Explicit per-entry list.** Chosen. `keep_prefixes = [...]` on the
`[env]` entry. Only strings that the config author lists as public
format markers leave the host. Entries without the key are not changed.

## Implementation

The surrogate is the old derivation (SHA-256 of domain, name and length
into ChaCha20) with its first `prefix.len()` bytes replaced by the
prefix. Thus:

- Without a matching prefix the surrogate is byte for byte the old one.
  The pinned-surrogate test did not change.
- The surrogate stays stable across restarts and token rotation, as long
  as the length and the prefix stay the same.
- The longest matching prefix wins, so the result does not depend on the
  list order. Layers concatenate arrays, and that order is not
  meaningful.
- A prefix is kept only if at least `MIN_INJECT_LEN` (8) bytes follow it.
  The proxy matches surrogates as bytes in header text, and a mostly
  public surrogate could match unrelated text.
- The random part is ASCII, so slicing it at the prefix length is always
  on a char boundary, and the byte length equals the real value's.

The copilot pack lists `github_pat_`, `gho_`, `ghu_` and `ghp_`. `ghp_`
is not a supported type. It is in the list so that a classic PAT gets
Copilot's clear error message instead of the login prompt.

The legacy `copilot-cli` preset is not changed. The golden file of the
released presets locks how they resolve, and the fix goes to the pack.
