# Sandbox templates

A template is a saved image and disk with installed packs. New sandboxes
start from a copy of the disk.

## Change

- New command `airlock templates` (alias `template`) with `list` (`ls`),
  `info <name>`, `create <name> [-f] [--log-level L] [-v]` and
  `remove <name> [-f]` (`rm`). Code: `cli/cmd_templates.rs`.
- `create` fails if the template exists. With `--force`, it re-creates
  the template. The old template stays until the new build is ready.
  Then a rename of a staging directory replaces it.
- New `templates` module:
  - `mod.rs`: storage, list, find and remove. A removal also removes the
    cached images that nothing uses.
  - `create.rs`: the build. It runs the wizard, gets the image, installs
    the packs on a new disk and saves the template.
  - `seed.rs`: copies the template into a new sandbox.
- Storage is `<data_dir>/templates/<name>/` with `airlock.toml` (the
  wizard config, `vm.image` pinned to `name@digest`, if known), `disk.img`,
  `disk.id`, `installs.json`, `ca.json` and `image` (a link that keeps the image
  from cleanup). The build runs in `.build-<name>/`. After a failed
  build only `sandbox/installs.log` stays there.
- Template names: 1 to 64 characters of letters, digits, `-`, `_` and
  `.`. The first character is a letter or a digit.
- New `util::clone`: reflink copy (`FICLONE` ioctl on Linux, `clonefile`
  on macOS). The fallback is a sparse copy with `SEEK_DATA` and
  `SEEK_HOLE`.
- Setup wizard:
  - New `Mode` (start or template). In template mode the wizard has no
    `Create airlock.toml` question and the start bar says "create".
  - New "Template" radio group, first section, only if templates exist.
    A selected template selects its packs with its argument values and
    grays them and the base image group out.
  - `GeneratedConfig.template` holds the choice.
- `ca.json` is the CA keypair of the build sandbox. A new sandbox gets a
  copy. Pack installs can run `update-ca-certificates`, which writes the
  CA into the CA bundles on the disk. The sandbox must keep the same CA.
- A new sandbox gets a copy of the template disk with a new disk id and
  the template install records. Only the extra packs install.
- If the first start from a template fails before the sandbox is ready,
  the copied disk, records, CA and image link are removed. The next start
  runs the wizard again.
- If the sandbox image is not the template image, the sandbox starts with
  an empty disk and installs all packs.
- The template config pins the image (`vm.image` is `name@digest`) when
  the image comes from a registry, also with the default `auto`
  resolution. An image from a local docker or podman engine stays
  unpinned: its image ID is not a registry digest, and a recorded
  registry digest does not match the cached image ID. The cached image
  stays until the pull policy (`if-changed`) asks the source again.
- All sandboxes from one template, and templates built on it, share the
  template CA keypair. The private key stays on the host in a file with
  mode 0600. A sandbox without a template gets its own CA.
- The image cleanup (GC sweep) first repairs the `image` hardlink of each
  template. A pull of the same image under another name writes the cache
  entry again with a new inode. The old link then no longer protects it.
- `SandboxLock::acquire_dir` locks a directory that is not yet a sandbox.
- The image step takes the cached-image fast path also when the pinned
  digest equals the cached image id. The cache entry can have another
  name. Thus a moved tag does not ask to re-create the sandbox.
- A sandbox in the project directory can be on another file system than
  the data directory. The reflink fails there and the copy is a normal
  copy.
- Manual: new top-level page `templates.md`, links in the wizard section and
  in the project layout page.
