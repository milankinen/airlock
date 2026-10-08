#!/usr/bin/env bats

load helpers

# Env vars of the config in the sandbox: layers, host values and masks.

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

# Test that the config env vars reach the sandbox, airlock.local.toml
# overrides airlock.toml, and ${VAR} gets the host value.
#   1. Print the env vars in the sandbox
#   2. Check each value
@test "config env reaches sandbox with local overrides and host substitution" {
    run_vm sh -c 'echo "base=$BASE_VAR local=$LOCAL_VAR override=$OVERRIDE_VAR subst=$SUBST_VAR"'
    assert_success
    assert_output_contains "base=from-base-config local=from-local-config override=from-local subst=substituted-from-host"
}

# Test that a masked env var has a placeholder of the same length as the
# real value, and not the real value.
#   1. Print the length and the value of the masked var in the sandbox
#   2. Check that the length is 21 (the host value) and the value is not
#      in the output
@test "masked env var has real value length but not its content" {
    run_vm sh -c 'printf "len=%s value=%s\n" "${#MASKED_VAR}" "$MASKED_VAR"'
    assert_success
    assert_output_contains "len=21"
    assert_output_not_contains "substituted-from-host"
}
