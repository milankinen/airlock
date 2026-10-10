-- Alpine Linux: the sandbox image, and with `allow-apk` the apk
-- package mirrors.

-- The image comes from the registry, not from a local image store. The
-- cached image stays in use when the tag moves. The image settings are
-- explicit, thus an image table in a user file cannot change them.
config.vm = {
    image = {
        name = "alpine:latest",
        resolution = "registry",
        insecure = false,
        ["pull-policy"] = "if-not-present",
    },
}

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
