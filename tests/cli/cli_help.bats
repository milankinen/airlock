#!/usr/bin/env bats

load helpers

@test "help and version print usage and version" {
    run_airlock --help
    assert_success
    assert_output_contains "Usage:"

    run_airlock -V
    assert_success
    assert_output_matches "^airlock [0-9]"
}

@test "start help lists network policy values" {
    run_airlock start --help
    assert_success
    assert_output_contains "--network <POLICY>"
    assert_output_contains "allow-always"
    assert_output_contains "deny-by-default"
}

@test "missing or unknown command or flag fails with usage error" {
    for args in "" "nonexistent" "start --bogus" "exec"; do
        run_airlock $args
        assert_failure 2
    done
}

@test "start with unknown network policy fails with usage error" {
    run_airlock start --network allow-all
    assert_failure 2
    assert_output_contains "invalid value 'allow-all'"
}
