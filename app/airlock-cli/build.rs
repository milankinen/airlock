//! Build script for the airlock CLI.
//!
//! Makes the checksum of each bundled VM asset group and the git commit
//! hash available to the CLI at compile time. Gives the macOS notifier app
//! to the CLI. Rebuilds the CLI when the VM assets, the notifier app or the
//! built-in packs change.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // One checksum for each asset group. A group extracts again only when
    // its own checksum changes. Add a group here for new bundled assets.
    if std::env::var("CARGO_FEATURE_DISTROLESS").is_err() {
        emit_checksum("AIRLOCK_KERNEL_CHECKSUM", &["Image", "initramfs.gz"]);
    }
    if target_os == "linux" {
        emit_checksum(
            "AIRLOCK_CLOUD_HYPERVISOR_CHECKSUM",
            &["cloud-hypervisor", "virtiofsd"],
        );
    }

    // The notifier app of macOS. A build without it gets an empty archive:
    // then airlock asks with a dialog.
    if target_os == "macos" {
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

/// Make the checksum of the asset group `files` in `target/vm` available to
/// the CLI as the compile-time env var `var`. A missing file counts as
/// empty.
fn emit_checksum(var: &str, files: &[&str]) {
    let mut hasher = DefaultHasher::new();
    for file in files {
        let path = format!("../../target/vm/{file}");
        hasher.write(&std::fs::read(&path).unwrap_or_default());
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rustc-env={var}={:016x}", hasher.finish());
}
