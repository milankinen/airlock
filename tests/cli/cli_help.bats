#!/usr/bin/env bats

load helpers

# Help, version and usage errors of the command line.

# Test that --help and -V work, so that users can find the commands and
# the version.
#   1. Run airlock --help and check that it prints the usage
#   2. Run airlock -V and check that it prints the version
@test "help and version print usage and version" {
    run_airlock --help
    assert_success
    assert_output_contains "Usage:"

    run_airlock -V
    assert_success
    assert_output_matches "^airlock [0-9]"
}

# Test that the help of start shows the --network flag and its policy
# values.
#   1. Run airlock start --help
#   2. Check that the flag and the policy values are in the output
@test "start help lists network policy values" {
    run_airlock start --help
    assert_success
    assert_output_contains "--network <POLICY>"
    assert_output_contains "allow-always"
    assert_output_contains "deny-by-default"
}

# Test that a bad command line exits with the usage error code 2.
#   1. Run airlock with no command, an unknown command, an unknown flag,
#      and exec without a command
#   2. Check that each run exits with code 2
@test "missing or unknown command or flag fails with usage error" {
    for args in "" "nonexistent" "start --bogus" "exec"; do
        run_airlock $args
        assert_failure 2
    done
}

# Test that an unknown --network value is a usage error that names the
# value.
#   1. Run airlock start --network allow-all
#   2. Check exit code 2 and the error message
@test "start with unknown network policy fails with usage error" {
    run_airlock start --network allow-all
    assert_failure 2
    assert_output_contains "invalid value 'allow-all'"
}
