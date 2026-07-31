# Stop destroying disk data on shrink and on blkid failure

## Symptoms

Two ways the persistent project disk (overlay upper layer + named caches)
could be wiped without the user asking:

1. **Host side.** Lowering `disk.size` in the config caused the next
   `airlock start` to delete and recreate `disk.img` at the smaller size,
   dropping all persisted state behind a passing "disk recreated" log line.
2. **Guest side.** If `/sbin/blkid` failed to *execute*, disk init treated the
   filesystem state as "needs formatting" and ran `mkfs.ext4` on `/dev/vda`,
   wiping an already-populated disk on a transient exec hiccup.

## Fixes

1. `vm/disk::prepare` no longer shrinks silently. When the existing image is
   larger than the configured size it asks the user to confirm (the default
   selection is the non-destructive one), and only on confirmation does it
   erase and recreate `disk.img` from scratch at the smaller size — so the user
   never has to delete the file by hand. Declining, or running without a TTY,
   keeps the larger image and prints a warning. Growing still works and
   preserves data (unchanged).
2. `init/linux/disk::setup` treats a `blkid` exec failure as "unknown — do not
   format" instead of "format". If the disk really is unformatted, the mount
   that follows fails loudly rather than silently reformatting. A disk is now
   only formatted when `blkid` positively reports no ext4 signature (the
   genuine first-run case).

## Docs

`docs/manual/src/configuration/disk.md` updated: decreasing `disk.size` now
prompts for confirmation and, if confirmed, erases and recreates the disk at
the smaller size; declining (or non-interactive) keeps the larger image.

## Testing

`mise lint` clean. The guest disk path is exercised by the VM bats suite
(`bats:vm`, needs KVM + Docker), not run here.
