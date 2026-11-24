# Git

The `git` pack installs Git with an SSH client.

```toml
[packs]
git = { version = "1" }
```

The pack has no args. It allows no hosts. Add a network rule for your Git
server, for example `github.com`.

The pack sets the Git commit author and committer to the values of the
host. At startup, Git reads the repository settings and applies matching
conditional includes (`includeIf`) from the host's global config.
The host variables `GIT_AUTHOR_*` and `GIT_COMMITTER_*` take precedence.
Changes to `.git/config` inside the sandbox can affect the identity at
the next start.
