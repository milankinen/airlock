# `airlock self-update [--check]`

Airlock can update its own binary to the latest GitHub release.

## Change

- New command `airlock self-update`. With `--check`, it only tells if a
  newer release is available.
- `self_update` module:
  - The latest version comes from the redirect of
    `github.com/<repo>/releases/latest` to `.../releases/tag/<version>`.
    The GitHub API is not used: it has a limit of 60 requests per hour
    without a sign-in, and the airlock sandbox network blocks it.
  - Versions (`vYYYY.M.N`) compare as numbers.
  - The update downloads the same archive and `.sha256` file that
    `install.sh` uses, for the same platform and variant (bundled or
    distroless). It checks the checksum, writes the binary next to the
    current binary, runs it with `-V` and checks the version. Then a
    rename replaces the current binary. Symlinks to the binary resolve
    first, so the update replaces the real file.
- The command runs before the settings load. Thus a bad settings file
  cannot block an update.
- Development builds (no version in the version slot) refuse the update.
- `reqwest` is now a direct dependency. `oci-client` already used it.
- Manual: new short "Update" section in `usage.md`.
