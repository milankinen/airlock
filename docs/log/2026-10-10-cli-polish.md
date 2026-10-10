# CLI output and command name polish

Small changes to the `airlock` CLI output and command names.

## Drop the "environment ready" line

`airlock start` printed `environment ready` after the image step. The
line gave no new information: the image line above it ("image cached" or
the pull progress) already tells that the image is ready, and the VM
boot follows directly. Both image paths in `oci::prepare` (new pull and
cached image) printed it. Both lines are removed.

## Logo in the top-level help

`airlock`, `airlock --help` and `airlock help` now show the "airlock"
logo of the setup wizard above the about line. The logo moved from the
wizard view to the `cli` module (`cli::LOGO`), so the wizard and the help
use the same lines. `main` sets it with clap `before_help` on the root
command only. Clap does not copy `before_help` to subcommands, so
`airlock help sandbox` and `airlock start --help` do not show it. The
logo is bold through `console`, which drops the style when color is off.
