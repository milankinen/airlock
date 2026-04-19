use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

fn main() {
    let mut hasher = DefaultHasher::new();

    let distroless = std::env::var("CARGO_FEATURE_DISTROLESS").is_ok();
    if !distroless {
        let kernel = std::fs::read("../../target/vm/Image").unwrap_or_default();
        let initramfs = std::fs::read("../../target/vm/initramfs.gz").unwrap_or_default();
        hasher.write(&kernel);
        hasher.write(&initramfs);
        println!("cargo:rerun-if-changed=../../target/vm/Image");
        println!("cargo:rerun-if-changed=../../target/vm/initramfs.gz");
    }

    #[cfg(target_os = "linux")]
    {
        let ch = std::fs::read("../../target/vm/cloud-hypervisor").unwrap_or_default();
        let vfs = std::fs::read("../../target/vm/virtiofsd").unwrap_or_default();
        hasher.write(&ch);
        hasher.write(&vfs);
        println!("cargo:rerun-if-changed=../../target/vm/cloud-hypervisor");
        println!("cargo:rerun-if-changed=../../target/vm/virtiofsd");
    }

    let checksum = format!("{:016x}", hasher.finish());
    println!("cargo:rustc-env=AIRLOCK_ASSETS_CHECKSUM={checksum}");

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
