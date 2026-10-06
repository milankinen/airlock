//! Whether the setup scripts can run in an image, checked on the host
//! from the image layers before the install boot.

use crate::oci::{self, OciImage};

/// Check that `image` runs as root (the scripts use the package manager,
/// and every process in the sandbox runs as the image user) and is
/// Alpine- or Debian-based (see [`supported`]).
pub fn check(image: &OciImage) -> anyhow::Result<()> {
    if image.uid != 0 {
        anyhow::bail!(
            "image {} runs as uid {}; packs install only into an image that runs as root",
            image.name,
            image.uid
        );
    }
    let os = oci::os_release(image)
        .ok_or_else(|| anyhow::anyhow!("image {} has no /etc/os-release", image.name))?;
    anyhow::ensure!(
        supported(&os),
        "image {} is {}; packs install only into Alpine- and Debian-based images",
        image.name,
        os.id
    );
    Ok(())
}

/// Whether `lib.sh` supports the distribution `os`: one of its `ID` and
/// `ID_LIKE` values is `alpine`, or `debian` or `ubuntu` (as `lib.sh`
/// detects it).
fn supported(os: &oci::OsRelease) -> bool {
    std::iter::once(&os.id)
        .chain(&os.id_like)
        .any(|id| matches!(id.as_str(), "alpine" | "debian" | "ubuntu"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(id: &str, like: &[&str]) -> oci::OsRelease {
        oci::OsRelease {
            id: id.into(),
            id_like: like.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn families() {
        assert!(supported(&os("alpine", &[])));
        assert!(supported(&os("debian", &[])));
        assert!(supported(&os("ubuntu", &["debian"])));
        assert!(supported(&os("ubuntu", &[])));
        assert!(supported(&os("pop", &["ubuntu", "debian"])));
        assert!(!supported(&os("fedora", &[])));
        assert!(!supported(&os("rocky", &["rhel", "fedora"])));
        assert!(!supported(&os("gentoo", &[])));
    }

    fn image(uid: u32) -> OciImage {
        OciImage {
            image_id: "sha256:abc".into(),
            name: "img".into(),
            image_layers: vec![],
            container_home: "/root".into(),
            uid,
            gid: 0,
            cmd: vec![],
            env: vec![],
            user: Some(String::new()),
        }
    }

    #[test]
    fn check_refuses_non_root_and_images_without_os_release() {
        let err = check(&image(1000)).unwrap_err();
        assert!(err.to_string().contains("uid 1000"), "{err}");
        // No layers → no os-release.
        let err = check(&image(0)).unwrap_err();
        assert!(err.to_string().contains("os-release"), "{err}");
    }
}
