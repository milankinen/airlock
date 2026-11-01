-- GitHub Copilot CLI pack
-- Hosts: https://docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/customize-cloud-agent/customize-the-agent-environment

config.env = {
    -- The sandbox sees a surrogate. The surrogate keeps the prefix,
    -- because Copilot finds the token type from it.
    COPILOT_GITHUB_TOKEN = {
        value = "${COPILOT_GITHUB_TOKEN}",
        mask = true,
        optional = true,
        keep_prefixes = { "github_pat_", "gho_", "ghu_", "ghp_" },
    },
    -- The install is root-owned. A new sandbox gets a newer version.
    COPILOT_AUTO_UPDATE = "false",
    -- For the Node.js parts of Copilot.
    NODE_EXTRA_CA_CERTS = "/etc/ssl/certs/ca-certificates.crt",
}

config.network = {
    rules = {
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

-- Shared by all sandboxes that use this pack.
config.mounts = {
    ["copilot-session"] = {
        source = pack.directory .. "/copilot",
        target = "~/.copilot",
        missing = "create-dir",
    },
}
