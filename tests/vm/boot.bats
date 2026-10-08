#!/usr/bin/env bats

load helpers

# Boot of the default sandbox and the command result.

setup_file() {
    vm_setup_file
    write_config '[vm]'
}

# Test that a command runs in the default Alpine sandbox and its output
# comes back to the host.
#   1. Run a command that prints text and the OS name
#   2. Check the text and that the OS is Alpine
@test "command in default alpine VM prints output" {
    run_vm sh -c 'echo "hello from vm" && cat /etc/os-release'
    assert_success
    assert_output_contains "hello from vm"
    assert_output_contains "Alpine"
}

# Test that the exit code of the command is the exit code of airlock.
#   1. Run a command that exits with 42
#   2. Check that airlock exits with 42
@test "command exit code is forwarded" {
    run_vm sh -c "exit 42"
    assert_failure 42
}

# Test that the sandbox has no /dev/kvm when the config does not ask
# for it.
#   1. Check in the sandbox that /dev/kvm does not exist
@test "kvm device is hidden by default" {
    run_vm sh -c 'test ! -e /dev/kvm'
    assert_success
}
