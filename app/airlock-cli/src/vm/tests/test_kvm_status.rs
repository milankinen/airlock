use std::os::unix::fs::PermissionsExt;

use crate::test_cfg::temp_dir;
use crate::vm::{KvmStatus, kvm_status_at};

#[test]
fn kvm_device_status_follows_what_opening_it_reports() {
    let dir = temp_dir();
    let path = dir.path().join("kvm");
    assert!(matches!(kvm_status_at(&path), KvmStatus::NotFound));

    std::fs::write(&path, b"").unwrap();
    assert!(matches!(kvm_status_at(&path), KvmStatus::Available));

    if unsafe { libc::geteuid() } != 0 {
        for mode in [0o400, 0o200] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(kvm_status_at(&path), KvmStatus::NoPermission));
        }
    }

    assert!(matches!(
        kvm_status_at(dir.path()),
        KvmStatus::Unavailable(_)
    ));
}
