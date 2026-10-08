#!/usr/bin/env bats

load helpers

setup_file() {
    if [[ "${AIRLOCK_TEST_NESTED_KVM:-}" != "1" ]]; then
        skip "set AIRLOCK_TEST_NESTED_KVM=1 on a host with nested virtualization"
    fi
    vm_setup_file
    write_config '[vm]
image = "python:3.13-alpine"
kvm = true'
}

@test "nested kvm can create VM" {
    run_vm python3 -c '
import fcntl
import os

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
