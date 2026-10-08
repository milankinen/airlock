#!/usr/bin/env bats

load helpers

# Nested KVM in the sandbox. Runs only with AIRLOCK_TEST_NESTED_KVM=1 on a
# host with nested virtualization.

setup_file() {
    if [[ "${AIRLOCK_TEST_NESTED_KVM:-}" != "1" ]]; then
        skip "set AIRLOCK_TEST_NESTED_KVM=1 on a host with nested virtualization"
    fi
    vm_setup_file
    write_config '[vm]
image = "python:3.13-alpine"
kvm = true'
}

# Test that the sandbox with kvm = true can use /dev/kvm to make a VM.
#   1. Open /dev/kvm in the sandbox and check the KVM API version
#   2. Make a VM with an ioctl and check that it succeeds
@test "nested kvm can create VM" {
    run_vm python3 -c '
import fcntl
import os

# ioctl numbers from linux/kvm.h. The stable KVM API version is 12.
KVM_GET_API_VERSION = 0xAE00
KVM_CREATE_VM = 0xAE01

with open("/dev/kvm", "r+b", buffering=0) as kvm:
    assert fcntl.ioctl(kvm, KVM_GET_API_VERSION, 0) == 12
    vm = fcntl.ioctl(kvm, KVM_CREATE_VM, 0)
    os.close(vm)
print("KVM VM creation succeeded")
'
    assert_success
    assert_output_contains "KVM VM creation succeeded"
}
