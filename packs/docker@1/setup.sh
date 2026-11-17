# Docker engine, CLI, Buildx and Compose v2 from distro packages.
# config.lua starts dockerd (no init system in the sandbox).
#
# Packages:
# - https://pkgs.alpinelinux.org/package/v3.24/community/x86_64/docker
# - https://packages.debian.org/trixie/docker.io
# - https://packages.ubuntu.com/resolute/docker.io
# - https://github.com/docker/compose/blob/v1/README.md (v1 is EOL)

airlock_steps 1
airlock_status "installing packages"
case "$DISTRO" in
    alpine)
        airlock_pkg_install docker-engine docker-cli docker-cli-buildx docker-cli-compose
        ;;
    debian)
        airlock_pkg_install docker.io ca-certificates
        # Debian 13+ has the client in docker-cli. Ubuntu and Debian 12
        # have it in docker.io.
        if ! command -v docker >/dev/null 2>&1; then
            airlock_pkg_install docker-cli
        fi
        # Optional: old releases do not have Buildx or Compose v2.
        if ! docker buildx version </dev/null >/dev/null 2>&1 3>&-; then
            if airlock_pkg_available docker-buildx; then
                airlock_pkg_install docker-buildx
            fi
        fi
        if ! docker compose version </dev/null >/dev/null 2>&1 3>&-; then
            if airlock_pkg_available docker-compose-v2; then
                # Ubuntu
                airlock_pkg_install docker-compose-v2
            elif airlock_pkg_available docker-compose; then
                # Debian 13+: Compose v2. Debian 12: Compose v1, skip.
                _candidate=$(apt-cache policy docker-compose 2>/dev/null 3>&- |
                    sed -n 's/^ *Candidate: *//p')
                case "$_candidate" in
                    1.*) airlock_log "skipping docker-compose $_candidate (Compose v1)" ;;
                    *) airlock_pkg_install docker-compose ;;
                esac
            fi
        fi
        ;;
esac

docker --version </dev/null 3>&- || airlock_fail 11 "docker --version failed"
dockerd --version </dev/null 3>&- || airlock_fail 11 "dockerd --version failed"
