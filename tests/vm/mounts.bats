#!/usr/bin/env bats
# Tests for VM mount functionality: directory and file mounts, rw/ro.
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

    # Create host-side test fixtures
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

teardown_file() {
    vm_teardown_file
}

setup() {
    cd "$FILE_TEMP_DIR" || return 1
}

# -- Directory mounts --

@test "rw directory mount contains expected file" {
    run_vm cat /data/rw/file.txt
    assert_success
    assert_output_contains "rw-content"
}

@test "ro directory mount contains expected file" {
    run_vm cat /data/ro/file.txt
    assert_success
    assert_output_contains "ro-content"
}

@test "rw directory mount allows writing" {
    run_vm sh -c 'echo "written-from-vm" > /data/rw/new_file.txt && cat /data/rw/new_file.txt'
    assert_success
    assert_output_contains "written-from-vm"
}

@test "ro directory mount rejects writes" {
    run_vm sh -c 'echo "should-fail" > /data/ro/new_file.txt 2>&1'
    assert_failure
}

@test "rw directory mount reflects host-side changes" {
    echo "new-host-content" > rw_dir/host_created.txt
    run_vm cat /data/rw/host_created.txt
    assert_success
    assert_output_contains "new-host-content"
}

# -- File mounts --

@test "rw file mount contains expected content" {
    run_vm cat /data/rw_file.txt
    assert_success
    assert_output_contains "rw-file-content"
}

@test "ro file mount contains expected content" {
    run_vm cat /data/ro_file.txt
    assert_success
    assert_output_contains "ro-file-content"
}

@test "rw file mount reflects host-side changes" {
    echo "updated-by-host" > rw_file.txt
    run_vm cat /data/rw_file.txt
    assert_success
    assert_output_contains "updated-by-host"
}

@test "rw file mount reflects guest-side changes" {
    run_vm sh -c 'echo "updated-by-guest" > /data/rw_file.txt'
    assert_success
    content="$(cat rw_file.txt)"
    [[ "$content" == *"updated-by-guest"* ]]
}

@test "ro file mount rejects writes" {
    run_vm sh -c 'echo "should-fail" > /data/ro_file.txt 2>&1'
    assert_failure
}
