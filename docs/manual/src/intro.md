<p style="display: flex; justify-content: center;">
  <img src="logo.png" alt="airlock" style="max-height: 256px;" />
</p>
<p style="display: flex; justify-content: center; gap: 0.5rem; padding-bottom: 1rem;">

   <a href="https://milankinen.github.io/airlock">
      <img src="https://img.shields.io/badge/docs-user_manual-blue?style=for-the-badge" alt="User manual" />
    </a>
   <a href="https://github.com/milankinen/airlock/actions/workflows/ci.yml">
      <img src="https://img.shields.io/github/actions/workflow/status/milankinen/airlock/ci.yml?style=for-the-badge" alt="Build" />
    </a>
    <a href="https://github.com/milankinen/airlock/releases/latest">
      <img src="https://img.shields.io/github/v/release/milankinen/airlock?style=for-the-badge" alt="Release" />
    </a>
</p>

---

`airlock` is a command-line tool that tries to make running AI agents inside
lightweight sandbox VMs so simple and smooth that there's never a good reason
to run them on the host machine anymore. The main design principles are:

* **No installation hassle** — a single self-contained binary, installed with
  one command, no extra dependencies
* **Lightweight and fast** — the sandbox should feel like a normal terminal
  tool: boots in seconds, minimal virtualization overhead
* **Project and tech stack agnostic** — no assumptions about your tooling;
  flexible enough that everyone can tailor the sandbox to their needs
* **Shareable** — sandbox configuration lives in version control and can be
  shared across a team or company

![Demo](./demo.svg)

## Motivation

**tl;dr** I kept running into the same problem: AI agents need broad access
to do their job, but that broad access is exactly what makes them dangerous
in a work context. Denylists don't scale. Allowlists inside a throwaway VM
do.

Working with AI agents at work requires extra care not to expose company
secrets. With agentic coding, the risk is especially high — agents have
access to the entire host machine and typically execute tasks
semi-autonomously. They don't *usually* do anything dangerous, but sometimes
they access sensitive data by accident while trying to fulfil a prompt, and
once that happens, the damage is already done.

Many AI agents already provide sandboxing capabilities, but I've noticed that
normal development tools require surprisingly wide filesystem access to work
properly — caches, registries, config files, credential stores. That's why
instead of *granting* access to specific resources, people end up trying to
*deny* access to non-allowed ones (like secrets). From a security perspective,
this is a nightmare: in any larger group there's **always someone** who stores
secrets in a different place, rendering global deny-based policies basically
useless.

The thing is, the actual secrets that a project and its AI agent truly need are
**very few**. I've found it's far easier and safer to build a sandbox that
exposes only those few secrets explicitly and lets the agent roam freely
inside. With a microVM, the agent and its tools get full access to the VM's
resources, so most things work out of the box. And because the blast radius is
limited to the VM boundary, the worst an agent can do is destroy the VM and
the mounted project files — both easily recoverable by re-creating the VM and
cloning from remote.

Of course, the agent can still leak project files over the network through
malicious prompt injections. That's why VM network traffic must be fully
controllable: some hosts can always be trusted, sometimes you want to run
manual steps inside the VM (like installing dependencies) that are safe,
sometimes the agent makes a legitimate request but you want to approve it
first — the use cases are countless. The system needs to adapt and make
enforcing strict network policies as simple and effortless as possible, so
that even the laziest of us actually follow them.

## Features

### VM-isolated sandboxes from any Linux OCI image

