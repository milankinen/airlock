#!/usr/bin/env bats

load helpers

# Named caches on the sandbox disk: refusal of cache paths that go through
# guest-made symlinks.

setup_file() {
    vm_setup_file
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
