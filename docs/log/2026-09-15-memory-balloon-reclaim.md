# Return freed sandbox memory to the host with a balloon "pump"

## Motivation

The VM has always carried a `VZVirtioTraditionalMemoryBalloonDeviceConfiguration`,
but nothing drove it. On Apple's Virtualization.framework the balloon is
purely host-controlled: guest pages go back to macOS only after the host
lowers `targetVirtualMachineMemorySize` on the live device, and the objc2
feature exposing that runtime class wasn't even enabled. So once a build or
test run inside the sandbox had touched, say, 6 GiB, the airlock process
kept those 6 GiB resident until the sandbox exited — the "memory
ballooning" people thought was on did nothing.

## Change

### Free-page pump, not a held shrink

The host cannot tell which guest pages are free on its own, and VZ offers
no free page reporting, so the balloon is used to *emulate* it. Every
10 s the CLI compares the host footprint of the VM (`proc_pid_rusage`
`ri_phys_footprint`, the number Activity Monitor shows) with the guest's
non-free memory (`MemTotal - MemFree`). When the host holds ≥ 512 MiB more
than that, it inflates the balloon by roughly the gap — never more than
`MemFree` minus a margin, never more than 2 GiB per cycle — waits until the
footprint has dropped (or a short size-scaled timeout), and deflates again.

The guest is therefore never left shrunk: the pages come back untouched
and cost the host nothing until written again, page cache is never
evicted, and a margin plus all reclaimable cache stays available during the
few-second window. Pumps are rate-limited to one a minute, and an
ineffective pump (< 10 % freed) doubles the wait up to 10 min so a host that
frees lazily or a guest handing over untouched pages can't cause churn.
The policy lives in `vm/balloon.rs` and is unit-tested; the loop holds
only a `Weak` to the backend and is aborted and awaited on shutdown so it
never delays VM teardown.

The guest now reports `MemFree` in `pollStats` (`MemoryStats.freeBytes`,
appended, wire-compatible). A guest binary without the field reports 0 and
the pump stays off rather than guessing. `[vm] balloon = false` disables
the pump.

### Linux uses the real thing

cloud-hypervisor gets `--balloon size=0,deflate_on_oom=on,free_page_reporting=on`:
the guest reports free 2 MiB chunks and the hypervisor discards them with no
host policy at all. The host-side pump is macOS-only.

### Monitor shows the host figure

The Sandbox tab status line and the Monitor tab memory widget render
`used: <host> (<guest>)` — the host's actual spend for the VM alongside the
guest's `MemTotal - MemAvailable`. `Runtime::launch` receives a
`vm::MemoryProbe` for this.

## Notes

- Guest kernel side needed nothing: `CONFIG_VIRTIO_BALLOON=y` on both
  arches, and `VIRTIO_BALLOON` selects `PAGE_REPORTING`.
- The balloon target must be a 1 MiB multiple; the policy rounds down.
- Whether VZ negotiates `VIRTIO_BALLOON_F_DEFLATE_ON_OOM` is unknown; it
  doesn't matter for the pump (the guest is only briefly inflated), but
  it's worth reading the balloon's `features` bits in the guest once.
- The macOS backend could only be type-checked on the Linux dev box up to
  the C build scripts of unrelated crates; runtime behaviour, pump
  throughput, and whether VZ frees eagerly still need confirming on a Mac.
