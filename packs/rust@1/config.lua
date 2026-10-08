-- Rust: the shared rustup homes of the setup script, the downloads of the
-- Rust project (rustup installs toolchains and components from
-- static.rust-lang.org, also for a rust-toolchain.toml), and with
-- `allow-cargo` crates.io.

-- The setup script gives the toolchain to rustup. An option there (a
-- leading `-`) or a character outside a toolchain name is an error.
local toolchain = pack.args["toolchain"]
if toolchain:sub(1, 1) == "-" or toolchain:find("[^%w._-]") then
    fail("toolchain `" .. toolchain .. "` is not a toolchain name for rustup "
        .. "(for example 1.90, 1.90.0 or nightly-2026-09-01)")
end

-- The rustup proxies (cargo, rustc, ...) read RUSTUP_HOME on each call.
-- Without it, they look in ~/.rustup and find no toolchain.
config.env = {
    RUSTUP_HOME = "/usr/local/rustup",
    CARGO_HOME = "/usr/local/cargo",
}

local rules = {
    ["rust-lang"] = {
        allow = {
            "static.rust-lang.org",
            "doc.rust-lang.org",
        },
    },
}
if pack.args["allow-cargo"] then
    rules["rust-packages"] = {
        allow = {
            "crates.io",
            "index.crates.io",
            "static.crates.io",
        },
    }
end
config.network = { rules = rules }
