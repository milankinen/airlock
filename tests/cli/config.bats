#!/usr/bin/env bats

load helpers

# The config command: export of the project config.

# Test that config prints the project files as TOML by default and does
# not print the user files. The user may save the output as a project file.
#   1. Write a user config, airlock.toml and airlock.local.toml
#   2. Run config
#   3. Check that the project values merge and the user value is not there
@test "config prints merged project files without user files" {
    mkdir -p .airlock proj
    printf '[vm]\nimage = "user:1"\ncpus = 7\n' >.airlock/config.toml
    cd proj
    write_config '[vm]
image = "project:1"

[packs]
python = { version = 1 }'
    write_local_config '[vm]
memory = 2048'
    run_airlock config
    assert_success
    assert_output_contains 'image = "project:1"'
    assert_output_contains 'memory = 2048'
    assert_output_contains '[packs.python]'
    assert_output_not_contains 'user:1'
    assert_output_not_contains 'cpus'
}

# Test that the JSON and YAML formats give parsable output with the same
# values.
#   1. Write a config
#   2. Run config with --format json and parse the output
#   3. Run config with --format yaml and check the keys
@test "config json and yaml formats parse" {
    write_config '[vm]
image = "project:1"'
    run_airlock config --format json
    assert_success
    python3 -c 'import json,sys; assert json.load(sys.stdin)["vm"]["image"] == "project:1"' <<<"$output"

    run_airlock config --format yaml
    assert_success
    assert_output_matches '^vm:'
    assert_output_matches '^  image: .?project:1'
}

# Test that config without a project config prints a warning to stderr
# and an empty document to stdout. Thus a saved export stays valid.
#   1. Run config with JSON format in an empty directory
#   2. Check exit code 0, that stdout is "{}" and that stderr has the
#      warning
@test "config without project config warns and prints empty document" {
    local err="$TEST_TEMP_DIR/stderr" out
    out="$(env XDG_DATA_HOME="$TEST_DATA_HOME" NO_COLOR=1 HOME="$TEST_TEMP_DIR" \
        RUST_BACKTRACE=0 "$AIRLOCK" config --format json 2>"$err" </dev/null)"
    [ "$out" = "{}" ]
    grep -q "No project config for $PWD" "$err"
}

# Test that config --resolved fails when the sandbox has no stored last run.
#   1. Write a config and make a sandbox
#   2. Run config --resolved
#   3. Check exit code 1 and the error message
@test "config resolved without last run fails" {
    make_sandbox_disk
    write_config '[vm]'
    run_airlock config --resolved
    assert_failure 1
    assert_output_contains "Cannot export config:"
    assert_output_contains "No config of a last run for $PWD — run \`airlock start\` first"
}

# Test that sandboxes config fails for an id that does not exist.
#   1. Run sandboxes config with an unknown id
#   2. Check exit code 1 and the error message
@test "sandboxes config with unknown id fails" {
    run_airlock sandboxes config nosuchid
    assert_failure 1
    assert_output_contains "Cannot export config:"
}

# Test that config --resolved prints the merged config of the last run,
# including user files, but without the `[packs]` table. The stored run
# has a user layer and a project layer with a pack.
#   1. Make a sandbox with a stored last run config
#   2. Run config --resolved
#   3. Check that the user and project values are there and `[packs` is not
@test "config resolved prints last run config with user values and without packs" {
    make_sandbox_disk
    write_config '[vm]'
    printf '%s' '{"last_run_config":{"user":[{"origin":"u","value":{"vm":{"cpus":3}},"legacy_presets":null}],"local":null,"project":[{"origin":"p","value":{"vm":{"image":"alpine:3"},"packs":{"python":{"version":1}}},"legacy_presets":null}]}}' \
        >"$BOX_DIR/run.json"
    run_airlock config --resolved
    assert_success
    assert_output_contains 'cpus = 3'
    assert_output_contains 'name = "alpine:3"'
    assert_output_not_contains '[packs'
}

# Test that config writes only the config to stdout. Log and status
# messages go to stderr, so a script can pipe the output to a file.
#   1. Write a config
#   2. Run config --format json with stderr dropped
#   3. Parse stdout as JSON and check the value
@test "config json stdout is clean without stderr" {
    write_config '[vm]
image = "project:1"'
    local out
    out="$(env XDG_DATA_HOME="$TEST_DATA_HOME" NO_COLOR=1 HOME="$TEST_TEMP_DIR" \
        RUST_BACKTRACE=0 "$AIRLOCK" config --format json 2>/dev/null </dev/null)"
    python3 -c 'import json,sys; assert json.load(sys.stdin)["vm"]["image"] == "project:1"' <<<"$out"
}

# Test that an MCP server name with a character outside letters, digits,
# `-` and `_` is a config error. The name is one URL path segment.
#   1. Write an [mcp."bad/name"] entry with a command
#   2. Run info
#   3. Check the config error that names the entry
@test "mcp server with invalid name reports config error" {
    assert_config_error info '[mcp."bad/name"]
command = "mise"' "mcp.bad/name" "letters, digits"
}

# Test that an MCP server with an empty command is a config error.
#   1. Write an [mcp.mise] entry with an empty command
#   2. Run info
#   3. Check the config error that names the entry
@test "mcp server with empty command reports config error" {
    assert_config_error info '[mcp.mise]
command = ""' "mcp.mise.command" "must not be empty"
}
