-- Debian: the sandbox image, and with `allow-apt` the apt package
-- mirrors. The rule also has the Ubuntu mirrors, for a `vm.image` of the
-- Ubuntu family in a config file.

-- The cached image stays in use when the tag moves. The image settings
-- are explicit, thus an image table in a user file cannot change them.
config.vm = {
    image = { name = "debian:stable-slim", insecure = false, ["pull-policy"] = "if-not-present" },
}

if pack.args["allow-apt"] then
    config.network = {
        rules = {
            ["debian-packages"] = {
                allow = {
                    "deb.debian.org",
                    "security.debian.org",
                    "archive.ubuntu.com",
                    "security.ubuntu.com",
                    "ports.ubuntu.com",
                    "ppa.launchpad.net",
                    "keyserver.ubuntu.com",
                },
            },
        },
    }
end
