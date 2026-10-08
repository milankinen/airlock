#!/usr/bin/env bats

load helpers

# Clipboard shims in the sandbox, with copy granted and paste not granted.
# The host must have a clipboard program.

setup_file() {
    vm_setup_file
    if ! command -v pbcopy >/dev/null 2>&1 \
        && ! command -v wl-copy >/dev/null 2>&1 \
        && ! command -v xclip >/dev/null 2>&1 \
        && ! command -v xsel >/dev/null 2>&1; then
        skip "host has no clipboard program"
    fi
    cat > airlock.toml <<'EOF'
[vm]

[clipboard]
copy = true
EOF
}

# Test that the clipboard shims are installed first on PATH and that the
# granted copy has its fifo.
#   1. Check that each shim exists and that wl-copy is the shim
#   2. Check that the copy fifo exists
@test "granted copy installs shims first on path with fifo behind them" {
    run_vm sh -c 'for f in wl-copy wl-paste xclip xsel; do
                      [ -x "/usr/local/bin/$f" ] || echo "missing $f"
                  done
                  command -v wl-copy
                  [ -p /run/airlock/clipboard.copy ] && echo copy-fifo'
    assert_success
    assert_output_not_contains "missing"
    assert_output_contains "/usr/local/bin/wl-copy"
    assert_output_contains "copy-fifo"
}

# Test that paste without a grant has no fifo and its shims fail at once,
# so that tools in the sandbox do not hang.
#   1. Check that the paste fifo does not exist
#   2. Run wl-paste and xclip -o with a time limit
#   3. Check that both exit with code 1
@test "ungranted paste has no fifo and its shims fail fast" {
    # The time limit makes a hang fail the test, so the run does not block.
    run_vm sh -c '[ -p /run/airlock/clipboard.paste ] && echo paste-fifo
                  timeout 5 wl-paste; echo "wl-paste rc=$?"
                  timeout 5 xclip -o; echo "xclip rc=$?"'
    assert_success
    assert_output_not_contains "paste-fifo"
    assert_output_contains "wl-paste rc=1"
    assert_output_contains "xclip rc=1"
}
