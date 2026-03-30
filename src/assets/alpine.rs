use crate::assets::AssetPaths;
use crate::error::{Error, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

const ALPINE_VERSION: &str = "3.23.3";
const ALPINE_ARCH: &str = "aarch64";

// PUI PUI Linux: minimal kernel built for Apple Virtualization framework
// https://github.com/Code-Hex/puipui-linux
const KERNEL_TAR_URL: &str = "https://github.com/Code-Hex/puipui-linux/releases/download/v1.0.3/puipui_linux_v1.0.3_aarch64.tar.gz";
const KERNEL_TAR_SHA256: &str =
    "dac4ce092db64d4901edf83c4d5061e74c9789f55655da7c737b5d0fc78cf54a";

const MINIROOTFS_URL: &str = "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/aarch64/alpine-minirootfs-3.23.3-aarch64.tar.gz";
const MINIROOTFS_SHA256: &str =
    "f219bb9d65febed9046951b19f2b893b331315740af32c47e39b38fcca4be543";

const INIT_SCRIPT: &str = r#"#!/bin/sh
mount -t proc none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev
mkdir -p /dev/pts
mount -t devpts none /dev/pts

hostname ezpez
ip link set lo up 2>/dev/null

echo "ezpez sandbox ready"

# Start a login shell in a new session with /dev/console as
# stdin/stdout/stderr. The hvc0 virtio console doesn't support
# being a controlling terminal (no job control), but echo and
# line editing work. Suppress the cosmetic warning via 2>/dev/null
# on the outer shell, then restore stderr inside.
setsid sh -c 'exec sh -l </dev/console >/dev/console 2>/dev/console' 2>/dev/null

# Shell exited — shut down
kill -TERM -1 2>/dev/null
sleep 0.2
/sbin/poweroff -f 2>/dev/null
"#;

fn cache_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| Error::VmConfig("cannot determine home directory".into()))?;
    let dir = base
        .home_dir()
        .join(".ezpez")
        .join("cache")
        .join(format!("alpine-{ALPINE_VERSION}-{ALPINE_ARCH}"));
    std::fs::create_dir_all(&dir).map_err(|source| Error::CacheDir {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

pub async fn ensure_alpine_assets() -> Result<AssetPaths> {
    let dir = cache_dir()?;
    let kernel_path = dir.join("Image");
    let initramfs_path = dir.join("initramfs.gz");

    if kernel_path.exists() && initramfs_path.exists() {
        debug!("assets already cached at {}", dir.display());
        return Ok(AssetPaths {
            kernel: kernel_path,
            initramfs: initramfs_path,
        });
    }

    info!("downloading assets...");

    // Download kernel (PUI PUI Linux, compatible with Apple Virtualization)
    if !kernel_path.exists() {
        let tar_path = dir.join("kernel.tar.gz");
        download_file(KERNEL_TAR_URL, &tar_path, Some(KERNEL_TAR_SHA256)).await?;
        extract_kernel(&tar_path, &kernel_path)?;
        let _ = std::fs::remove_file(&tar_path);
    }

    // Download minirootfs, build initramfs (tar→cpio conversion, no temp files)
    if !initramfs_path.exists() {
        let minirootfs_path = dir.join("minirootfs.tar.gz");
        download_file(MINIROOTFS_URL, &minirootfs_path, Some(MINIROOTFS_SHA256)).await?;
        build_initramfs(&minirootfs_path, &initramfs_path)?;
        let _ = std::fs::remove_file(&minirootfs_path);
    }

    info!("assets ready");
    Ok(AssetPaths {
        kernel: kernel_path,
        initramfs: initramfs_path,
    })
}

/// Extract and decompress Image.gz from the kernel tarball.
fn extract_kernel(tar_gz_path: &Path, output: &Path) -> Result<()> {
    let file = std::fs::File::open(tar_gz_path)?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.file_name().and_then(|n| n.to_str()) == Some("Image.gz") {
            // Decompress the gzipped kernel image
            let mut gz = GzDecoder::new(&mut entry);
            let mut out = std::fs::File::create(output)?;
            io::copy(&mut gz, &mut out)?;
            return Ok(());
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("Image") {
            let mut out = std::fs::File::create(output)?;
            io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }

    Err(Error::VmConfig(
        "kernel Image not found in tarball".into(),
    ))
}

/// Build a gzipped cpio (newc) initramfs from an Alpine minirootfs tarball.
/// Converts tar entries directly to cpio entries — no temp directory needed.
fn build_initramfs(minirootfs_tar_gz: &Path, output: &Path) -> Result<()> {
    info!("  building initramfs...");

    let out_file = std::fs::File::create(output)?;
    let gz = GzEncoder::new(out_file, Compression::default());
    let cpio_out = gz;
    let mut ino: u32 = 1;

    // First, add our custom /init script
    let data = INIT_SCRIPT.as_bytes();
    let builder = cpio::NewcBuilder::new("init")
        .ino(ino)
        .mode(0o100755)
        .nlink(1);
    ino += 1;
    let mut writer = builder.write(cpio_out, data.len() as u32);
    writer.write_all(data)?;
    let mut cpio_out = writer.finish()?;

    // Then, add all entries from the minirootfs tarball
    let file = std::fs::File::open(minirootfs_tar_gz)?;
    let gz_reader = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz_reader);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();

        // Normalize path: strip leading "./" or "/"
        let path = path.trim_start_matches("./").trim_start_matches('/');
        if path.is_empty() || path == "." {
            continue;
        }
        // Skip if it would conflict with our /init
        if path == "init" {
            continue;
        }

        let header = entry.header();
        let mode = header.mode().unwrap_or(0o100644);
        let uid = header.uid().unwrap_or(0) as u32;
        let gid = header.gid().unwrap_or(0) as u32;
        let mtime = header.mtime().unwrap_or(0) as u32;
        let entry_type = header.entry_type();

        let mut builder = cpio::NewcBuilder::new(path)
            .ino(ino)
            .uid(uid)
            .gid(gid)
            .mtime(mtime)
            .nlink(1);
        ino += 1;

        if entry_type.is_dir() {
            builder = builder.mode(0o40000 | (mode & 0o7777));
            let writer = builder.write(cpio_out, 0);
            cpio_out = writer.finish()?;
        } else if entry_type.is_symlink() {
            let link_target = header
                .link_name()?
                .map(|l| l.to_string_lossy().to_string())
                .unwrap_or_default();
            let link_bytes = link_target.as_bytes();
            builder = builder.mode(0o120000 | (mode & 0o7777));
            let mut writer = builder.write(cpio_out, link_bytes.len() as u32);
            writer.write_all(link_bytes)?;
            cpio_out = writer.finish()?;
        } else if entry_type.is_hard_link() {
            let link_target = header
                .link_name()?
                .map(|l| l.to_string_lossy().to_string())
                .unwrap_or_default();
            let link_bytes = link_target.as_bytes();
            builder = builder.mode(0o100000 | (mode & 0o7777));
            let mut writer = builder.write(cpio_out, link_bytes.len() as u32);
            writer.write_all(link_bytes)?;
            cpio_out = writer.finish()?;
        } else if entry_type.is_file() {
            builder = builder.mode(0o100000 | (mode & 0o7777));
            let size = entry.size() as u32;
            let mut writer = builder.write(cpio_out, size);
            io::copy(&mut entry, &mut writer)?;
            cpio_out = writer.finish()?;
        }
    }

    // Write cpio trailer and flush gzip
    let cpio_out = cpio::newc::trailer(cpio_out)?;
    cpio_out.finish()?;

    debug!("initramfs built at {}", output.display());
    Ok(())
}

async fn download_file(url: &str, dest: &Path, expected_sha256: Option<&str>) -> Result<()> {
    info!("  downloading {}", url.rsplit('/').next().unwrap_or(url));

    let response = reqwest::get(url).await.map_err(|e| Error::Download {
        url: url.to_string(),
        source: e,
    })?;

    if !response.status().is_success() {
        return Err(Error::Download {
            url: url.to_string(),
            source: response.error_for_status().unwrap_err(),
        });
    }

    let total_size = response.content_length();
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Download {
            url: url.to_string(),
            source: e,
        })?;
        file.write_all(&chunk).await?;
        hasher.update(&chunk);
        downloaded += chunk.len() as u64;

        if let Some(total) = total_size {
            let pct = (downloaded * 100) / total;
            eprint!("\r  progress: {pct}%");
        }
    }
    if total_size.is_some() {
        eprintln!();
    }

    file.flush().await?;
    drop(file);

    if let Some(expected) = expected_sha256 {
        let actual = hex::encode(hasher.finalize());
        if actual != expected {
            let _ = std::fs::remove_file(dest);
            return Err(Error::Checksum {
                path: dest.to_path_buf(),
                expected: expected.to_string(),
                actual,
            });
        }
        debug!("checksum verified for {}", dest.display());
    }

    Ok(())
}
