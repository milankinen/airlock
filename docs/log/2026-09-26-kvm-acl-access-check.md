# Honor ACLs when checking KVM access

## Problem

The preflight check only compared Unix mode bits against the user's UID
and groups, so it ignored ACLs. With `/dev/kvm` owned by `root:root` and
a named-user ACL granting access, it reported "permission denied"
although the device could be opened. The supplementary-group
branch also accepted group membership without checking the group
read/write bits.

## Change

`kvm_status` now opens `/dev/kvm` read/write and closes it again. The
kernel decides access, so ACLs and any other policy apply. `NotFound`
and `PermissionDenied` map to the existing statuses; any other open
error becomes `KvmStatus::Unavailable` and is shown as-is.

## Tests

`kvm_status_classifies_open_results` runs the check against a regular
file in a temp directory: missing, openable, read-only, write-only, and
a directory (open fails with a non-permission error). The permission
cases are skipped as root. `tempfile` is added as a dev-dependency.