airlock boots a lightweight Linux VM using
[Apple Virtualization](https://developer.apple.com/documentation/virtualization)
on macOS or [Cloud Hypervisor](https://www.cloudhypervisor.org/) + KVM on
Linux. The VM kernel and initramfs are embedded in the binary — there's
nothing else to install.

Before booting the VM, airlock pulls an OCI image (from a registry or local
Docker daemon), shares its layers into the VM via VirtioFS, and assembles an
overlayfs root filesystem inside the guest. The image can be anything: Ubuntu, Alpine,
Fedora, a custom CI image — if it runs on Linux, it works.

* Pull images from any reachable OCI registry (authentication supported via
  the built-in vault backed by the system keyring) — no Docker required
* Or use images from a local Docker daemon if you have one
* Selectively expose host environment variables into the VM
* Share host directories via fast VirtioFS mounts (bidirectional sync,
  read-only option available)
* Near-native speed ext4 block device for persistent VM state (installed
  packages, caches like `node_modules` or `~/.cargo/registry`)

### Full network control

The VM has no network interfaces of its own. All ingress and egress traffic
flows through a vsock RPC channel back to the host, where airlock enforces
network policies. This isn't just an HTTP proxy — it's full TCP traffic
control.

* Configurable allow/deny rules with wildcard host and port matching
* Transparent TLS interception (MITM) for rules with Lua middleware —
  a per-project root CA is generated automatically and installed into the
  VM's system certificate store
* Lua-scriptable HTTP request and response modification (inject headers,
  rewrite requests, conditionally deny)
* HTTP/2 and ALPN support
* Internal DNS that maps SNI hostnames for TLS termination
* Transparent host port and Unix socket forwarding into the VM

### Configuration as code

Sandbox configuration lives in a plain `airlock.toml` at the project root.
Check it into version control, and every team member gets the same sandbox
setup — same image, same network rules, same mounts. Local overrides go in
`airlock.local.toml` (gitignored). Built-in presets for common ecosystems
(Rust, Python, Node.js, and more) provide sensible defaults out of the box.

## Coming up next!

* MCP proxy for stdio-based MCP servers (e.g. Playwright MCP from inside the VM)
* System-admin-managed configuration defaults and policies
* Network configuration editing from the Monitor dashboard

## Similar projects

There are several tools in this space, each with a different focus. Here's how
airlock compares:

* [Microsandbox](https://github.com/microsandbox/microsandbox) — the closest
  open-source alternative. To be honest, this is a very promising project with
  very similar ideology and feature set. It focuses a bit more on being an SDK
  for programmatic usage whereas airlock focuses on pure terminal cli, but
  Microsandbox has a very decent CLI as well.
* [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) — microVM-based
  sandboxes with a deny-by-default network proxy, per-sandbox Docker daemon,
  and credential injection. Network policies are domain-level allowlists
  (HTTP/HTTPS only, no raw TCP control or scripting). Configuration is per-agent
  via CLI, not a shareable project-level config file.
* [OpenShell](https://github.com/NVIDIA/OpenShell) — NVIDIA's sandbox for AI
  agents using Docker containers with declarative YAML policies for filesystem,
  network (L4 + L7), and process access. Hot-reloadable, shareable policies.
  Requires Docker; container-level isolation, not VM.
* [nsjail](https://github.com/google/nsjail) — Google's lightweight process
  sandbox using Linux namespaces and seccomp-bpf. Single binary with a BPF
  policy language (conceptually similar to airlock's Lua scripting). Process-level
  isolation, not VM. Config via protobuf files (shareable but verbose).
  Linux only.
* [Codex CLI](https://github.com/openai/codex) — OpenAI's coding agent with
  built-in OS-level sandboxing (Seatbelt on macOS, Bubblewrap on Linux).
  Process-level isolation, binary on/off network control. Tightly coupled to
  the Codex agent, not a general-purpose sandbox. No shareable config.
* [Vibe](https://github.com/lynaghk/vibe) — lightweight Rust CLI that boots
  Debian VMs on ARM Macs using Apple Virtualization.framework. Zero-config,
  auto-shares project directory and credential dirs via VirtioFS. macOS only,
  Debian only (no OCI images), no network policy or shareable config.
* [Tart](https://tart.run/) — macOS and Linux VMs on Apple Silicon using
  Apple Virtualization.framework, OCI-compatible images. Designed for CI
  automation, not security sandboxing — no network policy or config-as-code.
* [Lima](https://lima-vm.io/) — launches Linux VMs on macOS with file sharing
  and port forwarding. YAML config files (shareable). General-purpose
  Linux-on-Mac tool, not a security sandbox — no network isolation.
