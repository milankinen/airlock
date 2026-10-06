#!/usr/bin/env bats
# Tests for `[packs]`: the install boot of "airlock start" on Alpine and
# Debian, with every installable built-in pack at version 1, and the list
# form, which installs nothing. Requires KVM (Linux) or Apple
# Virtualization (macOS) + docker, and an open network (the install
# scripts download packages and releases).
#
# Installing into an existing sandbox needs a terminal (the "Tools have
# been added" question); bats has none. So these tests cover the new-disk
# install, the retry of an unfinished install (no question), and the
# `--yes` re-create. "Continue with current sandbox" and "Install anyways"
# are covered by unit tests only.

load helpers

PACKS='[packs]
claude = { version = 1 }
codex = { version = 1 }
copilot = { version = 1 }
nodejs = { version = 1 }
python = { version = 1 }
rust = { version = 1 }
docker = { version = 1 }
git = { version = 1 }
mise = { version = 1 }'

# The command of every pack, run in the sandbox.
VERSIONS='claude --version && codex --version && copilot --version && node --version &&
npm --version && python3 --version && cargo --version && rustc --version &&
docker --version && git --version && mise version && echo ALL-PACKS-OK'

setup_file() {
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
    require_vm_support
    vm_setup_file

    # The copilot pack config masks ${COPILOT_GITHUB_TOKEN}: the start
    # fails when the host has no such variable. A dummy token is enough.
    export COPILOT_GITHUB_TOKEN=airlock-test

    # One project per distro; HOME stays in the parent directory.
    mkdir -p alpine debian
    printf '[vm]\nimage = "alpine:latest"\n\n%s\n' "$PACKS" >alpine/airlock.toml
    printf '[vm]\nimage = "debian:stable-slim"\n\n%s\n' "$PACKS" >debian/airlock.toml

    # A project with the list form only.
    mkdir -p legacy-list
    printf 'presets = ["python"]\n\n[vm]\nimage = "alpine:latest"\n' >legacy-list/airlock.toml
}

teardown_file() {
    vm_teardown_file
}

setup() {
    cd "$FILE_TEMP_DIR" || return 1
}

# A user file of the tests: it applies to every project in this file.
teardown() {
    rm -f "$FILE_TEMP_DIR/.airlock/config.toml"
}

# The pack names with lines in the install log of the last install boot.
logged_packs() {
    sed -n 's/^\[\([a-z0-9_-]*\)\] .*/\1/p' .airlock/sandbox/installs.log | sort -u | tr '\n' ' '
}

# The status of pack $1 in installs.json (pretty-printed, status first).
pack_status() {
    sed -n "/^    \"$1\": {/,/}/s/.*\"status\": \"\([a-z]*\)\".*/\1/p" .airlock/sandbox/installs.json
}

# A new sandbox disk installs every configured pack without a question
# (no terminal, no --yes). The disk is created while the sandbox is
# prepared, before the install.
check_install() {
    cd "$1" || return 1
    run_airlock start -- sh -c "$VERSIONS"
    assert_success
    assert_output_contains "disk created"
    [[ "$output" == *"disk created"*"Installing packs..."* ]]
    assert_output_not_contains "(network:"
    assert_output_not_contains "Starting the install VM"
    assert_output_contains "ALL-PACKS-OK"
    [[ "$(logged_packs)" == "claude codex copilot docker git mise nodejs python rust " ]]
    for id in claude codex copilot nodejs python rust docker git mise; do
        [[ "$(pack_status "$id")" == installed ]]
    done
}

check_second_start() {
    cd "$1" || return 1
    run_airlock start -- sh -c "$VERSIONS"
    assert_success
    assert_output_not_contains "Installing packs"
    assert_output_contains "ALL-PACKS-OK"
}

# Idempotency: every script runs again on a disk that has the pack. An
# `unconfirmed` record (the install did not confirm its disk write) makes
# the next start run the pack's script again on the same disk, without a
# question (a retry), so also without a terminal and without --yes.
check_rerun() {
    cd "$1" || return 1
    sed -i.bak 's/"status": "installed"/"status": "unconfirmed"/' .airlock/sandbox/installs.json
    rm .airlock/sandbox/installs.json.bak
    [[ "$(pack_status rust)" == unconfirmed ]]
    run_airlock start -- sh -c "$VERSIONS"
    assert_success
    assert_output_not_contains "disk created"
    assert_output_contains "Installing packs"
    assert_output_not_contains "failed (exit code"
    assert_output_contains "ALL-PACKS-OK"
    [[ "$(logged_packs)" == "claude codex copilot docker git mise nodejs python rust " ]]
}

# A pack removed from the config needs the "Tools have been removed"
# question: without a terminal and without --yes the start fails before the
# image pull. --yes re-creates the sandbox: a new disk with the configured
# packs only. The config is restored at the end, but the sandbox stays
# without mise: this check runs last for its project.
check_removed_pack() {
    cd "$1" || return 1
    sed -i.bak '/^mise = { version = 1 }$/d' airlock.toml
    run_airlock start -- sh -c 'echo NOT-REACHED'
    [[ "$status" -eq 2 ]]
    assert_output_contains "Tools changed in the sandbox (removed: mise). Run in a terminal or pass --yes."
    assert_output_not_contains "Preparing sandbox"
    assert_output_not_contains "NOT-REACHED"

    run_airlock start --yes -- sh -c 'echo RECREATED-OK'
    mv airlock.toml.bak airlock.toml
    assert_success
    assert_output_contains "disk created"
    assert_output_contains "RECREATED-OK"
    ! grep -q '"mise"' .airlock/sandbox/installs.json
    for id in claude codex copilot nodejs python rust docker git; do
        [[ "$(pack_status "$id")" == installed ]]
    done
}

check_exit_code() {
    cd "$1" || return 1
    run_airlock start -- sh -c 'exit 7'
    [[ "$status" -eq 7 ]]
}

# The list form installs nothing: no install boot, no install records, and
# no python3 in the sandbox (the Alpine image has none).
check_legacy_installs_nothing() {
    cd "$1" || return 1
    run_airlock start -- sh -c 'command -v python3 || echo NO-PYTHON'
    assert_success
    assert_output_not_contains "Installing packs"
    assert_output_contains "NO-PYTHON"
    [[ ! -e .airlock/sandbox/installs.json ]]
    [[ ! -e .airlock/sandbox/installs.log ]]
}

@test "alpine: a new sandbox installs every pack without a question, then runs the command" {
    check_install alpine
}

@test "alpine: a second start installs nothing" {
    check_second_start alpine
}

@test "alpine: an unconfirmed install runs every script again" {
    check_rerun alpine
}

@test "alpine: start returns the exit code of the command" {
    check_exit_code alpine
}

@test "alpine: a removed pack needs a terminal or --yes, and --yes re-creates the sandbox" {
    check_removed_pack alpine
}

@test "a legacy list entry installs nothing" {
    check_legacy_installs_nothing legacy-list
}

@test "debian: a new sandbox installs every pack without a question, then runs the command" {
    check_install debian
}

@test "debian: a second start installs nothing" {
    check_second_start debian
}

@test "debian: an unconfirmed install runs every script again" {
    check_rerun debian
}

@test "debian: start returns the exit code of the command" {
    check_exit_code debian
}

@test "debian: a removed pack needs a terminal or --yes, and --yes re-creates the sandbox" {
    check_removed_pack debian
}
