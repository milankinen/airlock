# Fixes for the refactor leftovers

The code and test comment reviews after the test suite rework
(`2026-10-08-test-suite-rework.md`, `2026-10-08-code-comment-rewrite.md`)
found bugs, weak tests and open decisions. They were listed in
`LEFTOVERS.md` on the `refactor-leftovers` branch. Each section below is
one commit.

## Error messages name the right setting and command

The vault backends told the user to set `settings.vault = "..."`. The
real key is `vault.storage` in the settings file, as the "vault is
disabled" message already says. The CA error told the user to run
`airlock up`, a command that does not exist. It now says `airlock start`.

## Remove a stale dead-code allow

The signal number function had an `allow(dead_code)` for a time when no
caller existed. The daemon start code calls it now, so the attribute
only hid future real dead code.

## Passthrough conflict check ignores case and a trailing dot

The check that refuses a passthrough target which overlaps a middleware,
inject or service target compared literal/literal and wildcard/wildcard
pairs as raw strings. Only the mixed pair went through the run-time host
matcher, which ignores case and one trailing dot. So `Example.com` or
`example.com.` gave no conflict with `example.com`, but at run time both
match the same host. Both patterns now go through the same canonical
form as the run-time matcher before the compare.
