# Drop the per-layer `.ok` marker; the rename is the commit point

The layer cache used to write an empty `.ok` file into `<digest>/`
immediately after the atomic rename, and every reader (`ensure_layer_cached`,
the two `ensure_registry_image` filters, `fetch_and_extract_layer`'s
short-circuit, `prepare()`'s fast path, and the docker stream-splitter's
cached-layer classifier) checked for it before trusting the entry.

That check was redundant. `<digest>/` only comes into existence through
`std::fs::rename(tmp, layer_dir)` at the end of `extract_tarball_to_cache`
— the extraction runs in `<digest>.tmp/rootfs/` first, and the final
rename is atomic. A crashed or half-extracted run leaves `<digest>.tmp/`
behind, which `gc::sweep` reaps; it never produces a partial `<digest>/`.
So the invariant "`<digest>/rootfs/` exists ⇒ this layer is complete" is
already guaranteed by the filesystem, and the marker was just a belt on
top of the suspenders.

Every check flips to `p.join("rootfs").is_dir()`. No behavioural change
on any existing cache — new runs stop writing `.ok`, and stale `.ok`
files from previous versions are harmless (nothing reads them, and
`gc::sweep` still tidies up stray layer entries by the usual rules).

The idempotency test grabs the layer dir's mtime instead of the
marker's, which tests the same property (second `ensure_layer_cached`
call doesn't touch the on-disk entry).
