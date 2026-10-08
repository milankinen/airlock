#!/usr/bin/env bats

load helpers

setup_file() {
    vm_setup_file

    export HOST_TEST_VALUE="substituted-from-host"

    cat > airlock.toml <<'EOF'
[vm]

[env]
BASE_VAR = "from-base-config"
OVERRIDE_VAR = "from-base"
SUBST_VAR = "${HOST_TEST_VALUE}"
MASKED_VAR = { value = "${HOST_TEST_VALUE}", mask = true }
EOF

    cat > airlock.local.toml <<'EOF'
[env]
LOCAL_VAR = "from-local-config"
OVERRIDE_VAR = "from-local"
EOF
}

@test "config env reaches sandbox with local overrides and host substitution" {
    run_vm sh -c 'echo "base=$BASE_VAR local=$LOCAL_VAR override=$OVERRIDE_VAR subst=$SUBST_VAR"'
    assert_success
    assert_output_contains "base=from-base-config local=from-local-config override=from-local subst=substituted-from-host"
}

@test "masked env var has real value length but not its content" {
    run_vm sh -c 'printf "len=%s value=%s\n" "${#MASKED_VAR}" "$MASKED_VAR"'
    assert_success
    assert_output_contains "len=21"
    assert_output_not_contains "substituted-from-host"
}
