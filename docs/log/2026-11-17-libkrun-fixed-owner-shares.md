# libkrun shares: fixed owner semantics

## Symptom

With the libkrun backend, a file that the guest made in a share had
another mode on the host than in the guest:

```
guest$ touch foo && chmod 755 foo && ls -l foo
-rwxr-xr-x 1 root root 0 Nov 17 18:30 foo
host$ ls -l foo
-rw-------@ 1 mla staff 0 Nov 17 20:30 foo
```

Scripts that the guest made were not executable on the host.

## Cause

libkrun's macOS passthrough (default `LinuxComplete` semantics) gives the
guest full Linux semantics on a macOS dir. The libkrun process runs as the
host user, so it cannot set other owners or make special files on the
host. It keeps the guest uid, gid and mode in the
`user.containers.override_stat` xattr and shows only the xattr to the
guest. New files are `0600` and new dirs `0700` on the host, and chmod in
the guest changes only the xattr. With these modes the process can always
serve guest root, which ignores the mode bits.

## Options

1. `LinuxSimplified`: host mode bits, each file shows as owned by the
   caller (like Apple Virtualization). libkrun turns off the attribute
   cache, because the owner depends on the caller. No idmaps.
2. A libkrun patch: in `LinuxComplete`, also copy the mode bits to the
   host. airlock then keeps a patch on libkrun.
3. `LinuxSimplifiedFixedOwner` (new in libkrun `main`): host mode bits,
   one fixed owner for all files and callers, so the attribute cache
   stays on. No idmaps.

A dev switch set the semantics of the project and dir shares. See
`2026-11-17-libkrun-update.md` for the new pin. The same
scripts ran in the guest in each mode (median ms, 1068-file repo, 5000
small files). `complete` ran on the old libkrun pin, the others on the
new pin:

| Test                 | simplified | complete | fixed |
| -------------------- | ---------: | -------: | ----: |
| stat 5k files (warm) |        530 |       96 |    43 |
| stat 2k missing      |        675 |      118 |   106 |
| read 5k small files  |       3328 |      923 |   930 |
| chmod 5k files       |       3583 |      631 |   485 |
| create 5k files      |       4807 |     3689 |  4166 |
| git status (warm)    |        508 |       76 |    58 |
| git add -A + reset   |        659 |       85 |    69 |
| rg (warm)            |        696 |      102 |    92 |
| git clone            |       3283 |     2003 |  2762 |
| npm-like extract 2k  |       3182 |     2721 |  2724 |
| cargo check (clean)  |       4043 |     3977 |  1371 |

Without the attribute cache, each stat and lookup is a host request, so
`simplified` was 5-8x slower on metadata-heavy tools. `fixed` was as fast
as `complete` or faster, except file creation (10-35% slower, short runs).
The `cargo check` gain comes most likely from the new pin (transparent
huge pages for guest RAM), not from the semantics.

## Fix

The user data shares (project, `dir_N`, `files/rw`, `files/ro`) use
`LinuxSimplifiedFixedOwner` with the container user as the owner. The
image layers keep `LinuxComplete` and the root idmap: they need the image
owners and the setuid bits from the xattrs, and overlayfs whiteouts.

* The VM description has the container user and marks the user data
  shares. The libkrun backend sets the semantics and the owner on these
  shares. It sets the owner after the semantics, because a new semantics
  clears the owner.
* libkrun refuses idmaps with these semantics. The libkrun backend gives
  a user data map that changes no id, so guest init makes no idmap on
  these shares. The image map does not change.

Effects:

* The container user owns all files in the user data shares, also the
  share root. Other guest users (for example root in a non-root image)
  see the same owner. This is the same as with the idmap.
* chown in the guest has no effect.
* Guest modes are host modes. A guest `chmod 000` gives `000` on the
  host. The host checks the mode for the libkrun process, so guest root
  can then get `EACCES` (not tested).
* Files that the guest made with the old semantics show their host mode
  (usually `0600`) in the guest. The guest mode in the xattr is ignored.
  A chmod fixes them.
* The guest can make files that are executable on the host, for example
  git hooks. Apple Virtualization already permits this.

## Testing

* Rust tests pass. A bats test checks that a guest chmod shows on the
  host (macOS arm64 only).
* Manual: with the dev switch on macOS, the guest modes showed on the
  host, and the benchmark above ran.
* The final code (no dev switch) is not run on macOS yet.
