# Plan: Replace crun with Direct Process Spawning

## Overview

Remove crun, the OCI bundle (`config.json`), and the `/ez/oci-run` / `/ez/oci-exec`
wrapper scripts. The supervisor takes over all mounts that crun was doing, then
fork+chroot+exec's each process directly. The capnp protocol is extended to carry
env/uid/gid/cwd that previously lived in `config.json`.

---

## 1. What We're Removing

### Files deleted entirely

- `vm/rootfs/ez/oci-run`
- `vm/rootfs/ez/oci-exec`
- `crates/ez/src/oci/config.rs` — `generate_config`, `parse_user`; entire module

### Code paths eliminated

**`crates/ez/src/oci.rs` (`build_bundle`):**
- Call to `config::generate_config(...)` and its argument block
- `pub(crate) mod config;` declaration

**`crates/ez/src/rpc/supervisor.rs`:**
- `build_command()` — returns `("/ez/oci-run", [])` — gone entirely
- `build_exec_command(...)` — builds crun-exec arg list — gone entirely
- `EZ_DEV_NO_CRUN` dev-mode escape hatch in both functions

**`crates/ez/src/cli_server.rs`:**
- `build_exec_command` call and import; `cwd`/`env` now sent directly to supervisor

**`vm/rootfs/build.sh`:**
- `crun` from the `apk add` line
- `cp /ez-oci-run` and `cp /ez-oci-exec` lines and the `/ez` mkdir

---

## 2. Protocol Changes

```capnp
interface Supervisor {
  start @0 (
    stdin      :Stdin,
    pty        :PtyConfig,
    network    :NetworkProxy,
    logs       :LogSink,
    logFilter  :Text,
    epoch      :UInt64,
    hostPorts  :List(UInt16),
    sockets    :List(SocketForward),
    # --- new: replaces config.json ---
    cmd        :Text,
    args       :List(Text),
    env        :List(Text),        # "KEY=value" strings
    cwd        :Text,
    uid        :UInt32,
    gid        :UInt32,
    nestedVirt :Bool,              # expose /dev/kvm
  ) -> (proc :Process);

  shutdown @1 () -> ();

  exec @2 (
    stdin  :Stdin,
    pty    :PtyConfig,
    cmd    :Text,
    args   :List(Text),
    cwd    :Text,                  # new: was encoded in crun args
    env    :List(Text)             # new: was encoded in crun args
  ) -> (proc :Process);
}
```

**Changes vs current:**
- `start`: adds `env`, `cwd`, `uid`, `gid`, `nestedVirt`; removes `tlsPassthrough`
  (unused). `cmd`/`args` now carry the real container command, not `/ez/oci-run`.
- `exec`: adds `cwd` and `env` as first-class fields (previously packed into crun args).
- `CliService.exec`: no change needed.

**Capnp field numbering:** existing ordinals are unchanged; new fields appended at the
next available ordinals.

---

## 3. Guest (ezd) Changes

### 3a. New `init::setup_container_mounts` (linux.rs)

Called from `main.rs` after `init::setup()` but before `process::spawn_in_container`,
once the overlayfs is assembled at `/mnt/overlay/rootfs`.

Does all mounts that crun's config.json was doing:

1. **`/proc`** — `mount("proc", "/mnt/overlay/rootfs/proc", "proc", MS_NOSUID|MS_NOEXEC|MS_NODEV)`
2. **`/sys`** — `mount("sysfs", "/mnt/overlay/rootfs/sys", "sysfs", MS_RDONLY|...)`
3. **`/dev`** — recursive bind-mount host `/dev` → `/mnt/overlay/rootfs/dev` (avoids
   mknod; all standard devices already present in VM)
4. **`/dev/pts`** — `mount("devpts", "/mnt/overlay/rootfs/dev/pts", "devpts", ...)`
5. **`/dev/shm`** — `mount("shm", "/mnt/overlay/rootfs/dev/shm", "tmpfs", "mode=1777")`
6. **`/.ez/files_rw`** (if any file mounts) —
   `bind_mount("/mnt/overlay/files_rw", "/mnt/overlay/rootfs/.ez/files_rw", false)`
7. **`/.ez/files_ro`** (if any read-only file mounts) —
   `bind_mount("/mnt/overlay/files_ro", "/mnt/overlay/rootfs/.ez/files_ro", true)`
8. **Socket forwards** — for each `(host_sock, guest_path)`: create socket file at
   `/mnt/disk/sockets/<name>` as placeholder, then
   `bind_mount("/mnt/disk/sockets/<name>", "/mnt/overlay/rootfs/<guest_path>", false)`.
   `net::socket::start` already calls `remove_file` before binding, so the placeholder
   is replaced safely.
