#!/usr/bin/env bats

load helpers

# The stop command when no sandbox runs.

# Test that stop succeeds and tells the user when no sandbox runs, so
# that scripts can call it at any time.
#   1. Run airlock stop with no sandbox
#   2. Check exit code 0 and the message
#   3. Write a config and run airlock stop again
#   4. Check exit code 0 and the same message
@test "stop without running sandbox prints not running" {
    run_airlock stop
    assert_success
    assert_output_contains "The sandbox does not run"

    write_config '[vm]'
    run_airlock stop
    assert_success
    assert_output_contains "The sandbox does not run"
}
