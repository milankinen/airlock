#!/usr/bin/env bats
# Tests for "airlock start" error paths (non-interactive, no VM boot).

load helpers

# On Linux without KVM, "airlock start" exits immediately with a KVM error
# (the system check comes first, before config loading and every sandbox
# check). These tests skip in that case.

has_kvm_or_not_linux() {
    [[ "$(uname)" != "Linux" ]] || [[ -r /dev/kvm ]]
}

@test "start with no config file fails in non-interactive mode" {
    has_kvm_or_not_linux || skip "no KVM access"
    run_airlock start
    assert_failure 2
    assert_output_contains "No airlock config in $PWD. Run \`airlock start\` in a terminal, or create airlock.toml."
    [[ ! -e airlock.toml && ! -e .airlock/airlock.toml ]]
}

# The system check (KVM) comes first: guarded.
@test "start with a sandbox disk but no config fails" {
    has_kvm_or_not_linux || skip "no KVM access"
    mkdir -p .airlock/sandbox
    touch .airlock/sandbox/disk.img
    run_airlock start
    assert_failure 2
    assert_output_contains "A sandbox exists in $PWD, but there is no config."
    assert_output_contains "Put the config in airlock.toml or .airlock/airlock.toml (restore it), or run \`airlock rm\` to start over."
}

@test "start with an install state but no config fails" {
    has_kvm_or_not_linux || skip "no KVM access"
    mkdir -p .airlock/sandbox
    echo '{}' >.airlock/sandbox/installs.json
    run_airlock start
    assert_failure 2
    assert_output_contains "A sandbox exists in $PWD, but there is no config."
}

@test "start with invalid config reports config error" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config 'not valid toml ['
    run_airlock start
    assert_failure 2
    assert_output_contains "Config error"
}

@test "start with unknown preset reports error" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config 'presets = ["does-not-exist"]'
    run_airlock start
    assert_failure 2
    assert_output_contains "unknown preset"
}

@test "start with valid config gets past config loading" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[vm]'
    run_airlock start
    # Will fail somewhere after config loading (no VM infrastructure),
    # but should NOT fail with "Config error" or "No airlock.toml"
    assert_output_not_contains "Config error"
    assert_output_not_contains "No airlock.toml"
}

@test "start --quiet suppresses log output" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[vm]'
    run_airlock --quiet start
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
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[network]
policy = "deny-by-default"

[network.rules.example]
allow = ["example.com:443"]'
    run_airlock start --verbose --network allow-always
    assert_output_not_contains "Config error"
    assert_output_contains "(policy: allow-always)"
}

@test "start --help lists the pack install flags" {
    run_airlock start --help
    assert_success
    assert_output_not_contains "--reauth"
    assert_output_contains "--yes"
}

@test "start with tools added and no terminal fails before the image pull" {
    has_kvm_or_not_linux || skip "no KVM access"
    # An image that cannot be pulled: the check must come first.
    write_config '[vm]
image = "airlock-test.invalid/no-such-image:1"

[packs]
python = { version = 1 }'
    # An existing sandbox disk: a pack added to it needs the question.
    mkdir -p .airlock/sandbox
    truncate -s 1M .airlock/sandbox/disk.img
    echo 0123456789abcdef0123456789abcdef >.airlock/sandbox/disk.id
    run_airlock start
    assert_failure 2
    assert_output_contains "Tools changed in the sandbox (added: python). Run in a terminal or pass --yes."
    assert_output_not_contains "Preparing sandbox"
    [[ ! -e .airlock/sandbox/installs.json ]]
}

@test "start with tools removed and no terminal fails before the image pull" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[vm]
image = "airlock-test.invalid/no-such-image:1"'
    # An existing sandbox disk with an install record of mise.
    mkdir -p .airlock/sandbox
    truncate -s 1M .airlock/sandbox/disk.img
    echo 0123456789abcdef0123456789abcdef >.airlock/sandbox/disk.id
    local fp
    fp="$(printf 'a%.0s' {1..64})"
    echo '{"version":1,"disk":[81985529216486895,81985529216486895],"packs":{"mise":{"status":"installed","fingerprint":"'"$fp"'","at":1}}}' \
        >.airlock/sandbox/installs.json
    run_airlock start
    assert_failure 2
    assert_output_contains "Tools changed in the sandbox (removed: mise). Run in a terminal or pass --yes."
    assert_output_not_contains "Preparing sandbox"
    [[ -e .airlock/sandbox/disk.img ]]
}

