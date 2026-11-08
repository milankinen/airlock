#!/usr/bin/env bats

load helpers

# The libkrun VM backend. Runs only on macOS arm64, the only host that can
# run libkrun.

setup_file() {
    if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
        skip "libkrun runs only on macOS arm64"
    fi
    vm_setup_file
    mkdir -p ro_dir
    printf 'vmm = "libkrun"\n' >>"$TEST_HOME/.airlock/settings.toml"
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
    [[ "$(cat guest.txt)" == "guest-data" ]] || false
    run_vm sh -c 'rm "$PWD/guest.txt"'
    assert_success
    [[ ! -e guest.txt ]] || false
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
    [[ ! -e ro_dir/new.txt ]] || false
}

# Test that host files show as owned by guest root. Else git refuses the
# project as "dubious ownership".
#   1. Make a file in the project dir on the host
#   2. Check that the file, the project dir and a rootfs file show as root
@test "libkrun shares show host user files as owned by root" {
    echo host-data > host.txt
    run_vm sh -c 'stat -c %u:%g "$PWD/host.txt" "$PWD" /etc/passwd'
    rm -f host.txt
    assert_success
    [[ "$(echo "$output" | grep -c '^0:0$')" == 3 ]] || false
}

# Test that a non-root container user owns the host files, and that the
# rootfs keeps the image owners. The image runs as `curl_user`.
#   1. Make a project with the image and a host file
#   2. Check that the file shows as the container user and group, and that
#      the user can write in the project dir
#   3. Check that a rootfs file shows as root and the image home dir as
#      `curl_user`
@test "libkrun shares show host user files as owned by non-root container user" {
    mkdir -p nonroot
    cd nonroot
    printf '[vm]\nimage = "curlimages/curl:latest"\n' >airlock.toml
    echo host-data >host.txt
    run_vm sh -c 'id -u -n; id -g -n; stat -c %U:%G "$PWD/host.txt"
        echo x >"$PWD/guest.txt" && echo write-ok
        stat -c %u:%g /etc/passwd; stat -c %U /home/curl_user'
    cd ..
    rm -rf nonroot
    assert_success
    local user group
    user="$(echo "$output" | sed -n 1p)"
    group="$(echo "$output" | sed -n 2p)"
    [[ "$user" != root ]] || false
    [[ "$(echo "$output" | sed -n 3p)" == "$user:$group" ]] || false
    assert_output_contains "write-ok"
    [[ "$(echo "$output" | sed -n 5p)" == "0:0" ]] || false
    [[ "$(echo "$output" | sed -n 6p)" == "curl_user" ]] || false
}

# Test that the rootfs keeps the setuid bit of an image file.
#   1. Make a project with an image that has a setuid root `su`
#   2. Check the mode and owner of `su`
@test "libkrun rootfs keeps setuid bit of image file" {
    mkdir -p setuid
    cd setuid
    printf '[vm]\nimage = "debian:stable-slim"\n' >airlock.toml
    run_vm stat -c %a:%u:%g /usr/bin/su
    cd ..
    rm -rf setuid
    assert_success
    assert_output_contains "4755:0:0"
}
