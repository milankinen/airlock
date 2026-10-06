-- Alpine Linux: the sandbox image, and with `package-installs` the apk
-- package mirrors.

config.vm = { image = "alpine:latest" }

if pack.args["package-installs"] then
    config.network = {
        rules = {
            ["alpine-packages"] = {
                allow = {
                    "dl-cdn.alpinelinux.org",
                    "*.alpinelinux.org",
                },
            },
        },
    }
end
