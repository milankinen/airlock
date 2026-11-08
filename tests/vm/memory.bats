#!/usr/bin/env bats

load helpers

# Memory that the guest frees goes back to the host. Runs only on Linux,
# where the test can read the memory use of the cloud-hypervisor process.

setup_file() {
    if [[ "$(uname -s)" != "Linux" ]]; then
        skip "the test reads the memory use of cloud-hypervisor on Linux"
    fi
    vm_setup_file
    write_config '[vm]
image = "alpine:3.23"
memory = "2 GB"'
}

teardown() {
    kill_bg
}

# Print the PID of the cloud-hypervisor process of this test file. Its
# sockets are in the data home of the file.
ch_pid() {
    local dir
    for dir in /proc/[0-9]*; do
        if [[ "$(cat "$dir/comm" 2>/dev/null)" == "cloud-hyperviso" ]] &&
            tr '\0' ' ' <"$dir/cmdline" 2>/dev/null | grep -qF "$FILE_DATA_HOME"; then
            echo "${dir#/proc/}"
            return 0
        fi
    done
    return 1
}

# Print the resident memory of process $1 in MiB.
rss_mib() {
    awk '/^VmRSS:/ { print int($2 / 1024) }' "/proc/$1/status"
}

# Test that the host gets back the memory that the guest frees. Without
# this, a VM keeps the memory of its highest use until it stops.
#   1. Fill 800 MiB of the guest tmpfs and read the host memory use
#   2. Remove the file in the guest
#   3. Check that the host memory use goes down by 600 MiB or more
@test "memory freed in guest goes back to host" {
    rm -f filled freed checked
    airlock_bg "$FILE_TEMP_DIR/memory.log" --quiet start -- sh -c '
dd if=/dev/zero of=/tmp/fill bs=1M count=800 2>/dev/null
touch filled
# Wait until the host has read the memory use.
while [ ! -e measured ]; do sleep 0.2; done
rm /tmp/fill
touch freed
# Keep the VM on until the host check is done, at most 60 s.
i=0
while [ ! -e checked ] && [ $i -lt 300 ]; do sleep 0.2; i=$((i + 1)); done
'
    wait_until '[[ -e filled ]]' 120
    local pid filled_rss
    pid="$(ch_pid)"
    filled_rss="$(rss_mib "$pid")"
    touch measured
    wait_until '[[ -e freed ]]' 30
    wait_until '(( $(rss_mib "$pid") < filled_rss - 600 ))' 30 || {
        echo "rss: filled ${filled_rss} MiB, now $(rss_mib "$pid") MiB" >&2
        return 1
    }
    touch checked
    wait "$BG_PID"
    rm -f filled measured freed checked
}
