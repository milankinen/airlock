#!/usr/bin/env bats

load helpers

# Pack installs on new and existing sandboxes, with Alpine and Debian
# images. The tests of each image run in order and use the same sandbox.

# The config of all packs.
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

# Version commands of all packs, to run in the sandbox.
VERSIONS='claude --version && codex --version && copilot --version && node --version &&
npm --version && python3 --version && cargo --version && rustc --version &&
docker --version && git --version && mise version && echo ALL-PACKS-OK'

setup_file() {
    vm_setup_file

    export COPILOT_GITHUB_TOKEN=airlock-test

    mkdir -p alpine debian
    printf '[vm]\nimage = "alpine:latest"\n\n%s\n' "$PACKS" >alpine/airlock.toml
    printf '[vm]\nimage = "debian:stable-slim"\n\n%s\n' "$PACKS" >debian/airlock.toml

    mkdir -p legacy-list
    printf 'presets = ["python"]\n\n[vm]\nimage = "alpine:latest"\n' >legacy-list/airlock.toml
}

# The names of the packs that have lines in the install log of the last
# install boot.
logged_packs() {
    sed -n 's/^\[\([a-z0-9_-]*\)\] .*/\1/p' .airlock/sandbox/installs.log | sort -u | tr '\n' ' '
}

# The status of pack $1 in installs.json. The file must be pretty-printed
# with the status first.
pack_status() {
    sed -n "/^    \"$1\": {/,/}/s/.*\"status\": \"\([a-z]*\)\".*/\1/p" .airlock/sandbox/installs.json
}

# Check that a new disk installs every pack without a question (no terminal,
# no --yes). The disk is made when the sandbox is prepared, before the
# install. The install runs in the same boot, not in a separate install VM.
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

# Check that a second start installs nothing and the pack commands work.
check_second_start() {
    cd "$1" || return 1
    run_airlock start -- sh -c "$VERSIONS"
    assert_success
    assert_output_not_contains "Installing packs"
    assert_output_contains "ALL-PACKS-OK"
}

# Check that each script can run again on a disk that has the pack. An
# "unconfirmed" record means that the install did not confirm its disk
# write. The next start then runs the script of the pack again on the same
# disk. This is a retry, so it asks no question and needs no terminal.
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

# Check that a pack removed from the config needs a question. Without a
# terminal and without --yes, start fails before the image pull. --yes makes
# a new sandbox: a new disk with only the packs of the config. The config
# comes back at the end, but the sandbox stays without mise. Thus this check
# must run last for its project.
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

# Check that the list form installs nothing: no install boot, no install
# records and no python3 in the sandbox (the Alpine image has no python3).
check_legacy_installs_nothing() {
    cd "$1" || return 1
    run_airlock start -- sh -c 'command -v python3 || echo NO-PYTHON'
    assert_success
    assert_output_not_contains "Installing packs"
    assert_output_contains "NO-PYTHON"
    [[ ! -e .airlock/sandbox/installs.json ]]
    [[ ! -e .airlock/sandbox/installs.log ]]
}

# Test that a new Alpine sandbox installs every pack without a question,
# and then runs the command.
#   1. Run start with all packs and no terminal
#   2. Check the install order, the log and that each pack is installed
@test "alpine: new sandbox installs every pack without question then runs command" {
    check_install alpine
}

# Test that the second start on the Alpine sandbox installs nothing.
#   1. Run start again
#   2. Check that there is no install and all pack commands work
@test "alpine: second start installs nothing" {
    check_second_start alpine
}

# Test that unconfirmed records make each pack script run again on the
# Alpine disk, so that the scripts must be safe to run two times.
#   1. Change all install records to unconfirmed
#   2. Run start and check that each pack installs again with no failure
@test "alpine: unconfirmed install runs every script again" {
    check_rerun alpine
}

# Test that a pack removed from the Alpine config needs a terminal or --yes,
# and --yes makes a new sandbox.
#   1. Remove mise from the config and check that start fails
#   2. Run start --yes and check that a new disk has the other packs only
@test "alpine: removed pack needs terminal or --yes and --yes re-creates sandbox" {
    check_removed_pack alpine
}

# Test that a list-form preset installs no pack.
#   1. Run start with presets = ["python"] on Alpine
#   2. Check that there is no install, no python3 and no install files
@test "list-form preset installs nothing" {
    check_legacy_installs_nothing legacy-list
}

# Test that a new Debian sandbox installs every pack without a question,
# and then runs the command.
#   1. Run start with all packs and no terminal
#   2. Check the install order, the log and that each pack is installed
@test "debian: new sandbox installs every pack without question then runs command" {
    check_install debian
}

# Test that the second start on the Debian sandbox installs nothing.
#   1. Run start again
#   2. Check that there is no install and all pack commands work
@test "debian: second start installs nothing" {
    check_second_start debian
}

# Test that unconfirmed records make each pack script run again on the
# Debian disk, so that the scripts must be safe to run two times.
#   1. Change all install records to unconfirmed
#   2. Run start and check that each pack installs again with no failure
@test "debian: unconfirmed install runs every script again" {
    check_rerun debian
}

# Test that a pack removed from the Debian config needs a terminal or
# --yes, and --yes makes a new sandbox.
#   1. Remove mise from the config and check that start fails
#   2. Run start --yes and check that a new disk has the other packs only
@test "debian: removed pack needs terminal or --yes and --yes re-creates sandbox" {
    check_removed_pack debian
}
