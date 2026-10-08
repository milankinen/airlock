#!/usr/bin/env bats

load helpers

setup_file() {
    vm_setup_file
    start_host_http_server 18080

    cat > airlock.toml <<'EOF'
[vm]

[network]
policy = "deny-by-default"

[network.ports.dev-server]
host = [18080]

[network.rules.http]
allow = ["example.com:80"]

[network.rules.https]
allow = ["example.com:443"]
EOF
}

@test "host without rule is unreachable" {
    run_vm sh -c 'wget -q -O- --timeout=5 http://httpbin.org/get 2>&1'
    assert_failure
}

@test "forwarded host port is reachable and other localhost port is not" {
    run_vm sh -c 'sleep 2
                  wget -q -O- --timeout=5 http://localhost:18080/index.html 2>&1
                  wget -q -O- --timeout=5 http://localhost:19999/ >/dev/null 2>&1 || echo other-port-denied'
    assert_success
    assert_output_contains "hello-from-host"
    assert_output_contains "other-port-denied"
}

@test "allowed host is reachable over http and https" {
    run_vm sh -c 'wget -q -O- --timeout=10 http://example.com/ 2>&1 && echo http-ok
                  wget -q -O- --timeout=10 https://example.com/ 2>&1 && echo https-ok'
    assert_success
    assert_output_contains "Example Domain"
    assert_output_contains "http-ok"
    assert_output_contains "https-ok"
}
