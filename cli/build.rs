use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

fn main() {
    let kernel = std::fs::read("../sandbox/out/Image").unwrap_or_default();
    let initramfs = std::fs::read("../sandbox/out/initramfs.gz").unwrap_or_default();

    let mut hasher = DefaultHasher::new();
    hasher.write(&kernel);
    hasher.write(&initramfs);
    let checksum = format!("{:016x}", hasher.finish());

    println!("cargo:rustc-env=EZPEZ_ASSETS_CHECKSUM={checksum}");
    println!("cargo:rerun-if-changed=../sandbox/out/Image");
    println!("cargo:rerun-if-changed=../sandbox/out/initramfs.gz");
}
