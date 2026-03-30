---
name: git-commit
description: Use always when committing changes to git.
---

# Git commit messages

Format:

```
<what changed, imperative mood, max 72 chars>;

<why this change was made and why this approach was chosen>
```

The first line describes **what** was changed — terse, imperative mood
("Add X;", "Fix Y;", not "Added X" or "Fixes Y"). Always end with ";".

The body describes **why**:
- Why the change was needed
- Why this approach was chosen over alternatives (if non-obvious)

The body is not needed for self-evident changes.

Example:

```
Add Virtio memory balloon device to the VM;

Some CLI workloads may have a high peak memory, after which the VM reserved 
memory is available for host OS use unless. With this virtio memory balloon 
device, the VM has capability to free memory aftwards. Note that this is not 
automatic yet, instead the guest OS and the host app must co-operate to 
trigger this memory reclaim. See next commit for more details about the 
co-operation implementation.
```

IMPORTANT: save the detailed work log to the beginning of `docs/LOG.md`
file to document the design/implementation rationale BEFORE the commit
and add the changes to the commit. DO NOT SKIP THIS STEP!

IMPORTANT: NEVER commit changes unless explicitly asked and ALWAYS
confirm the commit message from the user before commit. DO NOT SKIP 
THIS STEP!