#!/usr/bin/env bats

load helpers

# Clients of a shared sandbox process: exec starts the sandbox on demand,
# all clients use one VM, and the VM stops when the last client ends.

setup_file() {
    vm_setup_file
    write_config '[vm]'
}

# Stop the sandbox after each test, so that the next test starts with a
# stopped sandbox. Also end the background processes of the test.
teardown() {
    run_airlock stop
    kill_bg
}

# Print a guest shell command that waits for the file $1 in the project
# directory, for at most 60 seconds.
wait_for_file_cmd() {
    printf 'i=0; while [ ! -e %s ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done' "$1"
}

# Get the boot id of the VM from the first line of the file $1.
boot_id_in() {
    head -n 1 "$1" | tr -d '\r'
}

# Test that exec starts a stopped sandbox with the config of the last
# start, and that the sandbox stops when exec ends.
#   1. Run start with a command that exits, so that the sandbox stops
#   2. Check that the sandbox process does not run (run.lock is free)
#   3. Run exec and check its output
#   4. Check that run.lock is free again after exec
@test "exec starts stopped sandbox and stops it after" {
    run_vm true
    assert_success
    find_box_dir
    sandbox_stopped

    run_airlock --quiet exec echo from-exec
    assert_success
    assert_output_contains "from-exec"
    wait_until '! sandbox_running' 60
    sandbox_stopped
}

# Test that two execs at the same time use one VM, so that the sandbox
# boots only one time for them.
#   1. Run start, so that the sandbox exists and stops
#   2. Start two execs that print the boot id. Each exec waits until the
#      other exec ran, so that both run at the same time
#   3. Check that both execs succeed and print the same boot id
@test "two parallel execs share one VM" {
    run_vm true
    assert_success
    rm -f exec-one exec-two
    local first="$FILE_TEMP_DIR/first.out" second="$FILE_TEMP_DIR/second.out"
    local boot='cat /proc/sys/kernel/random/boot_id'

    airlock_bg "$first" --quiet exec sh -c "$boot; touch exec-one; $(wait_for_file_cmd exec-two)"
    local first_pid=$BG_PID
    airlock_bg "$second" --quiet exec sh -c "$boot; touch exec-two; $(wait_for_file_cmd exec-one)"
    local second_pid=$BG_PID
    wait "$first_pid"
    wait "$second_pid"
    [[ -e exec-one && -e exec-two ]]

    local first_id second_id
    first_id="$(boot_id_in "$first")"
    second_id="$(boot_id_in "$second")"
    [[ -n "$first_id" ]]
    [[ "$first_id" == "$second_id" ]]
}

# Test that start fails while an exec uses the sandbox, because start
# applies the config again. The error says that the sandbox runs and
# names the stop command.
#   1. Start an exec that waits until the test lets it end
#   2. Run start and check that it fails with "running" and `airlock stop`
#   3. Check that the exec still runs
@test "start fails while exec runs" {
    run_vm true
    assert_success
    local out="$FILE_TEMP_DIR/exec.out"
    rm -f exec-end
    airlock_bg "$out" --quiet exec sh -c "echo ready; $(wait_for_file_cmd exec-end)"
    local exec_pid=$BG_PID
    wait_until "grep -q ready '$out'" 120

    run_vm true
    assert_failure 1
    assert_output_contains "sandbox is running"
    assert_output_contains "airlock stop"
    kill -0 "$exec_pid"
}

# Test that start after the last exec waits until the sandbox stopped
# and then works. No wait for run.lock in the test, so start meets the
# stopping sandbox process.
#   1. Start an exec that waits until the test lets it end
#   2. Let the exec end and wait for it
#   3. Run start at once and check that it succeeds and runs its command
@test "start right after last exec ends waits for stop and works" {
    run_vm true
    assert_success
    find_box_dir
    rm -f exec-end
    local out="$FILE_TEMP_DIR/exec.out"
    airlock_bg "$out" --quiet exec sh -c "echo ready; $(wait_for_file_cmd exec-end)"
    local exec_pid=$BG_PID
    wait_until "grep -q ready '$out'" 120

    touch exec-end
    wait "$exec_pid"
    run_vm echo after-exec
    assert_success
    assert_output_contains "after-exec"
}

