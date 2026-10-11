# Template disk copy status line

A start from a template printed `disk copied from template <name>
(copy-on-write)` before the "Preparing sandbox..." header. The line
was outside the header group, and the clone kind is not useful to the
user.

Now the disk step prints `disk copied from template <size>` in the
same place as `disk created <size>`, under the header.

* The seed step does not print. `disk::ensure` (via
  `project::ensure_disk`) gets a `copied` flag. If the disk exists and
  the flag is set, it prints the copy line with the final size. This
  line replaces `disk grown to` when the copy grows to the configured
  size. The shrink prompt lines stay as before.
* An image change can reset the copied disk before the disk step. Then
  the disk file is missing, `ensure` takes the create branch, and the
  output shows `disk created`. Thus a "copied" line never shows for a
  disk that is not a copy.
* `airlock template build` with a base template uses the same flag.