9. **`/dev/kvm`** (if `nested_virt`) — `bind_mount("/dev/kvm", "/mnt/overlay/rootfs/dev/kvm", false)`

**Call-site split:** `init::setup()` handles everything up to `assemble_rootfs` + DNS
(as today). `setup_container_mounts(mounts_info, sockets, nested_virt)` is a new
separate call in `main.rs`'s init closure (which already has access to `sockets` from
the RPC params). `assemble_rootfs` returns a `ContainerMountInfo { has_rw_files: bool,
has_ro_files: bool }` to avoid exposing the internal `MountsConfig` struct.

### 3b. New `process::spawn_in_container` (process.rs)

Replaces `spawn(cmd, args, pty_size)`. Uses `CommandExt::pre_exec` to run in the child
after fork but before exec:

```rust
// pre_exec runs in child: async-signal-safe calls only
let pre = move || unsafe {
    libc::chroot(c"/mnt/overlay/rootfs".as_ptr());  // chroot first
    libc::chdir(cwd_cstr.as_ptr());                  // cwd relative to new root
    libc::setgid(gid);                               // setgid BEFORE setuid
    libc::setuid(uid);
    Ok(())
};
```

Environment: `Command::env_clear().envs(env_pairs)` — container gets only its declared env.

PTY path: `pty_process::Command` wraps `std::process::Command`; verify `pre_exec` is
accessible (it is via `DerefMut<Target = std::process::Command>`). If not, open the PTY
pair manually and use a plain `std::process::Command`.

