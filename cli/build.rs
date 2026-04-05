use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

fn main() {
    let mut hasher = DefaultHasher::new();

    #[cfg(target_os = "macos")]
    {
        let kernel = std::fs::read("../sandbox/out/Image").unwrap_or_default();
        let initramfs = std::fs::read("../sandbox/out/initramfs.gz").unwrap_or_default();
        hasher.write(&kernel);
        hasher.write(&initramfs);
        println!("cargo:rerun-if-changed=../sandbox/out/Image");
        println!("cargo:rerun-if-changed=../sandbox/out/initramfs.gz");
    }

    #[cfg(target_os = "linux")]
    {
        let rootfs = std::fs::read("../sandbox/out/rootfs.tar.gz").unwrap_or_default();
        let libkrun = std::fs::read("../sandbox/out/libkrun.so").unwrap_or_default();
        let libkrunfw = std::fs::read("../sandbox/out/libkrunfw.so").unwrap_or_default();
        hasher.write(&rootfs);
        hasher.write(&libkrun);
        hasher.write(&libkrunfw);
        println!("cargo:rerun-if-changed=../sandbox/out/rootfs.tar.gz");
        println!("cargo:rerun-if-changed=../sandbox/out/libkrun.so");
        println!("cargo:rerun-if-changed=../sandbox/out/libkrunfw.so");
    }

    let checksum = format!("{:016x}", hasher.finish());
    println!("cargo:rustc-env=EZPEZ_ASSETS_CHECKSUM={checksum}");
}
