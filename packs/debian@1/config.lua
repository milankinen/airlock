-- Debian: the sandbox image, and with `package-installs` the apt package
-- mirrors. The rule also has the Ubuntu mirrors, for a `vm.image` of the
-- Ubuntu family in a config file.

config.vm = { image = "debian:stable-slim" }

if pack.args["package-installs"] then
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
