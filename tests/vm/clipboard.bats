#!/usr/bin/env bats

load helpers

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

@test "ungranted paste has no fifo and its shims fail fast" {
    run_vm sh -c '[ -p /run/airlock/clipboard.paste ] && echo paste-fifo
                  timeout 5 wl-paste; echo "wl-paste rc=$?"
                  timeout 5 xclip -o; echo "xclip rc=$?"'
    assert_success
    assert_output_not_contains "paste-fifo"
    assert_output_contains "wl-paste rc=1"
    assert_output_contains "xclip rc=1"
}
