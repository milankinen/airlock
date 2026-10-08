#!/usr/bin/env bats

load helpers

setup() {
    setup_temp_dir
    mkdir -p proj
    cd proj || return 1
}

@test "rm without sandbox succeeds" {
    write_config '[vm]'
    run_airlock rm --force
    assert_success
}

@test "rm removes .airlock directory" {
    write_config '[vm]'
    mkdir -p .airlock
    run_airlock rm -f
    assert_success
    assert_output_contains "Sandbox removed"
    assert_output_not_contains "including"
    [[ ! -d .airlock ]]
}

@test "rm with broken config still removes sandbox" {
    mkdir -p .airlock/sandbox
    printf 'not [valid\n' > airlock.toml
    run_airlock rm -f
    assert_success
    assert_output_not_contains "could not be loaded"
    assert_output_contains "Sandbox removed"
    [[ ! -d .airlock ]]
}

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
