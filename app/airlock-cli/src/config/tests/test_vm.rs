use crate::config::config_values;

#[test]
fn nested_kvm_config_defers_host_support_check_to_backend() {
    let value = toml::from_str("[vm]\nkvm = true\n").unwrap();
    let config =
        config_values::parse(value).expect("KVM configuration must be accepted on Linux and macOS");
    assert!(config.vm.kvm);
}

#[test]
fn nested_kvm_is_disabled_by_default() {
    let config = config_values::parse(serde_json::json!({})).unwrap();
    assert!(!config.vm.kvm);
}
