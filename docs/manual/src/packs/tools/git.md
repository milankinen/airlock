# Git

The `git` pack installs Git with an SSH client.

```toml
[packs]
git = { version = "1" }
```

The pack has no args. It allows no hosts. Add a network rule for your Git
server, for example `github.com`.

The pack sets the Git commit author and committer from the host.
airlock uses the first value that it finds, in this order:

1. The host environment variables `GIT_AUTHOR_*` and `GIT_COMMITTER_*`
2. The Git config keys `author.*` and `committer.*`
3. The Git config keys `user.*`

airlock reads the Git config of the project repository. Thus sandboxes
of different repositories can have different authors.
