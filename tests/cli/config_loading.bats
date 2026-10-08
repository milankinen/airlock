#!/usr/bin/env bats

load helpers

# Config loading through the show command: file formats, precedence of
# files, presets, packs and env errors.

# Test that broken TOML gives a config error with the parser message.
#   1. Write airlock.toml that is not valid TOML and run show
#   2. Check exit code 2 and the TOML error
@test "invalid TOML syntax reports config error" {
    assert_config_error show 'not [valid'
    assert_output_matches "TOML|expected"
}

# Test that a missing config, an empty config and presets set to null all
# give the default config.
#   1. Run show without a config and check the default values
#   2. Run show again with an empty TOML file, a YAML file and a JSON file
#      with null presets
@test "missing, empty or null-presets config loads defaults" {
    mkdir -p .airlock/sandbox
    run_airlock show
    assert_success
    assert_output_contains "Image:"
    assert_output_contains "Network policy:"

    for file in airlock.toml airlock.yaml airlock.json; do
        case "$file" in
        airlock.toml) printf '' >"$file" ;;
        airlock.yaml) printf 'presets:\n' >"$file" ;;
        airlock.json) printf '{"presets": null}\n' >"$file" ;;
        esac
        run_airlock show
        assert_success
        assert_output_contains "Image:"
        rm "$file"
    done
}

# Test that the config files apply in the correct order, so that a later
# file overrides an earlier one.
#   1. Write the two files in ~/.airlock and check that config.toml wins
#   2. Add ~/.airlock.toml, the local .airlock/airlock.toml, airlock.toml
#      and airlock.local.toml, one by one
#   3. After each file, check that its image is used
#   4. Check that the CPU value of the first file stays
@test "config files of user and project apply in precedence order" {
    mkdir -p .airlock proj/.airlock/sandbox
    printf '[vm]\nimage = "user-airlock:1"\ncpus = 1\n' >.airlock/airlock.toml
    printf '[vm]\nimage = "user-config:1"\n' >.airlock/config.toml
    cd proj

    run_airlock show
    assert_success
    assert_output_contains "Image:    user-config:1"
    assert_output_contains "CPUs:     1"

    printf '[vm]\nimage = "user:1"\n' >"$TEST_TEMP_DIR/.airlock.toml"
    run_airlock show
    assert_output_contains "Image:    user:1"

    printf '[vm]\nimage = "local:1"\n' >.airlock/airlock.toml
    run_airlock show
    assert_output_contains "Image:    local:1"

    write_config '[vm]
image = "project:1"'
    run_airlock show
    assert_output_contains "Image:    project:1"

    write_local_config '[vm]
image = "project-local:1"'
    run_airlock show
    assert_success
    assert_output_contains "Image:    project-local:1"
    assert_output_contains "CPUs:     1"
}

# Test that the list-form presets load and add their network rules.
#   1. Write a config with four list presets
#   2. Run show and check the claude-code rule
@test "list presets load with their network rules" {
    mkdir -p .airlock/sandbox
    write_config 'presets = ["debian", "rust", "claude-code", "copilot-cli"]'
    run_airlock show
    assert_success
    assert_output_contains "claude-code: allow 4 deny 0"
}

# Test that a network rule can inject a masked env var, and that show
# does not print its value.
#   1. Write a masked env var and a rule that injects it
#   2. Run show
#   3. Check the rule summary and that the value is not in the output
@test "masked env injected into network rule loads" {
    mkdir -p .airlock/sandbox
    write_config '[env]
TOKEN = { value = "static-token-value", mask = true }

[network.rules.api]
allow = ["api.example.com:443"]
inject = ["TOKEN"]'
    run_airlock show
    assert_success
    assert_output_contains "api: allow 1 deny 0"
    assert_output_not_contains "static-token-value"
}

# Test that a rule can inject only a masked env var, so that a secret
# that is not masked does not go to the network.
#   1. Inject an env var that is not masked and check the error
#   2. Inject an env var that is not defined and check the error
@test "inject of unmasked or undefined env var reports config error" {
    assert_config_error show '[env]
TOKEN = "plain"

[network.rules.api]
allow = ["api.example.com:443"]
inject = ["TOKEN"]' "network.rules.api.inject" "TOKEN" "must be defined in [env] with mask = true"

    assert_config_error show '[network.rules.api]
allow = ["api.example.com:443"]
inject = ["NOPE"]' "NOPE" "must be defined in [env] with mask = true"
}

# Test that a typo in a masked env entry is a config error.
#   1. Write an env entry with "masked" in place of "mask"
#   2. Run show and check the config error
@test "masked env entry with unknown key reports config error" {
    assert_config_error show '[env]
TOKEN = { value = "x", masked = true }' "invalid configuration"
}

