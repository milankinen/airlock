# Technical implementation

These pages describe how airlock is put together internally — the
virtualization layer, the RPC protocol between host and guest, the
guest init sequence, how mounts and networking are wired up, and the
on-disk layout of sandbox and cache state.

Most users will never need any of this; it's documented for
contributors, people debugging unusual failures, and anyone evaluating
the security model in detail.

## Overview

airlock runs untrusted code inside a lightweight Linux VM. A single
`airlock` binary boots a VM, pulls an OCI container image, assembles
an overlayfs rootfs, and gives the user an interactive shell (or runs
a one-off command) inside the container. The VM provides
hardware-level isolation; the container provides a familiar
image-based environment.

### One vsock, one RPC connection

The central design decision: the host process and the in-VM
supervisor talk over a **single vsock connection** carrying a
**single [Cap'n Proto](https://capnproto.org/) RPC session**. Every
cross-boundary interaction — booting the container, attaching new
processes, forwarding stdio, polling stats, bridging outbound TCP,
streaming tracing logs — rides that one session.

Cap'n Proto specifically (over gRPC, JSON-RPC, or a bespoke framing):
its zero-copy wire format keeps the stdio hot path cheap, it treats
remote interfaces as first-class values (the supervisor calls outbound
TCP through a *capability* the host handed it, not through a URL it
could fabricate), and concurrent calls and streams are interleaved on
one socket without any multiplexing glue of our own. See
[RPC Protocol / Why Cap'n Proto](./technical/rpc.md#why-capn-proto)
for the detailed rationale.

This shapes almost everything else:

- **No second transport.** There is no virtio console for stdio, no
  separate vsock port for networking, no control channel for
  signals. Cap'n Proto RPC multiplexes many concurrent calls and
  streams over the one connection, so a single `read()`/`write()`
  loop in the supervisor is all the glue the VM needs.
- **Capabilities as plumbing.** Streams like stdin, stdout polling,
  and outbound TCP are modelled as Cap'n Proto *capabilities* passed
  in as arguments. The supervisor doesn't need the host's identity
  or address — it just calls back through the capability it was
  handed. That means the VM has no egress path of any kind: the only
  way out is an explicit capability the host chose to grant.
- **No daemonless hidden state.** There is no airlock daemon on the
  host, no shared socket directory, no broker. When the `airlock
  start` process dies, the vsock closes, the supervisor exits, and
  the VM is torn down. `airlock exec` is a thin client that reaches
  the same session through a Unix-socket bridge in the running
  `airlock start` process.
- **Same wire on every platform.** macOS uses host TCP (the Apple
  Virtualization framework's vsock surfaces that way) and Linux uses
  real `AF_VSOCK`, but the RPC schema and the code paths above it
  are identical.

The [RPC protocol](./technical/rpc.md) page has the full interface
list; the rest of this chapter assumes this one-session model.

### Components and channels

The static picture: what runs where, and how the pieces talk.

<div class="architecture-diagram">
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 760 400" role="img" aria-label="airlock components: host CLI and guest VM with their communication channels">
  <style>
    .arch-box { fill: none; stroke: currentColor; stroke-width: 1.5; }
    .arch-title { font: 600 14px -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif; fill: currentColor; }
    .arch-sub { font: 600 13px -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif; fill: currentColor; }
    .arch-item { font: 12px -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif; fill: currentColor; }
    .arch-conn { font: 11.5px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; fill: currentColor; opacity: 0.85; }
    .arch-bg { fill: currentColor; opacity: 0.03; }
    .arch-label-bg { fill: var(--bg, #fff); }
    .arch-arrow-line { stroke: currentColor; stroke-width: 1.3; fill: none; }
  </style>
  <defs>
    <marker id="arch-arrow" markerWidth="10" markerHeight="10" refX="8" refY="5" orient="auto">
      <path d="M0,1 L9,5 L0,9 Z" fill="currentColor"/>
    </marker>
    <marker id="arch-arrow-start" markerWidth="10" markerHeight="10" refX="2" refY="5" orient="auto-start-reverse">
      <path d="M0,1 L9,5 L0,9 Z" fill="currentColor"/>
    </marker>
  </defs>
  <rect class="arch-box" x="10" y="30" width="354" height="360" rx="8"/>
  <rect class="arch-bg" x="10" y="30" width="354" height="360" rx="8"/>
  <rect x="22" y="22" width="160" height="18" class="arch-label-bg"/>
  <text x="30" y="36" class="arch-title">HOST (macOS / Linux)</text>
  <rect class="arch-box" x="26" y="56" width="324" height="172" rx="6"/>
  <text x="42" y="80" class="arch-sub">airlock start (main process)</text>
  <g transform="translate(42, 104)">
    <text class="arch-item" x="0" y="0">• config + vault + OCI pull</text>
    <text class="arch-item" x="0" y="22">• VM boot (hypervisor API)</text>
    <text class="arch-item" x="0" y="44">• VirtioFS exporter (layers, mounts)</text>
    <text class="arch-item" x="0" y="66">• Network proxy: rules + TLS MITM</text>
    <text class="arch-item" x="0" y="88">• CLI server · stdio + signal relay</text>
  </g>
  <rect class="arch-box" x="26" y="256" width="324" height="112" rx="6"/>
  <text x="42" y="280" class="arch-sub">airlock exec (sibling invocation)</text>
  <g transform="translate(42, 304)">
    <text class="arch-item" x="0" y="0">• walks up for .airlock/sandbox/cli.sock</text>
    <text class="arch-item" x="0" y="22">• sends (cmd, args, cwd, env overrides)</text>
    <text class="arch-item" x="0" y="44">• no project load, no vault unlock</text>
  </g>
  <rect class="arch-box" x="396" y="30" width="354" height="360" rx="8"/>
  <rect class="arch-bg" x="396" y="30" width="354" height="360" rx="8"/>
  <rect x="408" y="22" width="138" height="18" class="arch-label-bg"/>
  <text x="416" y="36" class="arch-title">VM (Linux, ARM64)</text>
  <rect class="arch-box" x="412" y="56" width="322" height="72" rx="6"/>
  <text x="428" y="80" class="arch-sub">init (initramfs)</text>
  <text x="428" y="102" class="arch-item">one-shot: mount shares, disk,</text>
  <text x="428" y="118" class="arch-item">overlay, networking · then exec(airlockd)</text>
  <rect class="arch-box" x="412" y="148" width="322" height="140" rx="6"/>
  <text x="428" y="172" class="arch-sub">airlockd (supervisor)</text>
  <g transform="translate(428, 196)">
    <text class="arch-item" x="0" y="0">• vsock server :1024 (Cap'n Proto)</text>
    <text class="arch-item" x="0" y="22">• spawns + supervises container</text>
    <text class="arch-item" x="0" y="44">• bridges guest TCP ↔ host proxy</text>
    <text class="arch-item" x="0" y="66">• admin HTTP @ http://admin.airlock/</text>
  </g>
  <rect class="arch-box" x="412" y="308" width="322" height="60" rx="6"/>
  <text x="428" y="332" class="arch-sub">container process</text>
  <text x="428" y="354" class="arch-item">cmd running under chroot + uid/gid</text>
  <path class="arch-arrow-line" d="M 350,142 C 378,142 384,218 412,218" marker-end="url(#arch-arrow)" marker-start="url(#arch-arrow-start)"/>
  <rect x="344" y="168" width="82" height="16" class="arch-label-bg"/>
  <text x="385" y="180" class="arch-conn" text-anchor="middle">vsock · RPC</text>
  <path class="arch-arrow-line" d="M 188,256 L 188,228" marker-end="url(#arch-arrow)"/>
  <rect x="136" y="238" width="106" height="14" class="arch-label-bg"/>
  <text x="188" y="249" class="arch-conn" text-anchor="middle">cli.sock · RPC</text>
  <path class="arch-arrow-line" d="M 573,128 L 573,148" marker-end="url(#arch-arrow)"/>
  <rect x="556" y="132" width="34" height="14" class="arch-label-bg"/>
  <text x="573" y="143" class="arch-conn" text-anchor="middle">exec</text>
  <path class="arch-arrow-line" d="M 573,288 L 573,308" marker-end="url(#arch-arrow)"/>
  <rect x="536" y="292" width="74" height="14" class="arch-label-bg"/>
  <text x="573" y="303" class="arch-conn" text-anchor="middle">chroot + exec</text>
  <path class="arch-arrow-line" d="M 722,308 C 722,298 722,298 722,288" marker-end="url(#arch-arrow)" stroke-dasharray="3 3"/>
  <rect x="653" y="292" width="132" height="14" class="arch-label-bg"/>
  <text x="722" y="303" class="arch-conn" text-anchor="middle">TUN → TCP proxy</text>
</svg>
</div>

**Channels shown**

- **vsock · RPC** — the single Cap'n Proto RPC connection between the
  host `airlock start` process and the in-VM supervisor. Carries the
  `start` call (process + mount config + CA), ongoing `exec` calls,
  stats polling, deny notifications, stdio, and the `NetworkProxy`
  capability the guest uses to dial out.
- **cli.sock** — Unix-domain Cap'n Proto connection from an `airlock
  exec` invocation to the CLI server embedded in the main process.
  The server merges override env onto the sandbox's resolved base env
  and forwards the call onto the existing vsock.
- **VirtioFS** (not drawn) — each directory/file mount and the
  per-layer OCI cache are exported as VirtioFS shares, mounted by
  init at `/mnt/<tag>`, and bind-mounted into the rootfs.
- **TUN → TCP proxy** (dashed) — all guest TCP egress routes through
  `airlock0`, a TUN device owned by the supervisor. A userspace TCP
  stack (smoltcp) accepts each flow and dials back through
  `NetworkProxy` on the vsock.

### Startup flow

The dynamic picture: which component does which step, left-to-right
in time.

<div class="architecture-diagram architecture-flow">
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 760 420" role="img" aria-label="airlock startup flow: swimlane diagram of each component's role in boot and container exec">
  <style>
    .flow-lane-a { fill: currentColor; opacity: 0.025; }
    .flow-lane-b { fill: currentColor; opacity: 0.06; }
    .flow-divider { stroke: currentColor; stroke-width: 1; opacity: 0.25; }
    .flow-label { font: 600 12px -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif; fill: currentColor; }
    .flow-event { fill: var(--bg, #fff); stroke: currentColor; stroke-width: 1.2; }
    .flow-text { font: 12px -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif; fill: currentColor; }
    .flow-text-mono { font: 11.5px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; fill: currentColor; }
    .flow-axis { font: 11px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; fill: currentColor; opacity: 0.65; }
    .flow-arrow-line { stroke: currentColor; stroke-width: 1.2; fill: none; }
  </style>
  <defs>
    <marker id="flow-arrow" markerWidth="10" markerHeight="10" refX="8" refY="5" orient="auto">
      <path d="M0,1 L9,5 L0,9 Z" fill="currentColor"/>
    </marker>
  </defs>
  <line class="flow-arrow-line" x1="130" y1="22" x2="740" y2="22" marker-end="url(#flow-arrow)"/>
  <text x="148" y="17" class="flow-axis">time →</text>
  <rect class="flow-lane-a" x="0" y="40"  width="760" height="60"/>
  <rect class="flow-lane-b" x="0" y="100" width="760" height="60"/>
  <rect class="flow-lane-a" x="0" y="160" width="760" height="60"/>
  <rect class="flow-lane-b" x="0" y="220" width="760" height="60"/>
  <rect class="flow-lane-a" x="0" y="280" width="760" height="60"/>
  <rect class="flow-lane-b" x="0" y="340" width="760" height="60"/>
  <line class="flow-divider" x1="0" y1="40"  x2="760" y2="40"/>
  <line class="flow-divider" x1="0" y1="100" x2="760" y2="100"/>
  <line class="flow-divider" x1="0" y1="160" x2="760" y2="160"/>
  <line class="flow-divider" x1="0" y1="220" x2="760" y2="220"/>
  <line class="flow-divider" x1="0" y1="280" x2="760" y2="280"/>
  <line class="flow-divider" x1="0" y1="340" x2="760" y2="340"/>
  <line class="flow-divider" x1="0" y1="400" x2="760" y2="400"/>
  <line class="flow-divider" x1="124" y1="40" x2="124" y2="400"/>
  <text x="10" y="75"  class="flow-label">User</text>
  <text x="10" y="135" class="flow-label">airlock (CLI)</text>
  <text x="10" y="195" class="flow-label">Hypervisor</text>
  <text x="10" y="255" class="flow-label">init</text>
  <text x="10" y="315" class="flow-label">airlockd</text>
  <text x="10" y="375" class="flow-label">container</text>
  <rect class="flow-event" x="140" y="56" width="110" height="28" rx="4"/>
  <text x="195" y="75" class="flow-text-mono" text-anchor="middle">$ airlock start</text>
  <rect class="flow-event" x="140" y="116" width="110" height="28" rx="4"/>
  <text x="195" y="135" class="flow-text" text-anchor="middle">config + env</text>
  <rect class="flow-event" x="260" y="116" width="90" height="28" rx="4"/>
  <text x="305" y="135" class="flow-text" text-anchor="middle">OCI pull</text>
  <rect class="flow-event" x="360" y="116" width="80" height="28" rx="4"/>
  <text x="400" y="135" class="flow-text" text-anchor="middle">boot VM</text>
  <rect class="flow-event" x="450" y="176" width="130" height="28" rx="4"/>
  <text x="515" y="195" class="flow-text" text-anchor="middle">kernel + initramfs</text>
  <rect class="flow-event" x="450" y="236" width="160" height="28" rx="4"/>
  <text x="530" y="255" class="flow-text" text-anchor="middle">mount · disk · overlay · net</text>
  <rect class="flow-event" x="450" y="296" width="130" height="28" rx="4"/>
  <text x="515" y="315" class="flow-text" text-anchor="middle">listen vsock :1024</text>
  <rect class="flow-event" x="520" y="116" width="90" height="28" rx="4"/>
  <text x="565" y="135" class="flow-text" text-anchor="middle">start RPC</text>
  <rect class="flow-event" x="590" y="296" width="110" height="28" rx="4"/>
  <text x="645" y="315" class="flow-text" text-anchor="middle">chroot + exec</text>
  <rect class="flow-event" x="630" y="356" width="90" height="28" rx="4"/>
  <text x="675" y="375" class="flow-text" text-anchor="middle">process runs</text>
  <rect class="flow-event" x="630" y="116" width="90" height="28" rx="4"/>
  <text x="675" y="135" class="flow-text" text-anchor="middle">relay I/O</text>
  <path class="flow-arrow-line" d="M 195,84 L 195,116" marker-end="url(#flow-arrow)"/>
  <path class="flow-arrow-line" d="M 400,144 L 480,176" marker-end="url(#flow-arrow)"/>
  <path class="flow-arrow-line" d="M 515,204 L 515,236" marker-end="url(#flow-arrow)"/>
  <path class="flow-arrow-line" d="M 530,264 L 515,296" marker-end="url(#flow-arrow)"/>
  <path class="flow-arrow-line" d="M 565,144 C 565,220 565,260 560,296" marker-end="url(#flow-arrow)"/>
  <path class="flow-arrow-line" d="M 645,324 L 660,356" marker-end="url(#flow-arrow)"/>
  <path class="flow-arrow-line" d="M 705,356 C 705,280 705,200 705,144" stroke-dasharray="3 3" marker-end="url(#flow-arrow)"/>
  <rect x="708" y="236" width="50" height="14" class="arch-label-bg"/>
  <text x="712" y="247" class="flow-axis">stdio</text>
</svg>
</div>

Once the container is running, `airlock exec` reuses the same VM:
the invocation walks up to `cli.sock`, hands `(cmd, args, cwd, env
overrides)` to the CLI server, which merges the overrides onto the
sandbox's base env and forwards the call over the existing vsock to
`airlockd`, which forks a new process inside the container's chroot.
