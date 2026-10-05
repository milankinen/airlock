#!/usr/bin/env bats

load helpers

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

@test "joining sandbox mount namespace with setns lands in container rootfs" {
    run_vm sh -c 'nsenter --mount=/proc/self/ns/mnt -- sh -c "test -e /mnt/overlay && echo LEAK || echo CONTAINED"'
    assert_success
    assert_output_contains "CONTAINED"
}