**Exec sidecar:** Same `spawn_in_container` call. `uid`/`gid` come from supervisor
state stored after the `start` call (exec inherits the container's default user).

### 3c. rpc.rs changes

- `SupervisorImpl::start`: decode `env`, `cwd`, `uid`, `gid`, `nestedVirt`; pass to
  `setup_container_mounts` and `spawn_in_container`; store `uid`/`gid` in supervisor
  state for exec reuse.
- `SupervisorImpl::exec`: decode `cwd`, `env`; call `spawn_in_container` (no crun).
- `HostConnection` gains: `env: Vec<String>`, `cwd: String`, `uid: u32`, `gid: u32`,
  `nested_virt: bool`.

---

## 4. Host (ez) Changes

### 4a. oci.rs (`build_bundle`)

**Removed:** `config::generate_config(...)` call.

**Added:** the env/cmd/uid/gid assembly logic currently in `config.rs` moves here:
- `args` resolution (entrypoint + cmd merge, user override)
- `env` assembly (defaults + image env + user env with `subst::substitute`)
- `parse_user(config_user)` → uid/gid
- `cwd` from `project.guest_cwd`

`Bundle` (or a new `ProcessConfig` returned alongside it) gains:
`cmd`, `env: Vec<String>`, `cwd: String`, `uid: u32`, `gid: u32`.

`config.rs` is then empty and deleted.

### 4b. rpc/supervisor.rs

- `build_command` and `build_exec_command` removed.
- `Supervisor::start` gains: `cmd`, `env`, `cwd`, `uid`, `gid`, `nested_virt` params.
- `Supervisor::exec` gains: `cwd: &str`, `env: &[String]` params. `cmd`/`args` are the
  raw user command (not a crun invocation).

### 4c. cli_server.rs

`CliServiceImpl::exec` calls `supervisor.exec(stdin, pty_size, cmd, args, cwd, env)`
directly. The `build_exec_command` call is removed.

---

## 5. Rootfs Changes

```diff
-apk add --no-cache busybox-extras iproute2 cpio crun iptables e2fsprogs e2fsprogs-extra tar
+apk add --no-cache busybox-extras iproute2 cpio iptables e2fsprogs e2fsprogs-extra tar

-cp /ez-oci-run /ez/oci-run && chmod 755 /ez/oci-run
-cp /ez-oci-exec /ez/oci-exec && chmod 755 /ez/oci-exec
# remove the mkdir /ez line if nothing else uses /ez
```

`/init` is unchanged.

---

## 6. Feature Parity Checklist

| Feature | Current | New |
|---|---|---|
| `/proc` mount | crun via config.json | `setup_container_mounts` step 1 |
| `/sys` mount | crun via config.json | `setup_container_mounts` step 2 |
| `/dev` mount + devices | crun tmpfs + device nodes | bind-mount host `/dev` recursively |
| `/dev/pts` | crun devpts | `setup_container_mounts` step 4 |
| `/dev/shm` | crun tmpfs | `setup_container_mounts` step 5 |
| `/.ez/files_rw` bind | crun via config.json | `setup_container_mounts` step 6 |
| `/.ez/files_ro` bind | crun via config.json | `setup_container_mounts` step 7 |
| File symlinks | supervisor `assemble_rootfs` | unchanged |
| Dir mounts (virtiofs) | supervisor `assemble_rootfs` | unchanged |
| Cache mounts | supervisor `assemble_rootfs` | unchanged |
| Socket forwards bind | crun via config.json | `setup_container_mounts` step 8 |
| Env vars | config.json → crun | `build_bundle` → start RPC → `pre_exec` `env_clear + envs` |
| uid/gid | config.json → crun `setuid/setgid` | start RPC → `pre_exec` `setgid + setuid` |
| cwd | config.json → crun `chdir` | start RPC → `pre_exec` `chdir` (after `chroot`) |
| chroot | crun `--no-pivot` = `chroot` | `pre_exec` `chroot("/mnt/overlay/rootfs")` |
| PTY allocation | crun (terminal:true in config.json) | unchanged: `pty_process` in supervisor |
| PTY resize | via crun | unchanged: supervisor-owned PTY |
| Exec sidecar | `crun exec` → enters container ns | `spawn_in_container` with stored uid/gid |
| Signal forwarding | unchanged (supervisor relays) | unchanged |
| `/dev/kvm` (nested virt) | crun device node via config.json | `setup_container_mounts` step 9 |
| DNS | `setup_dns` writes resolv.conf | unchanged |
| Networking (iptables) | `setup_networking` | unchanged |
| Overlayfs / image layers | `assemble_rootfs` | unchanged |
| CA cert injection | overlayfs ca lowerdir | unchanged |
| Hostname | crun UTS namespace ("ezpez") | already set by `/init`; no UTS NS needed |

---

## 7. Risk Areas

### 7.1 Socket forward bind-mount timing

Socket files in `/mnt/disk/sockets/` are created lazily. `setup_container_mounts` must
pre-create them (as empty files or real sockets). `net::socket::start` already calls
`remove_file` before `UnixListener::bind`, so pre-creation is safe. The bind-mount
shares the inode, so the container sees the listener socket at `<guest_path>`.

### 7.2 `pre_exec` safety in async tokio

`pre_exec` runs post-fork, pre-exec. Only async-signal-safe calls allowed: `chroot`,
`chdir`, `setgid`, `setuid` all qualify. Tokio's existing use of `tokio::process::Command`
relies on the same mechanism.

### 7.3 `setgid` before `setuid`

`setgid` must be called first; once uid is dropped from root, gid cannot be changed.

### 7.4 `/dev` bind vs tmpfs+mknod

Recursive bind of host `/dev` exposes all VM devices to the container. Acceptable since
VM is the security boundary. The `/dev/kvm` bind (step 9) is subsumed by the recursive
bind, but explicit handling is kept for clarity and for the case where `/dev` is NOT
recursively bound.

### 7.5 `pty_process::Command` pre_exec access

Verify `pty_process::Command` exposes `CommandExt::pre_exec` via `DerefMut`. If not,
open the PTY pair manually (`pty_process::open()`) and use `std::process::Command`
directly with the pts as stdin/stdout/stderr — the relay logic in `process.rs` is
unchanged.

### 7.6 Dev mode escape hatch

`EZ_DEV_NO_CRUN=true` currently bypasses crun. Without crun this is moot. If a dev
shortcut is needed, it can skip `setup_container_mounts` and the `pre_exec`
chroot/setuid block — just `spawn()` directly as before.

---

## Implementation Order

1. Update `supervisor.capnp`; regenerate `supervisor_capnp.rs`
2. Move env/cmd/uid/gid logic from `config.rs` → `oci.rs`; delete `config.rs`
3. Update `rpc/supervisor.rs` — remove `build_command`/`build_exec_command`, set new fields
4. Update `cli_server.rs` — remove `build_exec_command`, call `supervisor.exec` directly
5. Implement `init::setup_container_mounts` in `ezd/src/init/linux.rs`
6. Implement `process::spawn_in_container` in `ezd/src/process.rs`
7. Update `ezd/src/rpc.rs` — decode new fields, thread to new functions, store uid/gid
8. Update `ezd/src/main.rs` — thread new params through init closure
9. Handle socket pre-creation in `net/socket.rs`
10. Update `vm/rootfs/build.sh` — remove crun, oci-run, oci-exec
11. Run feature parity checklist
