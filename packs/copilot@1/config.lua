-- GitHub Copilot CLI pack
--
-- API hosts from:
-- https://docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/customize-cloud-agent/customize-the-agent-environment

config.env = {
    -- The token from the host environment or the vault, if it exists.
    -- The real token stays on the host. The sandbox sees a random
    -- surrogate of the same length, and the `inject` rule below puts the
    -- real value into request headers at the host boundary.
    COPILOT_GITHUB_TOKEN = { value = "${COPILOT_GITHUB_TOKEN}", mask = true, optional = true },

    -- The setup script installs the Copilot CLI in /usr/local/bin, which
    -- the sandbox user cannot change, and the network rules do not allow
    -- the release downloads. Thus no self-updates. A newer Copilot CLI
    -- comes with a new sandbox.
    COPILOT_AUTO_UPDATE = "false",
    -- Copilot loads the system CA bundle (with the airlock CA). This
    -- variable also covers the Node.js parts of Copilot.
    NODE_EXTRA_CA_CERTS = "/etc/ssl/certs/ca-certificates.crt",
}

config.network = {
    rules = {
        -- Injection covers all hosts that this rule allows (also the
        -- telemetry hosts under *.githubcopilot.com). The token leaves the
        -- host only in requests where Copilot itself put the surrogate.
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
