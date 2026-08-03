use crate::config::config::{ImageRef, PullPolicy, Resolution};
use crate::config::load_config::parse_config;

fn parse(toml_str: &str) -> ImageRef {
    let value: toml::Value = toml::from_str(toml_str).unwrap();
    serde::Deserialize::deserialize(value["image"].clone()).unwrap()
}

/// Same thing through the real smart-config pipeline rather than a bare
/// `Deserialize` call — `ImageRef` reaches it via a `Serde<STRING|OBJECT>`
/// deserializer, so this is what actually proves a `[vm.image]` table with a
/// kebab-case key survives loading.
#[test]
fn pull_policy_survives_full_config_parse() {
    let value: serde_json::Value = toml::from_str(
        r#"
        [vm.image]
        name = "alpine:latest"
        pull-policy = "if-changed"
        "#,
    )
    .unwrap();
    let config = parse_config(value).unwrap();
    assert_eq!(config.vm.image.name, "alpine:latest");
    assert_eq!(config.vm.image.pull_policy, PullPolicy::IfChanged);
}

#[test]
fn plain_string_image_survives_full_config_parse() {
    let value: serde_json::Value = toml::from_str(
        r#"
        [vm]
        image = "alpine:3.20@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        "#,
    )
    .unwrap();
    let config = parse_config(value).unwrap();
    assert_eq!(config.vm.image.pull_policy, PullPolicy::IfNotPresent);
    assert!(config.vm.image.pinned_digest().is_some());
}

#[test]
fn string_form_defaults_to_if_not_present() {
    let image = parse(r#"image = "alpine:latest""#);
    assert_eq!(image.name, "alpine:latest");
    assert_eq!(image.pull_policy, PullPolicy::IfNotPresent);
    assert!(matches!(image.resolution, Resolution::Auto));
}

#[test]
fn object_form_reads_kebab_case_pull_policy() {
    // The TOML key is `pull-policy`, not the Rust field name.
    let image = parse(
        r#"
        [image]
        name = "alpine:latest"
        pull-policy = "if-changed"
    "#,
    );
    assert_eq!(image.pull_policy, PullPolicy::IfChanged);
}

#[test]
fn snake_case_pull_policy_is_accepted_too() {
    // Every other section in this config uses snake_case keys, so the
    // snake_case spelling must not silently parse to the default.
    let image = parse(
        r#"
        [image]
        name = "alpine:latest"
        pull_policy = "if-changed"
    "#,
    );
    assert_eq!(image.pull_policy, PullPolicy::IfChanged);
}

#[test]
fn object_form_without_pull_policy_defaults() {
    let image = parse(
        r#"
        [image]
        name = "alpine:latest"
        resolution = "registry"
    "#,
    );
    assert_eq!(image.pull_policy, PullPolicy::IfNotPresent);
    assert!(matches!(image.resolution, Resolution::Registry));
}

#[test]
fn unknown_pull_policy_is_rejected() {
    let value: toml::Value = toml::from_str(
        r#"
        [image]
        name = "alpine:latest"
        pull-policy = "always"
    "#,
    )
    .unwrap();
    let result: Result<ImageRef, _> = serde::Deserialize::deserialize(value["image"].clone());
    assert!(result.is_err(), "typo'd policies must fail closed");
}

#[test]
fn digest_pin_is_recognised() {
    let digest = format!("sha256:{}", "a".repeat(64));

    // Bare `repo@sha256:…`.
    let image = ImageRef::auto(format!("alpine@{digest}"));
    assert_eq!(image.pinned_digest(), Some(digest.as_str()));

    // Docker-compose style `repo:tag@sha256:…`.
    let image = ImageRef::auto(format!("alpine:3.20@{digest}"));
    assert_eq!(image.pinned_digest(), Some(digest.as_str()));

    // Registry with a port, which puts a `:` in the host too.
    let image = ImageRef::auto(format!("localhost:5005/alpine:3@{digest}"));
    assert_eq!(image.pinned_digest(), Some(digest.as_str()));
}

#[test]
fn unpinned_references_have_no_digest() {
    assert_eq!(ImageRef::auto("alpine").pinned_digest(), None);
    assert_eq!(ImageRef::auto("alpine:3.20").pinned_digest(), None);
    assert_eq!(
        ImageRef::auto("localhost:5005/alpine:3").pinned_digest(),
        None
    );
}

#[test]
fn malformed_digests_are_not_treated_as_pins() {
    // Missing algorithm separator.
    assert_eq!(ImageRef::auto("alpine@sha256").pinned_digest(), None);
    // Too short to be a real digest.
    assert_eq!(ImageRef::auto("alpine@sha256:abc").pinned_digest(), None);
    // Non-hex payload.
    assert_eq!(
        ImageRef::auto(format!("alpine@sha256:{}", "z".repeat(64))).pinned_digest(),
        None
    );
    // No name to query a source with — must not be mistaken for a pin, or the
    // Docker branch would end up querying `docker images` with an empty ref.
    assert_eq!(
        ImageRef::auto(format!("@sha256:{}", "a".repeat(64))).pinned_digest(),
        None
    );
}
