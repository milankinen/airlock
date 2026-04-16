#!/usr/bin/env bats
# Tests for "airlock show" without project data.

load helpers

@test "show without project data reports error" {
    write_config '[vm]'
    run_airlock show
    assert_failure 1
    assert_output_contains "No project data"
}

@test "show without any config reports no project data" {
    run_airlock show
    assert_failure 1
    assert_output_contains "No project data"
}
