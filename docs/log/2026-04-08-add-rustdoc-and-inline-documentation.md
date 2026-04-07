# Add rustdoc and inline code documentation

Comprehensive documentation pass across all three crates (protocol,
supervisor, cli) covering 52 source files.

## What was added

- **Module-level docs (`//!`)** on every module explaining its purpose
  and how it fits into the overall architecture
- **Rustdoc (`///`)** on all public types, functions, and significant
  internal types
- **Inline comments** explaining non-obvious design decisions and
  motivations (e.g. why raw vsock syscalls are needed, why iptables
  rules are ordered a certain way, why VirtioFS file-level mounts
  use symlinks instead of bind mounts)

## Approach

Documentation focuses on **why** over **what** — the code already
shows what it does. Comments explain architectural choices, constraints
from the hypervisor/VirtioFS/iptables, and the overall data flow
between host CLI, VM supervisor, and container.

Obvious getters, trait impls, and self-evident code were left
undocumented to avoid noise.
