# CLI output and command name polish

Small changes to the `airlock` CLI output and command names.

## Drop the "environment ready" line

`airlock start` printed `environment ready` after the image step. The
line gave no new information: the image line above it ("image cached" or
the pull progress) already tells that the image is ready, and the VM
boot follows directly. Both image paths in `oci::prepare` (new pull and
cached image) printed it. Both lines are removed.
