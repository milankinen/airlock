# Data directory is explicit, no global

## Problem

The data directory was in a process-wide global. `Context::load` set it
after the move of the old cache (`cache::migrate_legacy_cache`). Code
that read the global before that point got the default directory. The
global did not show the error: it fell back to the default without
notice. Thus a new early use of the data directory could skip the
move or use the wrong directory.

## Change

- `main` loads the settings first (`Settings::load`), then moves the
  old cache, then makes the context with `Context::new(settings,
  data_dir)`. No code uses the data directory before this point.
- The global and `cache::data_dir()` are gone. Each `cache` path
  function takes the data directory as its first argument. The callers
  get it from `Context.data_dir`.
- `Context::new` creates the data directory with mode 0700. Before,
  each `cache::data_dir()` call did this.
