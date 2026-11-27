#!/usr/bin/env bats

load helpers

# Changes of the configured image on an existing sandbox.

setup_file() {
    vm_setup_file
}

# Use the image $1 from the registry.
use_image() {
    write_config "[vm.image]
name = \"$1\"
resolution = \"registry\""
}

# Test that another tag of the same repository is an image change. The old
# digest also exists in that repository, so a start that uses it runs the
# old image and reports the new tag.
#   1. Start with alpine:3.22 and write a file to the sandbox disk
#   2. Change the tag to 3.23 and check that start without --yes refuses
#   3. Start with --yes and check that Alpine is 3.23 and the disk is new
@test "changed image tag in same repository re-creates sandbox" {
    local print='. /etc/os-release && echo "version=$VERSION_ID"'
    use_image "alpine:3.22"
    run_vm sh -c "$print && touch /root/marker"
    assert_success
    assert_output_contains "version=3.22."

    use_image "alpine:3.23"
    run_airlock start --allow-configuration-changes -- sh -c "$print"
    assert_failure 2
    assert_output_contains "sandbox image has been changed"

    run_airlock start --yes --allow-configuration-changes -- \
        sh -c "$print && test ! -e /root/marker"
    assert_success
    assert_output_contains "version=3.23."
}
