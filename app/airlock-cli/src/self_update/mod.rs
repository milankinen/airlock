//! Self-update of the airlock binary.
//!
//! Finds the latest airlock release, compares it with the running version
//! and replaces the running binary with the release binary for the same
//! platform and variant (bundled or distroless). The update verifies the
//! release checksum and runs the new binary once before it replaces the
//! old one. Thus a failed update keeps the old binary.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use bytes::{Bytes, BytesMut};
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};

/// GitHub repository that publishes the airlock releases.
const REPO: &str = "milankinen/airlock";

/// Location of the release information and the release files.
pub struct ReleaseSource {
    /// URL that redirects to `.../releases/tag/<version>` of the latest
    /// release.
    pub latest_url: String,
    /// Base URL of the release files. A file URL is
    /// `<download_url>/<version>/<file>`.
    pub download_url: String,
}

impl Default for ReleaseSource {
    fn default() -> Self {
        Self {
            latest_url: format!("https://github.com/{REPO}/releases/latest"),
            download_url: format!("https://github.com/{REPO}/releases/download"),
        }
    }
}

/// Release version in the `vYYYY.M.N` format, for example `v2026.9.3`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    parts: [u64; 3],
    tag: String,
}

impl Version {
    /// Parse a release tag.
    /// Returns:
    ///   The version, or error if the tag is not `vYYYY.M.N`.
    pub fn parse(tag: &str) -> anyhow::Result<Self> {
        let invalid =
            || anyhow::anyhow!(crate::messages::self_update::invalid_release_version(tag));
        let nums = tag.strip_prefix('v').ok_or_else(invalid)?;
        let parsed: Vec<u64> = nums
            .split('.')
            .map(|p| {
                // Do not accept signs, spaces or empty parts.
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                p.parse().map_err(|_| invalid())
            })
            .collect::<anyhow::Result<_>>()?;
        let parts: [u64; 3] = parsed.try_into().map_err(|_| invalid())?;
        Ok(Self {
            parts,
            tag: tag.to_string(),
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.tag)
    }
}

/// Make the HTTP client for the update requests.
/// Args:
///  - `current`: Version of the running airlock, for the user agent
pub fn http_client(current: &Version) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("airlock/{current}"))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_mins(1))
        .build()
        .context(crate::messages::self_update::HTTP_CLIENT_FAILED)
}

/// Get the version of the latest release.
// The latest release page redirects to the page of the release tag. This
// does not use the GitHub API, which has a low request limit without a
// sign-in.
pub async fn latest_version(
    client: &reqwest::Client,
    source: &ReleaseSource,
) -> anyhow::Result<Version> {
    let response = client
        .head(&source.latest_url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context(crate::messages::self_update::LATEST_RELEASE_FAILED)?;
    let url = response.url();
    let tag = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|_| url.path().contains("/releases/tag/"))
        .with_context(|| crate::messages::self_update::unexpected_latest_location(url))?;
    Version::parse(tag)
}

/// Return the name of the release archive for this platform and variant.
/// Returns:
///   The archive name, or error if no release exists for the platform.
pub fn archive_name(version: &Version) -> anyhow::Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!(crate::messages::self_update::no_release_for(other)),
    };
    let arch = match std::env::consts::ARCH {
        arch @ ("x86_64" | "aarch64") => arch,
        other => bail!(crate::messages::self_update::no_release_for(other)),
    };
    let variant = if cfg!(feature = "distroless") {
        "distroless"
    } else {
        "bundled"
    };
    Ok(format!("airlock-{version}-{os}-{arch}-{variant}.tar.gz"))
}

