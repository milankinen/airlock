# Fix concurrent layer extraction clobbering shared staging dirs

## Symptom

Two `airlock` processes pulling the same uncached image at the same time
could corrupt each other's layer pull: a download would come out truncated,
an extraction would end up missing files, or a just-published layer directory
would vanish out from under a process that was already reading it. Single
pulls, and pulls of images already in the cache, were unaffected.

## Root cause

`app/airlock-cli/src/oci/layer.rs` staged every pull through fixed,
per-digest names shared by all processes: `<digest>.download.tmp` for the
in-flight download and `<digest>.tmp/` for the in-flight extraction. Each
process began by `remove_file`/`remove_dir_all`-ing that shared path and then
wrote into it, so concurrent pullers overwrote each other's staging area. At
the commit step the extractor also did `remove_dir_all(<digest>/)` before its
rename, which could delete the final layer directory a sibling process had
just committed and was consuming.

## Fix

Both staging paths now include `std::process::id()` plus a monotonic
per-process counter, so every pull writes into its own directory/file and the
atomic rename onto the shared `<digest>.download` / `<digest>/` name is the
only cross-process handoff. The extraction commit no longer deletes an
existing final directory: because the fast path already returns early when
`<digest>/` exists, its presence at commit time means a peer won the race, so
we reuse that result and discard our own staging tree (also handling the
rename losing with ENOTEMPTY). Stale fixed-name download tmps from older
binaries are still cleaned up, and `gc::sweep` continues to reap the
process-unique tmps since they end in `.tmp`. The symlink-safe whiteout
handling (`safe_join`) is unchanged.

## Tests

Added `extract_reuses_existing_winner_dir`, which pre-creates the final layer
directory and asserts a subsequent extraction reuses it (winner content kept,
our entry not committed) and leaves no staging dir behind. The existing
`ensure_layer_cached_*` tests continue to cover the happy path with the new
unique tmp names. A real two-process race can't be reproduced
deterministically in a unit test, so only its observable commit-time outcome
is covered.
