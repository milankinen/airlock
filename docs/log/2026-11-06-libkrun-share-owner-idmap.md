# libkrun shares: host user shows as guest root

## Symptom

With the libkrun backend, all virtiofs files (project, dir mounts, rootfs
layers) showed the host uid and gid (for example 503/20) in the guest. The
guest runs as root, so git refused every repository with "detected dubious
ownership". Tools that call git broke too (hatch version plugins).

## Cause

libkrun's macOS passthrough (default `LinuxComplete` semantics) reads the
owner from the `user.containers.override_stat` xattr. If a file has no
xattr, the guest sees the host owner. libkrun writes the xattr when the
guest creates a file or does chmod/chown. So the ownership was mixed: host
files showed 503, files that guest root made showed 0.

The other backends do not have this problem:

* Cloud Hypervisor: virtiofsd `--translate-uid map:0:<host_uid>:1`.
* Apple Virtualization: the share shows files as root.

libkrun has no uid translation. `LinuxSimplified` semantics (owner = the
uid of the calling process) turns off xattrs and the attribute cache. That
breaks the overlay whiteouts on the layer shares and makes each `stat` a
host request, so it is not used.

## Fix

Guest init mounts each share with an idmap (kernel idmapped mounts):

* Each backend gives the id maps for its shares (`id_map`). Only libkrun
  gives maps. The host sends them in the optional `idMaps` field of
  `Supervisor.boot`. Null means no idmap, so the other backends do not
  change. A map that changes no id (for example the image map of a root host
  user) makes no namespace.
* airlockd makes two user namespaces. Their maps swap the host id with a
  guest id and keep all other ids (`<host> <guest> 1`, `<guest> <host> 1`,
  plus identity ranges up to 4294967294):
  * `layers` (image rootfs): host ↔ `0:0`. `/etc`, `/usr` and the other
    image files must be owned by root, else for example `sudo` refuses
    `/etc/sudoers`.
  * Project, `dir_N`, `files/rw`, `files/ro`: host ↔ container
    `uid:gid`. A non-root image user then owns the project files, can
    write them, and git accepts the repo.
* A forked child unshares the namespace. The parent writes the maps and
  opens `/proc/<pid>/ns/user`. The child does only raw syscalls, because
  airlockd can have other threads.
* For each share, with the new mount API: `fsopen("virtiofs")`,
  `fsconfig(source)`, `fsconfig(CMD_CREATE)`, `fsmount` (a detached
  mount), `fstatfs`, `mount_setattr(MOUNT_ATTR_IDMAP)`, `move_mount`. The
  kernel sets an idmap only on a detached mount, and `mount(2)` has no
  idmap option. So each share has one mount. Shares with no map use the
  same steps without `mount_setattr`.

Result in the guest:

* Files of the host user show as root (layers) or as the container user
  (user data shares).
* Files that this guest user creates are stored as the host user.
* Files of the swapped guest id show as the host id. All other ids stay
  the same.

Apple Virtualization shows each file as owned by the calling process (all
users own all files). An idmap is fixed for all callers, so libkrun cannot
copy this. Other guest users (for example root running git in a non-root
image) see the project as owned by the container user. libkrun's
`LinuxSimplified` semantics would copy VZ, but see above.

The image layers are overlayfs lowerdirs. Overlayfs reads them with the
credentials of the mounter (root), and a lower file shows one owner for
all callers. Copy-up keeps that owner. So the layer map sets the rootfs
owners for every guest user.

Details:

* `fstatfs` first: FUSE clears `SB_I_NOIDMAP` only when the INIT reply
  arrives with `FUSE_ALLOW_IDMAP`. INIT is asynchronous after the create,
  and a request waits for it.
* An idmap error stops the boot. In our setup the kernel refuses nothing:
  the guest kernel has user namespaces, the host ids are valid, and
  libkrun allows idmaps. A refusal means a bug or a libkrun change, and
  wrong owners with only a warning would hide it.
* The only id that the kernel refuses in a map is 4294967295. The host
  refuses an image `USER` with this id when it resolves the user.
* Overlayfs accepts idmapped lower layers, so the rootfs works.
* The libkrun passthrough allows idmaps only with `LinuxComplete`
  semantics, which is the default.

### Why a swap, not only `<host> 0 1`

With only one map line, all other ids are unmapped in the idmapped mount.
Then a non-root image user (for example uid 1000) cannot create files in a
share, and `chown` to any id other than 0 fails (`EOVERFLOW`). Before the
idmap, libkrun stored such owners in its xattr. The swap keeps that
working.

## Testing

* In a libkrun guest (kernel 6.18): `mount --bind -o X-mount.idmap=...` on
  a share showed host files as `0:0`, and new files as `0:0`. That test
  used a copy of an existing mount. The `fsmount` path with an idmap is not
  tested on libkrun yet. On Apple Virtualization it mounts plain shares,
  and with a map it gets to `mount_setattr`, where VZ refuses the idmap.
* In a guest as root: the user namespaces and the idmaps on a tmpfs gave
  the results above, for container `0:0` and `1000:1000`. Also `chown` to
  1000 worked and was stored as 1000, and uid 1000 could write in the user
  data share. On an Apple Virtualization share the kernel refused the
  idmap (`EINVAL`), so only libkrun gets maps.
* A unit test checks that the id map covers each id one time on both
  sides and swaps only the two ids.
* The full boot path on libkrun is not tested yet. Two bats tests in
  `tests/vm/libkrun.bats` cover it on macOS arm64: a root image and a
  non-root image (`curlimages/curl`, user `curl_user`).
* Image owners in the rootfs: see `2026-11-06-layer-owner-xattr.md`.

## Known effect

Files that guest root created under libkrun before this fix have the xattr
`0:0`. Through the swap they show as the host uid, and git can refuse them.
This affects `.git` dirs of repos that the guest cloned or made before the
fix. Only earlier libkrun test sessions made such files. Fix them one time
in the guest with `chown -R 0:0 <dir>`.

The host sends no share owner when the host uid or gid is 0, because root
needs no map.
