#!/usr/bin/env bats
# Tests for network middleware (Lua scripting).
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

    # Start a simple HTTP server on the host.
    mkdir -p http_root
    echo "hello-from-host" > http_root/index.html
    mkdir -p http_root/forbidden
    echo "secret-content" > http_root/forbidden/index.html
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

[network.rules.localhost]
allow = ["localhost:18081"]

[[network.rules.localhost.middleware]]
script = '''
if req:path():find("^/forbidden") then
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

@test "middleware allows non-forbidden path" {
    run_vm sh -c 'sleep 2 && wget -q -O- --timeout=5 http://localhost:18081/index.html 2>&1'
    assert_success
    assert_output_contains "hello-from-host"
}

@test "middleware denies forbidden path" {
    run_vm sh -c 'sleep 2 && wget -q -O- --timeout=5 http://localhost:18081/forbidden/ 2>&1'
    assert_failure
}