# Test that stop ends an exec that runs, and that the sandbox is stopped
# when stop returns.
#   1. Start an exec with a long command
#   2. Run stop and check its message
#   3. Check that run.lock is free
#   4. Check that the exec ends with exit code 1 and tells that the
#      sandbox stopped
#   5. Run stop again and check that it tells that the sandbox does not run
@test "stop ends long exec" {
    run_vm true
    assert_success
    find_box_dir
    local out="$FILE_TEMP_DIR/exec.out"
    airlock_bg "$out" exec sh -c 'echo ready; sleep 300'
    wait_until "grep -q ready '$out'" 120

    run_airlock stop
    assert_success
    assert_output_contains "Stopped the sandbox"
    sandbox_stopped

    local code=0
    wait "$BG_PID" || code=$?
    [[ "$code" -eq 1 ]]
    grep -q "the sandbox stopped" "$out"

    run_airlock stop
    assert_success
    assert_output_contains "The sandbox does not run"
}

# Test that stop succeeds when the sandbox does not run, and that exec
# without a previous start fails and asks for start.
#   1. Run start with a command that exits, then run stop
#   2. Check exit code 0 and "does not run"
#   3. In a new project directory, run stop and exec
#   4. Check that stop tells that the sandbox does not run
#   5. Check that exec fails and says "'airlock start' first"
@test "stop with stopped sandbox and exec without start" {
    run_vm true
    assert_success
    run_airlock stop
    assert_success
    assert_output_contains "The sandbox does not run"

    mkdir -p "$FILE_TEMP_DIR/never-started"
    cd "$FILE_TEMP_DIR/never-started"
    run_airlock stop
    assert_success
    assert_output_contains "The sandbox does not run"
    run_airlock exec true
    assert_failure
    assert_output_contains "'airlock start' first"
}

# Test that a port forward does not hold the sandbox. The sandbox stops
# when the last exec ends, and the forward then ends with an error.
#   1. Start an exec that waits until the test lets it end
#   2. Start a port forward on a free host port and wait until it is active
#   3. Let the exec end, and check that the sandbox stops
#   4. Check that the forward ends with exit code 1 and tells that the
#      sandbox stopped
@test "forward does not hold sandbox" {
    run_vm true
    assert_success
    find_box_dir
    rm -f exec-end
    local out="$FILE_TEMP_DIR/exec.out" fwd="$FILE_TEMP_DIR/forward.out"
    airlock_bg "$out" --quiet exec sh -c "echo ready; $(wait_for_file_cmd exec-end)"
    local exec_pid=$BG_PID
    wait_until "grep -q ready '$out'" 120
    local port
    port="$(free_port)"
    airlock_bg "$fwd" network forward "$port:8080"
    local forward_pid=$BG_PID
    wait_until "grep -q 'Forwarding ports' '$fwd'" 30

    touch exec-end
    wait "$exec_pid"
    wait_until '! sandbox_running' 60
    local code=0
    wait "$forward_pid" || code=$?
    [[ "$code" -eq 1 ]]
    grep -q "the sandbox stopped" "$fwd"
}

# Test that the exit code of the start command passes through, and that
# the sandbox is stopped when start returns. Thus a start directly after
# it works.
#   1. Run start with a command that exits with 7
#   2. Check exit code 7 and that the sandbox does not run
#   3. Run start again and check that it succeeds
@test "start exit code passes through and sandbox is stopped at exit" {
    run_vm sh -c 'exit 7'
    assert_failure 7
    find_box_dir
    sandbox_stopped

    run_vm true
    assert_success
}

# Test that an exec that attaches while start runs keeps the VM after
# start exits. The VM stops when the exec ends.
#   1. Start start with a command that waits until an exec attached
#   2. Start an exec in the same VM that waits until the test lets it end
#   3. Check that start exits with 0 and that the sandbox still runs
#   4. Let the exec end, and check that it ran in the same VM
#   5. Check that the sandbox stops after the exec
@test "exec attached during start keeps VM after start exits" {
    run_vm true
    assert_success
    find_box_dir
    rm -f exec-attached exec-end
    local start_out="$FILE_TEMP_DIR/start.out" exec_out="$FILE_TEMP_DIR/exec.out"
    local boot='cat /proc/sys/kernel/random/boot_id'
    airlock_bg "$start_out" --quiet start -- \
        sh -c "$boot; $(wait_for_file_cmd exec-attached)"
    local start_pid=$BG_PID
    wait_until "[[ -s '$start_out' ]]" 120
    airlock_bg "$exec_out" --quiet exec \
        sh -c "$boot; touch exec-attached; $(wait_for_file_cmd exec-end)"
    local exec_pid=$BG_PID

    wait "$start_pid"
    [[ -e exec-attached ]]
    sandbox_running
    touch exec-end
    wait "$exec_pid"
    [[ "$(boot_id_in "$start_out")" == "$(boot_id_in "$exec_out")" ]]
    wait_until '! sandbox_running' 60
}
