-- OpenAI codex base pack
--
-- Codex keeps its settings and sessions in the pack directory on the
-- host, shared by the sandboxes that use this pack.

config.mounts = {
    ["codex-dir"] = {
        source = pack.directory .. "/codex",
        target = "~/.codex",
        missing = "create-dir",
    },
}

config.network = {
    -- The openai service owns the ChatGPT sign-in and backend hosts
    -- (auth.openai.com, chatgpt.com). airlock allows them, keeps the real
    -- tokens on the host and gives Codex surrogates. `codex login` opens
    -- the sign-in page in the host browser.
    services = {
        openai = true,
    },
    rules = {
        -- The API, for API-key users. The real key goes into the
        -- requests.
        codex = {
            allow = {
                "api.openai.com:443",
            },
            inject = { "OPENAI_API_KEY" },
        },
    },
}

-- The API key from the host environment or the vault, if it exists. The
-- sandbox sees only a masked surrogate.
config.env = {
    OPENAI_API_KEY = { value = "${OPENAI_API_KEY}", mask = true, optional = true },
}

if pack.args["acp"] then
    -- The ACP adapter (codex-acp) runs this Codex binary. The adapter
    -- that the setup script compiles has no Codex of its own.
    config.env = config.env or {}
    config.env.CODEX_PATH = "/usr/local/bin/codex"
end
