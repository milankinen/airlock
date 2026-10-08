#!/usr/bin/env bats

load helpers

# Mounts of host files and directories, and the mount namespace of the
# sandbox.

setup_file() {
    vm_setup_file

    mkdir -p rw_dir ro_dir
    echo "rw-content" > rw_dir/file.txt
    echo "ro-content" > ro_dir/file.txt
    echo "rw-file-content" > rw_file.txt
    echo "ro-file-content" > ro_file.txt

    cat > airlock.toml <<'EOF'
[vm]

[mounts.rw_dir]
source = "./rw_dir"
target = "/data/rw"
read_only = false

[mounts.ro_dir]
source = "./ro_dir"
target = "/data/ro"
read_only = true

[mounts.rw_file]
source = "./rw_file.txt"
target = "/data/rw_file.txt"
read_only = false

[mounts.ro_file]
source = "./ro_file.txt"
target = "/data/ro_file.txt"
read_only = true
EOF
}

# Test that directory mounts show the host files and that only the
# writable mount accepts writes.
#   1. Make a new file on the host, then read all files in the sandbox
#   2. Write to the writable and the read-only mount in the sandbox
#   3. Check that only the write to the writable mount is on the host
@test "directory mounts share host files and only rw accepts writes" {
    echo "new-host-content" > rw_dir/host_created.txt
    run_vm sh -c 'cat /data/rw/file.txt /data/ro/file.txt /data/rw/host_created.txt
                  echo "written-from-vm" > /data/rw/new_file.txt && echo rw-write-ok
                  echo "should-fail" 2>/dev/null > /data/ro/new_file.txt || echo ro-write-denied'
    assert_success
    assert_output_contains "rw-content"
    assert_output_contains "ro-content"
    assert_output_contains "new-host-content"
    assert_output_contains "rw-write-ok"
    assert_output_contains "ro-write-denied"
    [[ "$(cat rw_dir/new_file.txt)" == "written-from-vm" ]]
    [[ ! -e ro_dir/new_file.txt ]]
}

# Test that file mounts show host changes, guest writes reach the host,
# and only the writable file accepts writes.
#   1. Read both files in the sandbox
#   2. Change the writable file on the host, then read it and write both
#      files in a new sandbox
#   3. Check that the guest write is on the host and the read-only file
#      did not change
@test "file mounts share host changes both ways and only rw accepts writes" {
    run_vm sh -c 'cat /data/rw_file.txt /data/ro_file.txt'
    assert_success
    assert_output_contains "rw-file-content"
    assert_output_contains "ro-file-content"

    echo "updated-by-host" > rw_file.txt
    run_vm sh -c 'cat /data/rw_file.txt
                  echo "updated-by-guest" > /data/rw_file.txt && echo rw-write-ok
                  echo "should-fail" 2>/dev/null > /data/ro_file.txt || echo ro-write-denied'
    assert_success
    assert_output_contains "updated-by-host"
    assert_output_contains "rw-write-ok"
    assert_output_contains "ro-write-denied"
    [[ "$(cat rw_file.txt)" == "updated-by-guest" ]]
    [[ "$(cat ro_file.txt)" == "ro-file-content" ]]
}

# Test that a process that joins the sandbox mount namespace stays in the
# container root, and does not see the VM root (/mnt/overlay).
#   1. Enter the mount namespace of the shell with nsenter
#   2. Check that /mnt/overlay does not exist there
@test "joining sandbox mount namespace with setns lands in container rootfs" {
    run_vm sh -c 'nsenter --mount=/proc/self/ns/mnt -- sh -c "test -e /mnt/overlay && echo LEAK || echo CONTAINED"'
    assert_success
    assert_output_contains "CONTAINED"
}

# Test that the guest hides .airlock only for a sandbox in the project. A
# sandbox in the data directory has nothing there, so the guest must not
# hide the directory or make it in the host project.
#   1. In a new project, start a sandbox in the data directory and list
#      .airlock in the sandbox
#   2. Check that the sandbox sees no .airlock and the host has none
#   3. In a second project, keep the sandbox in the project and list
#      .airlock in the sandbox
#   4. Check that .airlock is empty in the sandbox and has the sandbox on
#      the host
@test "airlock dir is masked only for sandbox in project" {
    mkdir -p boxed
    printf '[vm]\n' >boxed/airlock.toml
    cd boxed
    run_vm sh -c 'ls -A .airlock 2>/dev/null || echo NO-AIRLOCK'
    assert_success
    assert_output_contains "NO-AIRLOCK"
    [[ ! -e .airlock ]]

    mkdir -p ../in-project/.airlock/sandbox
    printf '[vm]\n' >../in-project/airlock.toml
    touch ../in-project/.airlock/sandbox/keep-in-project
    cd ../in-project
    run_vm sh -c 'echo "[$(ls -A .airlock)]"'
    assert_success
    assert_output_contains "[]"
    [[ -d .airlock/sandbox ]]
}
