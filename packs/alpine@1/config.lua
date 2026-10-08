-- Alpine Linux: the sandbox image, and with `allow-apk` the apk
-- package mirrors.

config.vm = { image = "alpine:latest" }

if pack.args["allow-apk"] then
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
