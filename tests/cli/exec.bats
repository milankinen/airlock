#!/usr/bin/env bats
# Tests for "airlock exec" without a running VM.

load helpers

@test "exec without VM reports no running VM" {
    write_config '[vm]'
    run_airlock exec bash
    assert_failure
    assert_output_contains "no running VM"
}

@test "exec without config still reports no running VM" {
    run_airlock exec bash
    assert_failure
    assert_output_contains "no running VM"
}

@test "exec without command argument fails" {
    write_config '[vm]'
    run_airlock exec
    assert_failure 2
}
