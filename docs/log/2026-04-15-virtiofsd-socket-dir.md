# Move virtiofsd sockets into sandbox/vfs/ subdirectory

After the sandbox state refactoring moved `runtime_dir` from
`~/.cache/airlock/<project>/` to `.airlock/sandbox/`, virtiofsd
sockets were created alongside overlay dirs and other sandbox files.
The socket path `vfs-files/rw.sock` also broke because the `/` in
the VirtioFS tag `files/rw` created a nested directory.

Fix: virtiofsd sockets now go into a dedicated `sandbox/vfs/`
subdirectory with tag slashes replaced by dashes in the filename
(e.g. `vfs/files-rw.sock`). Also fixed a pre-existing clippy
`needless_return` lint.
