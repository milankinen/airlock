#!/usr/bin/env bats
# Tests for "airlock show" without project data.

load helpers

@test "show without project data reports error" {
    write_config '[vm]'
    run_airlock show
    assert_failure 1
    assert_output_contains "No project data"
}

@test "show without any config reports no project data" {
    run_airlock show
    assert_failure 1
    assert_output_contains "No project data"
}

@test "show lists project packs" {
    mkdir -p .airlock/sandbox
    write_config '[packs]
python = { version = 1 }
codex = { version = 1 }
docker = { enabled = false }
alpine = { version = 1 }'
    run_airlock show
    assert_success
    assert_output_contains "Packs:"
    assert_output_contains "codex 1 — pending"
    assert_output_contains "python 1 — pending"
    assert_output_contains "alpine 1 — config only"
    assert_output_not_contains "docker"
}

@test "show does not list presets of the list form" {
    mkdir -p .airlock/sandbox
    write_config 'presets = ["python"]'
    run_airlock show
    assert_success
    assert_output_not_contains "Packs:"
    assert_output_contains "python-packages: allow 4 deny 0"
}

@test "show prints the install status of each pack" {
    mkdir -p .airlock/sandbox
    touch .airlock/sandbox/disk.img
    echo 00000000000000010000000000000002 > .airlock/sandbox/disk.id
    fp=$(printf 'a%.0s' {1..64})
    cat > .airlock/sandbox/installs.json <<JSON
{
  "version": 1,
  "disk": [1, 2],
  "image_id": "sha256:1",
  "packs": {
    "python": { "status": "installed", "fingerprint": "$fp", "at": 1 },
    "rust": { "status": "failed", "fingerprint": "$fp", "at": 1 },
    "docker": { "status": "kept", "confirmed": true, "fingerprint": "$fp", "at": 1 }
  }
}
JSON
    write_config '[packs]
python = { version = 1 }
rust = { version = 1 }
nodejs = { version = 1 }'
    run_airlock show
    assert_success
    assert_output_contains "python 1 — installed"
    assert_output_contains "rust 1 — install failed"
    assert_output_contains "nodejs 1 — pending"
    assert_output_contains "docker — kept, removed from config"
}

@test "show ignores install records of another disk" {
    mkdir -p .airlock/sandbox
    fp=$(printf 'a%.0s' {1..64})
    cat > .airlock/sandbox/installs.json <<JSON
{ "version": 1, "disk": [1, 2], "packs": {
    "python": { "status": "installed", "fingerprint": "$fp", "at": 1 } } }
JSON
    write_config '[packs]
python = { version = 1 }'
    run_airlock show
    assert_success
    assert_output_contains "python 1 — pending"
}

@test "show has no agent auth section" {
    mkdir -p .airlock/sandbox
    write_config '[packs]
claude = { version = 1 }
codex = { version = 1 }'
    CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-show-test-secret run_airlock show
    assert_success
    assert_output_not_contains "Agent auth"
    assert_output_not_contains "show-test-secret"
}

@test "show lists the enabled network services" {
    mkdir -p .airlock/sandbox
    write_config '[packs]
claude = { version = 1 }

[network.services]
openai = true'
    run_airlock show
    assert_success
    assert_output_contains "Services:"
    assert_output_contains "anthropic: not signed in"
    assert_output_contains "openai: not signed in"
}

@test "show ignores a leftover sign-in consent file" {
    mkdir -p .airlock/sandbox
    echo 'not json' >.airlock/sandbox/auth.json
    write_config '[packs]
claude = { version = 1 }'
    run_airlock show
    assert_success
    assert_output_not_contains "consent"
}

@test "show lists the install record of a list-form preset as removed" {
    mkdir -p .airlock/sandbox
    touch .airlock/sandbox/disk.img
    echo 00000000000000010000000000000002 > .airlock/sandbox/disk.id
    fp=$(printf 'a%.0s' {1..64})
    cat > .airlock/sandbox/installs.json <<JSON
{ "version": 1, "disk": [1, 2], "image_id": "sha256:1", "packs": {
    "python": { "status": "installed", "fingerprint": "$fp", "at": 1 } } }
JSON
    write_config 'presets = ["python"]'
    run_airlock show
    assert_success
    assert_output_contains "python — installed, removed from config"
    assert_output_not_contains "pending"
}

@test "show prints an unconfirmed install as install not confirmed" {
    mkdir -p .airlock/sandbox
    touch .airlock/sandbox/disk.img
    echo 00000000000000010000000000000002 > .airlock/sandbox/disk.id
    fp=$(printf 'a%.0s' {1..64})
    cat > .airlock/sandbox/installs.json <<JSON
{ "version": 1, "disk": [1, 2], "image_id": "sha256:1", "packs": {
    "rust": { "status": "unconfirmed", "fingerprint": "$fp", "at": 1 } } }
JSON
    write_config '[packs]
rust = { version = 1 }'
    run_airlock show
    assert_success
    assert_output_contains "rust 1 — install not confirmed"
}
