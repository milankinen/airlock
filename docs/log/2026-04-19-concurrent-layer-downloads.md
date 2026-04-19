# Concurrent OCI layer downloads with per-layer progress

Pulling an image used to walk the layer list serially, feeding bytes
into a single total-size progress bar. For images with one big
layer and several tiny ones the big layer blocked everything and the
bar barely moved for long stretches. The total also hid which layer
was active — a timeout on layer 7 of 12 showed up as "stuck at 64%".

Rework: up to three layers now download in parallel, and each in-flight
layer gets its own progress bar via `indicatif::MultiProgress`.

## Concurrency

`futures::stream::iter(...).buffer_unordered(3)` over the layer index
list. The closure inside the stream captures `&Reference`, `&auth`,
and `&layer_paths` by shared reference — all immutable, all fine to
alias. The whole `buffer_unordered` consumer is wrapped in the same
`tokio::select!` with `cli::interrupted()` that the old loop used, so
Ctrl+C still cancels in-flight downloads (they get dropped when the
outer future is dropped).

Three was picked as a default concurrency level: enough to overlap
serial TLS handshakes and fill the pipe on typical broadband, few
enough that file descriptors and TCP connections stay reasonable even
for 40-layer images. No setting for this yet; will add one only if a
real use case turns up.

## Progress UI

Initially I had a total-bytes bar pinned at the top plus per-layer
bars below. Looked busy, and the total bar left a ghost line on the
terminal when everything fit within `indicatif`'s redraw budget.
Dropped the total bar entirely. The per-layer bars are removed from
the `MultiProgress` as soon as they finish, and a final `mp.clear()`
flushes the display region.

For the cache-hit case, a single "X of Y layers found from cache"
line prints up front before any bar work starts — so if an image is
90% cached the user sees that immediately and only the remaining
downloads animate. The "downloaded N layers, <size>" summary at the
end now reports the count and size of what was actually fetched, not
the total image size.

## `pull_layer` signature

`registry::pull_layer` now takes both `per_layer: Option<&ProgressBar>`
and `overall: Option<&ProgressBar>`. The internal `ProgressWriter`
holds a `Vec<ProgressBar>` and broadcasts every `poll_write` byte
count to all of them. Today the overall bar is always `None`, but
keeping the plumbing in place costs nothing and lets future callers
(batch pulls, multi-image fetches) attach an aggregate bar without
touching the layer-download code again.
