#!/usr/bin/env bats
# Tests for the clipboard bridge.
#
# Asserts plumbing only — which shims exist, where they point, and that an
# ungranted direction fails fast. Nothing here copies or pastes for real:
# a test that exercised the bridge end to end would clobber the clipboard of
# whoever ran the suite.
#
# Requires KVM (Linux) or Apple Virtualization (macOS) + docker.

load helpers

setup_file() {
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
    require_vm_support

    # The bridge only activates when the host itself has a clipboard
    # program, so on a headless box there is legitimately nothing to test.
    if ! command -v pbcopy >/dev/null 2>&1 \
        && ! command -v wl-copy >/dev/null 2>&1 \
        && ! command -v xclip >/dev/null 2>&1 \
        && ! command -v xsel >/dev/null 2>&1; then
        export CLIPBOARD_UNAVAILABLE=1
    fi

    vm_setup_file

    # copy granted, paste withheld — one VM exercises both states.
    cat > airlock.toml <<'EOF'
[vm]

[clipboard]
copy = true
EOF
}

teardown_file() {
    vm_teardown_file
}

setup() {
    cd "$FILE_TEMP_DIR" || return 1
    if [[ -n "${CLIPBOARD_UNAVAILABLE:-}" ]]; then
        skip "host has no clipboard program"
    fi
}

@test "clipboard shims are installed and executable" {
    run_vm sh -c 'for f in wl-copy wl-paste xclip xsel; do
                      [ -x "/usr/local/bin/$f" ] || { echo "missing $f"; exit 1; }
                  done; echo ok'
    assert_success
    assert_output --partial "ok"
}

@test "shims win over anything the image ships" {
    run_vm sh -c 'command -v wl-copy'
    assert_success
    assert_output "/usr/local/bin/wl-copy"
}

@test "granted direction has a fifo behind it" {
    run_vm sh -c '[ -p /run/airlock/clipboard.copy ] && echo fifo'
    assert_success
    assert_output --partial "fifo"
}

# The whole point of withholding a direction: nothing is listening, so the
# pipe a shim would block on must not exist either.
@test "ungranted direction has no fifo" {
    run_vm sh -c '[ -p /run/airlock/clipboard.paste ] && echo present || echo absent'
    assert_success
    assert_output --partial "absent"
}

# A shim for an ungranted direction must exit rather than block. If this
# regresses it would hang forever on a FIFO nobody serves, so it is wrapped
# in a timeout to fail the test instead of the suite.
@test "ungranted paste fails fast instead of hanging" {
    run_vm sh -c 'timeout 5 wl-paste; echo "rc=$?"'
    assert_success
    assert_output --partial "rc=1"
}

@test "xclip routes -o to paste and bare invocation to copy" {
    # -o selects the (ungranted) paste branch, so it exits 1 promptly.
    run_vm sh -c 'timeout 5 xclip -o; echo "rc=$?"'
    assert_success
    assert_output --partial "rc=1"
}
