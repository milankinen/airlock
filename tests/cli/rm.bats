#!/usr/bin/env bats

load helpers

# The rm command: what it removes from .airlock and what it keeps.

setup() {
    setup_temp_dir
    mkdir -p proj
    cd proj || return 1
}

# Test that rm succeeds when there is no sandbox.
#   1. Write a config and make no sandbox
#   2. Run rm --force and check that it succeeds
@test "rm without sandbox succeeds" {
    write_config '[vm]'
    run_airlock rm --force
    assert_success
}

# Test that rm removes the .airlock directory of the project.
#   1. Write a config and make an empty .airlock directory
#   2. Run rm -f
#   3. Check the message and that .airlock is gone
@test "rm removes .airlock directory" {
    write_config '[vm]'
    mkdir -p .airlock
    run_airlock rm -f
    assert_success
    assert_output_contains "Sandbox removed"
    assert_output_not_contains "including"
    [[ ! -d .airlock ]]
}

# Test that a config that is not valid does not stop rm.
#   1. Make a sandbox directory and write a broken airlock.toml
#   2. Run rm -f
#   3. Check that there is no config warning and that .airlock is gone
@test "rm with broken config still removes sandbox" {
    mkdir -p .airlock/sandbox
    printf 'not [valid\n' > airlock.toml
    run_airlock rm -f
    assert_success
    assert_output_not_contains "could not be loaded"
    assert_output_contains "Sandbox removed"
    [[ ! -d .airlock ]]
}

# Test that rm also removes the local config .airlock/airlock.toml and
# tells the user.
#   1. Write a local config, with a sandbox disk and without one
#   2. Run rm -f
#   3. Check the message and that .airlock is gone
@test "rm with local config removes it with sandbox" {
    for disk in yes no; do
        mkdir -p .airlock
        printf '*\n' > .airlock/.gitignore
        printf '[vm]\ncpus = 1\n' > .airlock/airlock.toml
        if [[ "$disk" == yes ]]; then
            make_sandbox_disk
        fi
        run_airlock rm -f
        assert_success
        assert_output_contains "Sandbox removed (including .airlock/airlock.toml)"
        [[ ! -d .airlock ]]
    done
}

# Test that rm in the home directory keeps the user files in ~/.airlock,
# because there the project .airlock is also the user .airlock.
#   1. In the home directory, make a sandbox disk and user files in .airlock
#   2. Run rm -f and check that only the sandbox is gone
#   3. Run rm -f again and check that there is no sandbox to remove
@test "rm in home directory removes only sandbox" {
    cd "$TEST_TEMP_DIR" || return 1
    mkdir -p .airlock/sandbox .airlock/claude
    printf '[vm]\ncpus = 1\n' > .airlock/config.toml
    printf 'x\n' > .airlock/claude/settings.json
    printf 'v\n' > .airlock/vault.json
    touch .airlock/sandbox/disk.img
    run_airlock rm -f
    assert_success
    assert_output_contains "Sandbox removed (kept the other files in ~/.airlock: the project is the home directory)"
    [[ ! -e .airlock/sandbox ]]
    [[ -f .airlock/config.toml && -f .airlock/claude/settings.json && -f .airlock/vault.json ]]

    run_airlock rm -f
    assert_success
    assert_output_contains "No sandbox to remove"
    [[ -f .airlock/config.toml ]]
}

# Test that rm keeps user-level files that it finds in the project
# .airlock, and warns about them.
#   1. Make a sandbox disk and user-level auth and vault files in .airlock
#   2. Run rm -f
#   3. Check the warning, that the sandbox is gone and that the user
#      files stay
@test "rm with user files in .airlock removes only sandbox" {
    mkdir -p .airlock/sandbox .airlock/codex
    printf 'x\n' > .airlock/codex/auth.json
    printf '{}\n' > .airlock/vault.default.json
    touch .airlock/sandbox/disk.img
    run_airlock rm -f
    assert_success
    assert_output_contains "holds the user-level airlock file"
    assert_output_contains "Sandbox removed (kept the other files in .airlock: it holds user-level files"
    [[ ! -e .airlock/sandbox ]]
    [[ -f .airlock/codex/auth.json && -f .airlock/vault.default.json ]]
}
