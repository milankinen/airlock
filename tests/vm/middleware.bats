#!/usr/bin/env bats

load helpers

setup_file() {
    vm_setup_file

    cat > airlock.toml <<'EOF'
[vm]

[network]
policy = "deny-by-default"

[network.rules.example]
allow = ["example.org:443"]

[network.middleware.deny-forbidden]
target = ["example.org:443"]
script = '''
if req.path:find("^/forbidden") then
    req:deny()
end
'''
EOF
}

@test "middleware denies forbidden https path and allows others" {
    run_vm sh -c 'wget -q -O- --timeout=10 https://example.org/ >/dev/null 2>&1 && echo root-allowed
                  wget -q -O- --timeout=10 https://example.org/forbidden >/dev/null 2>&1 || echo forbidden-denied'
    assert_success
    assert_output_contains "root-allowed"
    assert_output_contains "forbidden-denied"
}
