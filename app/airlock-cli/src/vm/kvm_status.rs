//! KVM access check.
//!
//! Checks if the current user can use the KVM device of the host, and tells
//! the reason when the user cannot use it.

use crate::util::VerifiedPath;
use crate::{cli, messages};

/// Result of the KVM access check.
pub enum KvmStatus {
    /// `/dev/kvm` can be opened for read and write.
    Available,
    /// `/dev/kvm` does not exist.
    NotFound,
    /// The user has no permission to open `/dev/kvm`.
    NoPermission,
    /// `/dev/kvm` cannot be opened for a different reason.
    Unavailable(std::io::Error),
}

/// Check if the current user can use `/dev/kvm`.
pub fn kvm_status() -> KvmStatus {
    kvm_status_at("/dev/kvm")
}

/// Check if the current user can open the KVM device at `path`.
fn kvm_status_at(path: impl AsRef<std::ffi::OsStr>) -> KvmStatus {
    // The device path is fixed. On some systems it is a symlink.
    let opened = VerifiedPath::new_follow(path.as_ref(), None).and_then(|path| {
        // Open the file instead of reading the mode bits, so the kernel
        // also applies ACLs.
        path.open_with(std::fs::OpenOptions::new().read(true).write(true))
    });
    match opened {
        Ok(_) => KvmStatus::Available,
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => KvmStatus::NotFound,
            std::io::ErrorKind::PermissionDenied => KvmStatus::NoPermission,
            _ => KvmStatus::Unavailable(err),
        },
    }
}

/// Make sure that KVM is available. If not, show the reason and exit the
/// process.
pub fn require_kvm() {
    match kvm_status() {
        KvmStatus::Available => {}
        KvmStatus::NotFound => {
            cli::error!("{}", messages::vm::KVM_NOT_FOUND);
            cli::error!("{}", messages::vm::KVM_ENABLE_HINT);
            std::process::exit(1);
        }
        KvmStatus::NoPermission => {
            cli::error!("{}", messages::vm::KVM_NO_PERMISSION);
            cli::error!("{}", messages::vm::KVM_PERMISSION_HINT);
            std::process::exit(1);
        }
        KvmStatus::Unavailable(err) => {
            cli::error!("{}", messages::vm::kvm_open_failed(err));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{KvmStatus, kvm_status_at};
    use crate::test_cfg::temp_dir;

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
}
