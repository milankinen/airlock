# Load shared helpers from parent directory, then add CLI test helpers.
source "$(dirname "${BASH_SOURCE[0]}")/../helpers.bash"

# Write $2 as airlock.toml, run "airlock $1", and expect a config error
# (exit 2) whose output contains each further argument.
assert_config_error() {
    local command="$1" config="$2"
    shift 2
    write_config "$config"
    run_airlock "$command"
    assert_failure 2
    local expected
    for expected in "$@"; do
        assert_output_contains "$expected" || return 1
    done
}
