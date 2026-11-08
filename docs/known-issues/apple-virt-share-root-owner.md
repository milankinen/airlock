# Apple Virtualization: project dir shows as owned by root

Backend: Apple Virtualization (`vmm = "apple-virt"`, the macOS default).
Not affected: libkrun and Cloud Hypervisor. They put an id map on the
shares.

## Symptom

In a sandbox with a non-root image user, the files in the project show
as owned by the container user. The project dir itself (the share root)
shows as `root:root`:

```
$ stat -c '%U:%G %n' "$PWD/host.txt" "$PWD"
curl_user:curl_group /Users/me/project/host.txt
root:root /Users/me/project
```

Possible effect: git refuses a repo in the project root as "dubious
ownership", because git checks the owner of the worktree dir. This is
not yet checked on a Mac.

## Cause

The Apple share shows each file as owned by the user that reads it. The
share root is an exception: it shows as root. airlock puts no id map on
Apple shares, so nothing changes the owner of the share root.

## Test

`tests/vm/mounts.bats` "project files show as owned by non-root
container user" is skipped on macOS until this is fixed.

## Possible fixes

1. Put an id map on the Apple shares that changes only the share root.
   This is not easy: an id map changes all files with the same owner,
   and the share shows the other files as owned by their reader.
2. Set git `safe.directory` for the project dir in the guest when the
   backend has no id map. This fixes git, but `stat` still shows root.
