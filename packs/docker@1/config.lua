-- Docker engine inside the VM: starts `dockerd` as a daemon. With
-- `allow-pulls`, image pulls from Docker Hub and GitHub Container
-- Registry.
--
-- Registries accept pushes as well as pulls. With `allow-pulls`, a
-- sandboxed process can log in to any account and `docker push` data out
-- through these hosts.

if pack.args["allow-pulls"] then
    config.network = {
        rules = {
            ["docker-registries"] = {
                allow = {
                    -- Docker Hub: registry API, token service and blob
                    -- storage CDNs
                    "registry-1.docker.io",
                    "auth.docker.io",
                    "production.cloudfront.docker.com",
                    "production.cloudflare.docker.com",
                    "docker-images-prod.6aa30f8b08e16409b46e0173d6de2f56.r2.cloudflarestorage.com",
                    -- GitHub Container Registry and its blob storage
                    "ghcr.io",
                    "pkg-containers.githubusercontent.com",
                },
            },
        },
    }
end

-- The overlayfs storage driver of Docker cannot run on top of the
-- overlayfs root of the VM. Thus /var/lib/docker is a bind mount from the
-- ext4 disk at /airlock/disk. This also keeps images and build cache
-- across sandbox restarts. A daemon restart skips the bind mount when it
-- is already in place. `harden = false` because dockerd must create
-- namespaces and manage cgroups, and hardening blocks that.
config.daemons = {
    dockerd = {
        command = {
            "sh", "-c",
            "mkdir -p /airlock/disk/docker /var/lib/docker && { grep -qs ' /var/lib/docker ' /proc/mounts || mount --bind /airlock/disk/docker /var/lib/docker; } && exec dockerd",
        },
        harden = false,
        timeout = 10,
    },
}
