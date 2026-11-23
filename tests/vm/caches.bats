#!/usr/bin/env bats

load helpers

# Named caches on the sandbox disk: owner of the cache dirs and refusal of
# cache paths that go through guest-made symlinks.

setup_file() {
    vm_setup_file
}

# Test that a non-root image user can write in a named cache below the
# home directory, and that the data stays after a restart. A cache dir
# that root owns is not usable for this user. The image runs as the
# non-root user `curl_user`.
#   1. Make a project with the image and a cache for `~/.cache/x`
#   2. In the sandbox, write a file in the cache and check the cache owner
#   3. In a new sandbox, check that the file is still there
@test "named cache in home is writable by non-root user and persists" {
    mkdir -p cache-owner
    cd cache-owner
    printf '[vm]\nimage = "curlimages/curl:latest"\n\n[disk.cache.x]\npaths = ["~/.cache/x"]\n' >airlock.toml
    run_vm sh -c 'echo cache-data >~/.cache/x/file && echo "owner=$(stat -c %U ~/.cache/x)"'
    assert_success
    assert_output_contains "owner=curl_user"

    run_vm sh -c 'cat ~/.cache/x/file'
    cd ..
    rm -rf cache-owner
    assert_success
    assert_output_contains "cache-data"
}

# Test that a cache path in the project dir is skipped when the guest
# put a symlink in it. The guest can write the project dir, so a link
# could steer the cache mount to a dir of the image.
#   1. Make a project with a cache for `sub/cache`
#   2. In the sandbox, move `sub` away and make a new `sub` with the link
#      `cache` to `/usr/local/bin`, and save the content of the target
#   3. In a new sandbox, check that the target has the same content and
#      that nothing is mounted on it
@test "named cache path with symlink in project dir is skipped" {
    mkdir -p cache-link
    cd cache-link
    printf '[vm]\n\n[disk.cache.y]\npaths = ["sub/cache"]\n' >airlock.toml
    run_vm sh -c 'echo "md5=$(ls -A /usr/local/bin | sort | md5sum)"'
    assert_success
    local before
    before="$(echo "$output" | grep '^md5=')"

    # `sub/cache` is a mount point now, so it can not be removed. A rename
    # of its parent dir works.
    run_vm sh -c 'mv sub old && mkdir sub && ln -s /usr/local/bin sub/cache && echo link-ok'
    assert_success
    assert_output_contains "link-ok"

    run_vm sh -c 'echo "md5=$(ls -A /usr/local/bin | sort | md5sum)"
        echo "mounts=$(grep -c " /usr/local/bin " /proc/self/mountinfo)"'
    cd ..
    rm -rf cache-link
    assert_success
    assert_output_contains "$before"
    assert_output_contains "mounts=0"
}
