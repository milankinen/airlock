//! Build script for the airlock CLI.
//!
//! Makes the checksum of each bundled VM asset and the git commit hash
//! available at compile time. Turns on the libkrun backend on hosts that
//! can run it. Gives the macOS notifier app to the CLI. Rebuilds the CLI
//! when the VM assets, the notifier app or the built-in packs change.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // One checksum for each asset. Add new bundled assets here.
    if std::env::var("CARGO_FEATURE_DISTROLESS").is_err() {
        emit_checksum("Image");
        emit_checksum("initramfs.gz");
    }
    if target_os == "linux" {
        emit_checksum("cloud-hypervisor");
        emit_checksum("virtiofsd");
    }

    // libkrun runs only on macOS arm64. The `vm_libkrun` cfg marks its code.
    println!("cargo::rustc-check-cfg=cfg(vm_libkrun)");
    if target_os == "macos" && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") {
        println!("cargo:rustc-cfg=vm_libkrun");
        emit_checksum("libkrun.dylib");
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

/// Set the env var `AIRLOCK_CHECKSUM_<FILE>` (for example
/// `..._INITRAMFS_GZ`) to the checksum of `file` in `target/vm`. A missing
/// file counts as empty.
fn emit_checksum(file: &str) {
    let path = format!("../../target/vm/{file}");
    let mut hasher = DefaultHasher::new();
    hasher.write(&std::fs::read(&path).unwrap_or_default());
    println!("cargo:rerun-if-changed={path}");
    let var: String = file
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    println!(
        "cargo:rustc-env=AIRLOCK_CHECKSUM_{var}={:016x}",
        hasher.finish()
    );
}
