#!/usr/bin/env bats
# Tests for CLI help, version, and argument parsing.

load helpers

@test "airlock --help shows usage" {
    run_airlock --help
    assert_success
    assert_output_contains "Usage:"
}

@test "airlock -h shows usage" {
    run_airlock -h
    assert_success
    assert_output_contains "Usage:"
}

@test "airlock -V shows version" {
    run_airlock -V
    assert_success
    assert_output_matches "^airlock [0-9]"
}

@test "airlock start --help shows start options" {
    run_airlock start --help
    assert_success
    assert_output_contains "log-level"
}

@test "airlock exec --help shows exec options" {
    run_airlock exec --help
    assert_success
    assert_output_contains "env"
}

@test "airlock show --help succeeds" {
    run_airlock show --help
    assert_success
}

@test "airlock rm --help succeeds" {
    run_airlock rm --help
    assert_success
}

@test "airlock with no args fails" {
    run_airlock
    assert_failure 2
}

@test "airlock unknown subcommand fails" {
    run_airlock nonexistent
    assert_failure 2
}

@test "airlock start --bogus fails" {
    run_airlock start --bogus
    assert_failure 2
}
