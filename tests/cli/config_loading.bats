#!/usr/bin/env bats
# Tests for configuration file loading, parsing, and merging.
# Uses "airlock show" as entry point since it loads config without TTY gates.

load helpers

@test "invalid TOML syntax reports error" {
    write_config 'not [valid'
    run_airlock show
    assert_failure 2
    assert_output_matches "TOML|expected"
}

@test "unknown preset reports error" {
    write_config 'presets = ["nonexistent"]'
    run_airlock show
    assert_failure
    assert_output_contains "unknown preset"
}

@test "valid debian preset loads without config error" {
    write_config 'presets = ["debian"]'
    run_airlock show
    # Should fail on missing sandbox, not on config
    assert_failure
    assert_output_not_contains "Config error"
    assert_output_not_contains "unknown preset"
}

@test "valid rust preset loads without config error" {
    write_config 'presets = ["rust"]'
    run_airlock show
    assert_failure
    assert_output_not_contains "Config error"
}

@test "multiple presets load without config error" {
    write_config 'presets = ["debian", "rust", "claude-code"]'
    run_airlock show
    assert_failure
    assert_output_not_contains "Config error"
}

@test "empty config loads with defaults" {
    write_config ''
    run_airlock show
    assert_failure
    assert_output_not_contains "Config error"
}

@test "no config file loads with defaults" {
    run_airlock show
    assert_failure
    assert_output_not_contains "Config error"
}

@test "config merging: local env vars are included" {
    write_config '[env]
A = "from-base"'
    write_local_config '[env]
B = "from-local"'
    # Config loads successfully (fails later on missing sandbox)
    run_airlock show
    assert_failure
    assert_output_not_contains "Config error"
}

@test "config merging: local overrides base values" {
    write_config '[vm]
image = "base:1"'
    write_local_config '[vm]
image = "local:2"'
    run_airlock show
    assert_failure
    assert_output_not_contains "Config error"
}

@test "airlock.local.toml is loaded (regression)" {
    # This is a regression test for the Path::with_extension bug
    # where airlock.local.toml was silently skipped.
    write_config '[vm]'
    write_local_config '[env]
TEST_LOCAL_VAR = "loaded"'
    run_airlock show
    assert_failure
    assert_output_not_contains "Config error"
}

@test "masked env table form loads without config error" {
    write_config '[env]
TOKEN = { value = "static-token-value", mask = true }'
    run_airlock show
    assert_failure
    assert_output_not_contains "invalid configuration"
}

@test "inject of masked env var loads without config error" {
    write_config '[env]
TOKEN = { value = "static-token-value", mask = true }

[network.rules.api]
allow = ["api.example.com:443"]
inject = ["TOKEN"]'
    run_airlock show
    assert_failure
    assert_output_not_contains "must be defined in [env] with mask = true"
}

@test "inject of unmasked env var reports config error" {
    write_config '[env]
TOKEN = "plain"

[network.rules.api]
allow = ["api.example.com:443"]
inject = ["TOKEN"]'
    run_airlock show
    assert_failure
    assert_output_contains "network.rules.api.inject"
    assert_output_contains "TOKEN"
    assert_output_contains "must be defined in [env] with mask = true"
}

@test "inject of undefined env var reports config error" {
    write_config '[network.rules.api]
allow = ["api.example.com:443"]
inject = ["NOPE"]'
    run_airlock show
    assert_failure
    assert_output_contains "NOPE"
    assert_output_contains "must be defined in [env] with mask = true"
}

@test "masked env entry with unknown key reports config error" {
    write_config '[env]
TOKEN = { value = "x", masked = true }'
    run_airlock show
    assert_failure
    assert_output_contains "invalid configuration"
}

@test "local .airlock/airlock.toml is loaded" {
    mkdir -p .airlock/sandbox
    printf '[vm]\nimage = "local:1"\n' > .airlock/airlock.toml
    run_airlock show
    assert_success
    assert_output_contains "local:1"
}

