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

@test "airlock start --help lists the pack install flags" {
    run_airlock start --help
    assert_success
    assert_output_contains "--yes"
    assert_output_not_contains "--reauth"
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

@test "airlock start --help lists --network policy values" {
    run_airlock start --help
    assert_success
    assert_output_contains "--network <POLICY>"
    assert_output_contains "allow-always"
    assert_output_contains "deny-by-default"
}

@test "airlock start --network with unknown policy fails" {
    run_airlock start --network allow-all
    assert_failure 2
    assert_output_contains "invalid value 'allow-all'"
}

@test "airlock start --help describes the install flags with packs" {
    run_airlock start --help
    assert_success
    assert_output_contains "Answer every sandbox question with its default (re-create the sandbox)"
    assert_output_not_contains "sign in"
    assert_output_not_contains "tools"
}

@test "airlock start --help does not list the removed sign-in flags" {
    run_airlock start --help
    assert_success
    assert_output_not_contains "--no-agent-signin"
    assert_output_not_contains "--reauth"
}

@test "airlock start --reauth is not a flag" {
    run_airlock start --reauth
    assert_failure 2
    assert_output_contains "unexpected argument '--reauth'"
}

@test "airlock start --no-agent-signin is not a flag" {
    run_airlock start --no-agent-signin
    assert_failure 2
    assert_output_contains "unexpected argument '--no-agent-signin'"
}

@test "airlock agents is not a command" {
    run_airlock agents
    assert_failure 2
    assert_output_contains "unrecognized subcommand 'agents'"
}