/// Download the release `version` and replace the binary at `target` with
/// the release binary.
/// Args:
///  - `client`: HTTP client from [`http_client`]
///  - `source`: Location of the release files
///  - `version`: Release to install
///  - `target`: Path of the binary to replace
///  - `progress`: Progress bar for the archive download
pub async fn install(
    client: &reqwest::Client,
    source: &ReleaseSource,
    version: &Version,
    target: &Path,
    progress: &ProgressBar,
) -> anyhow::Result<()> {
    let archive_name = archive_name(version)?;
    let archive_url = format!("{}/{version}/{archive_name}", source.download_url);

    let checksum = download(client, &format!("{archive_url}.sha256"), None)
        .await
        .context(crate::messages::self_update::CHECKSUM_DOWNLOAD_FAILED)?;
    let expected = String::from_utf8_lossy(&checksum)
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .context(crate::messages::self_update::CHECKSUM_FILE_EMPTY)?;

    let archive = download(client, &archive_url, Some(progress))
        .await
        .with_context(|| crate::messages::self_update::archive_download_failed(&archive_name))?;
    let actual = hex::encode(Sha256::digest(&archive));
    if actual != expected {
        bail!(crate::messages::self_update::checksum_mismatch(
            &archive_name,
            expected,
            actual
        ));
    }

    let binary = extract_binary(&archive)
        .with_context(|| crate::messages::self_update::extract_failed(&archive_name))?;
    replace_binary(target, &binary, version)
}

/// Download the body of `url` to memory.
/// Args:
///  - `progress`: Progress bar to update with the downloaded bytes
async fn download(
    client: &reqwest::Client,
    url: &str,
    progress: Option<&ProgressBar>,
) -> anyhow::Result<Bytes> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    if let (Some(pb), Some(len)) = (progress, response.content_length()) {
        pb.set_length(len);
    }
    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        if let Some(pb) = progress {
            pb.set_position(body.len() as u64);
        }
    }
    Ok(body.freeze())
}

/// Return the content of the `airlock` file in the release archive.
fn extract_binary(archive: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() && entry.path()?.as_os_str() == "airlock" {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary)?;
            return Ok(binary);
        }
    }
    bail!(crate::messages::self_update::ARCHIVE_NO_BINARY)
}

/// Replace the binary at `target` with `binary`.
///
/// Writes the new binary next to `target` and runs it with `-V` to make
/// sure that it starts and reports `version`. Then moves it over `target`.
fn replace_binary(target: &Path, binary: &[u8], version: &Version) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = target
        .parent()
        .with_context(|| crate::messages::self_update::no_parent_directory(target.display()))?;
    // The new file must be in the same directory as the target. Then the
    // rename is atomic and a running airlock keeps its old file.
    let tmp = TempFile(dir.join(format!(".airlock-update-{}", std::process::id())));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&tmp.0)?;
        file.write_all(binary)?;
        file.sync_all()
    };
    write().map_err(|e| no_access_hint(e, dir))?;

    let output =
        run_version(&tmp.0).context(crate::messages::self_update::NEW_BINARY_RUN_FAILED)?;
    let reported = String::from_utf8_lossy(&output.stdout);
    let reported_version = reported.split_whitespace().nth(1);
    if !output.status.success() || reported_version != Some(&version.tag) {
        bail!(crate::messages::self_update::new_binary_wrong_version(
            reported.trim(),
            version
        ));
    }

    std::fs::rename(&tmp.0, target).map_err(|e| no_access_hint(e, dir))?;
    Ok(())
}

/// Run `binary -V` and return its output.
// Another thread can fork while the binary file is open for write. The
// child then holds the write handle until it calls exec, and Linux refuses
// to run the binary with ETXTBSY. Thus try again for a short time.
fn run_version(binary: &Path) -> std::io::Result<std::process::Output> {
    let mut tries = 0;
    loop {
        match Command::new(binary).arg("-V").output() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

/// Add a hint about missing write access to `e`.
fn no_access_hint(e: std::io::Error, dir: &Path) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow::anyhow!(crate::messages::self_update::no_write_access(dir.display()))
    } else {
        anyhow::Error::new(e).context(crate::messages::self_update::write_binary_failed(
            dir.display(),
        ))
    }
}

/// Temporary file that is deleted on drop if it still exists.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests;
