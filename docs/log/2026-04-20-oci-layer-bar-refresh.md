# Re-skin OCI layer progress bars; reuse each bar across phases

The per-layer progress bars shown during `airlock start` pulls had grown
a few readability problems:

- The `layer N` prefix and the `{bytes_per_sec}` speed readout stole
  most of the horizontal budget; on a typical 120-col terminal the bar
  itself shrank to ~20 cells.
- Each bar only tracked the download. Extraction was invisible — after
  the bar filled, the line sat there looking "done" while the host was
  actually spending seconds on gzip + tar + xattr writes, and the user
  had no feedback until extraction finished.
- Cached layers pre-filled their bars to 100% but still read
  `downloading`, which was a lie.

## What changed

The bar template and the `ensure_layer_cached` API now cooperate so a
single bar walks through the whole pipeline:

```
  downloading [━━━━━━━━━━━━━━╸          ]  6.2MB/12.0MB
  extracting  [━━━━━━━━━━━━━━━━━━━━━━╸  ] 11.3MB/12.0MB
  ready       [━━━━━━━━━━━━━━━━━━━━━━━━━] 12.0MB/12.0MB
  cached      [━━━━━━━━━━━━━━━━━━━━━━━━━]  4.1MB/4.1MB
```

Template-level tweaks in `cli::layer_progress_bar`:

- Phase label moved to the **left** (`{msg:<11}`), so the eye scans
  down a tidy column of `downloading`/`extracting`/`ready`/`cached`.
- Dropped the `layer N` prefix — the left-column phase carries enough
  identity, and extra vertical noise was more confusing than helpful.
- Dropped `bytes_per_sec`; the bar width freed up compensates.
- Bar chars switched to heavy horizontal (`━╸`) instead of full blocks
  (`=>`). Sits mid-row, reads as a slim solid line.
- Filled region is tinted ANSI 240 (dark gray) via `{bar:25.240}` so
  the bar reads as secondary UI rather than competing with log lines.

## Reusing the bar across phases

`layer::ensure_layer_cached` gained `progress: Option<&ProgressBar>`.
When supplied:

1. Download phase runs inside the caller's existing `pull_layer`, still
   incrementing the bar as tarball bytes hit disk.
2. On entering `extract_tarball_to_cache`, the bar is reset:
   `set_length(tarball_size)`, `set_position(0)`,
   `set_message("extracting")`.
3. Extraction reads through a `ProgressReader<R>` wrapping the file
   handle; each `Read::read` bumps the bar. Works for both compressed
   (registry) and plain (docker) tarballs because the wrapper sits
   below the gzip magic dispatch.
4. After the atomic rename to `<digest>/`, the bar's message is set to
   `ready` so finished layers read as done at a glance.

Registry path passes `Some(&per_layer)`; docker path passes `None` —
it's still behind a single "exporting from docker..." spinner and
adding per-layer bars there is a separate change.

## Blank-line spacer

`cli::progress_spacer` adds a zero-content `ProgressBar` as the last
line of the `MultiProgress`, which renders as a single blank line
between the bars and any subsequent log output. It's owned by the
same `MultiProgress` so `mp.clear()` takes it out with the rest.

Was needed because without it, the `"✓ downloaded N layers, M"` log
line printed right underneath the last bar with no separation and
read as if it belonged to that layer.

## Why not fancy-terminal guards

The heavy horizontal + dark-gray tint assumes a reasonably modern
terminal. `indicatif` already hides the bar when stdout isn't a TTY
(via `ProgressDrawTarget::hidden`) and `--silent` goes through the
same `multi_progress()` hidden draw target, so non-interactive
scripting is unaffected. Terminals without ANSI 256-color support
will render the tinted bar as default-fg, which is a graceful
degradation rather than a failure.
