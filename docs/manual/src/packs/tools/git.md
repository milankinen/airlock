# Git

The `git` pack installs Git with an SSH client.

```toml
[packs]
git = { version = "1" }
```

The pack has no args. It allows no hosts. Add a network rule for your Git
server, for example `github.com`.

The pack sets the Git commit author and committer to the values of the
host.
