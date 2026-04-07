# Fix initial PTY size for container shell

The container shell always started with the wrong terminal size. Three
bugs combined:

1. `crossterm::terminal::size()` returns `(cols, rows)` but the initial
   size was destructured as `(rows, cols)` — rows and cols swapped.

2. The PTY was resized after `spawn()`, but the process had already read
   the default size. Fixed by resizing before spawn.

3. Even with the outer PTY sized correctly, crun creates its own PTY for
   the container. Without `consoleSize` in the OCI config.json, crun
   uses the kernel default. Added `consoleSize` (height/width) to the
   OCI spec so crun sets the container PTY correctly at creation.

Added debug logging at all levels: host initial size, host resize,
supervisor initial PTY size, supervisor PTY resize events.
