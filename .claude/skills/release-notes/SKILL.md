---
name: release-notes
description: Use to generate release notes for the next airlock release. Writes a single-file markdown summary aimed at end users.
---

# Release notes

Writes release notes for the next airlock release to
`dev/tmp/release-notes.md`.

## Fetching the change set

Always scope to "commits since the last tagged release":

```bash
git log --oneline --reverse "$(git describe --tags --abbrev=0)..HEAD"
```

Use that list to decide the commits' user-facing impact. Inspect
individual commits (`git show --stat <sha>`, `git log -p <sha>`) only
when the one-line subject is ambiguous.

Record the previous tag name so the notes can reference it:

```bash
git describe --tags --abbrev=0
```

## Abstraction level

The audience is **end users**, not contributors. Write at the level
someone who runs `airlock start` would understand. That means:

- No symbol names, struct names, file paths, crate names, or function
  signatures. Never reference source modules.
- No signal names (`SIGSEGV`, `SIGABRT`), environment variables, or
  feature-flag names.
- No CLI flag names **unless** the flag itself changed. Existing flag
  names should be spelled out only when they help the user remember
  which feature you're talking about.
- No implementation detail words like "refactor", "wrap", "struct",
  "callback", "trait", "RPC", "mpsc", "atomic".

Use concrete, observable outcomes. "Log survives restarts" — not
"append-mode file handle with rotation". "Pressing `q` closes the
details pane" — not "hierarchical Esc handler". Binding-level
keyboard shortcuts (`q`, `F2`) and in-UI labels (monitor tab, sandbox
view) **are** part of the user's world; keep those.

## Style

- Plain heading, then one unordered list.
- Each bullet starts with a **bolded short phrase** stating the
  change, followed by a sentence or two of explanation.
- **Each bullet must be one unbroken line** in markdown source. No
  mid-bullet hard line breaks. Most markdown renderers reflow
  anyway, and keeping it on one line makes the file easy to scan
  and edit.
- Keep the total under ~10 bullets. If the commit list is longer,
  group related commits under one bullet or drop pure-internal ones.
- Omit commits that have no user-visible effect: internal refactors,
  lint fixes, doc-only changes, test-only changes, dev-container
  tweaks. When in doubt, leave it out.

## Output location

Write to `dev/tmp/release-notes.md`.

## Template

```markdown
# Release notes

Changes since `<previous tag>`:

- **<short bolded phrase>.** <one or two sentences of plain-language explanation on a single line>
- **<short bolded phrase>.** <one or two sentences of plain-language explanation on a single line>
- ...
```

## Worked examples

Good:

> **More robust macOS VM backend.** Closes off the two most-likely
> paths for the rare "sandbox disappeared with a broken terminal"
> failure: any framework error is now caught cleanly instead of
> aborting the process, and a late-arriving VM callback can no
> longer touch freed memory.

Bad (too much implementation detail, multiple lines):

> **Hardened AppleVmBackend.** Replaced `usize` VM pointer with
> `Arc<AtomicPtr<VZVirtualMachine>>` and wrapped VZ calls in
> `objc2::exception::catch` so `NSException` becomes `Err(String)`
> instead of triggering `abort()`.

Good:

> **Silent-exit diagnostics.** If the CLI ever does exit abnormally,
> the log now survives restarts (trimmed to about a megabyte on
> open) and captures every Rust panic or fatal native signal. A
> final exit-code line marks clean shutdowns — its absence tells
> you where the process actually died.

Bad (code references, low abstraction):

> **`diagnostics.rs` installed.** Adds a `panic::set_hook` and
> `libc::signal` handlers for `SIGSEGV/BUS/ILL/ABRT`. Logs land in
> `airlock.log` via `tracing::error!`.
