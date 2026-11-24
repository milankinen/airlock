# Git pack identity with conditional includes

A host config can select a commit email by remote URL. The pack used
`git config --global --includes`, which missed remotes in the repository
config. Commits in the sandbox then used the default email.
[Issue #27](https://github.com/milankinen/airlock/issues/27)

At startup, the pack runs `git config --get` in the project directory.
Git resolves conditional includes and repository settings there. Host
environment variables still take precedence.

The sandbox can edit `.git/config` and change the identity at the next
start. A repository include can also select another host Git config file.

`airlock sandboxes config <id> --resolved` runs the pack in the caller's
directory. Run it from the project to see the identity for that project.
