use crate::config::load_config::merge_json;

fn json(toml_str: &str) -> serde_json::Value {
    toml::from_str(toml_str).unwrap()
}

#[test]
fn objects_recursively() {
    let base = json(
        r#"
        [network]
        allowed_hosts = ["a.com"]
    "#,
    );
    let overlay = json(
        r#"
        [network]
        allowed_hosts = ["b.com"]
        tls_passthrough = ["c.com"]
    "#,
    );
    let merged = merge_json(base, overlay);
    let hosts = merged["network"]["allowed_hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2); // concatenated
    assert!(merged["network"]["tls_passthrough"].is_array());
}

#[test]
fn null_overlay_preserves_base() {
    let base = json("cpus = 2");
    let overlay = serde_json::json!({"cpus": null});
    assert_eq!(merge_json(base, overlay)["cpus"], 2);
}

#[test]
fn nested_null_preserves_base() {
    let base = json(
        r#"
        [network]
        allowed_hosts = ["a.com"]
    "#,
    );
    let overlay = serde_json::json!({"network": {"allowed_hosts": null}});
    let merged = merge_json(base, overlay);
    let hosts = merged["network"]["allowed_hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0], "a.com");
}

#[test]
fn primitive_overlay_wins() {
    let base = json("cpus = 2");
    let overlay = json("cpus = 8");
    assert_eq!(merge_json(base, overlay)["cpus"], 8);
}
