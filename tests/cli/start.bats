#!/usr/bin/env bats

load helpers

# The start command up to the image pull: config checks, pack changes and
# install state. Most tests use an image that does not exist, so start
# stops at the image pull and no VM boots.

# A config with an image that does not exist. Start stops at its pull.
BAD_IMAGE='[vm]
image = "airlock-test.invalid/no-such-image:1"'

# True if the host can run a VM: not Linux, or Linux with /dev/kvm access.
has_kvm_or_not_linux() {
    [[ "$(uname)" != "Linux" ]] || [[ -r /dev/kvm ]]
}

# Skip each test when KVM is not available, except the "without KVM" test.
setup() {
    setup_temp_dir
    if [[ "$BATS_TEST_DESCRIPTION" != *"without KVM"* ]]; then
        has_kvm_or_not_linux || skip "no KVM access"
    fi
}

# Test that start without a config and without a terminal fails and does
# not make a config.
#   1. Run start in an empty directory
#   2. Check exit code 2 and the message
#   3. Check that no config file exists
@test "start without config fails in non-interactive mode" {
    run_airlock start
    assert_failure 2
    assert_output_contains "No airlock config in $PWD. Run \`airlock start\` in a terminal, or create airlock.toml."
    [[ ! -e airlock.toml && ! -e .airlock/airlock.toml ]]
}

# Test that start refuses to continue when a sandbox exists but its config
# is gone, so that it does not use a sandbox with the wrong config.
#   1. Make a sandbox disk, run start and check the error
#   2. Remove the disk and keep only install records
#   3. Run start again and check the same error
@test "start with sandbox state but no config fails" {
    make_sandbox_disk
    run_airlock start
    assert_failure 2
    assert_output_contains "A sandbox exists in $PWD, but there is no config."
    assert_output_contains "Put the config in airlock.toml or $PWD/.airlock/airlock.toml (restore it), or run \`airlock rm\` to start over."

    rm .airlock/sandbox/disk.*
    echo '{}' >.airlock/sandbox/installs.json
    run_airlock start
    assert_failure 2
    assert_output_contains "A sandbox exists in $PWD, but there is no config."
}

