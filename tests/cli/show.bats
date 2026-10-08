#!/usr/bin/env bats

load helpers

@test "show without sandbox fails with or without config" {
    run_airlock show
    assert_failure 1
    assert_output_contains "No sandbox for $PWD"

    write_config '[vm]'
    run_airlock show
    assert_failure 1
    assert_output_contains "No sandbox for $PWD"
}

@test "show lists enabled project packs" {
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

@test "show lists list-form presets as network rules not packs" {
    mkdir -p .airlock/sandbox
    write_config 'presets = ["python"]'
    run_airlock show
    assert_success
    assert_output_not_contains "Packs:"
    assert_output_contains "python-packages: allow 4 deny 0"
}

@test "show prints install status of each pack on current disk" {
    make_sandbox_disk
    write_installs python=installed rust=failed codex=unconfirmed docker=kept
    write_config '[packs]
python = { version = 1 }
rust = { version = 1 }
codex = { version = 1 }
nodejs = { version = 1 }'
    run_airlock show
    assert_success
    assert_output_contains "python 1 — installed"
    assert_output_contains "rust 1 — install failed"
    assert_output_contains "codex 1 — install not confirmed"
    assert_output_contains "nodejs 1 — pending"
    assert_output_contains "docker — kept, removed from config"
}

@test "show ignores install records of another disk" {
    write_installs python=installed
    write_config '[packs]
python = { version = 1 }'
    run_airlock show
    assert_success
    assert_output_contains "python 1 — pending"
}

@test "show lists install record of list-form preset as removed" {
    make_sandbox_disk
    write_installs python=installed
    write_config 'presets = ["python"]'
    run_airlock show
    assert_success
    assert_output_contains "python — installed, removed from config"
    assert_output_not_contains "pending"
}

@test "show lists enabled network services without printing host secrets" {
    mkdir -p .airlock/sandbox
    echo 'not json' >.airlock/sandbox/auth.json
    write_config '[packs]
claude = { version = 1 }

[network.services]
openai = true'
    CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-show-test-secret run_airlock show
    assert_success
    assert_output_contains "Services:"
    assert_output_contains "anthropic: not signed in"
    assert_output_contains "openai: not signed in"
    assert_output_not_contains "show-test-secret"
}