@test "airlock.toml overrides local .airlock/airlock.toml" {
    mkdir -p .airlock/sandbox
    printf '[vm]\nimage = "local:1"\n' > .airlock/airlock.toml
    write_config '[vm]
image = "project:1"'
    run_airlock show
    assert_success
    assert_output_contains "project:1"
    assert_output_not_contains "local:1"
}

@test "local .airlock/airlock.toml overrides user config" {
    # run_airlock sets HOME to the test temp dir.
    printf '[vm]\nimage = "user:1"\n' > .airlock.toml
    mkdir -p .airlock/sandbox
    printf '[vm]\nimage = "local:1"\n' > .airlock/airlock.toml
    run_airlock show
    assert_success
    assert_output_contains "local:1"
    assert_output_not_contains "user:1"
}

@test "user ~/.airlock/airlock.toml is below ~/.airlock/config.toml" {
    # run_airlock sets HOME to the test temp dir; the project is a subdir.
    mkdir -p .airlock proj/.airlock/sandbox
    printf '[vm]\nimage = "user-airlock:1"\ncpus = 1\n' > .airlock/airlock.toml
    printf '[vm]\nimage = "user-config:1"\n' > .airlock/config.toml
    cd proj
    run_airlock show
    assert_success
    assert_output_contains "user-config:1"
    assert_output_contains "CPUs:     1"
}

@test "a leftover [tools] table in a user config file is ignored" {
    mkdir -p .airlock proj
    printf '[tools]\npython = {}\n' > .airlock/config.toml
    cd proj
    run_airlock show
    assert_output_not_contains "Config error"
    assert_output_not_contains "tools"
}

@test "[packs] with an unknown pack reports config error" {
    write_config '[packs]
nope = { version = 1 }'
    run_airlock show
    assert_failure 2
    assert_output_contains "invalid configuration"
    assert_output_contains "packs.nope"
    assert_output_contains "airlock.toml"
}

@test "[packs] with an unknown entry key reports config error" {
    mkdir -p .airlock/sandbox
    write_config '[packs]
codex = { version = 1, auth = "api-key" }'
    run_airlock show
    assert_failure 2
    assert_output_contains "invalid configuration"
    assert_output_contains "\`packs.codex.auth\` unknown key (known: version, enabled, args; args go in \`args = { auth = … }\`) (set in: $PWD/airlock.toml)"
}

@test "a leftover [tools] table in a project file is ignored" {
    mkdir -p .airlock/sandbox
    write_config '[tools]
python = {}'
    run_airlock show
    assert_success
    assert_output_not_contains "Packs:"
    assert_output_not_contains "python"
}

@test "a list in one file and a [packs] entry in another both apply" {
    mkdir -p .airlock proj/.airlock/sandbox
    printf 'presets = ["claude-code"]\n' > .airlock/config.toml
    cd proj
    write_config '[packs]
python = { version = "1" }'
    run_airlock show
    assert_success
    assert_output_contains "python 1 — pending"
    assert_output_not_contains "claude 1"
    assert_output_contains "claude-code: allow 4 deny 0"
}

@test "[packs] version 1 as a number equals version \"1\"" {
    mkdir -p .airlock/sandbox
    write_config '[packs]
python = { version = 1 }
rust = { version = "1" }'
    run_airlock show
    assert_success
    assert_output_contains "python 1 — pending"
    assert_output_contains "rust 1 — pending"
}

@test "[packs] with a fractional version reports config error" {
    write_config '[packs]
python = { version = 1.5 }'
    run_airlock show
    assert_failure 2
    assert_output_contains "\`packs.python.version\` must be a string or a whole number, not 1.5"
    assert_output_contains "$PWD/airlock.toml"
}

@test "[packs] entry without a version reports config error with a hint" {
    write_config '[packs]
python = {}'
    run_airlock show
    assert_failure 2
    assert_output_contains "\`packs.python\` needs \`version\` (set in: $PWD/airlock.toml): 1"
}

