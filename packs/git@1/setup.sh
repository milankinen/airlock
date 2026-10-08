# Git: Git from the distro packages, plus a CA bundle for HTTPS remotes
# and an SSH client for SSH remotes. On Debian also less. There, git only
# recommends these packages, and pkg_install does not install recommended
# packages.
#
# Sources:
# - https://pkgs.alpinelinux.org/package/v3.24/main/x86_64/git
#   (HTTPS through libcurl, which depends on ca-certificates-bundle.
#   ca-certificates adds update-ca-certificates, as in the other packs.
#   busybox in alpine:latest has the less and vi applets.)
# - https://pkgs.alpinelinux.org/package/v3.24/main/x86_64/openssh-client-default
#   (provides the name "openssh-client")
# - https://packages.debian.org/trixie/git (Recommends ca-certificates,
#   patch, less, ssh-client)
#
# No args.

airlock_steps 1
airlock_status "installing packages"
case "$DISTRO" in
    # "openssh-client" is a virtual name on Alpine. apk picks
    # openssh-client-default, and `apk info -e` matches any provider
    # (also openssh-client-krb5, which conflicts with -default).
    alpine) pkg_install git ca-certificates openssh-client ;;
    # debian:stable-slim has a pager (more of util-linux), but less is
    # better. No editor is installed. Thus `git commit` without -m needs
    # GIT_EDITOR.
    debian) pkg_install git ca-certificates less openssh-client ;;
esac

git --version </dev/null 3>&- || fail 11 "git --version failed"
ssh -V </dev/null 3>&- || fail 11 "ssh -V failed"
