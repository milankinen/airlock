-- Claude code development pack

config.env = {
    -- The network rules do not allow the telemetry endpoints. Thus turn
    -- off all telemetry.
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1",

    -- Tell Claude Code that it runs in a sandbox.
    IS_SANDBOX = "1",
    -- Claude Code uses the Node.js CA list. Add the airlock CA to it.
    NODE_EXTRA_CA_CERTS = "/etc/ssl/certs/ca-certificates.crt",

    -- The setup script installs Claude Code in /usr/local/bin, which the
    -- sandbox user cannot change. Thus no self-updates, and no warnings
    -- about the install location. A newer Claude Code comes with a new
    -- sandbox.
    DISABLE_AUTOUPDATER = "1",
    DISABLE_INSTALLATION_CHECKS = "1",
    -- Use the system ripgrep from the setup script. The bundled ripgrep
    -- does not run on musl (Alpine).
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
    -- (platform.claude.com, api.anthropic.com). airlock allows them, keeps
    -- the real tokens on the host and gives Claude Code surrogates.
    -- `claude /login` opens the sign-in page in the host browser.
    services = {
        anthropic = true,
    },
    rules = {
        -- Hosts that the service does not own: the Claude.ai origin and the
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
-- that use this pack. ~/.claude.json is not mounted. Each sandbox has its
-- own, which the setup script creates.
config.mounts = {
    ["claude-dir"] = {
        source = pack.directory .. "/claude",
        target = "~/.claude",
        missing = "create-dir",
    },
}
