# Add directory and file mounting via VirtioFS

### What

Mount the project directory and configurable paths into the container
via VirtioFS shares + bind mounts. Container shell starts in the
project directory (same absolute path as host).

### How it works

1. CLI collects mounts: project CWD (always) + config.mounts
2. Each directory becomes a VirtioFS share with a unique tag
3. File mounts: hard-linked into `~/.ezpez/projects/<hash>/files_{rw,ro}/`,
   shared as a VirtioFS directory. Bidirectional sync via hard links.
   Falls back to copy (with warning) on cross-device.
4. VirtioFS tags passed to guest via kernel cmdline (`ezpez.shares=...`)
5. Init script parses cmdline, mounts each tag at `/mnt/<tag>`
6. config.json bind-mounts from `/mnt/<tag>` into container paths

### Mount order (latter shadows former)

1. Project directory (CWD → same absolute path in container)
2. Config mounts in definition order

### Key decisions

- **One VirtioFS device per share** (not VZMultipleDirectoryShare) —
  simpler, and each share can independently be read-only or read-write.
- **Hard-link for file mounts** — same inode = bidirectional sync.
  Separate `files_rw/` and `files_ro/` directories since VirtioFS
  share read-only is set at the share level.
- **Tags via kernel cmdline** — `/sys/fs/virtiofs/*/tag` sysfs
  attribute doesn't exist in our kernel config. Cmdline is reliable.
- **Asset cache invalidation** — compare embedded binary size with
  cached file size to detect binary updates. Fixes stale initramfs
  after supervisor rebuild.
- **Container CWD = host CWD** — config.json `process.cwd` set to
  the canonical project path so the shell starts in the project dir.
