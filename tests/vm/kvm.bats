#!/usr/bin/env bats
# Tests for nested KVM access and VM creation.
# Requires nested virtualization; opt in with AIRLOCK_TEST_NESTED_KVM=1.

load helpers

setup_file() {
    if [[ "${AIRLOCK_TEST_NESTED_KVM:-}" != "1" ]]; then
        skip "set AIRLOCK_TEST_NESTED_KVM=1 on a host with nested virtualization"
    fi
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
    require_vm_support
    export TEST_TEMP_ROOT="$REPO_ROOT/.tmp/tests"
    vm_setup_file
    write_config '[vm]
image = "python:3.13-alpine"
kvm = true'
}

teardown_file() {
    vm_teardown_file
}

setup() {
    cd "$FILE_TEMP_DIR" || return 1
}

@test "nested KVM can create a VM" {
    run_vm python3 -c '
import fcntl
import os

with open("/dev/kvm", "r+b", buffering=0) as kvm:
    # KVM_GET_API_VERSION and KVM_CREATE_VM from linux/kvm.h.
    assert fcntl.ioctl(kvm, 0xAE00, 0) == 12
    vm = fcntl.ioctl(kvm, 0xAE01, 0)
    os.close(vm)
print("KVM VM creation succeeded")
'
    assert_success
    assert_output_contains "KVM VM creation succeeded"
}
