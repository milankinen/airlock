//! Build script for the airlock CLI.
//!
//! Makes the checksum of the bundled VM assets and the git commit hash
//! available to the CLI at compile time. Gives the macOS notifier app to the
//! CLI. Rebuilds the CLI when the VM assets, the notifier app or the
//! built-in packs change.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

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

    // The notifier app of macOS. A build without it gets an empty archive:
    // then airlock asks with a dialog.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let tar = std::fs::read("../../target/vm/notifier.tar").unwrap_or_default();
        let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("notifier.tar");
        std::fs::write(out, tar).unwrap();
        println!("cargo:rerun-if-changed=../../target/vm/notifier.tar");
    }

    // Built-in packs (`include_dir!` in `src/packs/builtin.rs`). Cargo scans
    // the directory, so new files also cause a rebuild.
    println!("cargo:rerun-if-changed=../../packs");
    // Released list-form presets (`include_dir!` in
    // `src/config/legacy_presets.rs`).
    println!("cargo:rerun-if-changed=src/config/presets");

    let checksum = format!("{:016x}", hasher.finish());
    println!("cargo:rustc-env=AIRLOCK_ASSETS_CHECKSUM={checksum}");

    // Embed the git commit hash for the version string.
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