# Test that an old [tools] table in a config is ignored and adds no packs.
#   1. Write a [tools] table in the user config and the project config
#   2. Run show
#   3. Check that there are no packs
@test "leftover tools table in user or project config is ignored" {
    mkdir -p .airlock proj/.airlock/sandbox
    printf '[tools]\npython = {}\n' >.airlock/config.toml
    cd proj
    write_config '[tools]
python = {}'
    run_airlock show
    assert_success
    assert_output_not_contains "Packs:"
    assert_output_not_contains "python"
}

# Test that a presets list in the user config and a [packs] table in the
# project config both apply.
#   1. Write presets = ["claude-code"] in the user config
#   2. Write a [packs] table with two packs in the project config
#   3. Run show and check both packs and the preset network rule, and that
#      the preset does not become the claude pack
@test "packs list in one file and packs table in another both apply" {
    mkdir -p .airlock proj/.airlock/sandbox
    printf 'presets = ["claude-code"]\n' >.airlock/config.toml
    cd proj
    write_config '[packs]
python = { version = "1" }
rust = { version = 1 }'
    run_airlock show
    assert_success
    assert_output_contains "python 1 — pending"
    assert_output_contains "rust 1 — pending"
    assert_output_not_contains "claude 1"
    assert_output_contains "claude-code: allow 4 deny 0"
}

# Test that a pack disabled in airlock.toml overrides the same pack in the
# local .airlock/airlock.toml.
#   1. Enable python in the local config and disable it in airlock.toml
#   2. Run show and check that python is not shown
@test "packs entry disabled in project file overrides local file entry" {
    mkdir -p proj/.airlock/sandbox
    printf '[packs]\npython = { version = 1 }\n' >proj/.airlock/airlock.toml
    cd proj
    write_config '[packs]
python = { enabled = false }'
    run_airlock show
    assert_success
    assert_output_not_contains "python"
}

# Test that each wrong [packs] entry gives a config error that names the
# entry and the file.
#   1. Write unknown packs, unknown keys, bad versions and a list-form name
#      in [packs], one at a time
#   2. Run show for each
#   3. Check exit code 2 and the message for each
@test "invalid packs entries report config error naming entry and file" {
    mkdir -p .airlock/sandbox
    local file="$PWD/airlock.toml"

    assert_config_error show '[packs]
nope = { version = 1 }' "invalid configuration" "packs.nope" "$file"

    assert_config_error show '[packs]
codex = { version = 1, auth = "api-key" }' \
        "\`packs.codex.auth\` unknown key (known: version, enabled, args; args go in \`args = { auth = … }\`) (set in: $file)"

    assert_config_error show '[packs]
python = { version = 1.5 }' \
        "\`packs.python.version\` must be a string or a whole number, not 1.5" "$file"

    assert_config_error show '[packs]
python = {}' "\`packs.python\` needs \`version\` (set in: $file): 1"

    assert_config_error show '[packs]
python = { version = "7" }' \
        "\`packs.python\`: version \"7\" is not supported (supported: \"1\")" "$file"

    assert_config_error show '[packs]
python = { version = "legacy" }' \
        "\`packs.python\`: version \"legacy\" is not supported; use the list form \`presets = [\"python\"]\` (set in: $file)"

    assert_config_error show '[packs]
mise = { version = "legacy" }' "\`packs.mise\`: version \"legacy\" is not supported (supported: \"1\")"

    assert_config_error show '[packs]
claude-code = { version = 1 }' \
        "\`packs.claude-code\` unknown pack (known: alpine, debian, claude, codex," \
        "\`claude-code\` is a list-form name: write \`presets = [\"claude-code\"]\`" "$file"
}

# Test that each wrong presets value gives a config error that names the
# file.
#   1. Write unknown presets, pack names, a number, a boolean and a string
#      as presets, one at a time
#   2. Run show for each
#   3. Check exit code 2 and the message for each
@test "invalid presets values report config error naming file" {
    local file="$PWD/airlock.toml"

    assert_config_error show 'presets = ["nope"]' \
        "$file: unknown preset \`nope\` (known: claude-code,"

    assert_config_error show 'presets = ["claude"]' \
        "$file: unknown preset \`claude\` (known: claude-code," \
        "; newer packs use the [packs] table of a project config file, for example \`[packs] claude = { version = 1 }\`"

    assert_config_error show 'presets = ["mise"]' \
        "; newer packs use the [packs] table of a project config file, for example \`[packs] mise = { version = 1 }\`"

    assert_config_error show 'presets = [1]' \
        "$file: \`presets\` list entries must be preset names (strings), not 1"

    assert_config_error show 'presets = true' \
        "$file: \`presets\` must be a list of preset names, not true"

    assert_config_error show 'presets = "python"' \
        "$file: \`presets\` must be a list of preset names, not \"python\""
}
