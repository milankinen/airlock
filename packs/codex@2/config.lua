-- OpenAI codex base pack. Settings and sessions stay in the pack
-- directory on the host, shared by the sandboxes that use this pack.

config.mounts = {
    ["codex-dir"] = {
        source = pack.directory .. "/codex",
        target = "~/.codex",
        missing = "create-dir",
    },
}

config.network = {
    -- ChatGPT sign-in. airlock keeps the real tokens on the host.
    services = { openai = true },
    rules = {
        codex = {
            allow = { "api.openai.com:443" },
            inject = { "OPENAI_API_KEY" },
        },
    },
}

config.env = {
    OPENAI_API_KEY = { value = "${OPENAI_API_KEY}", mask = true, optional = true },
}

if pack.args["acp"] then
    -- The compiled adapter has no Codex of its own.
    config.env.CODEX_PATH = "/usr/local/bin/codex"
end
