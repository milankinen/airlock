#!/usr/bin/env bats

load helpers

# The info command: packs, install status, network rules and services.

# Test that info fails when the project has no sandbox, also when a config
# exists.
#   1. Run info without a config and check the error
#   2. Write a config, run info again and check the same error
@test "info without sandbox fails with or without config" {
    run_airlock info
    assert_failure 1
    assert_output_contains "No sandbox for $PWD"

    write_config '[vm]'
    run_airlock info
    assert_failure 1
    assert_output_contains "No sandbox for $PWD"
}

# Test that info lists only the enabled packs of the config. A pack that
# is not installed yet is pending. A base image pack is config only.
#   1. Write a config with three packs and one disabled pack
#   2. Run info
#   3. Check the status of each pack and that the disabled pack is
#      not shown
@test "info lists enabled project packs" {
    mkdir -p .airlock/sandbox
    write_config '[packs]
python = { version = 1 }
codex = { version = 1 }
docker = { enabled = false }
alpine = { version = 1 }'
    run_airlock info
    assert_success
    assert_output_contains "Packs:"
    assert_output_contains "codex 1 — pending"
    assert_output_contains "python 1 — pending"
    assert_output_contains "alpine 1 — config only"
    assert_output_not_contains "docker"
}

# Test that a list-form preset gives network rules and no packs.
#   1. Write a config with presets = ["python"]
#   2. Run info
#   3. Check that there is no pack list and that the preset rule shows
@test "info lists list-form presets as network rules not packs" {
    mkdir -p .airlock/sandbox
    write_config 'presets = ["python"]'
    run_airlock info
    assert_success
    assert_output_not_contains "Packs:"
    assert_output_contains "python-packages: allow 4 deny 0"
}

# Test that info prints the install status of each pack from the install
# records of the current disk.
#   1. Make a sandbox disk with installed, failed, unconfirmed and kept
#      records
#   2. Write a config with these packs (but not the kept one) and one
#      new pack
#   3. Run info and check the status text of each pack
@test "info prints install status of each pack on current disk" {
    make_sandbox_disk
    write_installs python=installed rust=failed codex=unconfirmed docker=kept
    write_config '[packs]
python = { version = 1 }
rust = { version = 1 }
codex = { version = 1 }
nodejs = { version = 1 }'
    run_airlock info
    assert_success
    assert_output_contains "python 1 — installed"
    assert_output_contains "rust 1 — install failed"
    assert_output_contains "codex 1 — install not confirmed"
    assert_output_contains "nodejs 1 — pending"
    assert_output_contains "docker — kept, removed from config"
}

# Test that info does not use install records that belong to a different
# disk.
#   1. Write install records for disk [1, 2] but make no disk
#   2. Write a config with the same pack
#   3. Run info and check that the pack is pending
@test "info ignores install records of another disk" {
    write_installs python=installed
    write_config '[packs]
python = { version = 1 }'
    run_airlock info
    assert_success
    assert_output_contains "python 1 — pending"
}

# Test that an installed pack is shown as removed from config when the
# config has only the list-form preset of the same name.
#   1. Make a sandbox disk with an installed python record
#   2. Write a config with presets = ["python"]
#   3. Run info and check that python is removed from config, not pending
@test "info lists install record of list-form preset as removed" {
    make_sandbox_disk
    write_installs python=installed
    write_config 'presets = ["python"]'
    run_airlock info
    assert_success
    assert_output_contains "python — installed, removed from config"
    assert_output_not_contains "pending"
}

# Test that info lists the network services and does not print a host
# token.
#   1. Write a corrupt auth file and a config that enables two services
#   2. Run info with a Claude token in the host environment
#   3. Check that both services are not signed in and that the token is
#      not in the output
@test "info lists enabled network services without printing host secrets" {
    mkdir -p .airlock/sandbox
    echo 'not json' >.airlock/sandbox/auth.json
    write_config '[packs]
claude = { version = 1 }

[network.services]
openai = true'
    CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-show-test-secret run_airlock info
    assert_success
    assert_output_contains "Services:"
    assert_output_contains "anthropic: not signed in"
    assert_output_contains "openai: not signed in"
    assert_output_not_contains "show-test-secret"
}
