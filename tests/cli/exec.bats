#!/usr/bin/env bats

load helpers

# The exec command when no sandbox exists.

# Test that exec fails with a clear message when 'airlock start' never
# prepared a sandbox. Exec does not start a sandbox from the config alone.
#   1. Run airlock exec without a config and check the error
#   2. Write a config, run airlock exec again and check the same error
@test "exec without started sandbox fails with or without config" {
    run_airlock exec bash
    assert_failure
    assert_output_contains "'airlock start' first"

    write_config '[vm]'
    run_airlock exec bash
    assert_failure
    assert_output_contains "'airlock start' first"
}