@test "start with packs and no sandbox disk installs them without a terminal" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[vm]
image = "airlock-test.invalid/no-such-image:1"

[packs]
python = { version = 1 }'
    run_airlock start
    # The image pull fails, but not the early install check.
    assert_failure
    assert_output_not_contains "Tools changed"
    assert_output_contains "Preparing sandbox"
    # The disk comes after the image.
    [[ ! -e .airlock/sandbox/disk.img ]]
}

@test "start with the claude-code preset and no token fails before the image pull" {
    has_kvm_or_not_linux || skip "no KVM access"
    unset CLAUDE_CODE_OAUTH_TOKEN
    write_config 'presets = ["claude-code"]

[vm]
image = "airlock-test.invalid/no-such-image:1"'
    run_airlock start
    assert_failure 2
    assert_output_contains "Config error: env.CLAUDE_CODE_OAUTH_TOKEN"
    assert_output_not_contains "Preparing sandbox"
}

@test "start with the agent packs and no credentials passes the env check" {
    has_kvm_or_not_linux || skip "no KVM access"
    unset CLAUDE_CODE_OAUTH_TOKEN OPENAI_API_KEY
    write_config '[vm]
image = "airlock-test.invalid/no-such-image:1"

[packs]
claude = { version = 1 }
codex = { version = 1 }'
    run_airlock start
    # The image pull fails, but no sign-in, credential check or env error
    # comes before it.
    assert_failure
    assert_output_not_contains "Config error"
    assert_output_not_contains "Signing in"
    assert_output_contains "Preparing sandbox"
}

@test "start with a corrupt install state and no terminal fails" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[vm]
image = "airlock-test.invalid/no-such-image:1"'
    mkdir -p .airlock/sandbox
    echo 'not json' >.airlock/sandbox/installs.json
    run_airlock start
    assert_failure 2
    assert_output_contains "installs.json is not valid"
    assert_output_not_contains "Preparing sandbox"
}

@test "start with an unknown pack entry key and no terminal fails" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[packs]
codex = { version = 1, auth = "api-key" }'
    run_airlock start
    assert_failure 2
    assert_output_contains "\`packs.codex.auth\` unknown key (known: version, enabled, args; args go in \`args = { auth = … }\`) (set in: $PWD/airlock.toml)"
    [[ "$(cat airlock.toml)" == '[packs]
codex = { version = 1, auth = "api-key" }' ]]
}

@test "start with an invalid presets value reports the file" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config 'presets = true'
    run_airlock start
    assert_failure 2
    assert_output_contains "$PWD/airlock.toml: \`presets\` must be a list of preset names, not true"
}

@test "start with a string version \"1\" on an existing disk needs the tools question" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config '[vm]
image = "airlock-test.invalid/no-such-image:1"

[packs]
python = { version = "1" }'
    mkdir -p .airlock/sandbox
    truncate -s 1M .airlock/sandbox/disk.img
    echo 0123456789abcdef0123456789abcdef >.airlock/sandbox/disk.id
    run_airlock start
    assert_failure 2
    assert_output_contains "Tools changed in the sandbox (added: python). Run in a terminal or pass --yes."
}

@test "start with a list preset on an existing disk installs nothing" {
    has_kvm_or_not_linux || skip "no KVM access"
    write_config 'presets = ["python"]

[vm]
image = "airlock-test.invalid/no-such-image:1"'
    mkdir -p .airlock/sandbox
    truncate -s 1M .airlock/sandbox/disk.img
    echo 0123456789abcdef0123456789abcdef >.airlock/sandbox/disk.id
    run_airlock start
    assert_failure
    assert_output_not_contains "Tools changed"
    assert_output_contains "Preparing sandbox"
}
