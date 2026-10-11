//! Tests for the release version order.

use crate::self_update::Version;

/// Test that versions compare as numbers and not as text. Else a two-digit
/// month looks older than a one-digit month and the update is not found.
///   1. Compare versions that have one- and two-digit parts
///   2. Check that a bad tag is refused
#[test]
fn versions_with_more_digits_compare_as_newer() {
    let v = |tag| Version::parse(tag).unwrap();
    assert!(v("v2026.10.0") > v("v2026.9.3"));
    assert!(v("v2026.9.10") > v("v2026.9.9"));
    assert!(v("v2027.1.0") > v("v2026.12.5"));
    assert_eq!(v("v2026.9.3"), v("v2026.9.3"));

    for bad in [
        "2026.9.3",
        "v2026.9",
        "v2026.9.3.1",
        "v2026.-9.3",
        "v2026..3",
        "v2026.9.3-rc",
    ] {
        assert!(Version::parse(bad).is_err(), "{bad} was accepted");
    }
}
