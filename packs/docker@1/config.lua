-- Runs dockerd in the VM. `allow-pulls` opens Docker Hub and GHCR.
-- Warning: these hosts also accept `docker push`.

if pack.args["allow-pulls"] then
    config.network = {
        rules = {
            ["docker-registries"] = {
                allow = {
                    -- Docker Hub
                    "registry-1.docker.io",
                    "auth.docker.io",
                    "production.cloudfront.docker.com",
                    "production.cloudflare.docker.com",
                    "docker-images-prod.6aa30f8b08e16409b46e0173d6de2f56.r2.cloudflarestorage.com",
                    -- GHCR
                    "ghcr.io",
                    "pkg-containers.githubusercontent.com",
                },
            },
        },
    }
end

-- Docker overlayfs cannot run on the VM overlayfs root. Thus bind
-- /var/lib/docker to the persistent ext4 disk. /run persists too, so
-- remove stale PID files: a reused PID stops dockerd with exit 1.
-- dockerd needs namespaces and cgroups, thus `harden = false`.
config.daemons = {
    dockerd = {
        command = {
            "sh", "-c",
            "mkdir -p /airlock/disk/docker /var/lib/docker && { grep -qs ' /var/lib/docker ' /proc/mounts || mount --bind /airlock/disk/docker /var/lib/docker; } && rm -f /var/run/docker.pid /var/run/docker/containerd/containerd.pid && exec dockerd",
        },
        harden = false,
        timeout = 10,
    },
}
