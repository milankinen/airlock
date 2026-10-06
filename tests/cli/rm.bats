#!/usr/bin/env bats
# Tests for "airlock rm" behavior.

load helpers

# The project is a sub-directory: HOME is the test directory, and a
# project in the home directory is a case of its own (below).
setup() {
    mkdir -p "$TEST_TEMP_ROOT"
    TEST_TEMP_DIR="$(mktemp -d "$TEST_TEMP_ROOT/XXXXXXXX")"
    mkdir -p "$TEST_TEMP_DIR/proj"
    cd "$TEST_TEMP_DIR/proj" || return 1
}

@test "rm -f without cache dir succeeds silently" {
    write_config '[vm]'
    run_airlock rm -f
    assert_success
}

@test "rm -f removes .airlock directory" {
    write_config '[vm]'
    mkdir -p .airlock
    run_airlock rm -f
    assert_success
    assert_output_contains "Sandbox removed"
    assert_output_not_contains "including"
    [[ ! -d .airlock ]]
}

@test "rm --force synonym works" {
    write_config '[vm]'
    run_airlock rm --force
    assert_success
}

@test "rm -f removes sandbox data even if the config is broken" {
    mkdir -p .airlock/sandbox
    printf 'not [valid\n' > airlock.toml
    run_airlock rm -f
    assert_success
    # rm does not read the config.
    assert_output_not_contains "could not be loaded"
    assert_output_contains "Sandbox removed"
    [[ ! -d .airlock ]]
}

@test "rm -f removes the local config and .gitignore too" {
    mkdir -p .airlock/sandbox
    printf '*\n' > .airlock/.gitignore
    printf '[vm]\ncpus = 1\n' > .airlock/airlock.toml
    touch .airlock/sandbox/disk.img
    run_airlock rm -f
    assert_success
    assert_output_contains "Sandbox removed (including .airlock/airlock.toml)"
    [[ ! -d .airlock ]]
}

@test "rm -f with only a local config still removes the directory" {
    mkdir -p .airlock
    printf '*\n' > .airlock/.gitignore
    printf '[vm]\ncpus = 1\n' > .airlock/airlock.toml
    run_airlock rm -f
    assert_success
    assert_output_contains "Sandbox removed (including .airlock/airlock.toml)"
    [[ ! -d .airlock ]]
}

@test "rm -f in the home directory removes only the sandbox" {
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

@test "rm -f removes only the sandbox when .airlock holds user files" {
    # For example "sudo airlock rm -f" in the home directory: $HOME is
    # not the project, but .airlock is the user's airlock directory.
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
