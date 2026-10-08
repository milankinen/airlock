//! Tests for the KVM device status check.

use std::os::unix::fs::PermissionsExt;

use crate::test_cfg::temp_dir;
use crate::vm::{KvmStatus, kvm_status_at};

/// Test that the KVM status follows the result of an open of the device
/// file, so that the user gets the correct reason when KVM is not usable.
///   1. Check that a missing device gives "not found"
///   2. Create the device file and check that it gives "available"
///   3. Make the file read-only, then write-only, and check "no permission"
///   4. Check that a directory gives "unavailable"
#[test]
fn kvm_device_status_follows_what_opening_it_reports() {
    let dir = temp_dir();
    let path = dir.path().join("kvm");
    assert!(matches!(kvm_status_at(&path), KvmStatus::NotFound));

    std::fs::write(&path, b"").unwrap();
    assert!(matches!(kvm_status_at(&path), KvmStatus::Available));

    // Root can open the file with any mode, so skip this part as root.
    if unsafe { libc::geteuid() } != 0 {
        for mode in [0o400, 0o200] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(kvm_status_at(&path), KvmStatus::NoPermission));
        }
    }

    // A directory cannot open read-write, which is a different error.
    assert!(matches!(
        kvm_status_at(dir.path()),
        KvmStatus::Unavailable(_)
    ));
}
