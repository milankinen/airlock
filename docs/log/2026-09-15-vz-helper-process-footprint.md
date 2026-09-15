# Measure the VM's host footprint on the Virtualization.framework helper

## Motivation

The first version of the memory balloon pump measured the airlock process's
own physical footprint and treated it as "what the host spends on the VM".
On the Mac that number sat at a constant ~19 MB while the sandbox reported
500+ MB in use: Virtualization.framework does not map guest RAM into the
calling process at all. Each VM runs inside a separate XPC helper,
`com.apple.Virtualization.VirtualMachine`, which owns the guest memory and
the vCPU threads and is spawned by launchd, not as a child of airlock. The
monitor therefore showed a meaningless host figure, and the pump's gap
(host minus guest non-free) was always negative, so it never fired.

The framework offers no memory reporting of its own — the full surface is
`memorySize` and its min/max limits on the configuration, plus the
balloon's `targetVirtualMachineMemorySize` setter/getter — so the number
has to come from the OS.

## Change

`host_memory_bytes` on the Apple backend now reads `ri_phys_footprint` of
the helper process:

1. Enumerate pids with `proc_listpids(PROC_ALL_PIDS)` and keep those whose
   `proc_pidpath` ends in `/com.apple.Virtualization.VirtualMachine`.
   (`proc_name` is not usable — it truncates the 39-character name.)
2. Pick the helper launchd started on airlock's behalf via
   `responsibility_get_pid_responsible_for_pid`, an exported but
   undocumented libSystem symbol resolved with `dlsym` so its absence
   degrades gracefully. If that yields exactly one helper it is ours.
3. Otherwise, if exactly one helper exists on the system at all, assume it
   is ours (single-sandbox case). Anything else yields `None`: the monitor
   shows only the guest figure and the pump stays off rather than acting on
   another sandbox's numbers.

The pid is cached on the backend after the first successful read and
re-resolved if the footprint query starts failing (VM gone).

## Findings on macOS 26.6.2 (work in progress)

With the helper measured correctly, the pump still freed nothing. Facts
established from the logs (`balloon pump` lines in `.airlock/airlock.log`):

- The helper's physical footprint does track guest memory: holding 16–24 GiB
  in Python inside a 32 GB guest took the helper to 26–33 GB and drained
  `vm_stat` "Pages free" on the host to zero. Freeing it in the guest gives
  nothing back, as expected.
- The guest inflates on request: `MemFree` drops by exactly the requested
  size while the balloon is held (`MemTotal` stays, so `DEFLATE_ON_OOM` is
  negotiated). Device `0x0005` is present and bound to `virtio_balloon`.
- The helper's footprint never moved during any pump — including a 2 GiB
  balloon held 60 s when the footprint was 32.7 GB (every guest page
  touched) — and holds of up to 180 s changed nothing either.
- Under `memory_pressure -l critical`, macOS *compressed* 19 GB of the helper
  (resident size fell, footprint didn't). It did not discard balloon pages.

So Virtualization.framework's traditional balloon appears not to return
inflated pages to the host on this macOS release, at least not in any way
visible to footprint, resident size, or the compressor. The remaining
unanswered question — whether a large balloon of touched pages is dropped
under pressure — is what the `AIRLOCK_BALLOON_HOLD_MIB` mode is for.

Debug environment variables added while investigating (undocumented in
the manual on purpose):

- `AIRLOCK_BALLOON_TIMEOUT_SECS` — how long a pump waits for the footprint
  to drop before deflating.
- `AIRLOCK_BALLOON_MAX_PUMP_MIB` — per-cycle pump size cap.
- `AIRLOCK_BALLOON_HOLD_MIB` — inflate by this much once the guest has it
  free, never deflate, log host footprint/resident every tick.

The pump log line now also records the guest's `MemTotal`/`MemFree` and the
helper's footprint and resident size before and during the hold.

## Notes

- Whether responsible-pid attribution really points at airlock for a system
  XPC service is unverified; the single-helper fallback covers the common
  case either way. A `debug` log line reports the helper and attributed
  counts when identification fails.
- This file was written on a Linux dev box; the macOS code path is compiled
  and exercised only on the Mac.
