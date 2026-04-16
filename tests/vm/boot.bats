#!/usr/bin/env bats
# Tests for VM boot and basic command execution.
# Requires KVM (Linux) or Apple Virtualization (macOS) + docker.

load helpers

setup_file() {
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
    require_vm_support
    vm_setup_file
    write_config '[vm]'
}

teardown_file() {
    vm_teardown_file
}

setup() {
    cd "$FILE_TEMP_DIR" || return 1
}

@test "echo inside VM" {
    run_vm echo "hello from vm"
    assert_success
    assert_output_contains "hello from vm"
}

@test "exit code is forwarded" {
    run_vm sh -c "exit 42"
    assert_failure 42
}

@test "multiple commands" {
    run_vm sh -c "echo first && echo second"
    assert_success
    assert_output_contains "first"
    assert_output_contains "second"
}

@test "working directory" {
    run_vm pwd
    assert_success
    # Should be in the project mount
    assert_output_matches "/"
}

@test "default image is alpine" {
    run_vm cat /etc/os-release
    assert_success
    assert_output_contains "Alpine"
}
