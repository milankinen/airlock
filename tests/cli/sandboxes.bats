#!/usr/bin/env bats

load helpers

# The sandboxes command: list, details and removal. A sandbox needs a
# start, so the helpers make it with a start that stops at the image pull.

setup() {
    setup_temp_dir
}

# Test that the list is empty when there is no registered sandbox, also when
# the project has an old sandbox in its directory. The second run uses the
# `sandbox` and `ls` aliases, so that old scripts continue to work.
#   1. Run list in an empty directory and check the message
#   2. Make an old sandbox in the project, run `sandbox ls` and check the
#      message
@test "sandboxes list without registered sandboxes is empty" {
    run_airlock sandboxes list
    assert_success
    assert_output_contains "No sandboxes in"

    make_legacy_sandbox
    run_airlock sandbox ls
    assert_success
    assert_output_contains "No sandboxes in"
}

# Test that `airlock info`, its alias `show` and `airlock sandboxes info`
# print the same details of the sandbox, that the list has the sandbox and
# that JSON has the sandbox fields. Nothing changes.
#   1. Make a sandbox disk with an install record
#   2. Run sandboxes info and check the sandbox directory and the pack
#   3. Run info and show, and check that the output is the same
#   4. Run sandboxes info --json and check the project and the pack
#   5. Check that the record file is still there and that the project has no
#      .airlock directory
#   6. Run list and check that it has the project
@test "info, show and sandboxes info print same sandbox details" {
    make_sandbox_disk
    write_installs mise=installed
    run_airlock sandboxes info
    assert_success
    assert_output_contains "Sandbox:  $BOX_DIR"
    assert_output_contains "mise — installed, removed from config"
    local expected="$output"

    for command in info show; do
        run_airlock "$command"
        assert_success
        [[ "$output" == "$expected" ]]
    done

    run_airlock sandboxes info --json
    assert_success
    assert_output_contains "\"project\": \"$PWD\""
    assert_output_contains '"mise": "installed"'
    [[ -e "$BOX_DIR/installs.json" ]]
    [[ ! -e .airlock ]]

    run_airlock sandboxes list
    assert_success
    assert_output_contains "$PWD"
}

# Test that info and remove fail for a sandbox that does not exist.
#   1. Run info in an empty directory and check the error
#   2. Run info and remove with an id that is not registered, and with a
#      path in place of an id, and check the errors
@test "sandboxes info and remove of missing sandbox fail" {
    run_airlock sandboxes info
    assert_failure 1
    assert_output_contains "No sandbox for $PWD"

    for id in abcdefgh ../etc; do
        run_airlock sandboxes info "$id"
        assert_failure 1
        assert_output_contains "No sandbox $id"

        run_airlock sandboxes remove -f "$id"
        assert_failure 1
        assert_output_contains "No sandbox $id"
    done
}
