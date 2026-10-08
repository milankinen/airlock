//! Tests for the VM helpers: the KVM device check and the mount config.

#[cfg(target_os = "linux")]
mod test_kvm_status;
mod test_mounts;
