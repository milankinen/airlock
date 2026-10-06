-- GitHub Copilot CLI pack
--
-- API hosts from:
-- https://docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/customize-cloud-agent/customize-the-agent-environment

config.env = {
    -- The real token stays on the host: the sandbox sees a same-length
    -- random surrogate, and the `inject` rule below swaps the real value
    -- into request headers at the host boundary.
    COPILOT_GITHUB_TOKEN = { value = "${COPILOT_GITHUB_TOKEN}", mask = true },

    -- The setup script installs the Copilot CLI in /usr/local/bin, which
    -- the sandbox user cannot change, and the release downloads are not
    -- allowed: no self-updates. A newer Copilot CLI comes with a new
    -- sandbox.
    COPILOT_AUTO_UPDATE = "false",
    -- Copilot loads the system CA bundle (with the airlock CA); this also
    -- covers the Node.js parts of it.
    NODE_EXTRA_CA_CERTS = "/etc/ssl/certs/ca-certificates.crt",
}

config.network = {
    rules = {
        -- Injection covers every host this rule allows (including the
        -- telemetry hosts under *.githubcopilot.com) — the token only ever
        -- leaves the host in requests where Copilot itself put the
        -- surrogate.
        ["copilot-cli"] = {
            inject = { "COPILOT_GITHUB_TOKEN" },
            allow = {
                "github.com:443",
                "api.github.com:443",
                "*.githubcopilot.com:443",
            },
        },
    },
}

-- Copilot keeps its session and settings in the pack directory on the
-- host, shared by the sandboxes that use this pack.
config.mounts = {
    ["copilot-session"] = {
        source = pack.directory .. "/copilot",
        target = "~/.copilot",
        missing = "create-dir",
    },
}
