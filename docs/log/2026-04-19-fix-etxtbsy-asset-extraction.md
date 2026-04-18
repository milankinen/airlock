# Fix ETXTBSY on asset re-extraction

When the airlock binary is rebuilt (checksum changes), asset extraction
overwrites `cloud-hypervisor` and `virtiofsd` in `~/.cache/airlock/kernel/`.
If a previous VM is still running, the kernel refuses to write to a running
executable (ETXTBSY / errno 26).

Fix: write to a temp file (`.cloud-hypervisor.tmp`) then `rename()` into
place. Rename is atomic at the directory-entry level — the old inode stays
valid for the running process, and new invocations pick up the new binary.
