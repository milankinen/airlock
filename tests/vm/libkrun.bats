#!/usr/bin/env bats

load helpers

# The libkrun VM backend. Runs only on macOS arm64, the only host that can
# run libkrun.

setup_file() {
    if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
        skip "libkrun runs only on macOS arm64"
    fi
    vm_setup_file
    mkdir -p "$TEST_HOME/.airlock" ro_dir
    # macOS limits Unix socket paths to 104 bytes. The data home has a
    # short path, and the vsock sockets go below it.
    printf 'vmm = "libkrun"\ndata_dir = "%s/airlock"\n' "$FILE_DATA_HOME" \
        > "$TEST_HOME/.airlock/settings.toml"
    echo "ro-content" > ro_dir/file.txt
    write_config '[vm]

[mounts.ro_dir]
source = "./ro_dir"
target = "/data/ro"
read_only = true'
}

# Test that the libkrun backend boots the sandbox and gives the command
# output back to the host.
#   1. Run a command that prints text
#   2. Check the text
@test "libkrun sandbox runs command" {
    run_vm echo "hello from libkrun"
    assert_success
    assert_output_contains "hello from libkrun"
}

# Test that the guest can write and read the project dir, and that the host
# sees the changes. The VM process sandbox allows read and write in this
# share.
#   1. Write a file in the project dir in the sandbox and read it back
#   2. Check the file on the host
#   3. Remove the file in the sandbox and check that it is gone on the host
@test "libkrun project share is readable and writable" {
    run_vm sh -c 'echo guest-data > "$PWD/guest.txt" && cat "$PWD/guest.txt"'
    assert_success
    assert_output_contains "guest-data"
    [[ "$(cat guest.txt)" == "guest-data" ]]
    run_vm sh -c 'rm "$PWD/guest.txt"'
    assert_success
    [[ ! -e guest.txt ]]
}

# Test that the guest can read a read-only mount but not write to it.
#   1. Read the file of the read-only mount
#   2. Try to write to the read-only mount and check the failure
@test "libkrun read-only mount is readable but refuses writes" {
    run_vm cat /data/ro/file.txt
    assert_success
    assert_output_contains "ro-content"
    run_vm sh -c 'echo x > /data/ro/new.txt'
    assert_failure
    [[ ! -e ro_dir/new.txt ]]
}