@test "[packs] with an unsupported version reports config error" {
    write_config '[packs]
python = { version = "7" }'
    run_airlock show
    assert_failure 2
    assert_output_contains "\`packs.python\`: version \"7\" is not supported (supported: \"1\")"
    assert_output_contains "$PWD/airlock.toml"
}

@test "[packs] version \"legacy\" reports config error with the list form" {
    write_config '[packs]
python = { version = "legacy" }'
    run_airlock show
    assert_failure 2
    assert_output_contains "\`packs.python\`: version \"legacy\" is not supported; use the list form \`presets = [\"python\"]\` (set in: $PWD/airlock.toml)"
}

@test "[packs] mise at version \"legacy\" reports config error" {
    write_config '[packs]
mise = { version = "legacy" }'
    run_airlock show
    assert_failure 2
    assert_output_contains "\`packs.mise\`: version \"legacy\" is not supported (supported: \"1\")"
}

@test "[packs] with a list name as a key reports config error" {
    write_config '[packs]
claude-code = { version = 1 }'
    run_airlock show
    assert_failure 2
    assert_output_contains "\`packs.claude-code\` unknown pack (known: alpine, debian, claude, codex,"
    assert_output_contains "\`claude-code\` is a list-form name: write \`presets = [\"claude-code\"]\`"
    assert_output_contains "$PWD/airlock.toml"
}

@test "presets list with claude reports config error" {
    write_config 'presets = ["claude"]'
    run_airlock show
    assert_failure 2
    assert_output_contains "$PWD/airlock.toml: unknown preset \`claude\` (known: claude-code,"
    assert_output_contains "; newer packs use the [packs] table of a project config file, for example \`[packs] claude = { version = 1 }\`"
}

@test "presets list with mise reports config error" {
    write_config 'presets = ["mise"]'
    run_airlock show
    assert_failure 2
    assert_output_contains "$PWD/airlock.toml: unknown preset \`mise\` (known: claude-code,"
    assert_output_contains "; newer packs use the [packs] table of a project config file, for example \`[packs] mise = { version = 1 }\`"
}

@test "presets list with an unknown name reports the file" {
    write_config 'presets = ["nope"]'
    run_airlock show
    assert_failure 2
    assert_output_contains "$PWD/airlock.toml: unknown preset \`nope\` (known: claude-code,"
}

@test "presets list with a number entry reports config error" {
    write_config 'presets = [1]'
    run_airlock show
    assert_failure 2
    assert_output_contains "$PWD/airlock.toml: \`presets\` list entries must be preset names (strings), not 1"
}

@test "presets = true reports config error" {
    write_config 'presets = true'
    run_airlock show
    assert_failure 2
    assert_output_contains "$PWD/airlock.toml: \`presets\` must be a list of preset names, not true"
}

@test "presets as a string reports config error" {
    write_config 'presets = "python"'
    run_airlock show
    assert_failure 2
    assert_output_contains "$PWD/airlock.toml: \`presets\` must be a list of preset names, not \"python\""
}

@test "presets: null in YAML is treated as absent" {
    printf 'presets:\n' > airlock.yaml
    run_airlock show
    # Should fail on missing sandbox, not on config
    assert_failure
    assert_output_not_contains "Config error"
}

@test "presets: null in JSON is treated as absent" {
    printf '{"presets": null}\n' > airlock.json
    run_airlock show
    assert_failure
    assert_output_not_contains "Config error"
}

@test "valid copilot-cli preset loads" {
    mkdir -p .airlock/sandbox
    write_config 'presets = ["copilot-cli"]'
    run_airlock show
    assert_success
}


@test "[packs] enabled = false in a project file disables a local-file entry" {
    mkdir -p proj/.airlock/sandbox
    printf '[packs]\npython = { version = 1 }\n' > proj/.airlock/airlock.toml
    cd proj
    write_config '[packs]
python = { enabled = false }'
    run_airlock show
    assert_success
    assert_output_not_contains "python"
}
