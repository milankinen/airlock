-- Claude code development pack

config.env = {
    -- Claude Code has an "essential traffic" mode
    -- (CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC) and a "no telemetry" mode
    -- (DISABLE_TELEMETRY). The essential traffic mode turns off Remote
    -- Control (/remote-control). The no telemetry mode turns off telemetry
    -- and keeps Remote Control. DISABLE_ERROR_REPORTING turns off the
    -- error reports to Sentry. The network rules do not allow these hosts.
    DISABLE_TELEMETRY = "1",
    DISABLE_ERROR_REPORTING = "1",

    -- Tell Claude Code that it runs in a sandbox.
    IS_SANDBOX = "1",
    -- Claude Code uses the Node.js CA list. Add the airlock CA to it.
    NODE_EXTRA_CA_CERTS = "/etc/ssl/certs/ca-certificates.crt",

    -- No background updates. A newer Claude Code comes with a new
    -- sandbox, or with `claude update`. The launcher at ~/.local/bin/claude
    -- is a wrapper from the setup script. Thus no install checks.
    DISABLE_AUTOUPDATER = "1",
    DISABLE_INSTALLATION_CHECKS = "1",
    -- Use the system ripgrep from the setup script. The bundled ripgrep
    -- does not run on musl (Alpine).
    USE_BUILTIN_RIPGREP = "0",

    -- The MCP tool execute waits for the user to approve, then
    -- runs the command. The default overall limit of Claude Code is long
    -- enough. The idle limit for HTTP servers is 5 minutes. It ends a call
    -- without a response or progress message. Turn it off. The server
    -- ends a call with its own limits.
    CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT = "0",
}

if pack.args["acp"] then
    -- The ACP adapter (claude-agent-acp) runs this Claude Code wrapper.
    -- The adapter that the setup script compiles has no Claude Code of
    -- its own.
    config.env.CLAUDE_CODE_EXECUTABLE = "/usr/local/bin/claude"
end

-- Tokens from the host environment or the vault, if they exist. The
-- sandbox sees only masked surrogates. The `claude-tokens` rule below puts
-- the real values into the API requests.
local tokens = { "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY" }
for _, name in ipairs(tokens) do
    config.env[name] = { value = "${" .. name .. "}", mask = true, optional = true }
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
config.network.rules["claude-tokens"] = {
    allow = { "api.anthropic.com:443" },
    inject = tokens,
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