# Test that start reports config errors and does not change the config
# file.
#   1. Run start with broken TOML, an unknown preset, a bad presets value
#      and an unknown pack key
#   2. Check exit code 2 and each error message
#   3. Check that airlock.toml is not changed
@test "start with invalid config fails without touching config" {
    assert_config_error start 'not valid toml [' "Config error"
    assert_config_error start 'presets = ["does-not-exist"]' "unknown preset"
    assert_config_error start 'presets = true' \
        "$PWD/airlock.toml: \`presets\` must be a list of preset names, not true"
    assert_config_error start '[packs]
codex = { version = 1, auth = "api-key" }' \
        "\`packs.codex.auth\` unknown key (known: version, enabled, args; args go in \`args = { auth = … }\`) (set in: $PWD/airlock.toml)"
    [[ "$(cat airlock.toml)" == '[packs]
codex = { version = 1, auth = "api-key" }' ]]
}

# Test that --quiet removes the log output of start.
#   1. Run start and check that the log shows
#   2. Run start --quiet and check that the log does not show
@test "start --quiet suppresses log output" {
    write_config "$BAD_IMAGE"
    run_airlock start
    assert_output_contains "Preparing sandbox"

    run_airlock --quiet start
    assert_failure
    assert_output_not_contains "Preparing sandbox"
}

# Test that start on Linux without /dev/kvm access fails with a clear
# message.
#   1. Skip if the host is not Linux or KVM is available
#   2. Run start and check exit code 1 and the KVM message
@test "start without KVM on Linux fails" {
    [[ "$(uname)" == "Linux" ]] || skip "Linux only"
    [[ ! -r /dev/kvm ]] || skip "KVM is available"
    write_config '[vm]'
    run_airlock start
    assert_failure 1
    assert_output_contains "KVM not available"
}

# Test that --network replaces the network policy of the config.
#   1. Write a config with deny-by-default
#   2. Run start --verbose --network allow-always
#   3. Check that the rules summary shows allow-always
@test "start --network overrides config policy in verbose rules summary" {
    write_config '[network]
policy = "deny-by-default"

[network.rules.example]
allow = ["example.com:443"]'
    run_airlock start --verbose --network allow-always
    assert_output_not_contains "Config error"
    assert_output_contains "(policy: allow-always)"
}

# Test that a pack added to an existing disk needs a terminal or --yes,
# and start stops before the image pull.
#   1. Make a sandbox disk
#   2. Add a pack with the version as a number, then as a string
#   3. Run start and check exit code 2, the message and that no install
#      records exist
@test "start with pack added to existing disk and no terminal fails before image pull" {
    make_sandbox_disk
    for version in '1' '"1"'; do
        write_config "$BAD_IMAGE

[packs]
python = { version = $version }"
        run_airlock start
        assert_failure 2
        assert_output_contains "Tools changed in the sandbox (added: python). Run in a terminal or pass --yes."
        assert_output_not_contains "Preparing sandbox"
        [[ ! -e .airlock/sandbox/installs.json ]]
    done
}

# Test that a pack removed from the config needs a terminal or --yes, and
# start keeps the disk.
#   1. Make a sandbox disk with an installed mise record
#   2. Write a config without packs and run start
#   3. Check exit code 2, the message and that the disk still exists
@test "start with pack removed from existing disk and no terminal fails before image pull" {
    make_sandbox_disk
    write_installs mise=installed
    write_config "$BAD_IMAGE"
    run_airlock start
    assert_failure 2
    assert_output_contains "Tools changed in the sandbox (removed: mise). Run in a terminal or pass --yes."
    assert_output_not_contains "Preparing sandbox"
    [[ -e .airlock/sandbox/disk.img ]]
}

# Test that packs on a new sandbox need no question, so start continues
# without a terminal. The CLI tests have no VM, so the run stops at the
# image pull. The VM tests check the install. The new sandbox goes to the
# data directory, not to the project.
#   1. Write a config with a pack and make no disk
#   2. Run start
#   3. Check that there is no question and that the sandbox prepare starts
#   4. Check that the project has no .airlock directory and that
#      `sandboxes list` has the project
@test "start with packs and no sandbox disk asks no question without terminal" {
    write_config "$BAD_IMAGE

[packs]
python = { version = 1 }"
    run_airlock start
    assert_failure
    assert_output_not_contains "Tools changed"
    assert_output_contains "Preparing sandbox"
    [[ ! -e .airlock ]]
    run_airlock sandboxes list
    assert_success
    assert_output_contains "$PWD"
}

# Test that --yes moves a sandbox from the project directory into the data
# directory, before the image pull.
#   1. Make a sandbox disk in the project and write a config
#   2. Run start with --yes
#   3. Check that the move is reported and the project sandbox is gone
#   4. Check that `sandboxes list` has the project
@test "start with --yes moves project sandbox to data directory" {
    make_sandbox_disk
    write_config "$BAD_IMAGE"
    run_airlock start --yes
    assert_failure
    assert_output_contains "sandbox moved to"
    [[ ! -e .airlock/sandbox ]]
    run_airlock sandboxes list
    assert_success
    assert_output_contains "$PWD"
}

# Test that without a terminal and without --yes, a sandbox in the project
# directory stays there, and start tells the user how to move it.
#   1. Make a sandbox disk in the project and write a config
#   2. Run start
#   3. Check the notice and that the disk stays in the project
@test "start without terminal keeps project sandbox and tells how to move it" {
    make_sandbox_disk
    write_config "$BAD_IMAGE"
    run_airlock start
    assert_failure
    assert_output_contains "run \`airlock start\` in a terminal to move it"
    [[ -e .airlock/sandbox/disk.img ]]
}

# Test that a list-form preset is not a pack change, so start asks no
# question on an existing disk.
#   1. Make a sandbox disk and write a config with presets = ["python"]
#   2. Run start
#   3. Check that there is no question and that the sandbox prepare starts
@test "start with list preset on existing disk installs nothing" {
    make_sandbox_disk
    write_config "presets = [\"python\"]

$BAD_IMAGE"
    run_airlock start
    assert_failure
    assert_output_not_contains "Tools changed"
    assert_output_contains "Preparing sandbox"
}

# Test that the claude-code list preset needs its token on the host, and
# start stops before the image pull without it.
#   1. Remove the token from the environment and write the preset
#   2. Run start
#   3. Check exit code 2 and the config error about the token
@test "start with claude-code preset and no token fails before image pull" {
    unset CLAUDE_CODE_OAUTH_TOKEN
    write_config "presets = [\"claude-code\"]

$BAD_IMAGE"
    run_airlock start
    assert_failure 2
    assert_output_contains "Config error: env.CLAUDE_CODE_OAUTH_TOKEN"
    assert_output_not_contains "Preparing sandbox"
}

# Test that agent packs need no credentials in the host environment,
# because the tokens are optional. The user can sign in later.
#   1. Remove the tokens and write a config with the claude, codex and
#      copilot packs
#   2. Run start
#   3. Check that there is no config error and no sign-in, and that the
#      sandbox prepare starts
@test "start with agent packs and no credentials passes env check" {
    unset CLAUDE_CODE_OAUTH_TOKEN ANTHROPIC_API_KEY OPENAI_API_KEY COPILOT_GITHUB_TOKEN
    write_config "$BAD_IMAGE

[packs]
claude = { version = 1 }
codex = { version = 1 }
copilot = { version = 1 }"
    run_airlock start
    assert_failure
    assert_output_not_contains "Config error"
    assert_output_not_contains "Signing in"
    assert_output_contains "Preparing sandbox"
}

# Test that install records that are not valid stop start when there is
# no terminal to ask the user.
#   1. Write a config and an installs.json that is not JSON
#   2. Run start
#   3. Check exit code 2 and the message, before the sandbox prepare
@test "start with corrupt install state and no terminal fails" {
    write_config "$BAD_IMAGE"
    mkdir -p .airlock/sandbox
    echo 'not json' >.airlock/sandbox/installs.json
    run_airlock start
    assert_failure 2
    assert_output_contains "installs.json is not valid"
    assert_output_not_contains "Preparing sandbox"
}
