#!/usr/bin/env bats

load helpers

@test "exec without running sandbox fails with or without config" {
    run_airlock exec bash
    assert_failure
    assert_output_contains "no running sandbox"

    write_config '[vm]'
    run_airlock exec bash
    assert_failure
    assert_output_contains "no running sandbox"
}
