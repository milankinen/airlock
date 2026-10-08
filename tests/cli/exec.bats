#!/usr/bin/env bats

load helpers

# The exec command when no sandbox runs.

# Test that exec fails with a clear message when no sandbox runs.
#   1. Run airlock exec without a config and check the error
#   2. Write a config, run airlock exec again and check the same error
@test "exec without running sandbox fails with or without config" {
    run_airlock exec bash
    assert_failure
    assert_output_contains "no running sandbox"

    write_config '[vm]'
    run_airlock exec bash
    assert_failure
    assert_output_contains "no running sandbox"
}
