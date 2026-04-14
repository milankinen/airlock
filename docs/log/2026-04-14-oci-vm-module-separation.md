# OCI / VM module separation

## Problem

The `oci` module had grown to own three distinct concerns:

1. **Image resolution** — pulling/extracting the container image rootfs
2. **Mount preparation** — `ResolvedMount`, `MountType`, `resolve_mounts`
3. **Disk setup** — sparse disk image creation, cache overlay preparation

This created tight coupling: `Bundle` (returned by `oci::prepare`) carried
fully-resolved mounts, disk paths, and processed cmd/env — meaning `oci::prepare`
had to know about project config, user arg overrides, and VM-specific disk layout.
The `vm::start()` function received a fully-configured `Bundle` and had little
work left to do.

## Design

Clear module ownership:

| Module | Responsibility |
|--------|---------------|
| `oci` | Pull/extract image; return `OciImage` (rootfs path + raw metadata) |
| `vm/mount.rs` | `ResolvedMount`, `MountType`, `resolve_mounts` |
| `vm/disk.rs` | Sparse disk image + cache overlay prep |
| `project.rs` | CA cert installation into `overlay/ca/` |
| `vm::start()` | Full VM lifecycle: mount resolution, disk prep, share wiring, boot |

### `OciImage`

`oci::prepare()` now returns:

```rust
pub struct OciImage {
    pub rootfs: PathBuf,
    pub image_id: String,
    pub container_home: String,
    pub uid: u32,
    pub gid: u32,
    pub cmd: Vec<String>,   // raw image entrypoint+cmd (no user overrides)
    pub env: Vec<String>,   // base defaults + image env (no project overrides)
}
```

No mounts, no disk, no processed cmd/env. The OCI module does exactly one thing.

### `vm::start(args, project, image) -> (VmInstance, OwnedFd)`

All VM preparation happens here:
1. Install CA cert (`project.install_ca_cert(&image.rootfs)`)
2. Build project dir mount + resolve user mounts (sorted, tilde-expanded)
3. Hard-link file mounts into `overlay/files/{rw,ro}/` (copy fallback on EXDEV)
4. Prepare disk image + caches
5. Apply `args.args` / `--login` / `project.config.env` overrides to produce final cmd/env
6. Build VirtioFS share list and boot the VM backend
7. Connect vsock and return `(VmInstance, OwnedFd)`

### `VmInstance`

```rust
pub struct VmInstance {
    vm_handle: Box<dyn VmHandle>,  // private — dropping kills the VM
    pub image_id: String,
    pub mounts: Vec<mount::ResolvedMount>,
    pub disk_image: PathBuf,
    pub caches: Vec<disk::CacheEntry>,
    pub container_home: String,
    pub cmd: Vec<String>,   // fully resolved (args overrides + login shell applied)
    pub env: Vec<String>,   // fully resolved (project.config.env applied)
    pub cwd: String,
    pub uid: u32,
    pub gid: u32,
}
```

`VmInstance` is RAII: the private `Box<dyn VmHandle>` drops the VM backend on
`drop`. All fields needed by the supervisor RPC are exposed as `pub`.

### Call site (`cmd_up.rs`)

```rust
let image = oci::prepare(&args, &project, &terminal).await?;
let network = network::setup(&project, &image.container_home)?;
let (vm, vsock_fd) = vm::start(&args, &project, &image).await?;
supervisor.start(&args, &project, &vm, stdin, network, epoch, epoch_nanos).await?;
drop(vm); // kills VM
```

## Files changed

- `crates/airlock/src/oci.rs`: removed `Bundle`, `ResolvedMount`, `MountType`,
  `resolve_mounts`, `install_ca_cert`, `mod cache`, `mod tests`; added `OciImage`
- `crates/airlock/src/vm/mount.rs` (new): `ResolvedMount`, `MountType`,
  `resolve_mounts`, inline tests (moved from `oci/tests/test_resolve_mounts.rs`)
- `crates/airlock/src/vm/disk.rs` (new): moved from `oci/cache.rs`
- `crates/airlock/src/vm.rs`: `VmInstance` + full `start()` implementation
- `crates/airlock/src/project.rs`: `install_ca_cert` method
- `crates/airlock/src/rpc/supervisor.rs`: `bundle: &Bundle` → `vm: &VmInstance`
- `crates/airlock/src/network.rs`: `setup(project, container_home: &str)`
  instead of `setup(project, bundle: &Bundle)`
- `crates/airlock/src/cli/cmd_up.rs`: updated call sequence
- Deleted: `oci/cache.rs`, `oci/tests/mod.rs`, `oci/tests/test_resolve_mounts.rs`
