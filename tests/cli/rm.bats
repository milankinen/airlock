#!/usr/bin/env bats

load helpers

# The rm command: what it removes from the data directory and what it
# keeps in the project.

setup() {
    setup_temp_dir
    mkdir -p proj
    cd proj || return 1
}

# Test that rm succeeds when there is no sandbox, also with a broken
# config.
#   1. Write a config and make no sandbox
#   2. Run rm --force and check the message
#   3. Write a config that is not valid, run rm --force and check the same
@test "rm without sandbox succeeds" {
    write_config '[vm]'
    run_airlock rm --force
    assert_success
    assert_output_contains "No sandbox to remove"

    printf 'not [valid\n' > airlock.toml
    run_airlock rm --force
    assert_success
    assert_output_contains "No sandbox to remove"
}

# Test that a config that is not valid does not stop rm.
#   1. Make a sandbox and write a broken airlock.toml
#   2. Run rm -f
#   3. Check that there is no config warning and that the sandbox is gone
@test "rm with broken config still removes sandbox" {
    make_box
    printf 'not [valid\n' > airlock.toml
    run_airlock rm -f
    assert_success
    assert_output_not_contains "could not be loaded"
    assert_output_contains "Sandbox removed"
    [[ ! -e "$BOX_DIR" ]]
}

# Test that rm removes the sandbox from the data directory and from the
# registry, and keeps the files in the project and in the user .airlock
# directory.
#   1. Make a sandbox with a disk
#   2. Add files to the project .airlock directory and to ~/.airlock
#   3. Run rm -f
#   4. Check that the sandbox is gone and that `sandboxes list` is empty
#   5. Check that all the files stay
#   6. Run rm -f again and check that there is no sandbox to remove
@test "rm removes sandbox and keeps project and user files" {
    make_sandbox_disk
    write_config '[vm]'
    mkdir -p .airlock "$TEST_TEMP_DIR/.airlock/claude"
    printf 'x\n' > .airlock/notes.txt
    printf 'x\n' > "$TEST_TEMP_DIR/.airlock/claude/settings.json"
    printf 'v\n' > "$TEST_TEMP_DIR/.airlock/vault.json"
    run_airlock rm -f
    assert_success
    assert_output_contains "Sandbox removed"
    [[ ! -e "$BOX_DIR" ]]
    run_airlock sandboxes list
    assert_output_contains "No sandboxes in"
    [[ -f airlock.toml && -f .airlock/notes.txt ]]
    [[ -f "$TEST_TEMP_DIR/.airlock/claude/settings.json" && -f "$TEST_TEMP_DIR/.airlock/vault.json" ]]

    run_airlock rm -f
    assert_success
    assert_output_contains "No sandbox to remove"
}

# Test that rm does not touch a sandbox in the project directory that start
# did not import. The sandbox of the registry is the only one that rm
# removes.
#   1. Make a legacy sandbox in the project and no registered sandbox
#   2. Run rm -f
#   3. Check the message and that the legacy sandbox stays
@test "rm keeps unimported project sandbox" {
    make_legacy_sandbox
    run_airlock rm -f
    assert_success
    assert_output_contains "No sandbox to remove"
    [[ -e .airlock/sandbox/disk.img ]]
}
