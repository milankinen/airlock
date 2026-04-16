#!/usr/bin/env bats
# Tests for network middleware (Lua scripting) and port forwarding.
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

    # Start a simple HTTP server on the host for port-forward tests.
    mkdir -p http_root
    echo "hello-from-host" > http_root/index.html
    python3 -m http.server 18081 --directory http_root \
        >"$FILE_TEMP_DIR/http_server.log" 2>&1 &
    HTTP_PID=$!
    export HTTP_PID

    cat > airlock.toml <<'EOF'
[vm]

[network]
default_mode = "deny"

[network.ports.test-server]
host = [18081]

[network.rules.example]
allow = ["example.org:443"]

[[network.rules.example.middleware]]
script = '''
if req.path:find("^/forbidden") then
    req:deny()
end
'''
EOF
}

teardown_file() {
    vm_teardown_file
    if [[ -n "${HTTP_PID:-}" ]]; then
        kill "$HTTP_PID" 2>/dev/null || true
        wait "$HTTP_PID" 2>/dev/null || true
    fi
}

setup() {
    cd "$FILE_TEMP_DIR" || return 1
}

@test "port-forwarded localhost reaches host HTTP server" {
    run_vm sh -c 'sleep 2 && wget -q -O- --timeout=5 http://localhost:18081/index.html 2>&1'
    assert_success
    assert_output_contains "hello-from-host"
}

@test "non-forwarded port is denied" {
    run_vm sh -c 'sleep 2 && wget -q -O- --timeout=5 http://localhost:19999/ 2>&1'
    assert_failure
}

@test "middleware allows non-forbidden HTTPS path" {
    run_vm sh -c 'wget -q -O- --timeout=10 https://example.org/ 2>&1'
    assert_success
}

@test "middleware denies forbidden HTTPS path" {
    run_vm sh -c 'wget -q -O- --timeout=10 https://example.org/forbidden 2>&1'
    assert_failure
}
