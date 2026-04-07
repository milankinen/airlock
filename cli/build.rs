use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

fn main() {
    let mut hasher = DefaultHasher::new();

    let kernel = std::fs::read("../sandbox/out/Image").unwrap_or_default();
    let initramfs = std::fs::read("../sandbox/out/initramfs.gz").unwrap_or_default();
    hasher.write(&kernel);
    hasher.write(&initramfs);
    println!("cargo:rerun-if-changed=../sandbox/out/Image");
    println!("cargo:rerun-if-changed=../sandbox/out/initramfs.gz");

    #[cfg(target_os = "linux")]
    {
        let ch = std::fs::read("../sandbox/out/cloud-hypervisor").unwrap_or_default();
        let vfs = std::fs::read("../sandbox/out/virtiofsd").unwrap_or_default();
        hasher.write(&ch);
        hasher.write(&vfs);
        println!("cargo:rerun-if-changed=../sandbox/out/cloud-hypervisor");
        println!("cargo:rerun-if-changed=../sandbox/out/virtiofsd");
    }

    let checksum = format!("{:016x}", hasher.finish());
    println!("cargo:rustc-env=EZPEZ_ASSETS_CHECKSUM={checksum}");
}
