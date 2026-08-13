use smart_config::ByteSize;

use crate::config::load_config::parse_config;

fn parse(toml_str: &str) -> anyhow::Result<crate::config::Config> {
    let value: serde_json::Value = toml::from_str(toml_str).unwrap();
    parse_config(value)
}

/// Both directions must be off when the section is absent — the clipboard
/// is a hole in the sandbox and no project gets one by accident.
#[test]
fn absent_section_grants_nothing() {
    let config = parse(
        r#"
        [vm]
        image = "alpine:latest"
        "#,
    )
    .unwrap();
    assert!(!config.clipboard.copy);
    assert!(!config.clipboard.paste);
    assert_eq!(config.clipboard.copy_limit, ByteSize(1024 * 1024));
}

/// Declaring the section without naming a direction must not grant one
/// either — `[clipboard]` on its own is not an opt-in.
#[test]
fn empty_section_grants_nothing() {
    let config = parse(
        r#"
        [vm]
        image = "alpine:latest"

        [clipboard]
        "#,
    )
    .unwrap();
    assert!(!config.clipboard.copy);
    assert!(!config.clipboard.paste);
}

#[test]
fn directions_are_independent() {
    let config = parse(
        r#"
        [vm]
        image = "alpine:latest"

        [clipboard]
        copy = true
        "#,
    )
    .unwrap();
    assert!(config.clipboard.copy);
    assert!(!config.clipboard.paste, "copy must not imply paste");
}

#[test]
fn explicit_values() {
    let config = parse(
        r#"
        [vm]
        image = "alpine:latest"

        [clipboard]
        copy = true
        paste = true
        copy_limit = "2 MB"
        "#,
    )
    .unwrap();
    assert!(config.clipboard.copy);
    assert!(config.clipboard.paste);
    assert_eq!(config.clipboard.copy_limit, ByteSize(2 * 1024 * 1024));
}

/// A malformed limit must fail at load rather than silently falling back to
/// the default — a user who mistypes the cap should not quietly get 1 MB.
#[test]
fn bad_copy_limit_errors() {
    assert!(
        parse(
            r#"
            [vm]
            image = "alpine:latest"

            [clipboard]
            copy_limit = "banana"
            "#,
        )
        .is_err()
    );
}

#[test]
fn non_bool_direction_errors() {
    assert!(
        parse(
            r#"
            [vm]
            image = "alpine:latest"

            [clipboard]
            copy = "yes"
            "#,
        )
        .is_err()
    );
}
