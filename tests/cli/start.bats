#!/usr/bin/env bats

load helpers

BAD_IMAGE='[vm]
image = "airlock-test.invalid/no-such-image:1"'

has_kvm_or_not_linux() {
    [[ "$(uname)" != "Linux" ]] || [[ -r /dev/kvm ]]
}

setup() {
    setup_temp_dir
    if [[ "$BATS_TEST_DESCRIPTION" != *"without KVM"* ]]; then
        has_kvm_or_not_linux || skip "no KVM access"
    fi
}

@test "start without config fails in non-interactive mode" {
    run_airlock start
    assert_failure 2
    assert_output_contains "No airlock config in $PWD. Run \`airlock start\` in a terminal, or create airlock.toml."
    [[ ! -e airlock.toml && ! -e .airlock/airlock.toml ]]
}

@test "start with sandbox state but no config fails" {
    make_sandbox_disk
    run_airlock start
    assert_failure 2
    assert_output_contains "A sandbox exists in $PWD, but there is no config."
    assert_output_contains "Put the config in airlock.toml or .airlock/airlock.toml (restore it), or run \`airlock rm\` to start over."

    rm .airlock/sandbox/disk.*
    echo '{}' >.airlock/sandbox/installs.json
    run_airlock start
    assert_failure 2
    assert_output_contains "A sandbox exists in $PWD, but there is no config."
}

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

@test "start --quiet suppresses log output" {
    write_config "$BAD_IMAGE"
    run_airlock start
    assert_output_contains "Preparing sandbox"

    run_airlock --quiet start
    assert_failure
    assert_output_not_contains "Preparing sandbox"
}

@test "start without KVM on Linux fails" {
    [[ "$(uname)" == "Linux" ]] || skip "Linux only"
    [[ ! -r /dev/kvm ]] || skip "KVM is available"
    write_config '[vm]'
    run_airlock start
    assert_failure 1
    assert_output_contains "KVM not available"
}

@test "start --network overrides config policy in verbose rules summary" {
    write_config '[network]
policy = "deny-by-default"

[network.rules.example]
allow = ["example.com:443"]'
    run_airlock start --verbose --network allow-always
    assert_output_not_contains "Config error"
    assert_output_contains "(policy: allow-always)"
}

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

@test "start with packs and no sandbox disk installs them without terminal" {
    write_config "$BAD_IMAGE

[packs]
python = { version = 1 }"
    run_airlock start
    assert_failure
    assert_output_not_contains "Tools changed"
    assert_output_contains "Preparing sandbox"
    [[ ! -e .airlock/sandbox/disk.img ]]
}

@test "start with list preset on existing disk installs nothing" {
    make_sandbox_disk
    write_config "presets = [\"python\"]

$BAD_IMAGE"
    run_airlock start
    assert_failure
    assert_output_not_contains "Tools changed"
    assert_output_contains "Preparing sandbox"
}

@test "start with claude-code preset and no token fails before image pull" {
    unset CLAUDE_CODE_OAUTH_TOKEN
    write_config "presets = [\"claude-code\"]

$BAD_IMAGE"
    run_airlock start
    assert_failure 2
    assert_output_contains "Config error: env.CLAUDE_CODE_OAUTH_TOKEN"
    assert_output_not_contains "Preparing sandbox"
}

@test "start with agent packs and no credentials passes env check" {
    unset CLAUDE_CODE_OAUTH_TOKEN OPENAI_API_KEY
    write_config "$BAD_IMAGE

[packs]
claude = { version = 1 }
codex = { version = 1 }"
    run_airlock start
    assert_failure
    assert_output_not_contains "Config error"
    assert_output_not_contains "Signing in"
    assert_output_contains "Preparing sandbox"
}

@test "start with corrupt install state and no terminal fails" {
    write_config "$BAD_IMAGE"
    mkdir -p .airlock/sandbox
    echo 'not json' >.airlock/sandbox/installs.json
    run_airlock start
    assert_failure 2
    assert_output_contains "installs.json is not valid"
    assert_output_not_contains "Preparing sandbox"
}
