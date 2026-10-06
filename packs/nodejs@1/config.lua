-- Node.js: Node.js trusts only its own CA list; point it at the system
-- bundle, which has the airlock CA. With `npm-installs`, the npm and
-- Yarn registries.

-- The setup script gives the version to `nvm install`: an option there
-- (a leading `-`) or a character outside a version spec is an error.
local version = pack.args["node-version"]
if version:sub(1, 1) == "-" or version:find("[^%w./*_-]") then
    fail("node-version `" .. version .. "` is not a version for nvm "
        .. "(for example 22, 22.11.0 or lts/jod)")
end

config.env = {
    NODE_EXTRA_CA_CERTS = "/etc/ssl/certs/ca-certificates.crt",
}

if pack.args["npm-installs"] then
    config.network = {
        rules = {
            ["nodejs-packages"] = {
                allow = {
                    "registry.npmjs.org",
                    "registry.yarnpkg.com",
                },
            },
        },
    }
end
