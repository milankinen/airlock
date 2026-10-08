# Rewrite the code comments

The code comments of all crates were long, technical and often out of
date. This change rewrites all comments (`//!`, `///`, `//`) of the
Rust code and the bats tests in simple ASD-STE100 language, checks each
comment against the code, and documents every test. Only comments
change. The code is the same, except for rustfmt reflows.

## Rules

- Module headers (`//!`) describe the purpose of the module and its
  capabilities and concepts. They do not name types or functions and do
  not describe the implementation. Implementation notes that carry real
  value ("why", invariants) moved to the item or line they explain.
- Each exported item has a rustdoc. Functions with arguments use an
  `Args:` / `Returns:` layout. A blank `///` line comes before
  `Returns:` when it follows a list item, else clippy's
  `doc_lazy_continuation` fails.
- Each test has a rustdoc (bats: a `#` comment above `@test`): the
  purpose and reason, then a short numbered flow. Inline comments in
  test bodies only for tricky parts. `CLAUDE.md` ("Tests") now states
  this rule. It replaces the "no comments on tests" rule of the test
  rework.
- The pack-author API is now on public items: the Lua globals on
  `lua_config::evaluate`, the install status-line protocol on
  `InstallProgress`.

## Accuracy pass

Each comment was checked against the code. About 80 comments were
wrong and now match the code. Examples: commands that do not exist
(`airlock up`, `airlock go`), wrong config keys (`settings.vault`,
`max-restarts`), wrong schema names (`start @0`), "deny always wins"
(not under `allow-always` or for port forwards), the interceptor
scope (TLS only), the TUI quit signals (SIGHUP and SIGTERM), and old
iptables text in airlockd.

## Findings

Bugs and weak tests that the review found are listed in `LEFTOVERS.md`
on the `refactor-leftovers` branch. The review did not change code.

## Second review

A second pass checked all comments against the new "Code comments"
section in `CLAUDE.md`, with a focus on inline comments in function
bodies. It also covered the `.capnp` schemas, the pack scripts and the
released preset files. Comments in released packs and presets changed
without a new version: pack fingerprints and preset settings do not
depend on comments. About 13 more false comments now match the code,
most of them in airlockd after the move to the sandbox mount namespace.
