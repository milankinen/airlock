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

    // Embed git commit hash for version string
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "dev".to_string(), |s| s.trim().to_string());
    println!("cargo:rustc-env=GIT_HASH={hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/");
}
