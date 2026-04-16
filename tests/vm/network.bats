#!/usr/bin/env bats
# Tests for VM network rules: allow/deny, localhost ports, HTTP(S).
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

    # Start a simple HTTP server on the host for localhost port tests.
    mkdir -p http_root
    echo "hello-from-host" > http_root/index.html
    python3 -m http.server 18080 --directory http_root \
        >"$FILE_TEMP_DIR/http_server.log" 2>&1 &
    HTTP_PID=$!
    export HTTP_PID

    cat > airlock.toml <<'EOF'
[vm]

[network]
default_mode = "deny"

[network.ports.dev-server]
host = [18080]

[network.rules.http]
allow = ["example.com:80"]

[network.rules.https]
allow = ["example.com:443"]
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

# -- Default deny --

@test "denied host is unreachable" {
    run_vm sh -c 'wget -q -O- --timeout=5 http://httpbin.org/get 2>&1'
    assert_failure
}

# -- Localhost port forwarding --

@test "localhost port forward reaches host HTTP server" {
    # Port forwarding needs a moment after boot to stabilize
    run_vm sh -c 'sleep 2 && wget -q -O- --timeout=5 http://localhost:18080/index.html 2>&1'
    assert_success
    assert_output_contains "hello-from-host"
}

# -- HTTP (port 80) --

@test "allowed HTTP host is reachable" {
    run_vm sh -c 'wget -q -O- --timeout=10 http://example.com/ 2>&1'
    assert_success
    assert_output_contains "Example Domain"
}

# -- HTTPS (port 443) --

@test "allowed HTTPS host is reachable" {
    run_vm sh -c 'wget -q -O- --timeout=10 https://example.com/ 2>&1'
    assert_success
    assert_output_contains "Example Domain"
}
