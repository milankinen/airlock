# Cancellable OCI pull + per-image progress bars

The OCI pull pipeline had two friction points worth fixing together:

1. **Uninterruptible.** `ensure_image` and the synchronous `docker
   image save` pipeline ignored Ctrl+C. A large pull couldn't be
   cancelled without killing the whole process and leaving staging
   state behind.
2. **Partial progress.** Registry pulls rendered a per-layer bar only
   for layers being downloaded. Images with 8 layers of which 6 were
   cached showed 2 bars, giving no sense of "how much of this image
   is actually ready." Cached layers were summarized in a single log
   line and then disappeared from the live display.

## Cancellation

`ensure_image` is wrapped in `tokio::select!` against
`cli::interrupted()` at three points:

- `oci::prepare` races it against the broadcast-based
  `cli::interrupted()` future and bails `"cancelled by user"`.
- `ensure_docker_image` races the save-and-extract pipeline
  against the same signal.
- `ensure_registry_image` races the bounded-concurrency fetch stream
  against it.

The docker path was the tricky one. `docker image save` is a sync
`std::process::Child`, and the tar split is a blocking loop. Making
the whole thing `async` without pulling in `tokio-util/io-util` was
done by splitting the module entry into:

- `async fn save_layer_tarballs(image_ref)` — spawns the docker
  child, wraps it in a `DockerSaveGuard(Option<Child>)` drop guard,
  `take()`s its `ChildStdout`, then `tokio::task::spawn_blocking` the
  tar-streaming loop as `save_from_stream(stdout, &layers_root)`.
  Happy path reaps the child via `guard.0.take().wait()`; cancelled
  path relies on the guard's `Drop` to kill + wait.
- `fn save_from_stream(stdout, layers_root)` — the original sync
  pipeline, untouched.

Extraction in the docker path was moved behind `spawn_blocking` per
layer so the pipeline future stays cooperatively schedulable.

Also added a sync `cli::is_interrupted()` poll in the auth retry
loop in `prepare` — the loop calls `credentials::prompt` which is
blocking, so it can't be reached by an async `select!`; the poll
between attempts catches the signal at retry boundaries.

A bail message audit unified everything on `"cancelled by user"`
(previously mixed `"interrupted"`).

## All-layer progress display

`ensure_registry_image` now creates a `ProgressBar` per layer
(`layer  1` … `layer  N`), not just for the slice being fetched.
Cached layers get `set_position(total)` so they render as 100% from
the moment the display appears; in-flight layers stream through the
same bar as before. The `MultiProgress` is cleared once at the end
(success or cancel). Eyeball effect: you see the whole image at a
glance and the bars drop out together once the pull completes.

## DRY + auth loop cleanup

- `read_cached_image` + layer existence check was duplicated in
  `prepare`'s fast path and `ensure_image`'s digest-keyed short
  circuit. Extracted as `read_ready_image(path) -> Option<OciImage>`
  — returns `None` for "file missing" and "file there but some
  referenced layer is gone" (both mean "must re-resolve").
- The auth retry loop used to be two nested `match`es with
  duplicated `resolve_image` calls. Rewritten as a single `loop`
  with an explicit `auth: RegistryAuth::Anonymous` starting point
  that steps through anonymous → vault creds → prompt, saving the
  prompted creds only on the eventual success. Fewer states, less
  code.

## Dead code removed

The `ImageChangeAction::Recreate` branch had leftover
`remove_dir_all(overlay)` + `remove_dir_all(ca)` calls guarded by a
spinner. `overlay` is created unconditionally on every start later
in the same function, and the `ca` directory has been dead since CA
material moved to the RPC channel. All three lines plus the spinner
are gone.
