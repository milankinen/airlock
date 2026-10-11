---
name: release-notes
description: Use to generate release notes for the next airlock release. Writes a single-file markdown summary aimed at end users.
---

# Release notes

Writes release notes for the next airlock release to
`.tmp/release-notes.md`.

## Fetching the change set

Always scope to "commits since the last tagged release":

```bash
git log --reverse --format='%h %an <%ae> %s' "$(git describe --tags --abbrev=0)..HEAD"
```

Use that list to decide the commits' user-facing impact. Inspect
individual commits (`git show --stat <sha>`, `git log -p <sha>`) only
when the one-line subject is ambiguous.

The author of each commit is in the list. Compare it with the release
owner (`git config user.email`). Also look at `Co-authored-by:` lines
in the commit message. You need this for the thanks note (see
[Credits](#credits)).

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

Write like the user manual (`docs/manual/src/`): simple ASD-STE100
language (`/asd-ste100` skill, STE-flavored mode). Short sentences,
active voice, simple tenses, no semicolons, no phrasal verbs, no
marketing words.

- One section per change: a `##` heading with a short title, then
  one paragraph. No bullet lists.
- The paragraph has 1 to 4 short sentences. Say what changed and what
  the user sees or does now. Add a sentence only when it carries new
  information (a migration step, a caveat, an example command). Do not
  restate the heading. Do not add "you can now..." filler.
- If you write "in other words", "this means", or a parenthetical
  that says the same thing again, cut it.
- **Each paragraph is one unbroken line** in markdown source. No hard
  line breaks inside a paragraph. This keeps the file easy to scan and
  edit.
- A short code block after the paragraph is allowed when a new command
  or config key needs an example. Keep it to a few lines.
- Order the sections by user impact: new features first, then changes
  in behavior, then fixes.
- Keep the total under ~10 sections. If the commit list is longer,
  merge related commits into one section or drop pure-internal ones.
- Put small changes and fixes into one last section, `## Other changes
  and fixes`. A change is small when one sentence tells all of it and
  the user does not need an example or a migration step. In this
  section only, use a bullet list: one sentence per bullet, one line
  per bullet. Omit the section when there are no small changes.
- Omit commits that have no user-visible effect: internal refactors,
  lint fixes, doc-only changes, test-only changes, dev-container
  tweaks. When in doubt, leave it out.

After you write the file, lint it:

```bash
mise x -- python .claude/skills/asd-ste100/scripts/ste-lint.py .tmp/release-notes.md
```

Fix hard violations. Passive-voice and tense findings are advisory.

## Credits

When a person other than the release owner wrote the change, end the
note with `Thanks <name>!`. Use the commit author name. If more than
one person wrote the change, name all of them: `Thanks <name1> and
<name2>!`. Do not thank the release owner.

- In a `##` section, put the thanks as the last sentence of the
  paragraph.
- In `Other changes and fixes`, put the thanks at the end of the
  bullet.

## Output location

Write to `.tmp/release-notes.md`.

## Template

```markdown
# Release notes

Changes since `<previous tag>`.

## <Short title of change>

<One paragraph of 1 to 4 short plain-language sentences on a single line.>

## <Short title of change>

<One paragraph of 1 to 4 short plain-language sentences on a single line.> Thanks <name>!

## Other changes and fixes

- <One sentence on a single line.>
- <One sentence on a single line.> Thanks <name>!
```

## Worked examples

Good (short, concrete outcome):

> ## Project disk shrinks while the sandbox runs
>
> The project disk image gives deleted blocks back to the host every 10 minutes. Before, it stayed at its largest size until the sandbox stopped.

Good (migration step is new information):

> ## User config moves to `~/.airlock/`
>
> Airlock reads the user-level config from `~/.airlock/config.<ext>`. Before, the location was `~/.cache/airlock/config.<ext>`. Move your existing file before the next start.

Good (concrete symptom in plain language):

> ## `HOME` setting and `~` agree
>
> When you set `[env].HOME`, `~` in the sandbox now points to the same directory as `$HOME`. Before, the two were different.

Good (change from another person):

> ## Sandbox templates
>
> Save the packs of a sandbox as a template. A new sandbox from a template starts immediately with the packs installed. Thanks Jane Doe!

Good (small changes in one section):

> ## Other changes and fixes
>
> - `airlock templates list` shows the disk use of each template.
> - The monitor tab keeps its scroll position after a restart. Thanks Jane Doe!

Bad (long sentences, the second half restates the first):

> ## More robust macOS VM backend
>
> Closes off the two most-likely paths for the rare "sandbox disappeared with a broken terminal" failure: any framework error is now caught cleanly instead of aborting the process, and a late-arriving VM callback can no longer touch freed memory.

Bad (implementation detail, names internals):

> ## Hardened AppleVmBackend
>
> Replaced `usize` VM pointer with `Arc<AtomicPtr<VZVirtualMachine>>` and wrapped VZ calls in `objc2::exception::catch` so `NSException` becomes `Err(String)` instead of triggering `abort()`.
