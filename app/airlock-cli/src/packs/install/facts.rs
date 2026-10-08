//! Image checks for pack installs.
//!
//! Checks on the host, before the install boot, that the setup scripts can
//! run in the sandbox image.

use crate::oci::{self, OciImage};

/// Check that the setup scripts can run in `image`.
///
/// The image must run as root, because the scripts use the package manager
/// and all processes in the sandbox run as the image user. The image must
/// also be Alpine- or Debian-based (see [`supported`]). The check reads the
/// image layers.
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

/// Check if `lib.sh` supports the distribution `os`. One of its `ID` and
/// `ID_LIKE` values must be `alpine`, `debian` or `ubuntu`. This is the
/// same detection as in `lib.sh`.
fn supported(os: &oci::OsRelease) -> bool {
    std::iter::once(&os.id)
        .chain(&os.id_like)
        .any(|id| matches!(id.as_str(), "alpine" | "debian" | "ubuntu"))
}

#[cfg(test)]
mod tests {
    //! Tests of the image checks before a pack install.

    use super::*;

    /// An os-release with `ID` `id` and `ID_LIKE` `like`.
    fn os(id: &str, like: &[&str]) -> oci::OsRelease {
        oci::OsRelease {
            id: id.into(),
            id_like: like.iter().map(ToString::to_string).collect(),
        }
    }

    /// An image that runs as `uid` and has no layers (thus no os-release).
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

    /// Test that the check accepts the Alpine and Debian families by `ID`
    /// or `ID_LIKE`, and refuses other distros. It must agree with the
    /// detection in `lib.sh`.
    ///   1. Check that Alpine, Debian, Ubuntu and their derivatives pass
    ///   2. Check that Fedora, Rocky and Gentoo fail
    #[test]
    fn alpine_debian_and_ubuntu_families_are_supported_by_id_or_id_like() {
        assert!(supported(&os("alpine", &[])));
        assert!(supported(&os("debian", &[])));
        assert!(supported(&os("ubuntu", &[])));
        assert!(supported(&os("ubuntu", &["debian"])));
        assert!(supported(&os("pop", &["ubuntu", "debian"])));
        assert!(!supported(&os("fedora", &[])));
        assert!(!supported(&os("rocky", &["rhel", "fedora"])));
        assert!(!supported(&os("gentoo", &[])));
    }

    /// Test that the check refuses an image that does not run as root, and
    /// an image without os-release.
    ///   1. Check an image that runs as uid 1000 and check the error
    ///   2. Check a root image with no layers and check the error
    #[test]
    fn check_refuses_non_root_image_and_image_without_os_release() {
        let err = check(&image(1000)).unwrap_err();
        assert!(err.to_string().contains("uid 1000"), "{err}");
        let err = check(&image(0)).unwrap_err();
        assert!(err.to_string().contains("os-release"), "{err}");
    }
}
