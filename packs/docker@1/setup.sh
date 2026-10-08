# Docker: the engine, CLI, Buildx and Compose v2 from the distro
# packages. airlock starts dockerd as a daemon (config.toml) because no
# init system runs in the sandbox.
#
# Sources:
# - https://pkgs.alpinelinux.org/package/v3.24/community/x86_64/docker
#   (docker-engine, docker-cli, docker-cli-buildx)
# - https://pkgs.alpinelinux.org/package/v3.24/community/x86_64/docker-cli-compose
# - https://packages.debian.org/trixie/docker.io (dockerd only. The
#   client docker-cli is only recommended, and pkg_install does not
#   install recommended packages.)
# - https://packages.debian.org/trixie/docker-cli
# - https://packages.debian.org/trixie/docker-buildx
# - https://packages.debian.org/trixie/docker-compose (Compose v2.
#   Debian 12 has the Python Compose v1 under this name.)
# - https://packages.ubuntu.com/resolute/docker.io (includes the client)
# - https://packages.ubuntu.com/resolute/docker-compose-v2
# - https://github.com/docker/compose/blob/v1/README.md (v1 is end of life)
#
# No args.

airlock_steps 1
airlock_status "installing packages"
case "$DISTRO" in
    alpine)
        pkg_install docker-engine docker-cli docker-cli-buildx docker-cli-compose
        ;;
    debian)
        pkg_install docker.io ca-certificates
        # Debian 13 and later: the client is the separate docker-cli
        # package. Ubuntu and Debian 12 have it in docker.io.
        if ! command -v docker >/dev/null 2>&1; then
            pkg_install docker-cli
        fi
        # Buildx and Compose v2 are optional because older releases do
        # not have them. When the plugins are there, a new run does not
        # call apt.
        if ! docker buildx version </dev/null >/dev/null 2>&1 3>&-; then
            if pkg_available docker-buildx; then
                pkg_install docker-buildx
            fi
        fi
        if ! docker compose version </dev/null >/dev/null 2>&1 3>&-; then
            if pkg_available docker-compose-v2; then
                # Ubuntu
                pkg_install docker-compose-v2
            elif pkg_available docker-compose; then
                # Compose v2 on Debian 13 and later. On Debian 12 this is
                # the end-of-life Python Compose v1 (1.x). Skip that one.
                _candidate=$(apt-cache policy docker-compose 2>/dev/null 3>&- |
                    sed -n 's/^ *Candidate: *//p')
                case "$_candidate" in
                    1.*) log "skipping docker-compose $_candidate (Compose v1)" ;;
                    *) pkg_install docker-compose ;;
                esac
            fi
        fi
        ;;
esac

docker --version </dev/null 3>&- || fail 11 "docker --version failed"
dockerd --version </dev/null 3>&- || fail 11 "dockerd --version failed"
