#!/usr/bin/env bats
# Tests for VM environment variable handling.
# Verifies config env vars (including airlock.local.toml) are injected.
# Requires KVM (Linux) or Apple Virtualization (macOS) + docker.

load helpers

setup_file() {
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
    require_vm_support
    vm_setup_file

    export HOST_TEST_VALUE="substituted-from-host"

    cat > airlock.toml <<'EOF'
[vm]

[env]
BASE_VAR = "from-base-config"
OVERRIDE_VAR = "from-base"
SUBST_VAR = "${HOST_TEST_VALUE}"
EOF

    cat > airlock.local.toml <<'EOF'
[env]
LOCAL_VAR = "from-local-config"
OVERRIDE_VAR = "from-local"
EOF
}

teardown_file() {
    vm_teardown_file
}

setup() {
    cd "$FILE_TEMP_DIR" || return 1
}

@test "base config env var is injected" {
    run_vm sh -c 'echo $BASE_VAR'
    assert_success
    assert_output_contains "from-base-config"
}

@test "local config env var is injected" {
    run_vm sh -c 'echo $LOCAL_VAR'
    assert_success
    assert_output_contains "from-local-config"
}

@test "local config overrides base config env var" {
    run_vm sh -c 'echo $OVERRIDE_VAR'
    assert_success
    assert_output_contains "from-local"
    assert_output_not_contains "from-base"
}

@test "host env var substitution works" {
    run_vm sh -c 'echo $SUBST_VAR'
    assert_success
    assert_output_contains "substituted-from-host"
}
