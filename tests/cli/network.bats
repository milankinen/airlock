#!/usr/bin/env bats

load helpers

# The network forward command when no sandbox runs, and its argument
# checks.

# Test that network forward fails with a clear message when no sandbox
# runs.
#   1. Run airlock network forward with a valid forward
#   2. Check that it fails with the no running sandbox error
@test "network forward without running sandbox fails" {
    run_airlock network forward 8080
    assert_failure 1
    assert_output_contains "no running sandbox"
}

# Test that bad forward specs are usage errors, so that no connection to
# the sandbox starts.
#   1. Run airlock network forward with no forward, a text, port 0, a port
#      out of range, an empty sandbox port and an unknown mode
#   2. Check that each run exits with code 2
#   3. Check that port 0 names the problem
@test "network forward with bad forward spec fails with usage error" {
    for args in "" "web" "0" "70000" "8080:" "--mode=sideways 8080"; do
        run_airlock network forward $args
        assert_failure 2
    done

    run_airlock network forward 0
    assert_output_contains "invalid value '0'"
}

# Test that two forwards on the same listening port fail before the
# sandbox lookup.
#   1. Run airlock network forward with the same host port two times
#   2. Check that it fails with the duplicate port error
@test "network forward with duplicate port fails" {
    run_airlock network forward 8080 8080:9000
    assert_failure 1
    assert_output_contains "host port 8080 is in more than one forward"
}
