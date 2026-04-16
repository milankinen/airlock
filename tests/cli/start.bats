#!/usr/bin/env bats
# Tests for "airlock start" error paths (non-interactive, no VM boot).

load helpers

# On Linux without KVM, "airlock start" exits immediately with a KVM error
# before reaching config loading. These tests skip in that case.

has_kvm_or_not_linux() {
    [[ "$(uname)" != "Linux" ]] || [[ -r /dev/kvm ]]
}

@test "start with no config file fails in non-interactive mode" {
    has_kvm_or_not_linux || skip "no KVM access"
    run_airlock start
    assert_failure 2
    assert_output_contains "No airlock.toml found"
}

@test "start with invalid config reports config error" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config 'not valid toml ['
    run_airlock start
    assert_failure 2
    assert_output_contains "Config error"
}

@test "start with unknown preset reports error" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config 'presets = ["does-not-exist"]'
    run_airlock start
    assert_failure 2
    assert_output_contains "unknown preset"
}

@test "start with valid config gets past config loading" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[vm]'
    run_airlock start
    # Will fail somewhere after config loading (no VM infrastructure),
    # but should NOT fail with "Config error" or "No airlock.toml"
    assert_output_not_contains "Config error"
    assert_output_not_contains "No airlock.toml"
}

@test "start --quiet suppresses log output" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[vm]'
    run_airlock --quiet start
    assert_output_not_contains "Preparing sandbox"
}

@test "start without KVM on Linux fails" {
    [[ "$(uname)" == "Linux" ]] || skip "Linux only"
    [[ ! -r /dev/kvm ]] || skip "KVM is available"
    write_config '[vm]'
    run_airlock start
    assert_failure 1
    assert_output_contains "KVM not available"
}
