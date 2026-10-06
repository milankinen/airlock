-- Claude code development pack

config.env = {
    -- Telemetry endpoints are not allowed by network rules; disable
    -- telemetry entirely
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1",

    -- Tell that we're inside sandbox
    IS_SANDBOX = "1",
    -- Claude uses Node certs - must add airlock CA to the trusted certs
    NODE_EXTRA_CA_CERTS = "/etc/ssl/certs/ca-certificates.crt",

    -- The setup script installs Claude Code in /usr/local/bin, which the
    -- sandbox user cannot change: no self-updates, and no warnings about
    -- the install location. A newer Claude Code comes with a new sandbox.
    DISABLE_AUTOUPDATER = "1",
    DISABLE_INSTALLATION_CHECKS = "1",
    -- The system ripgrep from the setup script: the bundled one does not
    -- run on musl (Alpine).
    USE_BUILTIN_RIPGREP = "0",
}

if pack.args["acp"] then
    -- The ACP adapter (claude-agent-acp) runs this Claude Code binary.
    -- The adapter that the setup script compiles has no Claude Code of
    -- its own.
    config.env.CLAUDE_CODE_EXECUTABLE = "/usr/local/bin/claude"
end

config.network = {
    -- The anthropic service owns the sign-in and API hosts
    -- (platform.claude.com, api.anthropic.com): airlock allows them, keeps
    -- the real tokens on the host and gives Claude Code surrogates.
    -- `claude /login` opens the sign-in page in the host browser.
    services = {
        anthropic = true,
    },
    rules = {
        -- Hosts the service does not own: the Claude.ai origin and the
        -- downloads.
        ["claude-code"] = {
            allow = {
                "claude.ai:443",
                "downloads.claude.ai:443",
            },
        },
    },
}

-- Claude Code keeps its settings, sessions and credential file (with the
-- surrogates) in the pack directory on the host, shared by the sandboxes
-- that use this pack. ~/.claude.json is not mounted: each sandbox has its
-- own, which the setup script creates.
config.mounts = {
    ["claude-dir"] = {
        source = pack.directory .. "/claude",
        target = "~/.claude",
        missing = "create-dir",
    },
}
