#!/usr/bin/env bats
# Tests for "airlock rm" behavior.

load helpers

@test "rm -f without cache dir succeeds silently" {
    write_config '[vm]'
    run_airlock rm -f
    assert_success
}

@test "rm -f removes .airlock directory" {
    write_config '[vm]'
    mkdir -p .airlock
    run_airlock rm -f
    assert_success
    assert_output_contains "Sandbox removed"
    [[ ! -d .airlock ]]
}

@test "rm --force synonym works" {
    write_config '[vm]'
    run_airlock rm --force
    assert_success
}
