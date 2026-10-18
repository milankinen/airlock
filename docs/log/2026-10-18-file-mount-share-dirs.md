# Fix file mount shares on a second boot

With a file mount in the config (for example `~/.bashrc`), the second
`airlock start` of a sandbox failed:

```
refusing path .../overlay/files/ro/bashrc (...): .../overlay/files/ro changed after the check
```

The first boot worked. The fault came after `overlay/files/` existed
from an earlier boot.

## Cause

A verified path keeps the identity (device and inode) of each directory
that exists when the path is made. The boot made the paths of
`overlay/files/{rw,ro}` first, then removed the directories and made
them again. The new directories have new inodes, so the next check of
the path refused them as replaced.

## Fix

* Remove the old directories first, then make the verified paths. The
  paths then have no identity for the missing directories. The check
  after the hardlinks pins the new ones.
* The unused image argument of the share setup is gone, so the test can
  call it without an image.
* A test makes the shares two times with the same overlay dir. It failed
  with the same error before the fix.
