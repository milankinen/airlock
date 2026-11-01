-- Claude Code development pack

config.env = {
    -- Do not use CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC. It turns off
    -- Remote Control. The network rules do not allow the telemetry and
    -- error report hosts.
    DISABLE_TELEMETRY = "1",
    DISABLE_ERROR_REPORTING = "1",

    IS_SANDBOX = "1",
    NODE_EXTRA_CA_CERTS = "/etc/ssl/certs/ca-certificates.crt",

    -- Updates come with a new sandbox or `claude update`. The launcher is
    -- the wrapper of the setup script, thus no install checks.
    DISABLE_AUTOUPDATER = "1",
    DISABLE_INSTALLATION_CHECKS = "1",
    -- The bundled ripgrep does not run on musl.
    USE_BUILTIN_RIPGREP = "0",

    -- The 5 minute idle limit would end the MCP tool execute while it
    -- waits for user approval. The server has its own limits.
    CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT = "0",
}

if pack.args["acp"] then
    -- The adapter has no Claude Code of its own. It runs the wrapper.
    config.env.CLAUDE_CODE_EXECUTABLE = "/usr/local/bin/claude"
end

-- The sandbox sees only surrogates. The `claude-tokens` rule injects the
-- real values.
local tokens = { "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY" }
for _, name in ipairs(tokens) do
    config.env[name] = { value = "${" .. name .. "}", mask = true, optional = true }
end

config.network = {
    -- The service owns the sign-in and API hosts and keeps the real tokens
    -- on the host.
    services = {
        anthropic = true,
    },
    rules = {
        ["claude-code"] = {
            allow = {
                "claude.ai:443",
                "downloads.claude.ai:443",
            },
        },
    },
}
config.network.rules["claude-tokens"] = {
    allow = { "api.anthropic.com:443" },
    inject = tokens,
}

-- All sandboxes share ~/.claude. Each sandbox has its own ~/.claude.json,
-- which the setup script creates.
config.mounts = {
    ["claude-dir"] = {
        source = pack.directory .. "/claude",
        target = "~/.claude",
        missing = "create-dir",
    },
}
