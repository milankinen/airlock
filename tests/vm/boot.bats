#!/usr/bin/env bats

load helpers

setup_file() {
    vm_setup_file
    write_config '[vm]'
}

@test "command in default alpine VM prints output" {
    run_vm sh -c 'echo "hello from vm" && cat /etc/os-release'
    assert_success
    assert_output_contains "hello from vm"
    assert_output_contains "Alpine"
}

@test "command exit code is forwarded" {
    run_vm sh -c "exit 42"
    assert_failure 42
}

@test "kvm device is hidden by default" {
    run_vm sh -c 'test ! -e /dev/kvm'
    assert_success
}
