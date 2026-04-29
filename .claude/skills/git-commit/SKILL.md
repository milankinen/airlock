---
name: git-commit
description: Use always when committing changes to git.
---

# Before commit

IMPORTANT: `mise lint` command MUST SUCCEED before commiting ANYTHING
to version control. Fix the formatting and linting errors before continuing.
This is non-negotiable. DO NOT SKIP THIS STEP!

IMPORTANT: Always review the full `git diff` of staged/unstaged changes
before committing. Verify the diff matches the intended changes and does
not include unrelated modifications. DO NOT SKIP THIS STEP!

IMPORTANT: Check whether the commited changes affect any user-facing behavior,
configuration, CLI options, or features documented in `docs/manual/`.
If so, the relevant manual documentation MUST be updated as part of
the same commit. MANUAL MUST ALWAYS BE UP-TO-DATE! DO NOT SKIP THIS STEP!

# Git commit messages

Format:

```
<what changed, imperative mood, max 80 chars>

<why this change was made and why this approach was chosen>
```

The first line describes **what** was changed — terse, imperative mood
("Add X", "Fix Y", not "Added X" or "Fixes Y"). No trailing punctuation.

The body describes **why**:

- Why the change was needed (what the user-observable symptom was, or
  what concrete capability this enables)
- Why this approach was chosen over alternatives (if non-obvious)

The body is not needed for self-evident changes.

## Abstraction level

Commit messages are **read by humans skimming `git log`**, not by
contributors digging into the implementation. Aim for a concrete,
user-observable level of detail:

- Use **plain language** that someone running `airlock start` would
  understand. Show the symptom they would have seen, or the config
  they would now write.
- Use **config keys** (`[env].HOME`, `--monitor`), **CLI flags**, and
  **shell-level observations** (`$HOME` reports `/home/dev`,
  `target = "~/foo"` mounts at `/root/foo`) freely — these are part
  of the user's world.
- Avoid **symbol names**, **function signatures**, **module paths**,
  **struct field names**, **type names**, and **file paths**. Those
  belong in the log entry, not the commit message.
- Avoid implementation jargon: "refactor", "wrap", "thread through",
  "trait", "impl", "newtype", "callback".
- Aim for **3–6 sentences** in the body. If you can't say it that
  short, the rest belongs in the log entry — link to it (`See
  docs/log/<entry>.md`) only when the commit really needs the cross-
  reference for context, not as a default footer.

If the commit fixes a bug, **state the visible symptom first**, then
the fix in plain terms — readers want to know "could this have been
biting me?" before "how was it fixed?".

Good (concrete symptom + plain example + plain fix):

```
Honour `[env].HOME` in container-side `~` expansion

Tilde expansion in mount targets used to consult only the OCI
image's user record, so a `[env].HOME = "/home/dev"` override gave
a sandbox where `$HOME` and `~` disagreed: the shell reported
`/home/dev` but `target = "~/foo"` mounted at `/root/foo`. Now `~`
on the container side picks up the override too.
```

Bad (implementation-heavy, names internal symbols, too long):

```
Add `oci::effective_container_home` helper and thread it through

Introduces a free function in `oci.rs` that reads
`project.config.env.get("HOME")`, runs it through `vault.subst`,
falls back to `OciImage.container_home`. Threaded into
`network::setup` and `vm::start`, which forwards to
`assemble_mounts` and `disk::prepare`. The `OciImage.container_home`
field stays unchanged for backwards compatibility...
```

Another good example (capability + tradeoff explained at a high level):

```
Add Virtio memory balloon device to the VM

Some CLI workloads have a high peak memory; after the peak, the
RAM the VM reserved sits idle on the host. With this device the
guest can hand memory back to the host. Reclaim isn't automatic yet
— guest and host co-operate to trigger it; see the next commit.
```

IMPORTANT: If the commit contains changes to the technical implementation
(crates/, VM, supervisor), save a detailed work log entry to `docs/log/`
as an individual file named `<yyyy-mm-dd>-<title>.md` to document the
design/implementation rationale BEFORE the commit and add the file to
the commit. Other changes (docs, config, skills, scripts) do not need
a log entry. DO NOT SKIP THIS STEP!

IMPORTANT: NEVER commit changes unless explicitly asked and ALWAYS
confirm the commit message from the user before commit. DO NOT SKIP
THIS STEP!